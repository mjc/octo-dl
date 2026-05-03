//! All drawing / rendering functions.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Row, Table};

use crate::core::{FileLifecycle, PackageStatus};
use crate::format_bytes;

use super::app::{App, ConfigField, ConfirmAction, FileStatus, Popup, SortKey};
use super::visible::TuiRow;

pub fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    draw_main(frame, app);
    match app.popup {
        Popup::None => {}
        Popup::Login => draw_login_popup(frame, app),
        Popup::Config => draw_config_popup(frame, app),
        Popup::Confirm => draw_confirm_popup(frame, app),
        Popup::Sort => draw_sort_popup(frame, app),
    }
}

#[allow(clippy::too_many_lines)]
fn draw_main(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();

    // Outer block with title bar
    let title = " octo-dl ".to_string();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let title_right = format!(
        " {}% CPU | {} RAM | API: {}{}",
        (app.cpu_usage as u16).min(999),
        format_bytes(app.memory_rss),
        app.api_port,
        if app.paused { " | PAUSED" } else { "" }
    );

    let outer = Block::default()
        .title(title)
        .title_alignment(Alignment::Left)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if app.paused {
            Color::Yellow
        } else {
            Color::Cyan
        }));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    // Render title-right manually in the top border
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
            Constraint::Length(3), // URL input bar
            Constraint::Length(3), // Aggregate progress
            Constraint::Min(5),    // File list
            Constraint::Length(1), // Status line
            Constraint::Length(1), // Controls bar
        ])
        .split(inner);

    // --- URL input bar ---
    let url_style = if app.popup == Popup::None && app.url_input_active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let url_title = if app.url_input_active {
        " Add URL(s): editing "
    } else {
        " Add URL(s): press a "
    };
    let url_input = Paragraph::new(app.url_input.as_str())
        .block(
            Block::default()
                .title(url_title)
                .borders(Borders::ALL)
                .border_style(url_style),
        )
        .style(Style::default().fg(Color::White));
    frame.render_widget(url_input, chunks[0]);

    // --- Aggregate progress ---
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
    let gauge_label = aggregate_progress_label(app, pct, chunks[1].width);
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL))
        .gauge_style(Style::default().fg(Color::Green))
        .ratio(ratio)
        .label(gauge_label);
    frame.render_widget(gauge, chunks[1]);

    // --- File list ---
    draw_file_list(frame, app, chunks[2]);

    // --- Status line ---
    let status_spans = build_status_line(app);
    let status_line =
        Paragraph::new(Line::from(status_spans)).style(Style::default().fg(Color::White));
    frame.render_widget(status_line, chunks[3]);

    // --- Controls bar ---
    let controls = if app.url_input_active {
        "enter:add URLs  esc:cancel  paste:supported"
    } else if app.paused {
        "a:add URLs  up/down:select  enter:expand  s:sort  d:delete  r:retry  R:reset  c:config  q:quit"
    } else {
        "a:add URLs  up/down:select  enter:expand  s:sort  d:delete  r:retry  R:reset  c:config  q:quit"
    };
    let controls_bar = Paragraph::new(controls)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(controls_bar, chunks[4]);
}

fn build_status_line(app: &App) -> Vec<Span<'_>> {
    let mut spans = Vec::new();

    if app.authenticated {
        spans.push(Span::styled(
            " Logged in \u{2713}",
            Style::default().fg(Color::Green),
        ));
    } else if app.login.logging_in {
        spans.push(Span::styled(
            " Logging in...",
            Style::default().fg(Color::Yellow),
        ));
    } else if app.popup == Popup::Login {
        spans.push(Span::styled(
            " Awaiting login",
            Style::default().fg(Color::DarkGray),
        ));
    }

    let status = effective_status(app);
    if !status.is_empty() {
        spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(status, Style::default().fg(Color::Cyan)));
    }

    let error_count = app
        .files
        .iter()
        .filter(|f| matches!(f.status, FileStatus::Error(_)))
        .count();
    if error_count > 0 {
        spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("{error_count} failed"),
            Style::default().fg(Color::Red),
        ));
    }

    spans
}

fn effective_status(app: &App) -> String {
    if !is_processing_status(&app.status) || app.files.is_empty() {
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
        return format!("Downloading {downloading} file(s), {queued} queued");
    }
    if app.files_total > 0 {
        return format!(
            "Queued {} file(s), {}/{} complete",
            queued, app.files_completed, app.files_total
        );
    }
    format!("Queued {} file(s)", app.files.len())
}

fn is_processing_status(status: &str) -> bool {
    status.starts_with("Processing ")
}

struct FileListRow {
    icon: &'static str,
    color: Color,
    name: String,
    detail: String,
}

