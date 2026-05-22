//! All drawing / rendering functions.

mod dashboard;
mod popup;

use std::borrow::Cow;
use std::fmt::Write as _;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, Paragraph};

use crate::core::{FileLifecycle, PackageStatus};
use crate::format_bytes;

use self::dashboard::{
    compact_label, controls_label_from_snapshot, dashboard_aggregate_progress_label,
    dashboard_row_items, dashboard_status_line, focused_url_input_view, mega_url_label,
    package_status_style, text_width, truncate_end, truncate_end_cow,
};
use super::app::{App, FileEntry, FileStatus, Popup};
use super::dashboard::{DashboardChrome, DashboardUiMode, DownloadDashboardState, clamp_selection};
use super::visible::TuiRow;

pub fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    draw_interactive_dashboard(frame, app);
    match app.popup {
        Popup::None => {}
        Popup::Login => popup::draw_login_popup(frame, app),
        Popup::Config => popup::draw_config_popup(frame, app),
        Popup::Confirm => popup::draw_confirm_popup(frame, app),
        Popup::Sort => popup::draw_sort_popup(frame, app),
    }
}

fn draw_interactive_dashboard(frame: &mut ratatui::Frame, app: &mut App) {
    app.ensure_visible_rows_cache();
    let area = frame.area();
    let ram = format_bytes(app.memory_rss);
    let mut title_right = String::with_capacity(32 + ram.len());
    let _ = write!(
        title_right,
        " {}% CPU | {ram} RAM | API: {}",
        (app.cpu_usage as u16).min(999),
        app.api_port,
    );
    if app.paused {
        title_right.push_str(" | PAUSED");
    }

    let outer = Block::default()
        .title(" octo-dl ")
        .title_alignment(Alignment::Left)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if app.paused {
            Color::Yellow
        } else {
            Color::Cyan
        }));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let right_x = area
        .x
        .saturating_add(area.width)
        .saturating_sub(u16::try_from(title_right.len()).unwrap_or(u16::MAX) + 1);
    if right_x > area.x + 1 {
        frame.render_widget(
            Paragraph::new(title_right).style(Style::default().fg(if app.paused {
                Color::Yellow
            } else {
                Color::Cyan
            })),
            Rect::new(
                right_x,
                area.y,
                area.width.saturating_sub(right_x - area.x),
                1,
            ),
        );
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    if app.popup == Popup::None && app.url_input_active {
        let url_block = Block::default()
            .title(" Add URL(s): editing ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));
        let url_inner = url_block.inner(chunks[0]);
        let (url_value, cursor_col) =
            focused_url_input_view(&app.url_input, app.url_input_cursor, url_inner.width);
        frame.render_widget(
            Paragraph::new(url_value)
                .block(url_block)
                .style(Style::default().fg(Color::White)),
            chunks[0],
        );
        if let Some(cursor_col) = cursor_col
            && url_inner.height > 0
        {
            frame.set_cursor_position(Position::new(url_inner.x + cursor_col, url_inner.y));
        }
    } else {
        render_aggregate_progress_app(frame, app, chunks[0]);
    }

    draw_dashboard_file_list_app(frame, app, chunks[1]);

    let status_line = Paragraph::new(Line::from(dashboard_status_line_app(
        app,
        chunks[2].width,
        app.file_list_state.selected(),
    )))
    .style(Style::default().fg(Color::White));
    frame.render_widget(status_line, chunks[2]);

    let controls = controls_label_app(app, chunks[3].width);
    let controls_bar = Paragraph::new(controls)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(controls_bar, chunks[3]);
}

pub fn draw_dashboard(
    frame: &mut ratatui::Frame,
    state: &DownloadDashboardState,
    chrome: &DashboardChrome<'_>,
    list_state: &mut ratatui::widgets::ListState,
) {
    let area = frame.area();
    let title = match state.ui_mode {
        DashboardUiMode::Headless => " octo-dl headless ",
        DashboardUiMode::Tui => " octo-dl ",
        DashboardUiMode::Attached => " octo-dl attached ",
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ram = format_bytes(state.metrics.memory_rss);
    let mut title_right = String::with_capacity(32 + ram.len());
    let _ = write!(
        title_right,
        " {}% CPU | {ram} RAM | API: {}",
        (state.metrics.cpu_usage as u16).min(999),
        state.metrics.api_port,
    );
    if state.paused {
        title_right.push_str(" | PAUSED");
    }
    if state.read_only {
        title_right.push_str(" | READ-ONLY");
    }

    let outer = Block::default()
        .title(title)
        .title_alignment(Alignment::Left)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if state.paused {
            Color::Yellow
        } else {
            Color::Cyan
        }));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let right_x = area
        .x
        .saturating_add(area.width)
        .saturating_sub(u16::try_from(title_right.len()).unwrap_or(u16::MAX) + 1);
    if right_x > area.x + 1 {
        frame.render_widget(
            Paragraph::new(title_right).style(Style::default().fg(if state.paused {
                Color::Yellow
            } else {
                Color::Cyan
            })),
            Rect::new(
                right_x,
                area.y,
                area.width.saturating_sub(right_x - area.x),
                1,
            ),
        );
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let show_url_input = !state.read_only && state.popup == Popup::None && chrome.url_input_active;
    if show_url_input {
        let url_block = Block::default()
            .title(" Add URL(s): editing ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));
        let url_inner = url_block.inner(chunks[0]);
        let (url_value, cursor_col) =
            focused_url_input_view(chrome.url_input, chrome.url_input_cursor, url_inner.width);
        frame.render_widget(
            Paragraph::new(url_value)
                .block(url_block)
                .style(Style::default().fg(Color::White)),
            chunks[0],
        );
        if let Some(cursor_col) = cursor_col
            && url_inner.height > 0
        {
            frame.set_cursor_position(Position::new(url_inner.x + cursor_col, url_inner.y));
        }
    } else {
        render_aggregate_progress(frame, state, chunks[0]);
    }

    draw_dashboard_file_list(frame, state, list_state, chunks[1]);

    let status_line = Paragraph::new(Line::from(dashboard_status_line(
        state,
        chunks[2].width,
        list_state.selected(),
    )))
    .style(Style::default().fg(Color::White));
    frame.render_widget(status_line, chunks[2]);

    let controls = if state.read_only {
        truncate_end(
            "up/down:select  q:quit  read-only",
            usize::from(chunks[3].width),
        )
    } else {
        controls_label_from_snapshot(state, chrome, chunks[3].width)
    };
    let controls_bar = Paragraph::new(controls)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(controls_bar, chunks[3]);
}

fn render_aggregate_progress(
    frame: &mut ratatui::Frame,
    state: &DownloadDashboardState,
    area: Rect,
) {
    let ratio = if state.totals.total_size > 0 {
        #[allow(clippy::cast_precision_loss)]
        let r = state.totals.total_downloaded as f64 / state.totals.total_size as f64;
        r.min(1.0)
    } else {
        0.0
    };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let pct = (ratio * 100.0) as u16;
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL))
        .gauge_style(Style::default().fg(Color::Green))
        .ratio(ratio)
        .label(dashboard_aggregate_progress_label(state, pct, area.width));
    frame.render_widget(gauge, area);
}

