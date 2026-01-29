//! octo-tui - Interactive TUI for downloading MEGA files.

#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]

use std::time::Duration;
use std::{env, io, path::Path};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Row, Table, Wrap,
};
use tokio::sync::mpsc;

use octo_dl::{
    DlcKeyCache, DownloadConfig, DownloadProgress, FileStats, SavedCredentials, SessionState,
    SessionStatus, UrlEntry, UrlStatus,
};

// ============================================================================
// Event Types
// ============================================================================

#[derive(Debug)]
enum DownloadEvent {
    FileStart {
        name: String,
        size: u64,
    },
    Progress {
        name: String,
        bytes_delta: u64,
        speed: u64,
    },
    FileComplete {
        name: String,
    },
    Error {
        name: String,
        error: String,
    },
    SessionComplete {
        files_downloaded: usize,
        total_bytes: u64,
    },
    FilesCollected {
        total: usize,
        skipped: usize,
        partial: usize,
        total_bytes: u64,
    },
    StatusMessage(String),
}

// ============================================================================
// TUI Progress Adapter
// ============================================================================

struct TuiProgress {
    tx: mpsc::UnboundedSender<DownloadEvent>,
}

impl DownloadProgress for TuiProgress {
    fn on_file_start(&self, name: &str, size: u64) {
        let _ = self.tx.send(DownloadEvent::FileStart {
            name: name.to_string(),
            size,
        });
    }

    fn on_progress(&self, name: &str, bytes_delta: u64, speed: u64) {
        let _ = self.tx.send(DownloadEvent::Progress {
            name: name.to_string(),
            bytes_delta,
            speed,
        });
    }

    fn on_file_complete(&self, name: &str, _stats: &FileStats) {
        let _ = self.tx.send(DownloadEvent::FileComplete {
            name: name.to_string(),
        });
    }

    fn on_error(&self, name: &str, error: &str) {
        let _ = self.tx.send(DownloadEvent::Error {
            name: name.to_string(),
            error: error.to_string(),
        });
    }

    fn on_partial_detected(&self, name: &str, existing_size: u64, expected_size: u64) {
        let _ = self.tx.send(DownloadEvent::StatusMessage(format!(
            "Partial download detected: {name} ({existing_size}/{expected_size} bytes)"
        )));
    }
}

// ============================================================================
// Screens
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Login,
    UrlInput,
    Config,
    Download,
    Summary,
}

// ============================================================================
// App State
// ============================================================================

struct LoginState {
    email: String,
    password: String,
    mfa: String,
    active_field: usize,
    error: Option<String>,
}

impl LoginState {
    fn new() -> Self {
        Self {
            email: env::var("MEGA_EMAIL").unwrap_or_default(),
            password: env::var("MEGA_PASSWORD").unwrap_or_default(),
            mfa: env::var("MEGA_MFA").unwrap_or_default(),
            active_field: 0,
            error: None,
        }
    }

    const fn active_value_mut(&mut self) -> &mut String {
        match self.active_field {
            0 => &mut self.email,
            1 => &mut self.password,
            _ => &mut self.mfa,
        }
    }

    const fn field_count() -> usize {
        3
    }
}

struct UrlInputState {
    input: String,
    urls: Vec<String>,
    list_state: ListState,
    error: Option<String>,
}