impl FileListRow {
    fn from_file(app: &App, file: &super::app::FileEntry, include_package: bool) -> Self {
        let package_label = app.package_label_for_file(&file.id);
        let (icon, color) = match &file.status {
            FileStatus::Downloading => ("\u{25cf}", Color::Yellow),
            FileStatus::Queued => ("\u{25cb}", Color::DarkGray),
            FileStatus::Complete => ("\u{2713}", Color::Green),
            FileStatus::Error(_) => ("\u{2717}", Color::Red),
        };
        let detail = file_detail(app, file);
        let prefix = if include_package {
            package_label
                .as_deref()
                .map(|label| format!("[{}] ", compact_label(label)))
                .unwrap_or_default()
        } else {
            String::new()
        };

        Self {
            icon,
            color,
            name: format!("{prefix}{}", file.name),
            detail,
        }
    }

    fn into_child_item(self, selected: bool, content_width: usize) -> ListItem<'static> {
        let name_width = content_width.saturating_sub(self.detail.chars().count() + 10);
        let name = truncate_end(&self.name, name_width.max(10));
        let filler_width = content_width
            .saturating_sub(name.chars().count())
            .saturating_sub(self.detail.chars().count())
            .saturating_sub(6);
        let filler = " ".repeat(filler_width);
        let mut row_style = Style::default().fg(self.color);
        if selected {
            row_style = row_style.add_modifier(Modifier::BOLD);
        }

