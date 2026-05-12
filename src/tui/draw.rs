//! All drawing / rendering functions.

mod popup;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph};

use crate::core::PackageStatus;
use crate::format_bytes;

use super::app::{App, Popup};
use super::dashboard::{
    DashboardChrome, DashboardFileRow, DashboardFileStatus, DashboardPackageRow, DashboardRow,
    DashboardUiMode, DownloadDashboardState, aggregate_transfer_label as dashboard_transfer_label,
    clamp_selection, file_detail as dashboard_file_detail,
};

pub fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let state = app.dashboard_state(DashboardUiMode::Tui, false);
    draw_dashboard(
        frame,
        &state,
        &DashboardChrome::new(&app.url_input, app.url_input_active),
        &mut app.file_list_state,
    );
    match app.popup {
        Popup::None => {}
        Popup::Login => popup::draw_login_popup(frame, app),
        Popup::Config => popup::draw_config_popup(frame, app),
        Popup::Confirm => popup::draw_confirm_popup(frame, app),
        Popup::Sort => popup::draw_sort_popup(frame, app),
    }
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
    let title_right = format!(
        " {}% CPU | {} RAM | API: {}{}{}",
        (state.metrics.cpu_usage as u16).min(999),
        format_bytes(state.metrics.memory_rss),
        state.metrics.api_port,
        if state.paused { " | PAUSED" } else { "" },
        if state.read_only { " | READ-ONLY" } else { "" }
    );

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
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let url_style = if !state.read_only && state.popup == Popup::None && chrome.url_input_active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let url_title = if state.read_only {
        " Attached dashboard "
    } else if chrome.url_input_active {
        " Add URL(s): editing "
    } else {
        " Add URL(s): press a "
    };
    let url_block = Block::default()
        .title(url_title)
        .borders(Borders::ALL)
        .border_style(url_style);
    let url_inner = url_block.inner(chunks[0]);
    let (url_value, cursor_col) =
        if !state.read_only && state.popup == Popup::None && chrome.url_input_active {
            focused_url_input_view(chrome.url_input, url_inner.width)
        } else {
            (
                truncate_end(chrome.url_input, usize::from(url_inner.width)),
                None,
            )
        };
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
        .label(dashboard_aggregate_progress_label(
            state,
            pct,
            chunks[1].width,
        ));
    frame.render_widget(gauge, chunks[1]);

    draw_dashboard_file_list(frame, state, list_state, chunks[2]);

    let status_line = Paragraph::new(Line::from(dashboard_status_line(state, chunks[3].width)))
        .style(Style::default().fg(Color::White));
    frame.render_widget(status_line, chunks[3]);

    let controls = if state.read_only {
        truncate_end(
            "up/down:select  q:quit  read-only",
            usize::from(chunks[4].width),
        )
    } else {
        controls_label_from_snapshot(state, chrome, chunks[4].width)
    };
    let controls_bar = Paragraph::new(controls)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(controls_bar, chunks[4]);
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
    let items = state
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| dashboard_row_item(state, row, selected == Some(index), content_width))
        .collect::<Vec<_>>();
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