impl UrlInputState {
    fn new() -> Self {
        Self {
            input: String::new(),
            urls: Vec::new(),
            list_state: ListState::default(),
            error: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ConfigField {
    ChunksPerFile,
    ConcurrentFiles,
    ForceOverwrite,
    CleanupOnError,
}

impl ConfigField {
    const ALL: [Self; 4] = [
        Self::ChunksPerFile,
        Self::ConcurrentFiles,
        Self::ForceOverwrite,
        Self::CleanupOnError,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::ChunksPerFile => "Chunks per file",
            Self::ConcurrentFiles => "Concurrent files",
            Self::ForceOverwrite => "Force overwrite",
            Self::CleanupOnError => "Cleanup on error",
        }
    }
}

struct ConfigState {
    config: DownloadConfig,
    active_field: usize,
}

impl ConfigState {
    fn new() -> Self {
        Self {
            config: DownloadConfig::default(),
            active_field: 0,
        }
    }
}

#[derive(Debug, Clone)]
struct FileProgress {
    name: String,
    size: u64,
    downloaded: u64,
    status: FileDownloadStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileDownloadStatus {
    Pending,
    Downloading,
    Complete,
    Error,
}

struct DownloadState {
    files: Vec<FileProgress>,
    aggregate_downloaded: u64,
    aggregate_total: u64,
    errors: Vec<String>,
    files_completed: usize,
    files_total: usize,
    current_speed: u64,
    status_messages: Vec<String>,
}

impl DownloadState {
    const fn new() -> Self {
        Self {
            files: Vec::new(),
            aggregate_downloaded: 0,
            aggregate_total: 0,
            errors: Vec::new(),
            files_completed: 0,
            files_total: 0,
            current_speed: 0,
            status_messages: Vec::new(),
        }
    }
}

struct SummaryState {
    files_downloaded: usize,
    files_skipped: usize,
    total_bytes: u64,
    errors: Vec<String>,
}

struct App {
    screen: Screen,
    should_quit: bool,
    login: LoginState,
    url_input: UrlInputState,
    config: ConfigState,
    download: DownloadState,
    summary: Option<SummaryState>,
    session: Option<SessionState>,
    download_tx: Option<mpsc::UnboundedSender<DownloadEvent>>,
}

impl App {
    fn new() -> Self {
        Self {
            screen: Screen::Login,
            should_quit: false,
            login: LoginState::new(),
            url_input: UrlInputState::new(),
            config: ConfigState::new(),
            download: DownloadState::new(),
            summary: None,
            session: None,
            download_tx: None,
        }
    }
}

// ============================================================================
// Formatting Helpers
// ============================================================================

#[allow(clippy::cast_precision_loss)]
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

// ============================================================================
// Drawing
// ============================================================================

fn draw(frame: &mut ratatui::Frame, app: &App) {
    match app.screen {
        Screen::Login => draw_login(frame, app),
        Screen::UrlInput => draw_url_input(frame, app),
        Screen::Config => draw_config(frame, app),
        Screen::Download => draw_download(frame, app),
        Screen::Summary => draw_summary(frame, app),
    }
}

fn draw_login(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let block = Block::default()
        .title(" Login to MEGA ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3), // Email
            Constraint::Length(3), // Password
            Constraint::Length(3), // MFA
            Constraint::Length(2), // Error/status
            Constraint::Min(0),    // Help
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

    if let Some(ref err) = app.login.error {
        let error = Paragraph::new(err.as_str()).style(Style::default().fg(Color::Red));
        frame.render_widget(error, chunks[3]);
    }

    let help = Paragraph::new("Tab/Shift-Tab: navigate | Enter: submit | q: quit")
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(help, chunks[4]);
}

fn draw_url_input(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let block = Block::default()
        .title(" Add URLs / DLC Files ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3), // Input field
            Constraint::Min(5),    // URL list
            Constraint::Length(2), // Error/status
            Constraint::Length(1), // Help
        ])
        .split(inner);

    // Input field
    let input = Paragraph::new(app.url_input.input.as_str())
        .block(
            Block::default()
                .title(" URL or DLC path ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .style(Style::default().fg(Color::White));
    frame.render_widget(input, chunks[0]);

    // URL list
    let items: Vec<ListItem> = app
        .url_input
        .urls
        .iter()
        .enumerate()
        .map(|(i, url)| {
            let selected = app.url_input.list_state.selected() == Some(i);
            let style = if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(url.as_str()).style(style)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(format!(" URLs ({}) ", app.url_input.urls.len()))
            .borders(Borders::ALL),
    );
    frame.render_stateful_widget(list, chunks[1], &mut app.url_input.list_state.clone());

    if let Some(ref err) = app.url_input.error {
        let error = Paragraph::new(err.as_str()).style(Style::default().fg(Color::Red));
        frame.render_widget(error, chunks[2]);
    }

    let help =
        Paragraph::new("Enter: add URL | d: remove selected | Ctrl+N: next screen | q: quit")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center);
    frame.render_widget(help, chunks[3]);
}

fn draw_config(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let block = Block::default()
        .title(" Download Configuration ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Min(8),    // Config table
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
    let table =
        Table::new(rows, widths).block(Block::default().title(" Settings ").borders(Borders::ALL));
    frame.render_widget(table, chunks[0]);

    let help = Paragraph::new(
        "Up/Down: navigate | +/-: adjust | Space: toggle | Enter: start downloads | q: quit",
    )
    .style(Style::default().fg(Color::DarkGray))
    .alignment(Alignment::Center);
    frame.render_widget(help, chunks[1]);
}

#[allow(clippy::too_many_lines)]
fn draw_download(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let block = Block::default()
        .title(" Downloading ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3), // Aggregate progress
            Constraint::Length(2), // Stats line
            Constraint::Min(5),    // Per-file progress
            Constraint::Length(6), // Error log
            Constraint::Length(1), // Help
        ])
        .split(inner);

    // Aggregate progress
    let ratio = if app.download.aggregate_total > 0 {
        #[allow(clippy::cast_precision_loss)]
        let r = app.download.aggregate_downloaded as f64 / app.download.aggregate_total as f64;
        r.min(1.0)
    } else {
        0.0
    };

    let gauge_label = format!(
        "{}/{} files | {}/{}",
        app.download.files_completed,
        app.download.files_total,
        format_bytes(app.download.aggregate_downloaded),
        format_bytes(app.download.aggregate_total),
    );

    let gauge = Gauge::default()
        .block(
            Block::default()
                .title(" Overall Progress ")
                .borders(Borders::ALL),
        )
        .gauge_style(Style::default().fg(Color::Green))
        .ratio(ratio)
        .label(gauge_label);
    frame.render_widget(gauge, chunks[0]);

    // Stats line
    let speed_str = format!("Speed: {}/s", format_bytes(app.download.current_speed));
    let stats_line = Paragraph::new(speed_str).style(Style::default().fg(Color::Cyan));
    frame.render_widget(stats_line, chunks[1]);

    // Per-file progress
    let file_items: Vec<ListItem> = app
        .download
        .files
        .iter()
        .filter(|f| {
            f.status == FileDownloadStatus::Downloading || f.status == FileDownloadStatus::Pending
        })
        .take(chunks[2].height as usize)
        .map(|f| {
            let (status_icon, color) = match f.status {
                FileDownloadStatus::Pending => ("○", Color::DarkGray),
                FileDownloadStatus::Downloading => ("●", Color::Yellow),
                FileDownloadStatus::Complete => ("✓", Color::Green),
                FileDownloadStatus::Error => ("✗", Color::Red),
            };

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

            let text = format!(
                "{status_icon} {}: {}/{} ({pct}%)",
                f.name,
                format_bytes(f.downloaded),
                format_bytes(f.size),
            );
            ListItem::new(text).style(Style::default().fg(color))
        })
        .collect();

    let file_list =
        List::new(file_items).block(Block::default().title(" Files ").borders(Borders::ALL));
    frame.render_widget(file_list, chunks[2]);

    // Error log
    let error_items: Vec<ListItem> = app
        .download
        .errors
        .iter()
        .rev()
        .take(4)
        .map(|e| ListItem::new(e.as_str()).style(Style::default().fg(Color::Red)))
        .collect();

    let error_list = List::new(error_items).block(
        Block::default()
            .title(" Errors ")
            .borders(Borders::ALL)
            .border_style(if app.download.errors.is_empty() {
                Style::default()
            } else {
                Style::default().fg(Color::Red)
            }),
    );
    frame.render_widget(error_list, chunks[3]);

    let help = Paragraph::new("q: quit")
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(help, chunks[4]);
}

fn draw_summary(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let block = Block::default()
        .title(" Download Summary ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Min(8),    // Summary content
            Constraint::Length(1), // Help
        ])
        .split(inner);

    if let Some(ref summary) = app.summary {
        let lines = vec![
            Line::from(vec![
                Span::styled("Files downloaded:  ", Style::default().fg(Color::Cyan)),
                Span::raw(summary.files_downloaded.to_string()),
            ]),
            Line::from(vec![
                Span::styled("Files skipped:     ", Style::default().fg(Color::Cyan)),
                Span::raw(summary.files_skipped.to_string()),
            ]),
            Line::from(vec![
                Span::styled("Total size:        ", Style::default().fg(Color::Cyan)),
                Span::raw(format_bytes(summary.total_bytes)),
            ]),
            Line::from(""),
            if summary.errors.is_empty() {
                Line::from(Span::styled(
                    "All downloads completed successfully!",
                    Style::default().fg(Color::Green),
                ))
            } else {
                Line::from(Span::styled(
                    format!("{} error(s) occurred", summary.errors.len()),
                    Style::default().fg(Color::Red),
                ))
            },
        ];

        let mut all_lines = lines;
        for err in &summary.errors {
            all_lines.push(Line::from(Span::styled(
                format!("  - {err}"),
                Style::default().fg(Color::Red),
            )));
        }

        let text = Text::from(all_lines);
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: true });
        frame.render_widget(paragraph, chunks[0]);
    }

    let help = Paragraph::new("q: quit | r: restart")
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(help, chunks[1]);
}

// ============================================================================
// Input Handling
// ============================================================================

fn handle_input(app: &mut App, key: KeyEvent) {
    // Global quit
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    match app.screen {
        Screen::Login => handle_login_input(app, key),
        Screen::UrlInput => handle_url_input(app, key),
        Screen::Config => handle_config_input(app, key),
        Screen::Download => handle_download_input(app, key),
        Screen::Summary => handle_summary_input(app, key),
    }
}

fn handle_login_input(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Tab => {
            app.login.active_field = (app.login.active_field + 1) % LoginState::field_count();
        }
        KeyCode::BackTab => {
            app.login.active_field = if app.login.active_field == 0 {
                LoginState::field_count() - 1
            } else {
                app.login.active_field - 1
            };
        }
        KeyCode::Enter => {
            if app.login.email.is_empty() || app.login.password.is_empty() {
                app.login.error = Some("Email and password are required".to_string());
            } else {
                app.login.error = None;
                app.screen = Screen::UrlInput;
            }
        }
        KeyCode::Char('q') if app.login.active_field != 0 || app.login.email.is_empty() => {
            // Only quit on 'q' if not typing in a field with content
            app.should_quit = true;
        }
        KeyCode::Char(c) => {
            app.login.active_value_mut().push(c);
        }
        KeyCode::Backspace => {
            app.login.active_value_mut().pop();
        }
        KeyCode::Esc => {
            app.should_quit = true;
        }
        _ => {}
    }
}

fn handle_url_input(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let input = app.url_input.input.trim().to_string();
            if !input.is_empty() {
                app.url_input.urls.push(input);
                app.url_input.input.clear();
                app.url_input.error = None;
            }
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.url_input.urls.is_empty() {
                app.url_input.error = Some("Add at least one URL".to_string());
            } else {
                app.url_input.error = None;
                app.screen = Screen::Config;
            }
        }
        KeyCode::Char('d') | KeyCode::Delete => {
            if app.url_input.input.is_empty() {
                if let Some(selected) = app.url_input.list_state.selected()
                    && selected < app.url_input.urls.len()
                {
                    app.url_input.urls.remove(selected);
                    if app.url_input.urls.is_empty() {
                        app.url_input.list_state.select(None);
                    } else {
                        app.url_input
                            .list_state
                            .select(Some(selected.min(app.url_input.urls.len() - 1)));
                    }
                }
            } else if key.code == KeyCode::Char('d') {
                app.url_input.input.push('d');
            }
        }
        KeyCode::Up => {
            let len = app.url_input.urls.len();
            if len > 0 {
                let i = app.url_input.list_state.selected().unwrap_or(0);
                app.url_input
                    .list_state
                    .select(Some(if i == 0 { len - 1 } else { i - 1 }));
            }
        }
        KeyCode::Down => {
            let len = app.url_input.urls.len();
            if len > 0 {
                let i = app.url_input.list_state.selected().unwrap_or(0);
                app.url_input.list_state.select(Some((i + 1) % len));
            }
        }
        KeyCode::Char(c) => {
            app.url_input.input.push(c);
        }
        KeyCode::Backspace => {
            app.url_input.input.pop();
        }
        KeyCode::Esc => {
            app.should_quit = true;
        }
        _ => {}
    }
}