        ListItem::new(Line::from(vec![
            Span::styled(format!("   {} {name}", self.icon), row_style),
            Span::raw(filler),
            Span::styled(
                truncate_end(
                    &self.detail,
                    content_width.saturating_sub(name.chars().count() + 6),
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
    }
}

fn draw_file_list(frame: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let rows = app.visible_rows();
    let content_width = usize::from(area.width.saturating_sub(4));
    if app.file_list_state.selected().is_none() && !rows.is_empty() {
        app.file_list_state.select(Some(0));
    } else if let Some(selected) = app.file_list_state.selected()
        && selected >= rows.len()
    {
        app.file_list_state.select(rows.len().checked_sub(1));
    }

    let selected = app.file_list_state.selected();
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(display_index, row)| {
            row_item(app, row, selected == Some(display_index), content_width)
        })
        .collect();

    let file_list = List::new(items)
        .block(Block::default().borders(Borders::ALL))
        .highlight_symbol("")
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(file_list, area, &mut app.file_list_state);
}

fn row_item(app: &App, row: &TuiRow, selected: bool, content_width: usize) -> ListItem<'static> {
    match row {
        TuiRow::Package(package_id) => package_row_item(app, package_id, selected, content_width),
        TuiRow::File { file_id, .. } => app
            .files
            .iter()
            .find(|file| file.id == *file_id)
            .map(|file| {
                FileListRow::from_file(app, file, false).into_child_item(selected, content_width)
            })
            .unwrap_or_else(|| ListItem::new(Line::from(""))),
    }
}

#[derive(Default)]
struct PackageCounts {
    present: usize,
    complete: usize,
    downloaded: u64,
    size: u64,
}

fn package_counts(app: &App, package_id: &str) -> PackageCounts {
    let mut counts = PackageCounts::default();
    let Some(package) = app.core_state.packages.get(package_id) else {
        return counts;
    };
    for file_id in &package.file_ids {
        let Some(file) = app.core_state.files.get(file_id) else {
            continue;
        };
        if matches!(
            file.lifecycle,
            FileLifecycle::Skipped | FileLifecycle::Deleted
        ) {
            continue;
        }
        counts.present += 1;
        if matches!(file.lifecycle, FileLifecycle::Complete) {
            counts.complete += 1;
            counts.downloaded = counts.downloaded.saturating_add(file.size);
        } else {
            counts.downloaded = counts
                .downloaded
                .saturating_add(file.progress.visible_completed_bytes.min(file.size));
        }
        counts.size = counts.size.saturating_add(file.size);
    }
    counts
}

fn package_speed(app: &App, package_id: &str) -> u64 {
    app.core_state
        .packages
        .get(package_id)
        .map(|package| {
            package
                .file_ids
                .iter()
                .map(|file_id| app.file_speed(file_id))
                .sum()
        })
        .unwrap_or(0)
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

fn package_row_item(
    app: &App,
    package_id: &str,
    selected: bool,
    content_width: usize,
) -> ListItem<'static> {
    let Some(package) = app.core_state.packages.get(package_id) else {
        return ListItem::new(Line::from(""));
    };
    let expanded = app
        .visible_rows()
        .iter()
        .any(|row| matches!(row, TuiRow::File { package_id: id, .. } if id == package_id));
    let counts = package_counts(app, package_id);
    let percent = if counts.size == 0 {
        0
    } else {
        counts
            .downloaded
            .saturating_mul(100)
            .saturating_div(counts.size)
            .min(100)
    };
    let speed = package_speed(app, package_id);
    let speed_label = if speed > 0 {
        format!("{}/s", format_bytes(speed))
    } else if matches!(package.status, PackageStatus::Downloading) {
        "active".to_string()
    } else {
        String::new()
    };
    let detail = format!(
        "{}/{} files  {} / {}  {percent:>3}%  {speed_label}",
        counts.complete,
        counts.present,
        format_bytes(counts.downloaded),
        format_bytes(counts.size)
    );
    let (icon, color) = package_status_style(package.status);
    let marker = if expanded { "-" } else { "+" };
    let name_width = content_width.saturating_sub(detail.chars().count() + 9);
    let name = truncate_end(&package.display_name, name_width.max(12));
    let filler = " ".repeat(
        content_width
            .saturating_sub(name.chars().count())
            .saturating_sub(detail.chars().count())
            .saturating_sub(7),
    );
    let mut row_style = Style::default().fg(color);
    if selected {
        row_style = row_style.add_modifier(Modifier::BOLD);
    }
    ListItem::new(Line::from(vec![
        Span::styled(format!(" {marker} {icon} {name}"), row_style),
        Span::raw(filler),
        Span::styled(detail, Style::default().fg(Color::DarkGray)),
    ]))
}

fn file_detail(app: &App, file: &super::app::FileEntry) -> String {
    match &file.status {
        FileStatus::Downloading => {
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
            let bar = progress_bar(file.downloaded, file.size, 10);
            let speed = app.file_speed(&file.id);
            if speed > 0 {
                format!("[{bar}] {pct}%  {}/s", format_bytes(speed))
            } else {
                format!("[{bar}] {pct}%  active")
            }
        }
        FileStatus::Queued => "queued".to_string(),
        FileStatus::Complete => {
            format!("{}  done", format_bytes(file.size))
        }
        FileStatus::Error(msg) => msg.clone(),
    }
}

fn aggregate_progress_label(app: &App, pct: u16, width: u16) -> String {
    let bytes = format!(
        "{} / {}",
        format_bytes(app.total_downloaded),
        format_bytes(app.total_size)
    );
    let activity = aggregate_activity_label(app);
    let full = format!(
        "{pct}%  {}/{} files  {bytes}  {activity}",
        app.files_completed, app.files_total
    );
    if full.chars().count() <= usize::from(width.saturating_sub(2)) {
        return full;
    }

    let compact = format!(
        "{pct}%  {}/{}  {activity}",
        app.files_completed, app.files_total
    );
    truncate_end(&compact, usize::from(width.saturating_sub(2)))
}

fn aggregate_activity_label(app: &App) -> String {
    if app.current_speed > 0 {
        return format!("{}/s", format_bytes(app.current_speed));
    }

    if app
        .files
        .iter()
        .any(|file| matches!(file.status, FileStatus::Downloading))
    {
        return "active".to_string();
    }

    let queued = app
        .files
        .iter()
        .filter(|file| matches!(file.status, FileStatus::Queued))
        .count();
    if queued > 0 {
        return format!("{queued} queued");
    }

    "idle".to_string()
}

fn compact_label(value: &str) -> String {
    value
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(value)
        .to_string()
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

fn progress_bar(downloaded: u64, total: u64, width: usize) -> String {
    if total == 0 {
        return "\u{2591}".repeat(width);
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let filled = ((downloaded as f64 / total as f64) * width as f64) as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

fn draw_login_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(42, 12, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Login to MEGA ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Email
            Constraint::Length(3), // Password
            Constraint::Length(3), // MFA
            Constraint::Min(1),    // Error / help
        ])
        .split(inner);

    let fields = [
        ("Email", app.login.email(), false),
        ("Password", app.login.password(), true),
        ("MFA (optional)", app.login.mfa(), false),
    ];

    for (i, (label, value, masked)) in fields.iter().enumerate() {
        let is_active = app.login.active_field == i;
        let style = if is_active {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };

        let display_value = if *masked && !value.is_empty() {
            "*".repeat(value.len())
        } else {
            (*value).to_string()
        };

        let input = Paragraph::new(display_value)
            .block(
                Block::default()
                    .title(format!(" {label} "))
                    .borders(Borders::ALL)
                    .border_style(style),
            )
            .style(Style::default().fg(Color::White));
        frame.render_widget(input, chunks[i]);
    }

    // Error or help text
    if app.login.logging_in {
        let spinner = Paragraph::new(" Logging in...").style(Style::default().fg(Color::Yellow));
        frame.render_widget(spinner, chunks[3]);
    } else if let Some(ref err) = app.login.error {
        let error = Paragraph::new(format!(" {err}")).style(Style::default().fg(Color::Red));
        frame.render_widget(error, chunks[3]);
    } else {
        let help = Paragraph::new(" Tab: next | Enter: login | Esc: quit")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(help, chunks[3]);
    }
}

fn draw_config_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(40, 10, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Config ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),    // Settings table
            Constraint::Length(1), // Help
        ])
        .split(inner);

