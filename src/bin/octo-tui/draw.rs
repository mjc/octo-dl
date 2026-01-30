//! All drawing / rendering functions.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Row, Table,
};

use octo_dl::format_bytes;

use crate::app::{App, ConfigField, FileStatus, Popup};

pub fn draw(frame: &mut ratatui::Frame, app: &App) {
    draw_main(frame, app);
    match app.popup {
        Popup::None => {}
        Popup::Login => draw_login_popup(frame, app),
        Popup::Config => draw_config_popup(frame, app),
    }
}

#[allow(clippy::too_many_lines)]
fn draw_main(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();

    // Outer block with title bar
    let title = " octo-dl ".to_string();
    let title_right = format!(
        " {}% CPU | {} RAM | API: :{}{}",
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
    let url_style = if app.popup == Popup::None {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let url_input = Paragraph::new(app.url_input.as_str())
        .block(
            Block::default()
                .title(" Add URL(s): ")
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
    let gauge_label = format!(
        "{}%  {}/{} files  {}/s",
        pct,
        app.files_completed,
        app.files_total,
        format_bytes(app.current_speed),
    );
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
    let controls = if app.paused {
        "p:resume  d:delete  r:retry  c:config  q:quit"
    } else {
        "p:pause  d:delete  r:retry  c:config  q:quit"
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

    if !app.status.is_empty() {
        spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            app.status.as_str(),
            Style::default().fg(Color::Cyan),
        ));
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

fn draw_file_list(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    // Sort files: downloading first, then queued, then complete, then error
    let mut indexed: Vec<_> = app.files.iter().enumerate().collect();
    indexed.sort_by_key(|(_, f)| match &f.status {
        FileStatus::Downloading => 0,
        FileStatus::Queued => 1,
        FileStatus::Complete => 2,
        FileStatus::Error(_) => 3,
    });

    let items: Vec<ListItem> = indexed
        .iter()
        .map(|(i, f)| {
            let selected = app.file_list_state.selected() == Some(*i);

            let (icon, color) = match &f.status {
                FileStatus::Downloading => ("\u{25cf}", Color::Yellow),
                FileStatus::Queued => ("\u{25cb}", Color::DarkGray),
                FileStatus::Complete => ("\u{2713}", Color::Green),
                FileStatus::Error(_) => ("\u{2717}", Color::Red),
            };

            let detail = match &f.status {
                FileStatus::Downloading => {
                    #[allow(
                        clippy::cast_precision_loss,
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss
                    )]
                    let pct = if f.size > 0 {
                        ((f.downloaded as f64 / f.size as f64 * 100.0) as u64).min(100)
                    } else {
                        0
                    };
                    let bar = progress_bar(f.downloaded, f.size, 10);
                    format!("[{bar}] {pct}%  {}/s", format_bytes(f.speed))
                }
                FileStatus::Queued => "queued".to_string(),
                FileStatus::Complete => {
                    format!("{}  done", format_bytes(f.size))
                }
                FileStatus::Error(msg) => msg.clone(),
            };

            let style = if selected {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            };

            let text = format!(" {icon} {:<30}  {detail}", f.name);
            ListItem::new(text).style(style)
        })
        .collect();

    let file_list = List::new(items).block(Block::default().borders(Borders::ALL));
    frame.render_stateful_widget(file_list, area, &mut app.file_list_state.clone());
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
        ("Email", &app.login.email, false),
        ("Password", &app.login.password, true),
        ("MFA (optional)", &app.login.mfa, false),
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
            (*value).clone()
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

/// Returns a centered rectangle of the given size within `area`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}