fn handle_config_input(app: &mut App, key: KeyEvent) {
    let field_count = ConfigField::ALL.len();

    match key.code {
        KeyCode::Up | KeyCode::BackTab => {
            app.config.active_field = if app.config.active_field == 0 {
                field_count - 1
            } else {
                app.config.active_field - 1
            };
        }
        KeyCode::Down | KeyCode::Tab => {
            app.config.active_field = (app.config.active_field + 1) % field_count;
        }
        KeyCode::Char('+' | '=') | KeyCode::Right => {
            match ConfigField::ALL[app.config.active_field] {
                ConfigField::ChunksPerFile => {
                    app.config.config.chunks_per_file =
                        app.config.config.chunks_per_file.saturating_add(1);
                }
                ConfigField::ConcurrentFiles => {
                    app.config.config.concurrent_files =
                        app.config.config.concurrent_files.saturating_add(1);
                }
                ConfigField::ForceOverwrite => {
                    app.config.config.force_overwrite = !app.config.config.force_overwrite;
                }
                ConfigField::CleanupOnError => {
                    app.config.config.cleanup_on_error = !app.config.config.cleanup_on_error;
                }
            }
        }
        KeyCode::Char('-') | KeyCode::Left => match ConfigField::ALL[app.config.active_field] {
            ConfigField::ChunksPerFile => {
                app.config.config.chunks_per_file =
                    app.config.config.chunks_per_file.saturating_sub(1).max(1);
            }
            ConfigField::ConcurrentFiles => {
                app.config.config.concurrent_files =
                    app.config.config.concurrent_files.saturating_sub(1).max(1);
            }
            ConfigField::ForceOverwrite => {
                app.config.config.force_overwrite = !app.config.config.force_overwrite;
            }
            ConfigField::CleanupOnError => {
                app.config.config.cleanup_on_error = !app.config.config.cleanup_on_error;
            }
        },
        KeyCode::Char(' ') => match ConfigField::ALL[app.config.active_field] {
            ConfigField::ForceOverwrite => {
                app.config.config.force_overwrite = !app.config.config.force_overwrite;
            }
            ConfigField::CleanupOnError => {
                app.config.config.cleanup_on_error = !app.config.config.cleanup_on_error;
            }
            _ => {}
        },
        KeyCode::Enter => {
            app.screen = Screen::Download;
        }
        KeyCode::Char('q') | KeyCode::Esc => {
            app.should_quit = true;
        }
        _ => {}
    }
}

