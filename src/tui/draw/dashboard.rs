use std::collections::HashMap;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::ListItem;

use crate::core::PackageStatus;
use crate::format_bytes;
use crate::tui::app::Popup;
use crate::tui::dashboard::{
    DashboardChrome, DashboardFileRow, DashboardFileStatus, DashboardPackageRow, DashboardRow,
    DownloadDashboardState, aggregate_transfer_label as dashboard_transfer_label,
    file_detail as dashboard_file_detail,
};

pub(super) fn dashboard_row_items(
    state: &DownloadDashboardState,
    selected: Option<usize>,
    content_width: usize,
) -> Vec<ListItem<'static>> {
    let packages: HashMap<&str, &DashboardPackageRow> = state
        .packages
        .iter()
        .map(|package| (package.id.as_str(), package))
        .collect();
    let files: HashMap<&str, &DashboardFileRow> = state
        .files
        .iter()
        .map(|file| (file.id.as_str(), file))
        .collect();

    state
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| match row {
            DashboardRow::Package { package_id } => packages
                .get(package_id.as_str())
                .map(|package| {
                    dashboard_package_item(package, selected == Some(index), content_width)
                })
                .unwrap_or_else(|| ListItem::new(Line::from(""))),
            DashboardRow::File {
                package_id,
                file_id,
            } => files
                .get(file_id.as_str())
                .map(|file| {
                    dashboard_file_item(
                        file,
                        package_id.is_empty() && !file.package_id.is_empty(),
                        selected == Some(index),
                        content_width,
                    )
                })
                .unwrap_or_else(|| ListItem::new(Line::from(""))),
        })
        .collect()
}