    let rows: Vec<Row> = ConfigField::ALL
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let is_active = app.config.active_field == i;
            let style = if is_active {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let value = match field {
                ConfigField::ChunksPerFile => app.config.config.chunks_per_file.to_string(),
                ConfigField::ConcurrentFiles => app.config.config.concurrent_files.to_string(),
                ConfigField::ForceOverwrite => {
                    if app.config.config.force_overwrite {
                        "Yes".to_string()
                    } else {
                        "No".to_string()
                    }
                }
                ConfigField::CleanupOnError => {
                    if app.config.config.cleanup_on_error {
                        "Yes".to_string()
                    } else {
                        "No".to_string()
                    }
                }
            };

            let marker = if is_active { ">" } else { " " };
            Row::new(vec![marker.to_string(), field.label().to_string(), value]).style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(2),
        Constraint::Length(20),
        Constraint::Length(10),
    ];
    let table = Table::new(rows, widths);
    frame.render_widget(table, chunks[0]);

    let help = Paragraph::new(" Enter/Esc to close").style(Style::default().fg(Color::DarkGray));
    frame.render_widget(help, chunks[1]);
}

fn draw_sort_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(46, 10, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Sort ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(1)])
        .split(inner);

    let mut rows: Vec<Row> = SortKey::ALL
        .iter()
        .enumerate()
        .map(|(index, key)| {
            let active = app.sort.active_field == index;
            let selected = app.sort.key == *key;
            let style = if active {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            Row::new(vec![
                if active { ">" } else { " " }.to_string(),
                if selected { "*" } else { " " }.to_string(),
                key.label().to_string(),
            ])
            .style(style)
        })
        .collect();
    let direction_active = app.sort.active_field == SortKey::ALL.len();
    rows.push(
        Row::new(vec![
            if direction_active { ">" } else { " " }.to_string(),
            " ".to_string(),
            format!("Direction: {}", app.sort.direction.label()),
        ])
        .style(if direction_active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        }),
    );

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(20),
        ],
    );
    frame.render_widget(table, chunks[0]);

    let help = Paragraph::new(" Enter to apply | Space/Left/Right to change")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(help, chunks[1]);
}

fn draw_confirm_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(58, 7, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Confirm ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (action, target, id) = match app.pending_confirmation.as_ref() {
        Some(ConfirmAction::DeleteFile(id)) => ("Delete", "file", id.as_str()),
        Some(ConfirmAction::DeletePackage(id)) => ("Delete", "package", id.as_str()),
        Some(ConfirmAction::ResetFile(id)) => ("Reset", "file", id.as_str()),
        Some(ConfirmAction::ResetPackage(id)) => ("Reset", "package", id.as_str()),
        None => ("Confirm", "item", ""),
    };
    let name = if target == "package" {
        app.package_display_name(id)
    } else {
        app.files
            .iter()
            .find(|file| file.id == id)
            .map_or_else(|| id.to_string(), |file| file.name.clone())
    };

    let lines = vec![
        Line::from(Span::styled(
            format!("{action} {target}: {}", truncate_end(&name, 36)),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "y/Enter: confirm   n/Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}

/// Returns a centered rectangle of the given size within `area`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc;

    use super::*;
    use crate::core::{CoreEvent, ResolvedFile, ResolvedPackage};
    use crate::tui::app::{App, FileEntry, FileStatus};
    use crate::tui::event::DownloadEvent;

    fn test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();
        App::new(9723, tx, true)
    }

    fn render_text(app: &mut App) -> String {
        let backend = TestBackend::new(100, 24);
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
        assert!(rendered.contains("a:add URLs"));
        assert!(rendered.contains("up/down:select"));
        assert!(rendered.contains("queued.bin"));
        assert!(!rendered.contains("enter:add URLs"));
    }

    #[test]
    fn draw_main_shows_url_editing_mode() {
        let mut app = test_app();
        app.url_input_active = true;
        app.url_input = "https://mega.nz/file/test".to_string();

        let rendered = render_text(&mut app);

        assert!(rendered.contains("Add URL(s): editing"));
        assert!(rendered.contains("https://mega.nz/file/test"));
        assert!(rendered.contains("enter:add URLs"));
        assert!(rendered.contains("esc:cancel"));
        assert!(!rendered.contains("q:quit"));
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
    fn draw_main_active_and_error_packages_expand_by_default() {
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
        assert!(rendered.contains("active.bin"));
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
}