#[allow(clippy::missing_const_for_fn)]
fn handle_download_input(app: &mut App, key: KeyEvent) {
    if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
        app.should_quit = true;
    }
}

fn handle_summary_input(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.should_quit = true;
        }
        KeyCode::Char('r') => {
            // Restart
            app.login = LoginState::new();
            app.url_input = UrlInputState::new();
            app.config = ConfigState::new();
            app.download = DownloadState::new();
            app.summary = None;
            app.session = None;
            app.screen = Screen::Login;
        }
        _ => {}
    }
}

// ============================================================================
// Download Event Handling
// ============================================================================

fn handle_download_event(app: &mut App, event: DownloadEvent) {
    match event {
        DownloadEvent::FilesCollected {
            total,
            skipped,
            partial,
            total_bytes,
        } => {
            app.download.files_total = total;
            app.download.aggregate_total = total_bytes;
            app.download.status_messages.push(format!(
                "Found {total} files ({skipped} skipped, {partial} partial)"
            ));
        }
        DownloadEvent::FileStart { name, size } => {
            // Find existing entry or add new
            if let Some(fp) = app.download.files.iter_mut().find(|f| f.name == name) {
                fp.status = FileDownloadStatus::Downloading;
            } else {
                app.download.files.push(FileProgress {
                    name,
                    size,
                    downloaded: 0,
                    status: FileDownloadStatus::Downloading,
                });
            }
        }
        DownloadEvent::Progress {
            name,
            bytes_delta,
            speed,
        } => {
            if let Some(fp) = app.download.files.iter_mut().find(|f| f.name == name) {
                fp.downloaded = fp.downloaded.saturating_add(bytes_delta);
            }
            app.download.aggregate_downloaded = app
                .download
                .aggregate_downloaded
                .saturating_add(bytes_delta);
            app.download.current_speed = speed;
        }
        DownloadEvent::FileComplete { name } => {
            if let Some(fp) = app.download.files.iter_mut().find(|f| f.name == name) {
                fp.status = FileDownloadStatus::Complete;
                fp.downloaded = fp.size;
            }
            app.download.files_completed += 1;

            // Update session state
            if let Some(ref mut session) = app.session {
                let _ = session.mark_file_complete(&name);
            }
        }
        DownloadEvent::Error { name, error } => {
            if let Some(fp) = app.download.files.iter_mut().find(|f| f.name == name) {
                fp.status = FileDownloadStatus::Error;
            }
            app.download.errors.push(format!("{name}: {error}"));

            if let Some(ref mut session) = app.session {
                let _ = session.mark_file_error(&name, &error);
            }
        }
        DownloadEvent::SessionComplete {
            files_downloaded,
            total_bytes,
        } => {
            app.summary = Some(SummaryState {
                files_downloaded,
                files_skipped: app.download.files_total
                    - files_downloaded
                    - app.download.errors.len(),
                total_bytes,
                errors: app.download.errors.clone(),
            });
            app.screen = Screen::Summary;

            if let Some(ref mut session) = app.session {
                let _ = session.mark_completed();
            }
        }
        DownloadEvent::StatusMessage(msg) => {
            app.download.status_messages.push(msg);
        }
    }
}