fn dashboard_row_item(
    state: &DownloadDashboardState,
    row: &DashboardRow,
    selected: bool,
    content_width: usize,
) -> ListItem<'static> {
    match row {
        DashboardRow::Package { package_id } => state
            .packages
            .iter()
            .find(|package| package.id == *package_id)
            .map(|package| dashboard_package_item(package, selected, content_width))
            .unwrap_or_else(|| ListItem::new(Line::from(""))),
        DashboardRow::File {
            package_id,
            file_id,
        } => state
            .files
            .iter()
            .find(|file| file.id == *file_id)
            .map(|file| {
                dashboard_file_item(
                    file,
                    package_id.is_empty() && !file.package_id.is_empty(),
                    selected,
                    content_width,
                )
            })
            .unwrap_or_else(|| ListItem::new(Line::from(""))),
    }
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
    let prefix_width = prefix.chars().count();
    let detail_width = detail.chars().count().min(content_width / 2);
    let detail = truncate_end(&detail, detail_width);
    let name = truncate_end(
        &format!("{prefix_label}{}", file.name),
        content_width
            .saturating_sub(prefix_width)
            .saturating_sub(detail.chars().count())
            .saturating_sub(1),
    );
    let filler = " ".repeat(
        content_width
            .saturating_sub(prefix_width)
            .saturating_sub(name.chars().count())
            .saturating_sub(detail.chars().count()),
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

fn dashboard_package_item(
    package: &DashboardPackageRow,
    selected: bool,
    content_width: usize,
) -> ListItem<'static> {
    let (icon, color) = package_status_style(package.status);
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
    let prefix_width = prefix.chars().count();
    let detail_width = detail.chars().count().min(content_width / 2);
    let detail = truncate_end(&detail, detail_width);
    let name = truncate_end(
        &display_dashboard_package_name(package),
        content_width
            .saturating_sub(prefix_width)
            .saturating_sub(detail.chars().count())
            .saturating_sub(1),
    );
    let filler = " ".repeat(
        content_width
            .saturating_sub(prefix_width)
            .saturating_sub(name.chars().count())
            .saturating_sub(detail.chars().count()),
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
    if full.chars().count() <= content_width / 2 {
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

fn dashboard_status_line(state: &DownloadDashboardState, width: u16) -> Vec<Span<'static>> {
    let status = dashboard_effective_status(state);
    let error_count = state
        .files
        .iter()
        .filter(|file| file.status.is_error())
        .count();
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
        parts.push(Span::styled(
            format!("{error_count} failed"),
            Style::default().fg(Color::Red),
        ));
    }
    parts
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

fn controls_label_from_snapshot(
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

fn dashboard_aggregate_progress_label(
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

fn focused_url_input_view(value: &str, width: u16) -> (String, Option<u16>) {
    if width == 0 {
        return (String::new(), None);
    }

    let visible_width = usize::from(width.saturating_sub(1));
    if visible_width == 0 {
        return (String::new(), Some(0));
    }

    let char_count = value.chars().count();
    if char_count <= visible_width {
        return (value.to_string(), Some(char_count as u16));
    }

    (
        take_last_chars(value, visible_width),
        Some(width.saturating_sub(1)),
    )
}

fn is_processing_status(status: &str) -> bool {
    status.starts_with("Processing ")
}

fn package_status_style(status: PackageStatus) -> (&'static str, Color) {
    match status {
        PackageStatus::Downloading => ("\u{25cf}", Color::Yellow),
        PackageStatus::Failed => ("\u{2717}", Color::Red),
        PackageStatus::Complete => ("\u{2713}", Color::Green),
        PackageStatus::Partial => ("\u{25d0}", Color::Yellow),
        PackageStatus::Queued | PackageStatus::Pending => ("\u{25cb}", Color::DarkGray),
        PackageStatus::Skipped | PackageStatus::Deleted => ("\u{2715}", Color::DarkGray),
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

fn take_last_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    value
        .chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn truncate_end(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if value.chars().count() <= max_chars {
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc;

    use super::*;
    use crate::core::{CoreEvent, ResolvedFile, ResolvedPackage};
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

    #[test]
    fn draw_main_shows_command_mode_navigation() {
        let mut app = test_app();
        app.files.push(FileEntry {
            id: "queued.bin".to_string(),
            name: "queued.bin".to_string(),
            size: 10,
            downloaded: 0,
            status: FileStatus::Queued,
        });

        let rendered = render_text(&mut app);

        assert!(rendered.contains("Add URL(s): press a"));
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
    fn draw_main_narrow_width_keeps_quit_visible() {
        let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();
        let mut app = App::new(9723, tx, true);
        app.files.push(FileEntry {
            id: "queued.bin".to_string(),
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
                id: "active.bin".to_string(),
                name: "active.bin".to_string(),
                size: 10,
                downloaded: 5,
                status: FileStatus::Downloading,
            },
            FileEntry {
                id: "queued.bin".to_string(),
                name: "queued.bin".to_string(),
                size: 10,
                downloaded: 0,
                status: FileStatus::Queued,
            },
            FileEntry {
                id: "failed.bin".to_string(),
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
                id: "active.bin".to_string(),
                name: "active.bin".to_string(),
                size: 10,
                downloaded: 5,
                status: FileStatus::Downloading,
            },
            FileEntry {
                id: "failed.bin".to_string(),
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
            id: "danger.bin".to_string(),
            name: "danger.bin".to_string(),
            size: 10,
            downloaded: 0,
            status: FileStatus::Queued,
        });
        app.popup = Popup::Confirm;
        app.pending_confirmation = Some(ConfirmAction::DeleteFile("danger.bin".to_string()));

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
                id: "pkg-1".to_string(),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                display_name: "Mega Package".to_string(),
                files: vec![
                    ResolvedFile {
                        file_id: "first.bin".to_string(),
                        path: "first.bin".to_string(),
                        size: 10,
                    },
                    ResolvedFile {
                        file_id: "second.bin".to_string(),
                        path: "second.bin".to_string(),
                        size: 20,
                    },
                ],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileQueued {
            file_id: "first.bin".to_string(),
        });
        app.apply_core_event(CoreEvent::FileQueued {
            file_id: "second.bin".to_string(),
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
                id: "pkg-1".to_string(),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                display_name: "Mega Package".to_string(),
                files: vec![
                    ResolvedFile {
                        file_id: "b.bin".to_string(),
                        path: "b.bin".to_string(),
                        size: 20,
                    },
                    ResolvedFile {
                        file_id: "a.bin".to_string(),
                        path: "a.bin".to_string(),
                        size: 10,
                    },
                ],
                collision: None,
            },
        });
        app.expanded_packages.insert("pkg-1".to_string());

        let rendered = render_text(&mut app);

        assert!(rendered.contains("Mega Package"));
        assert!(rendered.contains("a.bin"));
        assert!(rendered.contains("b.bin"));
        assert!(
            rendered.find("a.bin").expect("a.bin should render")
                < rendered.find("b.bin").expect("b.bin should render")
        );
    }

    #[test]
    fn draw_main_uses_folder_name_for_url_package_row() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: "https://mega.nz/folder/abc#secret".to_string(),
                source_url: "https://mega.nz/folder/abc#secret".to_string(),
                display_name: "https://mega.nz/folder/abc#secret".to_string(),
                files: vec![ResolvedFile {
                    file_id: "file.bin".to_string(),
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
    fn draw_main_failed_packages_expand_by_default() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: "pkg-1".to_string(),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                display_name: "Mega Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: "active.bin".to_string(),
                    path: "active.bin".to_string(),
                    size: 20,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileFailed {
            file_id: "active.bin".to_string(),
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
                id: "pkg-1".to_string(),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                display_name: "Mega Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: "active.bin".to_string(),
                    path: "active.bin".to_string(),
                    size: 20,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: "active.bin".to_string(),
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
            id: "active.bin".to_string(),
            name: "active.bin".to_string(),
            size: 100,
            downloaded: 20,
            status: FileStatus::Downloading,
        });
        app.files.push(FileEntry {
            id: "queued.bin".to_string(),
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
            id: "active.bin".to_string(),
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
            id: "active.bin".to_string(),
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
