use std::borrow::Cow;
use std::fmt::Write as _;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, ListState};
use rustc_hash::FxHashMap;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::core::PackageStatus;
use crate::format_bytes;
use crate::tui::app::Popup;
use crate::tui::dashboard::{
    DashboardChrome, DashboardFileRow, DashboardFileStatus, DashboardPackageRow, DashboardRow,
    DashboardUiMode, DownloadDashboardState, aggregate_transfer_label as dashboard_transfer_label,
    clamp_selection, file_detail as dashboard_file_detail,
};

pub(super) fn draw_dashboard_file_list(
    frame: &mut Frame,
    state: &DownloadDashboardState,
    list_state: &mut ListState,
    area: Rect,
) {
    clamp_selection(list_state, state.rows.len());
    let block = Block::default().borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 || state.rows.is_empty() {
        return;
    }

    let selected = list_state.selected();
    let visible_height = usize::from(inner.height);
    if let Some(selected) = selected {
        let offset = list_state.offset_mut();
        if selected < *offset {
            *offset = selected;
        } else if selected >= offset.saturating_add(visible_height) {
            *offset = selected.saturating_sub(visible_height.saturating_sub(1));
        }
    }
    let offset = list_state.offset().min(state.rows.len().saturating_sub(1));
    *list_state.offset_mut() = offset;

    let render_width = usize::from(inner.width);
    let content_width = usize::from(area.width.saturating_sub(4));
    let blank = " ".repeat(render_width);
    let packages_by_id = state
        .packages
        .iter()
        .map(|package| (package.id.clone(), package))
        .collect::<FxHashMap<_, _>>();
    let files_by_id = state
        .files
        .iter()
        .map(|file| (file.id.clone(), file))
        .collect::<FxHashMap<_, _>>();
    for (line, row_index) in (offset..state.rows.len()).take(visible_height).enumerate() {
        let y = inner.y + u16::try_from(line).unwrap_or(0);
        let selected_row = selected == Some(row_index);
        frame.buffer_mut().set_stringn(
            inner.x,
            y,
            &blank,
            render_width,
            highlight_fill_style(selected_row),
        );

        match &state.rows[row_index] {
            DashboardRow::Package { package_id } => {
                if let Some(package) = packages_by_id.get(package_id) {
                    render_dashboard_package_row(
                        frame,
                        package,
                        inner.x,
                        y,
                        content_width,
                        selected_row,
                    );
                }
            }
            DashboardRow::File {
                package_id,
                file_id,
            } => {
                if let Some(file) = files_by_id.get(file_id) {
                    render_dashboard_file_row(
                        frame,
                        file,
                        package_id.is_empty() && !file.package_id.is_empty(),
                        inner.x,
                        y,
                        content_width,
                        selected_row,
                    );
                }
            }
        }
    }
}

fn render_dashboard_file_row(
    frame: &mut Frame,
    file: &DashboardFileRow,
    include_package: bool,
    x: u16,
    y: u16,
    content_width: usize,
    selected: bool,
) {
    let color = match &file.status {
        DashboardFileStatus::Downloading => Color::Yellow,
        DashboardFileStatus::Verifying => Color::Blue,
        DashboardFileStatus::Queued => Color::DarkGray,
        DashboardFileStatus::Complete => Color::Green,
        DashboardFileStatus::Error { .. } => Color::Red,
    };
    let detail_color = match &file.status {
        DashboardFileStatus::Downloading => Color::Yellow,
        DashboardFileStatus::Verifying => Color::Blue,
        _ => Color::DarkGray,
    };
    let prefix = match &file.status {
        DashboardFileStatus::Downloading | DashboardFileStatus::Verifying => "   \u{25cf} ",
        DashboardFileStatus::Queued => "   \u{25cb} ",
        DashboardFileStatus::Complete => "   \u{2713} ",
        DashboardFileStatus::Error { .. } => "   \u{2717} ",
    };
    let prefix_width = 5;
    let detail = dashboard_file_detail(file);
    let detail_width = text_width(&detail).min(content_width / 2);
    let detail = truncate_end(&detail, detail_width);
    let display_name = if include_package {
        file.package_label.as_deref().map_or_else(
            || Cow::Borrowed(file.name.as_str()),
            |label| Cow::Owned(package_prefix_label(label) + &file.name),
        )
    } else {
        Cow::Borrowed(file.name.as_str())
    };
    let name = truncate_end(
        display_name.as_ref(),
        content_width
            .saturating_sub(prefix_width)
            .saturating_sub(text_width(detail.as_ref()))
            .saturating_sub(1),
    );
    let name_width = text_width(name.as_ref());
    let filler_width = content_width
        .saturating_sub(prefix_width)
        .saturating_sub(name_width)
        .saturating_sub(text_width(detail.as_ref()));
    let row_style = highlight_style(color, selected);
    let detail_style = highlight_style(detail_color, selected);
    let mut cursor = x;
    write_text(frame, &mut cursor, y, prefix, prefix_width, row_style);
    write_text(frame, &mut cursor, y, name.as_ref(), name_width, row_style);
    cursor = cursor.saturating_add(u16::try_from(filler_width).unwrap_or(u16::MAX));
    write_text(
        frame,
        &mut cursor,
        y,
        detail.as_ref(),
        text_width(detail.as_ref()),
        detail_style,
    );
}

