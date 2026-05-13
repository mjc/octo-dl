//! All drawing / rendering functions.

mod dashboard;
mod popup;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Gauge, List, Paragraph};

use crate::format_bytes;

use self::dashboard::{
    controls_label_from_snapshot, dashboard_aggregate_progress_label, dashboard_row_items,
    dashboard_status_line, focused_url_input_view, truncate_end,
};
use super::app::{App, Popup};
use super::dashboard::{DashboardChrome, DashboardUiMode, DownloadDashboardState, clamp_selection};

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

    let status_line = Paragraph::new(Line::from(dashboard_status_line(
        state,
        chunks[3].width,
        list_state.selected(),
    )))
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
            rendered.find("a.bin").expect("a.bin should render")
                < rendered.find("b.bin").expect("b.bin should render")
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
