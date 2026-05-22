use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, Wrap};

use crate::tui::app::{App, ConfigField, ConfirmAction, SortKey};

use super::truncate_end;

pub(super) fn draw_login_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(64, 14, frame.area());
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
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(1),
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

    if app.login.logging_in {
        let spinner = Paragraph::new(" Logging in...").style(Style::default().fg(Color::Yellow));
        frame.render_widget(spinner, chunks[3]);
    } else if let Some(ref err) = app.login.error {
        let error = Paragraph::new(format!(" Login failed: {err}"))
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: true });
        frame.render_widget(error, chunks[3]);
    } else {
        let help = Paragraph::new(" Tab: next | Enter: login | Esc: quit")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(help, chunks[3]);
    }
}

pub(super) fn draw_config_popup(frame: &mut ratatui::Frame, app: &App) {
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
        .constraints([Constraint::Min(4), Constraint::Length(1)])
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

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(20),
            Constraint::Length(10),
        ],
    );
    frame.render_widget(table, chunks[0]);

    let help = Paragraph::new(" Enter/Esc to close").style(Style::default().fg(Color::DarkGray));
    frame.render_widget(help, chunks[1]);
}

pub(super) fn draw_sort_popup(frame: &mut ratatui::Frame, app: &App) {
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

pub(super) fn draw_confirm_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(58, 7, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Confirm ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (action, target, name) = match app.pending_confirmation.as_ref() {
        Some(ConfirmAction::DeleteFile(id)) => (
            "Delete",
            "file",
            app.files
                .iter()
                .find(|file| file.id == *id)
                .map_or_else(|| id.to_string(), |file| file.name.clone()),
        ),
        Some(ConfirmAction::DeletePackage(id)) => (
            "Delete",
            "package",
            app.core_state
                .packages
                .get(id)
                .map_or_else(|| id.to_string(), |package| package.display_name.clone()),
        ),
        Some(ConfirmAction::ResetFile(id)) => (
            "Reset",
            "file",
            app.files
                .iter()
                .find(|file| file.id == *id)
                .map_or_else(|| id.to_string(), |file| file.name.clone()),
        ),
        Some(ConfirmAction::ResetPackage(id)) => (
            "Reset",
            "package",
            app.core_state
                .packages
                .get(id)
                .map_or_else(|| id.to_string(), |package| package.display_name.clone()),
        ),
        None => ("Confirm", "item", String::new()),
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

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}