fn render_dashboard_package_row(
    frame: &mut Frame,
    package: &DashboardPackageRow,
    x: u16,
    y: u16,
    content_width: usize,
    selected: bool,
) {
    let (icon, color) = package_status_style(package.status, package.percent);
    let marker = if package.present_files > 1 {
        if package.expanded { "-" } else { "+" }
    } else {
        " "
    };
    let speed_label = if matches!(package.status, PackageStatus::Downloading) {
        "active"
    } else {
        ""
    };
    let detail = dashboard_package_detail(package, speed_label, content_width);
    let prefix_width = 5;
    let detail_width = text_width(&detail).min(content_width / 2);
    let detail = truncate_end(&detail, detail_width);
    let detail_width = text_width(&detail);
    let name = truncate_end(
        &display_dashboard_package_name(package),
        content_width
            .saturating_sub(prefix_width)
            .saturating_sub(detail_width)
            .saturating_sub(1),
    );
    let name_width = text_width(&name);
    let filler_width = content_width
        .saturating_sub(prefix_width)
        .saturating_sub(name_width)
        .saturating_sub(detail_width);
    let row_style = highlight_style(color, selected);
    let detail_style = highlight_style(Color::DarkGray, selected);
    let mut cursor = x;
    write_text(frame, &mut cursor, y, " ", 1, row_style);
    write_text(frame, &mut cursor, y, marker, 1, row_style);
    write_text(frame, &mut cursor, y, " ", 1, row_style);
    write_text(frame, &mut cursor, y, icon, 1, row_style);
    write_text(frame, &mut cursor, y, " ", 1, row_style);
    write_text(frame, &mut cursor, y, name.as_ref(), name_width, row_style);
    cursor = cursor.saturating_add(u16::try_from(filler_width).unwrap_or(u16::MAX));
    write_text(
        frame,
        &mut cursor,
        y,
        detail.as_ref(),
        detail_width,
        detail_style,
    );
}

fn write_text(frame: &mut Frame, x: &mut u16, y: u16, text: &str, width: usize, style: Style) {
    if text.is_empty() || width == 0 {
        return;
    }
    frame.buffer_mut().set_stringn(*x, y, text, width, style);
    *x = x.saturating_add(u16::try_from(width).unwrap_or(u16::MAX));
}

