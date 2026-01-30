//! Interactive TUI mode for octo.

use std::io;
use std::time::Duration;

use crossterm::event::Event;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::layout::{Constraint, Direction, Layout};

use crate::AppConfig;

/// Runs the TUI mode with the given configuration.
///
/// # Errors
///
/// Returns an error if the TUI cannot be initialized or run.
pub async fn run(config: AppConfig) -> io::Result<()> {
    // Initialize terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Start API server if enabled
    if config.api.enabled {
        let api_host = config.api.host.clone();
        let api_port = config.api.port;
        tokio::spawn(async move {
            if let Err(e) = crate::api::run_server(&api_host, api_port).await {
                log::error!("API server error: {}", e);
            }
        });
    }

    let api_port = config.api.port;
    let api_enabled = config.api.enabled;

    // Main TUI loop
    loop {
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(2),
                ])
                .split(f.area());

            let title = if api_enabled {
                format!("octo TUI (API enabled on port {})", api_port)
            } else {
                "octo TUI".to_string()
            };

            let header = Block::default()
                .title(title)
                .borders(Borders::ALL);
            let header_text = Paragraph::new("Enter URLs to download, or paste from browser bookmarklet\nPress 'q' to quit, 'c' for config")
                .block(header);
            f.render_widget(header_text, chunks[0]);

            let content = Block::default()
                .title("Download Queue")
                .borders(Borders::ALL);
            let content_text = Paragraph::new("[TUI mode - full implementation coming soon]")
                .block(content);
            f.render_widget(content_text, chunks[1]);

            let footer = Block::default().borders(Borders::TOP);
            let footer_text = Paragraph::new("Status: Ready").block(footer);
            f.render_widget(footer_text, chunks[2]);
        })?;

        // Poll for events with 100ms timeout
        if crossterm::event::poll(Duration::from_millis(100))? {
            match crossterm::event::read()? {
                Event::Key(key) => {
                    if key.code == crossterm::event::KeyCode::Char('q') {
                        break;
                    }
                }
                Event::Paste(_) => {
                    // Handle paste in full implementation
                }
                _ => {}
            }
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    Ok(())
}