fn render_aggregate_progress_app(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let ratio = if app.total_size > 0 {
        #[allow(clippy::cast_precision_loss)]
        let r = app.total_downloaded as f64 / app.total_size as f64;
        r.min(1.0)
    } else {
        0.0
    };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let pct = (ratio * 100.0) as u16;
    let label = aggregate_progress_label_app(app, pct, area.width);
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL))
        .gauge_style(Style::default().fg(Color::Green))
        .ratio(ratio)
        .label(label);
    frame.render_widget(gauge, area);
}

fn draw_dashboard_file_list_app(frame: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let rows_len = app.cached_visible_rows().len();
    clamp_selection(&mut app.file_list_state, rows_len);
    let inner = Block::default().borders(Borders::ALL).inner(area);
    frame.render_widget(Block::default().borders(Borders::ALL), area);
    if inner.width == 0 || inner.height == 0 || rows_len == 0 {
        return;
    }

    let selected = app.file_list_state.selected();
    let visible_height = usize::from(inner.height);
    if let Some(selected) = selected {
        let offset = app.file_list_state.offset_mut();
        if selected < *offset {
            *offset = selected;
        } else if selected >= offset.saturating_add(visible_height) {
            *offset = selected.saturating_sub(visible_height.saturating_sub(1));
        }
    }
    let offset = app.file_list_state.offset().min(rows_len.saturating_sub(1));
    *app.file_list_state.offset_mut() = offset;

    let content_width = usize::from(inner.width);
    let blank = " ".repeat(content_width);
    for (line, row_index) in (offset..rows_len).take(visible_height).enumerate() {
        let y = inner.y + u16::try_from(line).unwrap_or(0);
        let selected_row = selected == Some(row_index);
        let row_style = if selected_row {
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        frame
            .buffer_mut()
            .set_stringn(inner.x, y, &blank, content_width, row_style);
        match &app.cached_visible_rows()[row_index] {
            TuiRow::Package(package_id) => {
                render_package_row_app(
                    frame,
                    app,
                    *package_id,
                    inner.x,
                    y,
                    content_width,
                    selected_row,
                );
            }
            TuiRow::File {
                package_id,
                file_id,
            } => {
                render_file_row_app(
                    frame,
                    app,
                    package_id.is_none(),
                    file_id,
                    inner.x,
                    y,
                    content_width,
                    selected_row,
                );
            }
        }
    }
}

fn render_file_row_app(
    frame: &mut ratatui::Frame,
    app: &App,
    include_package: bool,
    file_id: &crate::core::FileId,
    x: u16,
    y: u16,
    content_width: usize,
    selected: bool,
) {
    let Some(file) = app
        .visible_file_positions
        .get(file_id)
        .and_then(|index| app.files.get(*index))
    else {
        return;
    };
    let status = file_status_for_draw(app, file);
    let (icon, color) = match &status {
        FileStatus::Downloading => ("\u{25cf}", Color::Yellow),
        FileStatus::Queued => ("\u{25cb}", Color::DarkGray),
        FileStatus::Complete => ("\u{2713}", Color::Green),
        FileStatus::Error(_) => ("\u{2717}", Color::Red),
    };
    let detail_color = if app.verifying_files.contains(file_id) {
        Color::Blue
    } else if matches!(status, FileStatus::Downloading) {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let prefix_label = if include_package {
        app.package_label_for_file(file_id)
    } else {
        None
    };
    let detail = file_detail_app(app, file, &status);
    let prefix_width = 5;
    let detail_width = text_width(&detail).min(content_width / 2);
    let detail = truncate_end_cow(&detail, detail_width);
    let detail_width = text_width(&detail);
    let name_width = content_width
        .saturating_sub(prefix_width)
        .saturating_sub(detail_width)
        .saturating_sub(1);
    let owned_display_name;
    let display_name = if let Some(prefix_label) = prefix_label {
        owned_display_name = prefixed_file_label(&prefix_label, &file.name);
        owned_display_name.as_str()
    } else {
        file.name.as_str()
    };
    let name = truncate_end_cow(display_name, name_width);
    let name_width = text_width(&name);
    let filler_width = content_width
        .saturating_sub(prefix_width)
        .saturating_sub(name_width)
        .saturating_sub(detail_width);
    let mut row_style = Style::default().fg(if app.verifying_files.contains(file_id) {
        Color::Blue
    } else {
        color
    });
    if selected {
        row_style = row_style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
    }
    render_segments(
        frame,
        x,
        y,
        [
            ("   ", row_style),
            (icon, row_style),
            (" ", row_style),
            (&name, row_style),
            ("", row_style),
            (&detail, Style::default().fg(detail_color)),
        ],
        filler_width,
        selected,
    );
}

fn prefixed_file_label(prefix_label: &str, file_name: &str) -> String {
    let compact = compact_label(prefix_label);
    let mut prefixed = String::with_capacity(compact.len() + file_name.len() + 3);
    prefixed.push('[');
    prefixed.push_str(&compact);
    prefixed.push_str("] ");
    prefixed.push_str(file_name);
    prefixed
}

fn render_package_row_app(
    frame: &mut ratatui::Frame,
    app: &App,
    package_id: crate::core::PackageId,
    x: u16,
    y: u16,
    content_width: usize,
    selected: bool,
) {
    let Some(package) = app.core_state.packages.get(&package_id) else {
        return;
    };
    let mut present = 0_usize;
    let mut complete = 0_usize;
    let mut downloaded = 0_u64;
    let mut size = 0_u64;
    let mut common_folder = None;
    let mut folder_conflict = false;
    for file in app.core_state.files.values() {
        if file.package_id != package_id {
            continue;
        }
        let folder = file.path.split('/').next().filter(|part| !part.is_empty());
        match (common_folder, folder) {
            (None, Some(folder)) => common_folder = Some(folder),
            (Some(existing), Some(folder)) if existing == folder => {}
            (Some(_), Some(_)) => folder_conflict = true,
            _ => {}
        }
        let file_complete = matches!(file.lifecycle, FileLifecycle::Complete);
        let visible = if file_complete {
            file.size
        } else {
            crate::core::visible_completed_bytes_for_display(file)
        };
        present += 1;
        complete += usize::from(file_complete);
        downloaded = downloaded.saturating_add(visible);
        size = size.saturating_add(file.size);
    }
    let percent = percent(downloaded, size);
    let expanded = app.expanded_packages.contains(&package_id)
        || matches!(package.status, PackageStatus::Failed);
    let (icon, color) = package_status_style(package.status, percent);
    let marker = if present > 1 {
        if expanded { "-" } else { "+" }
    } else {
        " "
    };
    let speed_label = if matches!(package.status, PackageStatus::Downloading) {
        "active"
    } else {
        ""
    };
    let detail = package_detail_app(
        complete,
        present,
        downloaded,
        size,
        percent,
        speed_label,
        content_width,
    );
    let prefix_width = 5;
    let detail_width = text_width(&detail).min(content_width / 2);
    let detail = truncate_end_cow(&detail, detail_width);
    let detail_width = text_width(&detail);
    let name_source = package_display_name_app(
        &package.display_name,
        (!folder_conflict).then_some(common_folder).flatten(),
    );
    let name = truncate_end_cow(
        &name_source,
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
    let mut row_style = Style::default().fg(color);
    if selected {
        row_style = row_style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
    }
    render_segments(
        frame,
        x,
        y,
        [
            (" ", row_style),
            (marker, row_style),
            (" ", row_style),
            (icon, row_style),
            (" ", row_style),
            (&name, row_style),
            ("", row_style),
            (&detail, Style::default().fg(Color::DarkGray)),
        ],
        filler_width,
        selected,
    );
}

fn render_segments<const N: usize>(
    frame: &mut ratatui::Frame,
    mut x: u16,
    y: u16,
    segments: [(&str, Style); N],
    filler_width: usize,
    selected: bool,
) {
    for (text, style) in segments {
        if text.is_empty() {
            x = x.saturating_add(u16::try_from(filler_width).unwrap_or(u16::MAX));
            continue;
        }
        let style = if selected {
            style.bg(Color::DarkGray)
        } else {
            style
        };
        frame.buffer_mut().set_string(x, y, text, style);
        x = x.saturating_add(u16::try_from(text_width(text)).unwrap_or(u16::MAX));
    }
}

fn file_status_for_draw(app: &App, file: &FileEntry) -> FileStatus {
    if app.verifying_files.contains(&file.id) {
        FileStatus::Downloading
    } else {
        file.status.clone()
    }
}

fn file_detail_app<'a>(app: &App, file: &'a FileEntry, status: &'a FileStatus) -> Cow<'a, str> {
    if app.verifying_files.contains(&file.id) || matches!(status, FileStatus::Downloading) {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let pct = if file.size > 0 {
            ((file.downloaded as f64 / file.size as f64 * 100.0) as u64).min(100)
        } else {
            0
        };
        let file_speed = app.file_speed(&file.id);
        let formatted_speed = if !app.verifying_files.contains(&file.id) && file_speed > 0 {
            Some(format_bytes(file_speed))
        } else {
            None
        };
        let speed_extra = formatted_speed.as_ref().map_or(8, |speed| speed.len() + 4);
        let mut detail = String::with_capacity(10 * "\u{2588}".len() + speed_extra + 8);
        detail.push('[');
        push_progress_bar_app(&mut detail, file.downloaded, file.size, 10);
        let _ = write!(detail, "] {pct}%");
        if app.verifying_files.contains(&file.id) {
            detail.push_str("  verify");
        } else if let Some(speed) = formatted_speed {
            detail.push_str("  ");
            detail.push_str(&speed);
            detail.push_str("/s");
        } else {
            detail.push_str("  active");
        }
        return Cow::Owned(detail);
    }
    match status {
        FileStatus::Queued => Cow::Borrowed("queued"),
        FileStatus::Complete => {
            let formatted_size = format_bytes(file.size);
            let mut detail = String::with_capacity(formatted_size.len() + 6);
            detail.push_str(&formatted_size);
            detail.push_str("  done");
            Cow::Owned(detail)
        }
        FileStatus::Error(message) => Cow::Borrowed(message),
        FileStatus::Downloading => unreachable!("downloading handled above"),
    }
}

fn push_progress_bar_app(out: &mut String, downloaded: u64, total: u64, width: usize) {
    if total == 0 {
        for _ in 0..width {
            out.push('\u{2591}');
        }
        return;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let filled = ((downloaded as f64 / total as f64) * width as f64) as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    for _ in 0..filled {
        out.push('\u{2588}');
    }
    for _ in 0..empty {
        out.push('\u{2591}');
    }
}

fn package_detail_app(
    completed: usize,
    present: usize,
    downloaded: u64,
    total_bytes: u64,
    percent: u64,
    speed_label: &str,
    content_width: usize,
) -> String {
    let downloaded = format_bytes(downloaded);
    let total = format_bytes(total_bytes);
    let mut full = String::with_capacity(40 + downloaded.len() + total.len() + speed_label.len());
    let _ = write!(
        full,
        "{completed}/{present} files  {downloaded} / {total}  {percent:>3}%  {speed_label}"
    );
    if text_width(&full) <= content_width / 2 {
        return full;
    }
    let mut compact = String::with_capacity(24 + total.len() + speed_label.len());
    let _ = write!(
        compact,
        "{completed}/{present}  {total}  {percent:>3}%  {speed_label}"
    );
    truncate_end(&compact, content_width / 2)
}

fn package_display_name_app(display_name: &str, folder_label: Option<&str>) -> String {
    if !display_name.starts_with("http://") && !display_name.starts_with("https://") {
        return compact_label(display_name);
    }
    if let Some(label) = folder_label {
        return label.to_string();
    }
    if let Some(label) = mega_url_label(display_name) {
        return label;
    }
    compact_label(display_name.split('#').next().unwrap_or(display_name))
}

fn aggregate_progress_label_app(app: &App, pct: u16, width: u16) -> String {
    let downloaded = format_bytes(app.total_downloaded);
    let total = format_bytes(app.total_size);
    let mut bytes = String::with_capacity(downloaded.len() + total.len() + 3);
    bytes.push_str(&downloaded);
    bytes.push_str(" / ");
    bytes.push_str(&total);
    let transfer = aggregate_transfer_label_app(app);
    let mut full = String::with_capacity(32 + bytes.len() + transfer.len());
    let _ = write!(
        full,
        "{pct}%  {}/{} files  {bytes}  {transfer}",
        app.files_completed, app.files_total
    );
    if full.chars().count() <= usize::from(width.saturating_sub(2)) {
        return full;
    }
    let mut compact = String::with_capacity(20 + transfer.len());
    let _ = write!(
        compact,
        "{pct}%  {}/{}  {transfer}",
        app.files_completed, app.files_total
    );
    if compact.chars().count() <= usize::from(width.saturating_sub(2)) {
        return compact;
    }
    let mut shortest = String::with_capacity(6 + transfer.len());
    let _ = write!(shortest, "{pct}%  {transfer}");
    truncate_end(&shortest, usize::from(width.saturating_sub(2)))
}

fn aggregate_transfer_label_app(app: &App) -> String {
    if app.current_speed == 0 {
        return aggregate_activity_label_app(app);
    }
    let formatted_speed = format_bytes(app.current_speed);
    let mut speed = String::with_capacity(formatted_speed.len() + 2);
    speed.push_str(&formatted_speed);
    speed.push_str("/s");
    let remaining = app.total_size.saturating_sub(app.total_downloaded);
    if remaining == 0 {
        return speed;
    }
    let eta_secs = remaining.div_ceil(app.current_speed).max(1);
    let eta = crate::format_duration(std::time::Duration::from_secs(eta_secs));
    let mut label = String::with_capacity(speed.len() + eta.len() + 7);
    label.push_str(&speed);
    label.push_str("  eta ");
    label.push_str(&eta);
    label
}

fn aggregate_activity_label_app(app: &App) -> String {
    if app.current_speed > 0
        || app.files.iter().any(|file| {
            matches!(file.status, FileStatus::Downloading) || app.verifying_files.contains(&file.id)
        })
    {
        return "active".to_string();
    }
    let queued = app
        .files
        .iter()
        .filter(|file| matches!(file.status, FileStatus::Queued))
        .count();
    if queued > 0 {
        let mut label = String::with_capacity(16);
        let _ = write!(label, "{queued} queued");
        return label;
    }
    "idle".to_string()
}

fn dashboard_status_line_app(app: &App, width: u16, selected: Option<usize>) -> Vec<Span<'static>> {
    let error_count = app
        .files
        .iter()
        .filter(|file| matches!(file.status, FileStatus::Error(_)))
        .count();
    let downloading = app
        .files
        .iter()
        .filter(|file| matches!(file.status, FileStatus::Downloading))
        .count();
    let queued = app
        .files
        .iter()
        .filter(|file| matches!(file.status, FileStatus::Queued))
        .count();
    let selected_error = selected.and_then(|index| selected_error_message_app(app, index));
    let width = usize::from(width);

    if width <= 16 && error_count > 0 {
        return vec![Span::styled(
            failed_count_label_app(error_count),
            Style::default().fg(Color::Red),
        )];
    }
    if width <= 32 && downloading > 0 {
        let activity = compact_activity_label_app(downloading, queued);
        let failure = (error_count > 0).then(|| failed_count_label_app(error_count));
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
    if app.authenticated {
        parts.push(Span::styled(
            "Logged in \u{2713}",
            Style::default().fg(Color::Green),
        ));
    } else if app.login.logging_in {
        parts.push(Span::styled(
            "Logging in...",
            Style::default().fg(Color::Yellow),
        ));
    }
    let status = effective_status_app(app);
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
        let error_text = selected_error.unwrap_or_else(|| failed_count_label_app(error_count));
        parts.push(Span::styled(
            truncate_end(&error_text, width.saturating_sub(12)),
            Style::default().fg(Color::Red),
        ));
    }
    parts
}

fn selected_error_message_app(app: &App, index: usize) -> Option<String> {
    match app.cached_visible_rows().get(index)? {
        TuiRow::File { file_id, .. } => app
            .visible_file_positions
            .get(file_id)
            .and_then(|position| app.files.get(*position))
            .and_then(|file| match &file.status {
                FileStatus::Error(message) => Some(message.clone()),
                _ => None,
            }),
        TuiRow::Package(package_id) => app
            .core_state
            .packages
            .get(package_id)
            .and_then(|package| package.error.clone()),
    }
}

fn effective_status_app(app: &App) -> String {
    if !app.status.starts_with("Processing ") || app.files.is_empty() {
        return app.status.clone();
    }
    let downloading = app
        .files
        .iter()
        .filter(|file| matches!(file.status, FileStatus::Downloading))
        .count();
    let queued = app
        .files
        .iter()
        .filter(|file| matches!(file.status, FileStatus::Queued))
        .count();
    if downloading > 0 {
        let mut status = String::with_capacity(40);
        let _ = write!(status, "Downloading {downloading} file(s), {queued} queued");
        return status;
    }
    if app.files_total > 0 {
        let mut status = String::with_capacity(40);
        let _ = write!(
            status,
            "Queued {} file(s), {}/{} complete",
            queued, app.files_completed, app.files_total
        );
        return status;
    }
    let mut status = String::with_capacity(24);
    let _ = write!(status, "Queued {} file(s)", app.files.len());
    status
}

fn failed_count_label_app(error_count: usize) -> String {
    let mut label = String::with_capacity(16);
    let _ = write!(label, "{error_count} failed");
    label
}

fn compact_activity_label_app(downloading: usize, queued: usize) -> String {
    let mut label = String::with_capacity(16);
    let _ = write!(label, "Dl {downloading}, {queued} q");
    label
}

fn controls_label_app(app: &App, width: u16) -> String {
    let text = if app.url_input_active {
        if width >= 34 {
            "enter:add  esc:cancel  paste:ok"
        } else if width >= 24 {
            "enter:add  esc:cancel"
        } else if width >= 14 {
            "enter:add  esc"
        } else {
            "esc"
        }
    } else if app.popup != Popup::None {
        "esc:close"
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

fn percent(downloaded: u64, size: u64) -> u64 {
    if size == 0 {
        0
    } else {
        downloaded.saturating_mul(100).saturating_div(size).min(100)
    }
}

fn draw_dashboard_file_list(
    frame: &mut ratatui::Frame,
    state: &DownloadDashboardState,
    list_state: &mut ratatui::widgets::ListState,
    area: Rect,
) {
    clamp_selection(list_state, state.rows.len());
    let content_width = usize::from(area.width.saturating_sub(4));
    let selected = list_state.selected();
    let items = dashboard_row_items(state, selected, content_width);
    let file_list = List::new(items)
        .block(Block::default().borders(Borders::ALL))
        .highlight_symbol("")
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(file_list, area, list_state);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc;

    use super::*;
    use crate::core::{CoreEvent, ResolvedFile, ResolvedPackage};
    use crate::test_support::package_id;
    use crate::tui::app::{App, ConfirmAction, FileEntry, FileStatus};
    use crate::tui::event::DownloadEvent;

    fn test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();
        App::new(9723, tx, true)
    }

    fn render_text(app: &mut App) -> String {
        render_text_with_size(app, 100, 24)
    }

    fn render_text_with_size(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("draw should succeed");
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut output = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                let cell = buffer.cell((x, y)).expect("cell should exist");
                output.push_str(cell.symbol());
            }
            output.push('\n');
        }
        output
    }

    fn render_buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("draw should succeed");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn draw_main_shows_command_mode_navigation() {
        let mut app = test_app();
        app.files.push(FileEntry {
            id: "queued.bin".to_string().into(),
            name: "queued.bin".to_string(),
            size: 10,
            downloaded: 0,
            status: FileStatus::Queued,
        });

        let rendered = render_text(&mut app);

        assert!(!rendered.contains("Add URL(s):"));
        assert!(rendered.contains("0%"));
        assert!(rendered.contains("a:add"));
        assert!(rendered.contains("up/down:select"));
        assert!(rendered.contains("queued.bin"));
        assert!(!rendered.contains("enter:add"));
    }

    #[test]
    fn draw_main_shows_url_editing_mode() {
        let mut app = test_app();
        app.url_input_active = true;
        app.url_input = "https://mega.nz/file/test".to_string();

        let rendered = render_text(&mut app);

        assert!(rendered.contains("Add URL(s): editing"));
        assert!(rendered.contains("https://mega.nz/file/test"));
        assert!(rendered.contains("enter:add"));
        assert!(rendered.contains("esc:cancel"));
        assert!(!rendered.contains("q:quit"));
    }

    #[test]
    fn draw_main_swaps_url_input_back_to_progress_when_editing_ends() {
        let mut app = test_app();
        app.url_input_active = true;
        app.url_input = "https://mega.nz/file/test".to_string();

        let editing = render_text(&mut app);
        assert!(editing.contains("Add URL(s): editing"));
        assert!(editing.contains("https://mega.nz/file/test"));

        app.url_input_active = false;
        app.url_input.clear();

        let command_mode = render_text(&mut app);
        assert!(!command_mode.contains("Add URL(s):"));
        assert!(!command_mode.contains("https://mega.nz/file/test"));
        assert!(command_mode.contains("0%"));
        assert!(command_mode.contains("a:add"));
    }

    #[test]
    fn draw_main_narrow_width_keeps_quit_visible() {
        let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();
        let mut app = App::new(9723, tx, true);
        app.files.push(FileEntry {
            id: "queued.bin".to_string().into(),
            name: "queued.bin".to_string(),
            size: 10,
            downloaded: 0,
            status: FileStatus::Queued,
        });

        let backend = TestBackend::new(20, 16);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw should succeed");
        let rendered = {
            let buffer = terminal.backend().buffer();
            let area = buffer.area;
            let mut output = String::new();
            for y in area.y..area.y + area.height {
                for x in area.x..area.x + area.width {
                    let cell = buffer.cell((x, y)).expect("cell should exist");
                    output.push_str(cell.symbol());
                }
                output.push('\n');
            }
            output
        };

        assert!(rendered.contains("q:quit"));
        assert!(rendered.contains("a:add"));
    }

    #[test]
    fn draw_main_narrow_status_prioritizes_activity_and_failures() {
        let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();
        let mut app = App::new(9723, tx, true);
        app.authenticated = true;
        app.status = "Processing 3 URL(s)...".to_string();
        app.files = vec![
            FileEntry {
                id: "active.bin".to_string().into(),
                name: "active.bin".to_string(),
                size: 10,
                downloaded: 5,
                status: FileStatus::Downloading,
            },
            FileEntry {
                id: "queued.bin".to_string().into(),
                name: "queued.bin".to_string(),
                size: 10,
                downloaded: 0,
                status: FileStatus::Queued,
            },
            FileEntry {
                id: "failed.bin".to_string().into(),
                name: "failed.bin".to_string(),
                size: 10,
                downloaded: 0,
                status: FileStatus::Error("boom".to_string()),
            },
        ];

        let rendered = render_text_with_size(&mut app, 28, 16);

        assert!(rendered.contains("Dl 1, 1 q"));
        assert!(rendered.contains("1 failed"));
        assert!(!rendered.contains("Logged in"));
    }

    #[test]
    fn draw_main_tight_status_falls_back_to_failure_summary() {
        let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();
        let mut app = App::new(9723, tx, true);
        app.authenticated = true;
        app.status = "Processing 2 URL(s)...".to_string();
        app.files = vec![
            FileEntry {
                id: "active.bin".to_string().into(),
                name: "active.bin".to_string(),
                size: 10,
                downloaded: 5,
                status: FileStatus::Downloading,
            },
            FileEntry {
                id: "failed.bin".to_string().into(),
                name: "failed.bin".to_string(),
                size: 10,
                downloaded: 0,
                status: FileStatus::Error("boom".to_string()),
            },
        ];

        let rendered = render_text_with_size(&mut app, 14, 16);

        assert!(rendered.contains("1 failed"));
        assert!(!rendered.contains("Logged in"));
        assert!(!rendered.contains("Downloading"));
    }

    #[test]
    fn draw_login_popup_wraps_long_login_error() {
        let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();
        let mut app = App::new(9723, tx, true);
        app.popup = Popup::Login;
        app.login.error = Some("invalid RSA private key format".to_string());

        let rendered = render_text_with_size(&mut app, 54, 18);

        assert!(rendered.contains("Login failed"));
        assert!(rendered.contains("invalid RSA private key format"));
        assert!(!rendered.contains("Login failed: Login failed"));
    }

    #[test]
    fn draw_main_narrow_url_mode_keeps_escape_visible() {
        let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();
        let mut app = App::new(9723, tx, true);
        app.url_input_active = true;

        let backend = TestBackend::new(16, 16);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw should succeed");
        let rendered = {
            let buffer = terminal.backend().buffer();
            let area = buffer.area;
            let mut output = String::new();
            for y in area.y..area.y + area.height {
                for x in area.x..area.x + area.width {
                    let cell = buffer.cell((x, y)).expect("cell should exist");
                    output.push_str(cell.symbol());
                }
                output.push('\n');
            }
            output
        };

        assert!(rendered.contains("esc"));
        assert!(rendered.contains("enter:add"));
    }

    #[test]
    fn draw_confirm_popup_shows_destructive_action_prompt() {
        let mut app = test_app();
        app.files.push(FileEntry {
            id: "danger.bin".to_string().into(),
            name: "danger.bin".to_string(),
            size: 10,
            downloaded: 0,
            status: FileStatus::Queued,
        });
        app.popup = Popup::Confirm;
        app.pending_confirmation = Some(ConfirmAction::DeleteFile("danger.bin".to_string().into()));

        let rendered = render_text(&mut app);

        assert!(rendered.contains("Confirm"));
        assert!(rendered.contains("Delete file: danger.bin"));
        assert!(rendered.contains("y/Enter: confirm"));
        assert!(rendered.contains("n/Esc: cancel"));
    }

    #[test]
    fn draw_main_renders_package_rows_as_primary_rows() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg-1", "https://mega.nz/folder/pkg"),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string().clone()),
                display_name: "Mega Package".to_string(),
                files: vec![
                    ResolvedFile {
                        file_id: "first.bin".to_string().into(),
                        path: "first.bin".to_string(),
                        size: 10,
                    },
                    ResolvedFile {
                        file_id: "second.bin".to_string().into(),
                        path: "second.bin".to_string(),
                        size: 20,
                    },
                ],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileQueued {
            file_id: "first.bin".to_string().into(),
        });
        app.apply_core_event(CoreEvent::FileQueued {
            file_id: "second.bin".to_string().into(),
        });

        let rendered = render_text(&mut app);

        assert!(rendered.contains("Mega Package"));
        assert!(rendered.contains("0/2 files"));
        assert!(!rendered.contains("first.bin"));
        assert!(!rendered.contains("second.bin"));
        assert!(!rendered.contains(">>"));
        assert_eq!(app.file_list_state.selected(), Some(0));
    }

    #[test]
    fn draw_main_expanding_package_shows_file_children() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg-1", "https://mega.nz/folder/pkg"),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string().clone()),
                display_name: "Mega Package".to_string(),
                files: vec![
                    ResolvedFile {
                        file_id: "b.bin".to_string().into(),
                        path: "b.bin".to_string(),
                        size: 20,
                    },
                    ResolvedFile {
                        file_id: "a.bin".to_string().into(),
                        path: "a.bin".to_string(),
                        size: 10,
                    },
                ],
                collision: None,
            },
        });
        app.expanded_packages
            .insert(package_id("pkg-1", "https://mega.nz/folder/pkg"));

        let rendered = render_text(&mut app);

        assert!(rendered.contains("Mega Package"));
        assert!(rendered.contains("a.bin"));
        assert!(rendered.contains("b.bin"));
        assert!(
            rendered.find("b.bin").expect("b.bin should render")
                < rendered.find("a.bin").expect("a.bin should render")
        );
    }

    #[test]
    fn draw_main_uses_folder_name_for_url_package_row() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id(
                    "https://mega.nz/folder/abc#secret",
                    "https://mega.nz/folder/abc#secret",
                ),
                source_url: "https://mega.nz/folder/abc#secret".to_string(),
                key: crate::core::PackageKey::new(
                    "https://mega.nz/folder/abc#secret".to_string().clone(),
                ),
                display_name: "https://mega.nz/folder/abc#secret".to_string(),
                files: vec![ResolvedFile {
                    file_id: "file.bin".to_string().into(),
                    path: "Folder Name/file.bin".to_string(),
                    size: 10,
                }],
                collision: None,
            },
        });

        let rendered = render_text(&mut app);

        assert!(rendered.contains("Folder Name"));
        assert!(!rendered.contains("https://mega.nz"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn draw_main_uses_progress_glyph_for_merged_package_rows() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg-merged", "https://mega.nz/folder/a"),
                source_url: "https://mega.nz/folder/a".to_string(),
                key: crate::core::PackageKey::new("Merged Package".to_string()),
                display_name: "Merged Package".to_string(),
                files: vec![
                    ResolvedFile {
                        file_id: "episode-1.mkv".to_string().into(),
                        path: "season/episode-1.mkv".to_string(),
                        size: 100,
                    },
                    ResolvedFile {
                        file_id: "episode-2.mkv".to_string().into(),
                        path: "season/episode-2.mkv".to_string(),
                        size: 100,
                    },
                ],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: "episode-1.mkv".to_string().into(),
            size: 100,
        });
        app.apply_core_event(CoreEvent::FileProgress {
            file_id: "episode-1.mkv".to_string().into(),
            total_bytes_delta: 50,
            network_bytes_delta: 50,
        });

        let rendered = render_text(&mut app);

        assert!(rendered.contains("◑ Merged Package"));
        assert!(rendered.contains(" 25%"));
    }

    #[test]
    fn draw_main_colors_active_file_progress_yellow() {
        let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();
        let mut app = App::new(9723, tx, true);
        app.files.push(FileEntry {
            id: "active.bin".to_string().into(),
            name: "active.bin".to_string(),
            size: 100,
            downloaded: 40,
            status: FileStatus::Downloading,
        });

        let buffer = render_buffer(&mut app, 100, 24);
        let area = buffer.area;
        let mut saw_yellow_progress = false;
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                let cell = buffer.cell((x, y)).expect("cell should exist");
                if cell.symbol() == "4" && cell.fg == Color::Yellow {
                    saw_yellow_progress = true;
                    break;
                }
            }
            if saw_yellow_progress {
                break;
            }
        }

        assert!(
            saw_yellow_progress,
            "active file progress should render in yellow"
        );
    }

    #[test]
    fn draw_main_colors_verification_progress_blue() {
        let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();
        let mut app = App::new(9723, tx, true);
        let file_id: crate::core::FileId = "verify.bin".to_string().into();
        app.files.push(FileEntry {
            id: file_id.clone(),
            name: "verify.bin".to_string(),
            size: 100,
            downloaded: 40,
            status: FileStatus::Queued,
        });
        app.verifying_files.insert(file_id);

        let buffer = render_buffer(&mut app, 100, 24);
        let area = buffer.area;
        let mut saw_blue_progress = false;
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                let cell = buffer.cell((x, y)).expect("cell should exist");
                if cell.symbol() == "4" && cell.fg == Color::Blue {
                    saw_blue_progress = true;
                    break;
                }
            }
            if saw_blue_progress {
                break;
            }
        }

        assert!(
            saw_blue_progress,
            "verification progress should render in blue"
        );
    }

    #[test]
    fn draw_main_failed_packages_expand_by_default() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg-1", "https://mega.nz/folder/pkg"),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string().clone()),
                display_name: "Mega Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: "active.bin".to_string().into(),
                    path: "active.bin".to_string(),
                    size: 20,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileFailed {
            file_id: "active.bin".to_string().into(),
            message: "boom".to_string(),
        });

        let rendered = render_text(&mut app);

        assert!(rendered.contains("Mega Package"));
        assert!(rendered.contains("active.bin"));
    }

    #[test]
    fn draw_main_downloading_packages_do_not_auto_expand() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg-1", "https://mega.nz/folder/pkg"),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string().clone()),
                display_name: "Mega Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: "active.bin".to_string().into(),
                    path: "active.bin".to_string(),
                    size: 20,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: "active.bin".to_string().into(),
            size: 20,
        });

        let rendered = render_text(&mut app);

        assert!(rendered.contains("Mega Package"));
        assert!(!rendered.contains("active.bin"));
    }

    #[test]
    fn draw_main_replaces_stale_processing_status_when_files_exist() {
        let mut app = test_app();
        app.status = "Processing 13 URL(s)...".to_string();
        app.files_total = 2;
        app.files.push(FileEntry {
            id: "active.bin".to_string().into(),
            name: "active.bin".to_string(),
            size: 100,
            downloaded: 20,
            status: FileStatus::Downloading,
        });
        app.files.push(FileEntry {
            id: "queued.bin".to_string().into(),
            name: "queued.bin".to_string(),
            size: 100,
            downloaded: 0,
            status: FileStatus::Queued,
        });

        let rendered = render_text(&mut app);

        assert!(!rendered.contains("Processing 13 URL"));
        assert!(rendered.contains("Downloading 1 file(s), 1 queued"));
    }

    #[test]
    fn draw_main_does_not_show_zero_byte_rate_for_active_work() {
        let mut app = test_app();
        app.files_total = 1;
        app.total_size = 100;
        app.total_downloaded = 20;
        app.current_speed = 0;
        app.files.push(FileEntry {
            id: "active.bin".to_string().into(),
            name: "active.bin".to_string(),
            size: 100,
            downloaded: 20,
            status: FileStatus::Downloading,
        });

        let rendered = render_text(&mut app);

        assert!(!rendered.contains("0 B/s"));
        assert!(rendered.contains("active"));
    }

    #[test]
    fn draw_main_shows_bandwidth_and_eta_for_active_transfers() {
        let mut app = test_app();
        app.files_total = 2;
        app.files_completed = 0;
        app.total_size = 100;
        app.total_downloaded = 20;
        app.current_speed = 20;
        app.files.push(FileEntry {
            id: "active.bin".to_string().into(),
            name: "active.bin".to_string(),
            size: 100,
            downloaded: 20,
            status: FileStatus::Downloading,
        });

        let rendered = render_text(&mut app);

        assert!(rendered.contains("20 B/s"));
        assert!(rendered.contains("eta 4.0s"));
    }
}