fn highlight_fill_style(selected: bool) -> Style {
    if selected {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn highlight_style(color: Color, selected: bool) -> Style {
    let style = Style::default().fg(color);
    if selected {
        style.bg(Color::DarkGray).add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn dashboard_package_detail(
    package: &DashboardPackageRow,
    speed_label: &str,
    content_width: usize,
) -> String {
    let mut full = String::with_capacity(72 + speed_label.len());
    let _ = write!(
        full,
        "{}/{} files  {} / {}  {:>3}%  {speed_label}",
        package.completed_files,
        package.present_files,
        format_bytes(package.downloaded_bytes),
        format_bytes(package.total_bytes),
        package.percent
    );
    if text_width(&full) <= content_width / 2 {
        return full;
    }
    let mut compact = String::with_capacity(48 + speed_label.len());
    let _ = write!(
        compact,
        "{}/{}  {}  {:>3}%  {speed_label}",
        package.completed_files,
        package.present_files,
        format_bytes(package.total_bytes),
        package.percent
    );
    truncate_end(&compact, content_width / 2)
}

fn display_dashboard_package_name(package: &DashboardPackageRow) -> String {
    if !package.display_name.starts_with("http://") && !package.display_name.starts_with("https://")
    {
        return compact_label(&package.display_name);
    }
    if let Some(label) = &package.folder_label {
        return label.clone();
    }
    if let Some(label) = mega_url_label(&package.display_name) {
        return label;
    }
    compact_label(
        package
            .display_name
            .split('#')
            .next()
            .unwrap_or(&package.display_name),
    )
}

fn package_prefix_label(label: &str) -> String {
    let compact = compact_label(label);
    let mut prefixed = String::with_capacity(compact.len() + 3);
    prefixed.push('[');
    prefixed.push_str(&compact);
    prefixed.push_str("] ");
    prefixed
}

pub(super) fn dashboard_status_line(
    state: &DownloadDashboardState,
    width: u16,
    selected: Option<usize>,
) -> Vec<Span<'static>> {
    let status = dashboard_effective_status(state);
    let error_count = state
        .files
        .iter()
        .filter(|file| file.status.is_error())
        .count();
    let selected_error = selected.and_then(|index| selected_error_message(state, index));
    let downloading = state
        .files
        .iter()
        .filter(|file| file.status.is_downloading())
        .count();
    let queued = state
        .files
        .iter()
        .filter(|file| file.status.is_queued())
        .count();
    let width = usize::from(width);

    if width <= 16 && error_count > 0 {
        return vec![Span::styled(
            failed_count_label(error_count),
            Style::default().fg(Color::Red),
        )];
    }

    if width <= 32 && downloading > 0 {
        let activity = compact_activity_label(downloading, queued);
        let failure = (error_count > 0).then(|| failed_count_label(error_count));
        if let Some(failure) = failure {
            return vec![
                Span::styled(activity, Style::default().fg(Color::Cyan)),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                Span::styled(failure, Style::default().fg(Color::Red)),
            ];
        }
        return vec![Span::styled(activity, Style::default().fg(Color::Cyan))];
    }

    let mut parts = Vec::new();
    if state.authenticated {
        parts.push(Span::styled(
            "Logged in \u{2713}",
            Style::default().fg(Color::Green),
        ));
    } else if state.logging_in {
        parts.push(Span::styled(
            "Logging in...",
            Style::default().fg(Color::Yellow),
        ));
    }
    if !status.is_empty() {
        if !parts.is_empty() {
            parts.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
        }
        parts.push(Span::styled(
            truncate_end(&status, width.saturating_sub(12)),
            Style::default().fg(Color::Cyan),
        ));
    }
    if error_count > 0 {
        if !parts.is_empty() {
            parts.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
        }
        let error_text = selected_error.unwrap_or_else(|| failed_count_label(error_count));
        parts.push(Span::styled(
            truncate_end(&error_text, width.saturating_sub(12)),
            Style::default().fg(Color::Red),
        ));
    }
    parts
}

fn selected_error_message(state: &DownloadDashboardState, index: usize) -> Option<String> {
    let row = state.rows.get(index)?;
    match row {
        DashboardRow::File { file_id, .. } => state
            .files
            .iter()
            .find(|file| file.id == *file_id)
            .and_then(|file| match &file.status {
                DashboardFileStatus::Error { message } => Some(message.clone()),
                _ => None,
            }),
        DashboardRow::Package { package_id } => state
            .packages
            .iter()
            .find(|package| package.id == *package_id)
            .and_then(|package| package.error.clone()),
    }
}

fn dashboard_effective_status(state: &DownloadDashboardState) -> String {
    if !is_processing_status(&state.status) || state.files.is_empty() {
        return state.status.clone();
    }
    let downloading = state
        .files
        .iter()
        .filter(|file| file.status.is_downloading())
        .count();
    let queued = state
        .files
        .iter()
        .filter(|file| file.status.is_queued())
        .count();
    if downloading > 0 {
        let mut status = String::with_capacity(40);
        let _ = write!(status, "Downloading {downloading} file(s), {queued} queued");
        return status;
    }
    if state.totals.files_total > 0 {
        let mut status = String::with_capacity(40);
        let _ = write!(
            status,
            "Queued {} file(s), {}/{} complete",
            queued, state.totals.files_completed, state.totals.files_total
        );
        return status;
    }
    let mut status = String::with_capacity(24);
    let _ = write!(status, "Queued {} file(s)", state.files.len());
    status
}

fn failed_count_label(error_count: usize) -> String {
    let mut label = String::with_capacity(16);
    let _ = write!(label, "{error_count} failed");
    label
}

fn compact_activity_label(downloading: usize, queued: usize) -> String {
    let mut label = String::with_capacity(16);
    let _ = write!(label, "Dl {downloading}, {queued} q");
    label
}

pub(super) fn controls_label_from_snapshot(
    state: &DownloadDashboardState,
    chrome: &DashboardChrome<'_>,
    width: u16,
) -> String {
    let text = if chrome.url_input_active {
        if width >= 34 {
            "enter:add  esc:cancel  paste:ok"
        } else if width >= 24 {
            "enter:add  esc:cancel"
        } else if width >= 14 {
            "enter:add  esc"
        } else {
            "esc"
        }
    } else if state.popup != Popup::None {
        "esc:close"
    } else if state.ui_mode == DashboardUiMode::Attached {
        if width >= 80 {
            "up/down:select  p:pause  d:del  r:retry  alt-r:verify  R:reset  q:quit"
        } else if width >= 52 {
            "up/down:select  p:pause  d:del  r:retry  R:reset  q:quit"
        } else if width >= 32 {
            "up/down:select  p:pause  d:del  r:retry  q:quit"
        } else {
            "p:pause  d:del  r:retry  q:quit"
        }
    } else if width >= 100 {
        "a:add  up/down:select  enter:open  s:sort  d:del  r:retry  alt-r:verify  R:reset  c:cfg  q:quit"
    } else if width >= 86 {
        "a:add  up/down:select  enter:open  d:del  r:retry  alt-r:verify  R:reset  q:quit"
    } else if width >= 58 {
        "a:add  enter:open  s:sort  d:del  r:retry  q:quit"
    } else if width >= 40 {
        "a:add  enter:open  d:del  q:quit"
    } else if width >= 18 {
        "a:add  q:quit"
    } else {
        "q:quit"
    };
    truncate_end(text, usize::from(width))
}

pub(super) fn dashboard_aggregate_progress_label(
    state: &DownloadDashboardState,
    pct: u16,
    width: u16,
) -> String {
    let mut bytes = String::with_capacity(32);
    let _ = write!(
        bytes,
        "{} / {}",
        format_bytes(state.totals.total_downloaded),
        format_bytes(state.totals.total_size)
    );
    let transfer = dashboard_transfer_label(state);
    let mut full = String::with_capacity(32 + bytes.len() + transfer.len());
    let _ = write!(
        full,
        "{pct}%  {}/{} files  {bytes}  {transfer}",
        state.totals.files_completed, state.totals.files_total
    );
    if full.chars().count() <= usize::from(width.saturating_sub(2)) {
        return full;
    }
    let mut compact = String::with_capacity(20 + transfer.len());
    let _ = write!(
        compact,
        "{pct}%  {}/{}  {transfer}",
        state.totals.files_completed, state.totals.files_total
    );
    if compact.chars().count() <= usize::from(width.saturating_sub(2)) {
        return compact;
    }
    let mut shortest = String::with_capacity(6 + transfer.len());
    let _ = write!(shortest, "{pct}%  {transfer}");
    truncate_end(&shortest, usize::from(width.saturating_sub(2)))
}

pub(super) fn focused_url_input_view(
    value: &str,
    cursor: usize,
    width: u16,
) -> (String, Option<u16>) {
    if width == 0 {
        return (String::new(), None);
    }

    let visible_width = usize::from(width.saturating_sub(1));
    if visible_width == 0 {
        return (String::new(), Some(0));
    }

    let char_count = value.chars().count();
    let cursor = cursor.min(char_count);
    if char_count <= visible_width {
        return (value.to_string(), Some(cursor as u16));
    }

    let start = cursor.saturating_sub(visible_width);
    (
        value.chars().skip(start).take(visible_width).collect(),
        Some((cursor - start) as u16),
    )
}

fn is_processing_status(status: &str) -> bool {
    status.starts_with("Processing ")
}

pub(super) fn package_status_style(status: PackageStatus, percent: u64) -> (&'static str, Color) {
    match status {
        PackageStatus::Downloading => (package_progress_icon(percent), Color::Yellow),
        PackageStatus::Failed => ("\u{2717}", Color::Red),
        PackageStatus::Complete => ("\u{2713}", Color::Green),
        PackageStatus::Partial => (package_progress_icon(percent), Color::Yellow),
        PackageStatus::Queued | PackageStatus::Pending => ("\u{25cb}", Color::DarkGray),
    }
}

fn package_progress_icon(percent: u64) -> &'static str {
    match percent {
        0 => "\u{25cb}",
        1..=24 => "\u{25d4}",
        25..=74 => "\u{25d1}",
        75..=99 => "\u{25d5}",
        _ => "\u{25cf}",
    }
}

pub(super) fn mega_url_label(value: &str) -> Option<String> {
    let marker = "mega.nz/";
    let start = value.find(marker)? + marker.len();
    let path = &value[start..];
    let mut parts = path.split(['/', '#']);
    match (parts.next(), parts.next()) {
        (Some("folder"), Some(id)) if !id.is_empty() => {
            let mut label = String::with_capacity("Folder ".len() + id.len());
            label.push_str("Folder ");
            label.push_str(id);
            Some(label)
        }
        (Some("file"), Some(id)) if !id.is_empty() => {
            let mut label = String::with_capacity("File ".len() + id.len());
            label.push_str("File ");
            label.push_str(id);
            Some(label)
        }
        _ => None,
    }
}

pub(super) fn compact_label(value: &str) -> String {
    value
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(value)
        .to_string()
}

pub(super) fn truncate_end(value: &str, max_width: usize) -> String {
    truncate_end_cow(value, max_width).into_owned()
}

pub(super) fn truncate_end_cow(value: &str, max_width: usize) -> Cow<'_, str> {
    if max_width == 0 {
        return Cow::Borrowed("");
    }
    if value.is_ascii() {
        if value.len() <= max_width {
            return Cow::Borrowed(value);
        }
        if max_width <= 1 {
            return Cow::Borrowed("\u{2026}");
        }
        let mut truncated = value[..max_width.saturating_sub(1)].to_string();
        truncated.push('\u{2026}');
        return Cow::Owned(truncated);
    }
    if text_width(value) <= max_width {
        return Cow::Borrowed(value);
    }
    if max_width <= 1 {
        return Cow::Borrowed("\u{2026}");
    }
    let mut width = 0_usize;
    let mut truncated = String::new();
    for ch in value.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width.saturating_add(ch_width) > max_width.saturating_sub(1) {
            break;
        }
        width = width.saturating_add(ch_width);
        truncated.push(ch);
    }
    truncated.push('\u{2026}');
    Cow::Owned(truncated)
}

pub(super) fn text_width(value: &str) -> usize {
    if value.is_ascii() {
        value.len()
    } else {
        UnicodeWidthStr::width(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{text_width, truncate_end};

    #[test]
    fn text_width_uses_unicode_display_width() {
        assert_eq!(text_width("表"), 2);
        assert_eq!(text_width("a表"), 3);
    }

    #[test]
    fn truncate_end_respects_unicode_display_width() {
        assert_eq!(truncate_end("表x", 2), "…");
        assert_eq!(truncate_end("ab表cd", 4), "ab…");
        assert_eq!(text_width(&truncate_end("a表cd", 4)), 4);
    }
}