// ============================================================================
// Download Task
// ============================================================================

#[allow(clippy::too_many_lines)]
async fn run_download(
    email: String,
    password: String,
    mfa: Option<String>,
    urls: Vec<String>,
    config: DownloadConfig,
    tx: mpsc::UnboundedSender<DownloadEvent>,
) {
    let progress = TuiProgress { tx: tx.clone() };

    let _ = tx.send(DownloadEvent::StatusMessage(
        "Creating HTTP client...".to_string(),
    ));

    let http = match reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(30))
        .build()
    {
        Ok(http) => http,
        Err(e) => {
            let _ = tx.send(DownloadEvent::Error {
                name: "setup".to_string(),
                error: format!("Failed to build HTTP client: {e}"),
            });
            return;
        }
    };

    // Process DLC files
    let dlc_cache = DlcKeyCache::new();
    let mut all_urls = Vec::new();
    for url in &urls {
        if Path::new(url)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("dlc"))
        {
            let _ = tx.send(DownloadEvent::StatusMessage(format!(
                "Processing DLC: {url}"
            )));
            match octo_dl::parse_dlc_file(url, &http, &dlc_cache).await {
                Ok(dlc_urls) => {
                    let _ = tx.send(DownloadEvent::StatusMessage(format!(
                        "DLC {url}: {} MEGA link(s)",
                        dlc_urls.len()
                    )));
                    all_urls.extend(dlc_urls);
                }
                Err(e) => {
                    let _ = tx.send(DownloadEvent::Error {
                        name: url.clone(),
                        error: format!("DLC parse error: {e}"),
                    });
                }
            }
        } else {
            all_urls.push(url.clone());
        }
    }

    let _ = tx.send(DownloadEvent::StatusMessage("Logging in...".to_string()));

    let mut mega_client = match mega::Client::builder().build(http) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(DownloadEvent::Error {
                name: "login".to_string(),
                error: format!("Failed to create MEGA client: {e}"),
            });
            return;
        }
    };

    if let Err(e) = mega_client.login(&email, &password, mfa.as_deref()).await {
        let _ = tx.send(DownloadEvent::Error {
            name: "login".to_string(),
            error: format!("Login failed: {e}"),
        });
        return;
    }

    let _ = tx.send(DownloadEvent::StatusMessage(
        "Login successful. Fetching file lists...".to_string(),
    ));

    let downloader = octo_dl::Downloader::new(mega_client, config);

    // Fetch all URLs — collect nodes first, then collect files
    let mut node_sets: Vec<mega::Nodes> = Vec::new();
    for url in &all_urls {
        let _ = tx.send(DownloadEvent::StatusMessage(format!("Fetching: {url}")));
        match downloader.client().fetch_public_nodes(url).await {
            Ok(nodes) => {
                node_sets.push(nodes);
            }
            Err(e) => {
                let _ = tx.send(DownloadEvent::Error {
                    name: url.clone(),
                    error: format!("Fetch failed: {e}"),
                });
            }
        }
    }

    let mut all_collected_items = Vec::new();
    let mut actual_skipped = 0;
    let mut actual_partial = 0;

    for nodes in &node_sets {
        let collected = downloader.collect_files(nodes, &progress).await;
        actual_skipped += collected.skipped;
        actual_partial += collected.partial;
        all_collected_items.extend(collected.to_download);
    }

    let total_bytes: u64 = all_collected_items.iter().map(|i| i.node.size()).sum();
    let total_files = all_collected_items.len();

    let _ = tx.send(DownloadEvent::FilesCollected {
        total: total_files,
        skipped: actual_skipped,
        partial: actual_partial,
        total_bytes,
    });

    if all_collected_items.is_empty() {
        let _ = tx.send(DownloadEvent::SessionComplete {
            files_downloaded: 0,
            total_bytes: 0,
        });
        return;
    }

    // Download files sequentially (avoids lifetime issues with buffer_unordered + tokio::spawn)
    let mut files_downloaded = 0usize;
    let mut bytes_downloaded = 0u64;

    for item in &all_collected_items {
        let result = downloader
            .download_file(item.node, &item.path, &progress)
            .await;

        match result {
            Ok(stats) => {
                files_downloaded += 1;
                bytes_downloaded += stats.size;
            }
            Err(e) => {
                let _ = tx.send(DownloadEvent::Error {
                    name: item.path.clone(),
                    error: format!("Download failed: {e}"),
                });
            }
        }
    }

    let _ = tx.send(DownloadEvent::SessionComplete {
        files_downloaded,
        total_bytes: bytes_downloaded,
    });
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> io::Result<()> {
    // Initialize terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (download_tx, mut download_rx) = mpsc::unbounded_channel::<DownloadEvent>();

    let mut app = App::new();
    app.download_tx = Some(download_tx.clone());

    // Check for resumable session
    if let Some(session) = SessionState::latest() {
        // Pre-fill from session
        if let Some((email, password, mfa)) = session.credentials.decrypt() {
            app.login.email = email;
            app.login.password = password;
            app.login.mfa = mfa.unwrap_or_default();
        }

        // Pre-fill URLs
        app.url_input.urls = session.urls.iter().map(|u| u.url.clone()).collect();

        app.session = Some(session);
    }

    let mut download_started = false;

    loop {
        terminal.draw(|f| draw(f, &app))?;

        // Poll for events with 100ms timeout
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            let was_on_config = app.screen == Screen::Config;
            handle_input(&mut app, key);

            // Check if we need to start downloads (transition from Config to Download)
            if was_on_config && app.screen == Screen::Download && !download_started {
                download_started = true;

                let email = app.login.email.clone();
                let password = app.login.password.clone();
                let mfa = if app.login.mfa.is_empty() {
                    None
                } else {
                    Some(app.login.mfa.clone())
                };
                let urls = app.url_input.urls.clone();
                let config = app.config.config.clone();
                let tx = download_tx.clone();

                // Create session state
                let url_entries: Vec<UrlEntry> = urls
                    .iter()
                    .map(|url| UrlEntry {
                        url: url.clone(),
                        status: UrlStatus::Pending,
                    })
                    .collect();

                let session = SessionState::new(
                    SavedCredentials::encrypt(&email, &password, mfa.as_deref()),
                    config.clone(),
                    url_entries,
                );
                let _ = session.save();
                app.session = Some(session);

                tokio::spawn(async move {
                    run_download(email, password, mfa, urls, config, tx).await;
                });
            }
        }

        // Drain download events (non-blocking)
        while let Ok(event) = download_rx.try_recv() {
            handle_download_event(&mut app, event);
        }

        if app.should_quit {
            // Save session state on quit
            if let Some(ref mut session) = app.session
                && session.status != SessionStatus::Completed
            {
                let _ = session.mark_paused();
            }
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