fn dashboard_file_item(
    file: &DashboardFileRow,
    include_package: bool,
    selected: bool,
    content_width: usize,
) -> ListItem<'static> {
    let (icon, color) = match &file.status {
        DashboardFileStatus::Downloading => ("\u{25cf}", Color::Yellow),
        DashboardFileStatus::Queued => ("\u{25cb}", Color::DarkGray),
        DashboardFileStatus::Complete => ("\u{2713}", Color::Green),
        DashboardFileStatus::Error { .. } => ("\u{2717}", Color::Red),
    };
    let detail_color = match &file.status {
        DashboardFileStatus::Downloading => Color::Yellow,
        _ => Color::DarkGray,
    };
    let prefix_label = if include_package {
        file.package_label
            .as_deref()
            .map(|label| format!("[{}] ", compact_label(label)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let detail = dashboard_file_detail(file);
    let prefix = format!("   {icon} ");
    let prefix_width = text_width(&prefix);
    let detail_width = text_width(&detail).min(content_width / 2);
    let detail = truncate_end(&detail, detail_width);
    let detail_width = text_width(&detail);
    let name = truncate_end(
        &format!("{prefix_label}{}", file.name),
        content_width
            .saturating_sub(prefix_width)
            .saturating_sub(detail_width)
            .saturating_sub(1),
    );
    let name_width = text_width(&name);
    let filler = " ".repeat(
        content_width
            .saturating_sub(prefix_width)
            .saturating_sub(name_width)
            .saturating_sub(detail_width),
    );
    let mut row_style = Style::default().fg(color);
    if selected {
        row_style = row_style.add_modifier(Modifier::BOLD);
    }
    ListItem::new(Line::from(vec![
        Span::styled(format!("{prefix}{name}"), row_style),
        Span::raw(filler),
        Span::styled(detail, Style::default().fg(detail_color)),
    ]))
}

fn dashboard_package_item(
    package: &DashboardPackageRow,
    selected: bool,
    content_width: usize,
) -> ListItem<'static> {
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
    let prefix = format!(" {marker} {icon} ");
    let prefix_width = text_width(&prefix);
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
    let filler = " ".repeat(
        content_width
            .saturating_sub(prefix_width)
            .saturating_sub(name_width)
            .saturating_sub(detail_width),
    );
    let mut row_style = Style::default().fg(color);
    if selected {
        row_style = row_style.add_modifier(Modifier::BOLD);
    }
    ListItem::new(Line::from(vec![
        Span::styled(format!("{prefix}{name}"), row_style),
        Span::raw(filler),
        Span::styled(detail, Style::default().fg(Color::DarkGray)),
    ]))
}

fn dashboard_package_detail(
    package: &DashboardPackageRow,
    speed_label: &str,
    content_width: usize,
) -> String {
    let full = format!(
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
    let compact = format!(
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
            format!("{error_count} failed"),
            Style::default().fg(Color::Red),
        )];
    }

    if width <= 32 && downloading > 0 {
        let activity = format!("Dl {downloading}, {queued} q");
        let failure = (error_count > 0).then(|| format!("{error_count} failed"));
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
        let error_text = selected_error.unwrap_or_else(|| format!("{error_count} failed"));
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
        return format!("Downloading {downloading} file(s), {queued} queued");
    }
    if state.totals.files_total > 0 {
        return format!(
            "Queued {} file(s), {}/{} complete",
            queued, state.totals.files_completed, state.totals.files_total
        );
    }
    format!("Queued {} file(s)", state.files.len())
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
    } else if width >= 86 {
        "a:add  up/down:select  enter:open  s:sort  d:del  r:retry  R:reset  c:cfg  q:quit"
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
    let bytes = format!(
        "{} / {}",
        format_bytes(state.totals.total_downloaded),
        format_bytes(state.totals.total_size)
    );
    let transfer = dashboard_transfer_label(state);
    let full = format!(
        "{pct}%  {}/{} files  {bytes}  {transfer}",
        state.totals.files_completed, state.totals.files_total
    );
    if full.chars().count() <= usize::from(width.saturating_sub(2)) {
        return full;
    }
    let compact = format!(
        "{pct}%  {}/{}  {transfer}",
        state.totals.files_completed, state.totals.files_total
    );
    if compact.chars().count() <= usize::from(width.saturating_sub(2)) {
        return compact;
    }
    truncate_end(
        &format!("{pct}%  {transfer}"),
        usize::from(width.saturating_sub(2)),
    )
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

fn package_status_style(status: PackageStatus, percent: u64) -> (&'static str, Color) {
    match status {
        PackageStatus::Downloading => (package_progress_icon(percent), Color::Yellow),
        PackageStatus::Failed => ("\u{2717}", Color::Red),
        PackageStatus::Complete => ("\u{2713}", Color::Green),
        PackageStatus::Partial => (package_progress_icon(percent), Color::Yellow),
        PackageStatus::Queued | PackageStatus::Pending => ("\u{25cb}", Color::DarkGray),
        PackageStatus::Skipped | PackageStatus::Deleted => ("\u{2715}", Color::DarkGray),
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

fn mega_url_label(value: &str) -> Option<String> {
    let marker = "mega.nz/";
    let start = value.find(marker)? + marker.len();
    let path = &value[start..];
    let mut parts = path.split(['/', '#']);
    match (parts.next(), parts.next()) {
        (Some("folder"), Some(id)) if !id.is_empty() => Some(format!("Folder {id}")),
        (Some("file"), Some(id)) if !id.is_empty() => Some(format!("File {id}")),
        _ => None,
    }
}

fn compact_label(value: &str) -> String {
    value
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(value)
        .to_string()
}

pub(super) fn truncate_end(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if value.is_ascii() {
        if value.len() <= max_chars {
            return value.to_string();
        }
        if max_chars <= 1 {
            return "\u{2026}".to_string();
        }
        let mut truncated = value[..max_chars.saturating_sub(1)].to_string();
        truncated.push('\u{2026}');
        return truncated;
    }
    if text_width(value) <= max_chars {
        return value.to_string();
    }
    if max_chars <= 1 {
        return "\u{2026}".to_string();
    }
    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('\u{2026}');
    truncated
}

fn text_width(value: &str) -> usize {
    if value.is_ascii() {
        value.len()
    } else {
        value.chars().count()
    }
}
