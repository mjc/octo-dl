//! All drawing / rendering functions.

mod dashboard;
mod popup;

use std::fmt::Write as _;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};

use self::dashboard::{
    compact_label, controls_label_from_snapshot, dashboard_aggregate_progress_label,
    dashboard_status_line, draw_dashboard_file_list, focused_url_input_view, package_status_style,
    text_width, truncate_end,
};
use super::app::{App, FileEntry, FileStatus, Popup};
use super::dashboard::{DashboardChrome, DashboardUiMode, DownloadDashboardState, clamp_selection};
use super::visible::TuiRow;
use crate::core::{FileLifecycle, PackageStatus};

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
    let mut title_right = String::with_capacity(48);
    let _ = write!(title_right, " {}% CPU | ", (app.cpu_usage as u16).min(999),);
    push_byte_label(&mut title_right, app.memory_rss);
    let _ = write!(title_right, " RAM | API: {}", app.api_port);
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
    let mut title_right = String::with_capacity(64);
    let _ = write!(
        title_right,
        " {}% CPU | ",
        (state.metrics.cpu_usage as u16).min(999),
    );
    push_byte_label(&mut title_right, state.metrics.memory_rss);
    let _ = write!(title_right, " RAM | API: {}", state.metrics.api_port);
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
    let detail_color = if app.is_verification_active(file_id) {
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
    let detail = FileDetail::new(app, file, &status);
    let prefix_width = 5;
    let detail_width = detail.width().min(content_width / 2);
    let owned_display_name;
    let display_name = if let Some(prefix_label) = prefix_label {
        owned_display_name = prefixed_file_label(&prefix_label, &file.name);
        owned_display_name.as_str()
    } else {
        file.name.as_str()
    };
    let slots = RowSlots::new(
        content_width,
        prefix_width,
        detail_width,
        text_width(display_name),
    );
    let mut row_style = Style::default().fg(if app.is_verification_active(file_id) {
        Color::Blue
    } else {
        color
    });
    if selected {
        row_style = row_style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
    }
    let row_style = selected_style(row_style, selected);
    let detail_style = selected_style(Style::default().fg(detail_color), selected);
    let mut cursor = x;
    render_text(frame, &mut cursor, y, "   ", row_style);
    render_text(frame, &mut cursor, y, icon, row_style);
    render_text(frame, &mut cursor, y, " ", row_style);
    render_truncated_text(
        frame,
        &mut cursor,
        y,
        display_name,
        slots.name_width,
        row_style,
    );
    cursor = cursor.saturating_add(u16::try_from(slots.filler_width).unwrap_or(u16::MAX));
    detail.render(frame, &mut cursor, y, slots.detail_width, detail_style);
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
    let stats = PackageRowStats::new(app, package_id);
    let percent = percent(stats.downloaded, stats.size);
    let package_status = package.status();
    let expanded = app.expanded_packages.contains(&package_id)
        || matches!(package_status, PackageStatus::Failed);
    let (icon, mut color) = package_status_style(package_status, percent);
    if stats.active() {
        color = Color::Yellow;
    }
    let marker = if stats.present > 1 {
        if expanded { "-" } else { "+" }
    } else {
        " "
    };
    let detail = PackageDetail {
        completed: stats.complete,
        present: stats.present,
        downloaded: stats.downloaded,
        total_bytes: stats.size,
        percent,
        speed_label: stats.activity_label(package_status),
    };
    let prefix_width = 5;
    let detail_width = detail.width(content_width / 2);
    let name = PackageName::new(&package.display_name, stats.folder_label());
    let slots = RowSlots::new(content_width, prefix_width, detail_width, name.width());
    let mut row_style = Style::default().fg(color);
    if selected {
        row_style = row_style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
    }
    let row_style = selected_style(row_style, selected);
    let detail_style = selected_style(Style::default().fg(Color::DarkGray), selected);
    let mut cursor = x;
    render_text(frame, &mut cursor, y, " ", row_style);
    render_text(frame, &mut cursor, y, marker, row_style);
    render_text(frame, &mut cursor, y, " ", row_style);
    render_text(frame, &mut cursor, y, icon, row_style);
    render_text(frame, &mut cursor, y, " ", row_style);
    name.render(frame, &mut cursor, y, slots.name_width, row_style);
    cursor = cursor.saturating_add(u16::try_from(slots.filler_width).unwrap_or(u16::MAX));
    detail.render(frame, &mut cursor, y, slots.detail_width, detail_style);
}

struct RowSlots {
    name_width: usize,
    detail_width: usize,
    filler_width: usize,
}

impl RowSlots {
    fn new(
        content_width: usize,
        prefix_width: usize,
        detail_width: usize,
        natural_name_width: usize,
    ) -> Self {
        let name_limit = content_width
            .saturating_sub(prefix_width)
            .saturating_sub(detail_width)
            .saturating_sub(1);
        let name_width = natural_name_width.min(name_limit);
        let filler_width = content_width
            .saturating_sub(prefix_width)
            .saturating_sub(name_width)
            .saturating_sub(detail_width);
        Self {
            name_width,
            detail_width,
            filler_width,
        }
    }
}

struct PackageRowStats<'a> {
    present: usize,
    complete: usize,
    downloaded: u64,
    size: u64,
    common_folder: Option<&'a str>,
    folder_conflict: bool,
    downloading: bool,
    verifying: bool,
}

impl<'a> PackageRowStats<'a> {
    fn new(app: &'a App, package_id: crate::core::PackageId) -> Self {
        let mut stats = Self {
            present: 0,
            complete: 0,
            downloaded: 0,
            size: 0,
            common_folder: None,
            folder_conflict: false,
            downloading: false,
            verifying: false,
        };
        for file in app.core_state.files.values() {
            if file.package_id != package_id {
                continue;
            }
            stats.downloading |= matches!(file.lifecycle, FileLifecycle::Downloading);
            stats.verifying |= app.is_verification_active(&file.id);
            stats.record_folder(file.path.split('/').next().filter(|part| !part.is_empty()));

            let file_complete = matches!(file.lifecycle, FileLifecycle::Complete);
            let visible = if file_complete {
                file.size
            } else {
                crate::core::visible_completed_bytes_for_display(file)
            };
            stats.present += 1;
            stats.complete += usize::from(file_complete);
            stats.downloaded = stats.downloaded.saturating_add(visible);
            stats.size = stats.size.saturating_add(file.size);
        }
        stats
    }

    fn record_folder(&mut self, folder: Option<&'a str>) {
        match (self.common_folder, folder) {
            (None, Some(folder)) => self.common_folder = Some(folder),
            (Some(existing), Some(folder)) if existing == folder => {}
            (Some(_), Some(_)) => self.folder_conflict = true,
            _ => {}
        }
    }

    fn active(&self) -> bool {
        self.downloading || self.verifying
    }

    fn activity_label(&self, status: PackageStatus) -> &'static str {
        if self.verifying {
            "verify"
        } else if self.downloading || matches!(status, PackageStatus::Downloading) {
            "active"
        } else {
            ""
        }
    }

    fn folder_label(&self) -> Option<&'a str> {
        (!self.folder_conflict)
            .then_some(self.common_folder)
            .flatten()
    }
}

fn selected_style(style: Style, selected: bool) -> Style {
    if selected {
        style.bg(Color::DarkGray)
    } else {
        style
    }
}

fn render_text(frame: &mut ratatui::Frame, x: &mut u16, y: u16, text: &str, style: Style) {
    frame.buffer_mut().set_string(*x, y, text, style);
    *x = x.saturating_add(u16::try_from(text_width(text)).unwrap_or(u16::MAX));
}

fn render_truncated_text(
    frame: &mut ratatui::Frame,
    x: &mut u16,
    y: u16,
    value: &str,
    max_width: usize,
    style: Style,
) {
    if max_width == 0 {
        return;
    }
    if text_width(value) <= max_width {
        render_text(frame, x, y, value, style);
        return;
    }
    if max_width <= 1 {
        render_text(frame, x, y, "\u{2026}", style);
        return;
    }

    let prefix_width = max_width - 1;
    if value.is_ascii() {
        let prefix = &value[..prefix_width.min(value.len())];
        render_text(frame, x, y, prefix, style);
        render_text(frame, x, y, "\u{2026}", style);
        return;
    }

    let mut written = 0_usize;
    for ch in value.chars() {
        if written >= prefix_width {
            break;
        }
        let mut buf = [0_u8; 4];
        let text = ch.encode_utf8(&mut buf);
        render_text(frame, x, y, text, style);
        written += text_width(text);
    }
    render_text(frame, x, y, "\u{2026}", style);
}

fn render_clipped_text(
    frame: &mut ratatui::Frame,
    x: &mut u16,
    y: u16,
    remaining: &mut usize,
    text: &str,
    style: Style,
) {
    if *remaining == 0 {
        return;
    }
    let width = text_width(text);
    if width <= *remaining {
        render_text(frame, x, y, text, style);
        *remaining -= width;
        return;
    }
    render_truncated_text(frame, x, y, text, *remaining, style);
    *remaining = 0;
}

fn file_status_for_draw(app: &App, file: &FileEntry) -> FileStatus {
    if app.is_verification_active(&file.id) {
        FileStatus::Downloading
    } else {
        file.status.clone()
    }
}

enum FileDetail<'a> {
    Borrowed(&'a str),
    Complete {
        size: u64,
    },
    Active {
        downloaded: u64,
        size: u64,
        pct: u64,
        suffix: ActiveDetailSuffix,
    },
}

enum ActiveDetailSuffix {
    Active,
    Verify,
    Speed { bytes_per_sec: u64 },
}

impl<'a> FileDetail<'a> {
    fn new(app: &App, file: &'a FileEntry, status: &'a FileStatus) -> Self {
        let verifying = app.is_verification_active(&file.id);
        if verifying || matches!(status, FileStatus::Downloading) {
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
            let suffix = if verifying {
                ActiveDetailSuffix::Verify
            } else if file_speed > 0 {
                ActiveDetailSuffix::Speed {
                    bytes_per_sec: file_speed,
                }
            } else {
                ActiveDetailSuffix::Active
            };
            return Self::Active {
                downloaded: file.downloaded,
                size: file.size,
                pct,
                suffix,
            };
        }
        match status {
            FileStatus::Queued => Self::Borrowed("queued"),
            FileStatus::Complete => Self::Complete { size: file.size },
            FileStatus::Error(message) => Self::Borrowed(message),
            FileStatus::Downloading => unreachable!("downloading handled above"),
        }
    }

    fn width(&self) -> usize {
        match self {
            Self::Borrowed(value) => text_width(value),
            Self::Complete { size } => byte_label_width(*size) + "  done".len(),
            Self::Active { pct, suffix, .. } => 1 + 10 + 2 + decimal_len(*pct) + 1 + suffix.width(),
        }
    }

    fn render(
        &self,
        frame: &mut ratatui::Frame,
        x: &mut u16,
        y: u16,
        max_width: usize,
        style: Style,
    ) {
        match self {
            Self::Borrowed(value) => render_truncated_text(frame, x, y, value, max_width, style),
            Self::Complete { size } => {
                let mut remaining = max_width;
                render_byte_label(frame, x, y, &mut remaining, *size, style);
                render_clipped_text(frame, x, y, &mut remaining, "  done", style);
            }
            Self::Active {
                downloaded,
                size,
                pct,
                suffix,
            } => render_active_file_detail(
                frame,
                x,
                y,
                max_width,
                *downloaded,
                *size,
                *pct,
                suffix,
                style,
            ),
        }
    }
}

impl ActiveDetailSuffix {
    fn width(&self) -> usize {
        match self {
            Self::Active => 8,
            Self::Verify => 8,
            Self::Speed { bytes_per_sec } => 2 + byte_label_width(*bytes_per_sec) + 2,
        }
    }

    fn render(
        &self,
        frame: &mut ratatui::Frame,
        x: &mut u16,
        y: u16,
        remaining: &mut usize,
        style: Style,
    ) {
        match self {
            Self::Active => render_clipped_text(frame, x, y, remaining, "  active", style),
            Self::Verify => render_clipped_text(frame, x, y, remaining, "  verify", style),
            Self::Speed { bytes_per_sec } => {
                render_clipped_text(frame, x, y, remaining, "  ", style);
                render_byte_label(frame, x, y, remaining, *bytes_per_sec, style);
                render_clipped_text(frame, x, y, remaining, "/s", style);
            }
        }
    }
}

fn render_active_file_detail(
    frame: &mut ratatui::Frame,
    x: &mut u16,
    y: u16,
    max_width: usize,
    downloaded: u64,
    size: u64,
    pct: u64,
    suffix: &ActiveDetailSuffix,
    style: Style,
) {
    let mut remaining = max_width;
    render_clipped_text(frame, x, y, &mut remaining, "[", style);
    render_progress_bar_clipped(frame, x, y, &mut remaining, downloaded, size, 10, style);
    render_clipped_text(frame, x, y, &mut remaining, "] ", style);
    let mut pct_buf = [0_u8; 20];
    let pct_text = decimal_str(pct, &mut pct_buf);
    render_clipped_text(frame, x, y, &mut remaining, pct_text, style);
    render_clipped_text(frame, x, y, &mut remaining, "%", style);
    suffix.render(frame, x, y, &mut remaining, style);
}

fn render_progress_bar_clipped(
    frame: &mut ratatui::Frame,
    x: &mut u16,
    y: u16,
    remaining: &mut usize,
    downloaded: u64,
    total: u64,
    width: usize,
    style: Style,
) {
    if *remaining == 0 {
        return;
    }
    let filled = if total == 0 {
        0
    } else {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        { ((downloaded as f64 / total as f64) * width as f64) as usize }.min(width)
    };
    for index in 0..width {
        if *remaining == 0 {
            return;
        }
        let text = if index < filled {
            "\u{2588}"
        } else {
            "\u{2591}"
        };
        render_clipped_text(frame, x, y, remaining, text, style);
    }
}

fn decimal_len(value: u64) -> usize {
    if value == 0 {
        return 1;
    }
    value.ilog10() as usize + 1
}

fn decimal_str(value: u64, buf: &mut [u8; 20]) -> &str {
    let mut value = value;
    let mut index = buf.len();
    loop {
        index -= 1;
        buf[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[index..]).expect("decimal digits are valid UTF-8")
}

struct PackageDetail<'a> {
    completed: usize,
    present: usize,
    downloaded: u64,
    total_bytes: u64,
    percent: u64,
    speed_label: &'a str,
}

impl PackageDetail<'_> {
    fn width(&self, max_width: usize) -> usize {
        if self.full_width() <= max_width {
            self.full_width()
        } else {
            self.compact_width().min(max_width)
        }
    }

    fn full_width(&self) -> usize {
        decimal_len(self.completed as u64)
            + 1
            + decimal_len(self.present as u64)
            + " files  ".len()
            + byte_label_width(self.downloaded)
            + " / ".len()
            + byte_label_width(self.total_bytes)
            + "  ".len()
            + 3
            + "%  ".len()
            + text_width(self.speed_label)
    }

    fn compact_width(&self) -> usize {
        decimal_len(self.completed as u64)
            + 1
            + decimal_len(self.present as u64)
            + "  ".len()
            + byte_label_width(self.total_bytes)
            + "  ".len()
            + 3
            + "%  ".len()
            + text_width(self.speed_label)
    }

    fn render(
        &self,
        frame: &mut ratatui::Frame,
        x: &mut u16,
        y: u16,
        max_width: usize,
        style: Style,
    ) {
        let mut remaining = max_width;
        if self.full_width() <= max_width {
            self.render_count(frame, x, y, &mut remaining, style);
            render_clipped_text(frame, x, y, &mut remaining, " files  ", style);
            render_byte_label(frame, x, y, &mut remaining, self.downloaded, style);
            render_clipped_text(frame, x, y, &mut remaining, " / ", style);
            render_byte_label(frame, x, y, &mut remaining, self.total_bytes, style);
        } else {
            self.render_count(frame, x, y, &mut remaining, style);
            render_clipped_text(frame, x, y, &mut remaining, "  ", style);
            render_byte_label(frame, x, y, &mut remaining, self.total_bytes, style);
        }
        render_clipped_text(frame, x, y, &mut remaining, "  ", style);
        render_padded_percent(frame, x, y, &mut remaining, self.percent, style);
        render_clipped_text(frame, x, y, &mut remaining, "%  ", style);
        render_clipped_text(frame, x, y, &mut remaining, self.speed_label, style);
    }

    fn render_count(
        &self,
        frame: &mut ratatui::Frame,
        x: &mut u16,
        y: u16,
        remaining: &mut usize,
        style: Style,
    ) {
        let mut completed = [0_u8; 20];
        let mut present = [0_u8; 20];
        render_clipped_text(
            frame,
            x,
            y,
            remaining,
            decimal_str(self.completed as u64, &mut completed),
            style,
        );
        render_clipped_text(frame, x, y, remaining, "/", style);
        render_clipped_text(
            frame,
            x,
            y,
            remaining,
            decimal_str(self.present as u64, &mut present),
            style,
        );
    }
}

enum PackageName<'a> {
    Borrowed(&'a str),
    Prefixed {
        prefix: &'static str,
        value: &'a str,
    },
}

impl<'a> PackageName<'a> {
    fn new(display_name: &'a str, folder_label: Option<&'a str>) -> Self {
        if !display_name.starts_with("http://") && !display_name.starts_with("https://") {
            return Self::Borrowed(compact_label_ref(display_name));
        }
        if let Some(label) = folder_label {
            return Self::Borrowed(label);
        }
        if let Some(name) = mega_url_name(display_name) {
            return name;
        }
        Self::Borrowed(compact_label_ref(
            display_name.split('#').next().unwrap_or(display_name),
        ))
    }

    fn width(&self) -> usize {
        match self {
            Self::Borrowed(value) => text_width(value),
            Self::Prefixed { prefix, value } => text_width(prefix) + text_width(value),
        }
    }

    fn render(
        &self,
        frame: &mut ratatui::Frame,
        x: &mut u16,
        y: u16,
        max_width: usize,
        style: Style,
    ) {
        match self {
            Self::Borrowed(value) => render_truncated_text(frame, x, y, value, max_width, style),
            Self::Prefixed { prefix, value } => {
                let mut remaining = max_width;
                render_clipped_text(frame, x, y, &mut remaining, prefix, style);
                render_clipped_text(frame, x, y, &mut remaining, value, style);
            }
        }
    }
}

fn compact_label_ref(value: &str) -> &str {
    value
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(value)
}

fn mega_url_name(value: &str) -> Option<PackageName<'_>> {
    let marker = "mega.nz/";
    let start = value.find(marker)? + marker.len();
    let path = &value[start..];
    let mut parts = path.split(['/', '#']);
    match (parts.next(), parts.next()) {
        (Some("folder"), Some(id)) if !id.is_empty() => Some(PackageName::Prefixed {
            prefix: "Folder ",
            value: id,
        }),
        (Some("file"), Some(id)) if !id.is_empty() => Some(PackageName::Prefixed {
            prefix: "File ",
            value: id,
        }),
        _ => None,
    }
}

fn byte_label_width(bytes: u64) -> usize {
    let label = ByteLabel::new(bytes);
    label.width()
}

struct ByteLabel {
    whole: u64,
    frac: u64,
    unit: &'static str,
    fractional: bool,
}

impl ByteLabel {
    fn new(bytes: u64) -> Self {
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;
        if bytes >= GB {
            return Self::scaled(bytes, GB, "GB");
        }
        if bytes >= MB {
            return Self::scaled(bytes, MB, "MB");
        }
        if bytes >= KB {
            return Self::scaled(bytes, KB, "KB");
        }
        Self {
            whole: bytes,
            frac: 0,
            unit: "B",
            fractional: false,
        }
    }

    fn scaled(bytes: u64, unit_bytes: u64, unit: &'static str) -> Self {
        let scaled =
            ((u128::from(bytes) * 100) + (u128::from(unit_bytes) / 2)) / u128::from(unit_bytes);
        Self {
            whole: (scaled / 100) as u64,
            frac: (scaled % 100) as u64,
            unit,
            fractional: true,
        }
    }

    fn width(&self) -> usize {
        decimal_len(self.whole) + if self.fractional { 3 } else { 0 } + 1 + self.unit.len()
    }

    fn push_to(&self, out: &mut String) {
        push_decimal(out, self.whole);
        if self.fractional {
            out.push('.');
            out.push(char::from(b'0' + (self.frac / 10) as u8));
            out.push(char::from(b'0' + (self.frac % 10) as u8));
        }
        out.push(' ');
        out.push_str(self.unit);
    }
}

fn push_decimal(out: &mut String, value: u64) {
    let mut buf = [0_u8; 20];
    out.push_str(decimal_str(value, &mut buf));
}

fn push_byte_label(out: &mut String, bytes: u64) {
    ByteLabel::new(bytes).push_to(out);
}

fn render_byte_label(
    frame: &mut ratatui::Frame,
    x: &mut u16,
    y: u16,
    remaining: &mut usize,
    bytes: u64,
    style: Style,
) {
    let label = ByteLabel::new(bytes);
    let mut whole = [0_u8; 20];
    render_clipped_text(
        frame,
        x,
        y,
        remaining,
        decimal_str(label.whole, &mut whole),
        style,
    );
    if label.fractional {
        render_clipped_text(frame, x, y, remaining, ".", style);
        let tens = (label.frac / 10) as u8;
        let ones = (label.frac % 10) as u8;
        let frac = [b'0' + tens, b'0' + ones];
        let frac = std::str::from_utf8(&frac).expect("decimal digits are valid UTF-8");
        render_clipped_text(frame, x, y, remaining, frac, style);
    }
    render_clipped_text(frame, x, y, remaining, " ", style);
    render_clipped_text(frame, x, y, remaining, label.unit, style);
}

fn render_padded_percent(
    frame: &mut ratatui::Frame,
    x: &mut u16,
    y: u16,
    remaining: &mut usize,
    percent: u64,
    style: Style,
) {
    let percent_width = decimal_len(percent);
    for _ in percent_width..3 {
        render_clipped_text(frame, x, y, remaining, " ", style);
    }
    let mut pct = [0_u8; 20];
    render_clipped_text(
        frame,
        x,
        y,
        remaining,
        decimal_str(percent, &mut pct),
        style,
    );
}

fn aggregate_progress_label_app(app: &App, pct: u16, width: u16) -> String {
    let mut bytes = String::with_capacity(
        byte_label_width(app.total_downloaded) + byte_label_width(app.total_size) + 3,
    );
    push_byte_label(&mut bytes, app.total_downloaded);
    bytes.push_str(" / ");
    push_byte_label(&mut bytes, app.total_size);
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
    let mut speed = String::with_capacity(byte_label_width(app.current_speed) + 2);
    push_byte_label(&mut speed, app.current_speed);
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
            matches!(file.status, FileStatus::Downloading) || app.is_verification_active(&file.id)
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc;

    use super::*;
    use crate::core::{CoreEvent, ResolvedFile, ResolvedPackage};
    use crate::test_support::package_id;
    use crate::tui::app::{App, ConfirmAction, FileEntry, FileStatus};
    use crate::tui::dashboard::{
        DashboardFileRow, DashboardFileStatus, DashboardMetrics, DashboardRow, DashboardTotals,
        DashboardUiMode, DownloadDashboardState,
    };
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

    fn render_dashboard_text_with_size(
        state: DownloadDashboardState,
        width: u16,
        height: u16,
        selected: Option<usize>,
    ) -> String {
        let buffer = render_dashboard_buffer(state, width, height, selected);
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

    fn render_dashboard_buffer(
        state: DownloadDashboardState,
        width: u16,
        height: u16,
        selected: Option<usize>,
    ) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        let mut list_state = ratatui::widgets::ListState::default();
        list_state.select(selected);
        terminal
            .draw(|frame| {
                draw_dashboard(
                    frame,
                    &state,
                    &DashboardChrome::read_only(),
                    &mut list_state,
                )
            })
            .expect("draw should succeed");
        terminal.backend().buffer().clone()
    }

    fn buffer_contains_text_with_color(
        buffer: &ratatui::buffer::Buffer,
        needle: &str,
        color: Color,
    ) -> bool {
        let needle = needle.chars().collect::<Vec<_>>();
        let area = buffer.area;
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                let mut matched = true;
                for (offset, expected) in needle.iter().enumerate() {
                    let Some(cell) = buffer.cell((x + offset as u16, y)) else {
                        matched = false;
                        break;
                    };
                    if cell.symbol().chars().next() != Some(*expected) || cell.fg != color {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    return true;
                }
            }
        }
        false
    }

    fn buffer_contains_text_with_style(
        buffer: &ratatui::buffer::Buffer,
        needle: &str,
        fg: Color,
        bg: Color,
    ) -> bool {
        let needle = needle.chars().collect::<Vec<_>>();
        let area = buffer.area;
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                let mut matched = true;
                for (offset, expected) in needle.iter().enumerate() {
                    let Some(cell) = buffer.cell((x + offset as u16, y)) else {
                        matched = false;
                        break;
                    };
                    if cell.symbol().chars().next() != Some(*expected)
                        || cell.fg != fg
                        || cell.bg != bg
                    {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    return true;
                }
            }
        }
        false
    }

    fn render_single_line(
        width: u16,
        draw_fn: impl FnOnce(&mut ratatui::Frame, &mut u16),
    ) -> String {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| {
                let mut x = 0;
                draw_fn(frame, &mut x);
            })
            .expect("draw should succeed");
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut output = String::new();
        for x in area.x..area.x + area.width {
            let cell = buffer.cell((x, area.y)).expect("cell should exist");
            output.push_str(cell.symbol());
        }
        output
    }

    fn render_byte_label_text(bytes: u64, width: usize) -> String {
        render_single_line(width as u16, |frame, x| {
            let mut remaining = width;
            render_byte_label(frame, x, 0, &mut remaining, bytes, Style::default());
        })
    }

    fn render_package_detail_text(detail: &PackageDetail<'_>, width: usize) -> String {
        render_single_line(width as u16, |frame, x| {
            detail.render(frame, x, 0, width, Style::default());
        })
    }

    fn render_file_detail_text(detail: &FileDetail<'_>, width: usize) -> String {
        render_single_line(width as u16, |frame, x| {
            detail.render(frame, x, 0, width, Style::default());
        })
    }

    fn render_package_name_text(name: &PackageName<'_>, width: usize) -> String {
        render_single_line(width as u16, |frame, x| {
            name.render(frame, x, 0, width, Style::default());
        })
    }

    #[test]
    fn byte_label_renders_without_allocated_format_bytes_path() {
        let cases = [
            (0, "0 B"),
            (500, "500 B"),
            (1024, "1.00 KB"),
            (1536, "1.50 KB"),
            (1_048_576, "1.00 MB"),
            (1_073_741_824, "1.00 GB"),
        ];

        for (bytes, expected) in cases {
            let rendered = render_byte_label_text(bytes, expected.len());
            assert_eq!(rendered, expected);
            assert_eq!(byte_label_width(bytes), expected.len());
        }
    }

    #[test]
    fn byte_label_rounds_scaled_units_like_format_bytes() {
        let rendered = render_byte_label_text(1537, "1.50 KB".len());

        assert_eq!(rendered, "1.50 KB");
    }

    #[test]
    fn truncated_text_handles_ascii_and_tiny_widths() {
        let wide = render_single_line(5, |frame, x| {
            render_truncated_text(frame, x, 0, "download.bin", 5, Style::default());
        });
        let tiny = render_single_line(1, |frame, x| {
            render_truncated_text(frame, x, 0, "download.bin", 1, Style::default());
        });

        assert_eq!(wide, "down\u{2026}");
        assert_eq!(tiny, "\u{2026}");
    }

    #[test]
    fn file_detail_renders_active_verify_speed_and_complete_states() {
        let active = FileDetail::Active {
            downloaded: 40,
            size: 100,
            pct: 40,
            suffix: ActiveDetailSuffix::Active,
        };
        let verifying = FileDetail::Active {
            downloaded: 40,
            size: 100,
            pct: 40,
            suffix: ActiveDetailSuffix::Verify,
        };
        let speed = FileDetail::Active {
            downloaded: 40,
            size: 100,
            pct: 40,
            suffix: ActiveDetailSuffix::Speed {
                bytes_per_sec: 1536,
            },
        };
        let complete = FileDetail::Complete { size: 1536 };

        assert_eq!(
            render_file_detail_text(&active, active.width()),
            "[████░░░░░░] 40%  active"
        );
        assert_eq!(
            render_file_detail_text(&verifying, verifying.width()),
            "[████░░░░░░] 40%  verify"
        );
        assert_eq!(
            render_file_detail_text(&speed, speed.width()),
            "[████░░░░░░] 40%  1.50 KB/s"
        );
        assert_eq!(
            render_file_detail_text(&complete, complete.width()),
            "1.50 KB  done"
        );
    }

    #[test]
    fn file_detail_clips_active_detail_to_available_width() {
        let active = FileDetail::Active {
            downloaded: 40,
            size: 100,
            pct: 40,
            suffix: ActiveDetailSuffix::Active,
        };

        let rendered = render_file_detail_text(&active, 12);

        assert_eq!(rendered, "[████░░░░░░\u{2026}");
    }

    #[test]
    fn package_detail_renders_full_and_compact_forms() {
        let detail = PackageDetail {
            completed: 1,
            present: 2,
            downloaded: 1536,
            total_bytes: 2048,
            percent: 75,
            speed_label: "active",
        };

        assert_eq!(
            render_package_detail_text(&detail, detail.full_width()),
            "1/2 files  1.50 KB / 2.00 KB   75%  active"
        );
        assert_eq!(
            render_package_detail_text(&detail, detail.compact_width()),
            "1/2  2.00 KB   75%  active"
        );
    }

    #[test]
    fn package_detail_clips_compact_form_when_width_is_tiny() {
        let detail = PackageDetail {
            completed: 12,
            present: 345,
            downloaded: 1536,
            total_bytes: 2048,
            percent: 99,
            speed_label: "verify",
        };

        let rendered = render_package_detail_text(&detail, 10);

        assert_eq!(rendered, "12/345  2.");
    }

    #[test]
    fn package_name_borrows_folder_labels_and_mega_url_ids() {
        let folder_label =
            PackageName::new("https://mega.nz/folder/abc#secret", Some("Folder Name"));
        let folder_url = PackageName::new("https://mega.nz/folder/abc#secret", None);
        let file_url = PackageName::new("https://mega.nz/file/def#secret", None);
        let plain_path = PackageName::new("/downloads/Series Name", None);

        assert_eq!(
            render_package_name_text(&folder_label, folder_label.width()),
            "Folder Name"
        );
        assert_eq!(
            render_package_name_text(&folder_url, folder_url.width()),
            "Folder abc"
        );
        assert_eq!(
            render_package_name_text(&file_url, file_url.width()),
            "File def"
        );
        assert_eq!(
            render_package_name_text(&plain_path, plain_path.width()),
            "Series Name"
        );
    }

    #[test]
    fn package_name_truncates_prefixed_mega_labels_without_allocating_name_string() {
        let name = PackageName::new("https://mega.nz/folder/abcdef#secret", None);

        let rendered = render_package_name_text(&name, 8);

        assert_eq!(rendered, "Folder \u{2026}");
    }

    #[test]
    fn row_slots_allocate_name_detail_and_filler_widths() {
        let slots = RowSlots::new(40, 5, 12, 80);

        assert_eq!(slots.detail_width, 12);
        assert_eq!(slots.name_width, 22);
        assert_eq!(slots.filler_width, 1);
    }

    #[test]
    fn row_slots_saturate_when_content_is_too_narrow() {
        let slots = RowSlots::new(8, 5, 12, 80);

        assert_eq!(slots.detail_width, 12);
        assert_eq!(slots.name_width, 0);
        assert_eq!(slots.filler_width, 0);
    }

    #[test]
    fn package_row_stats_collects_totals_activity_and_common_folder() {
        let mut app = test_app();
        let package_id = package_id("pkg-1", "https://mega.nz/folder/pkg");
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id,
                source_url: "https://mega.nz/folder/pkg".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string()),
                display_name: "Mega Package".to_string(),
                files: vec![
                    ResolvedFile {
                        file_id: "one.bin".to_string().into(),
                        path: "Folder/one.bin".to_string(),
                        size: 100,
                    },
                    ResolvedFile {
                        file_id: "two.bin".to_string().into(),
                        path: "Folder/two.bin".to_string(),
                        size: 200,
                    },
                ],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: "one.bin".to_string().into(),
            size: 100,
        });
        app.apply_core_event(CoreEvent::FileProgress {
            file_id: "one.bin".to_string().into(),
            total_bytes_delta: 40,
            network_bytes_delta: 40,
        });
        let verifying_id: crate::core::FileId = "two.bin".to_string().into();
        app.verifying_files.insert(verifying_id.clone());
        app.verification_inflight_files.insert(verifying_id.clone());
        app.verification_targets
            .insert(verifying_id, crate::tui::app::VerificationTarget::Resume);

        let stats = PackageRowStats::new(&app, package_id);

        assert_eq!(stats.present, 2);
        assert_eq!(stats.complete, 0);
        assert_eq!(stats.downloaded, 40);
        assert_eq!(stats.size, 300);
        assert_eq!(stats.folder_label(), Some("Folder"));
        assert!(stats.active());
        assert_eq!(stats.activity_label(PackageStatus::Queued), "verify");
    }

    #[test]
    fn package_row_stats_suppresses_conflicting_folder_label() {
        let mut app = test_app();
        let package_id = package_id("pkg-1", "https://mega.nz/folder/pkg");
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id,
                source_url: "https://mega.nz/folder/pkg".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string()),
                display_name: "Mega Package".to_string(),
                files: vec![
                    ResolvedFile {
                        file_id: "one.bin".to_string().into(),
                        path: "One/one.bin".to_string(),
                        size: 100,
                    },
                    ResolvedFile {
                        file_id: "two.bin".to_string().into(),
                        path: "Two/two.bin".to_string(),
                        size: 200,
                    },
                ],
                collision: None,
            },
        });

        let stats = PackageRowStats::new(&app, package_id);

        assert_eq!(stats.folder_label(), None);
        assert_eq!(stats.activity_label(PackageStatus::Downloading), "active");
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
        app.verifying_files.insert(file_id.clone());
        app.verification_inflight_files.insert(file_id.clone());
        app.verification_targets
            .insert(file_id, crate::tui::app::VerificationTarget::Resume);

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
    fn draw_main_does_not_show_verify_without_explicit_target() {
        let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();
        let mut app = App::new(9723, tx, true);
        let file_id: crate::core::FileId = "queued.bin".to_string().into();
        app.files.push(FileEntry {
            id: file_id.clone(),
            name: "queued.bin".to_string(),
            size: 100,
            downloaded: 0,
            status: FileStatus::Queued,
        });
        app.verifying_files.insert(file_id.clone());
        app.verification_inflight_files.insert(file_id);

        let buffer = render_buffer(&mut app, 100, 24);

        assert!(
            !buffer_contains_text_with_color(&buffer, "queued.bin", Color::Blue),
            "inflight bookkeeping without a target should not render as active verification"
        );
    }

    #[test]
    fn draw_main_colors_package_yellow_while_downloading() {
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

        let buffer = render_buffer(&mut app, 100, 24);

        assert!(
            buffer_contains_text_with_color(&buffer, "Mega Package", Color::Yellow),
            "downloading package row should render in yellow"
        );
    }

    #[test]
    fn draw_main_colors_package_yellow_while_verifying() {
        let mut app = test_app();
        let file_id: crate::core::FileId = "verify.bin".to_string().into();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg-1", "https://mega.nz/folder/pkg"),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string().clone()),
                display_name: "Mega Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.clone(),
                    path: "verify.bin".to_string(),
                    size: 20,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileCompleted {
            file_id: file_id.clone(),
        });
        app.verifying_files.insert(file_id.clone());
        app.verification_inflight_files.insert(file_id.clone());
        app.verification_targets
            .insert(file_id, crate::tui::app::VerificationTarget::Completed);

        let buffer = render_buffer(&mut app, 100, 24);

        assert!(
            buffer_contains_text_with_color(&buffer, "Mega Package", Color::Yellow),
            "verifying package row should render in yellow"
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

    fn sample_dashboard_state() -> DownloadDashboardState {
        DownloadDashboardState {
            authenticated: true,
            paused: false,
            logging_in: false,
            login_error: None,
            popup: Popup::None,
            ui_mode: DashboardUiMode::Attached,
            read_only: true,
            status: "ready".to_string(),
            packages: Vec::new(),
            files: Vec::new(),
            rows: Vec::new(),
            totals: DashboardTotals {
                total_downloaded: 0,
                total_size: 0,
                files_completed: 0,
                files_total: 0,
                current_speed: 0,
                run_total_bytes: 0,
                run_completed_bytes: 0,
                run_file_total: 0,
                run_file_completed: 0,
            },
            metrics: DashboardMetrics {
                cpu_usage: 0.0,
                memory_rss: 0,
                api_port: 9723,
            },
            config: crate::DownloadConfig::default(),
        }
    }

    #[test]
    fn draw_dashboard_snapshot_renders_orphan_file_package_prefix() {
        let mut state = sample_dashboard_state();
        state.files.push(DashboardFileRow {
            id: "file-1".to_string(),
            package_id: "pkg-1".to_string(),
            name: "movie.mkv".to_string(),
            size: 100,
            downloaded: 0,
            speed: 0,
            status: DashboardFileStatus::Queued,
            package_label: Some("pkg".to_string()),
        });
        state.rows.push(DashboardRow::File {
            package_id: String::new(),
            file_id: "file-1".to_string(),
        });

        let rendered = render_dashboard_text_with_size(state, 80, 12, None);

        assert!(rendered.contains("[pkg] movie.mkv"));
    }

    #[test]
    fn draw_dashboard_snapshot_keeps_selected_missing_row_highlight() {
        let mut state = sample_dashboard_state();
        state.rows.push(DashboardRow::Package {
            package_id: "missing".to_string(),
        });

        let buffer = render_dashboard_buffer(state, 80, 12, Some(0));

        assert!(buffer_contains_text_with_style(
            &buffer,
            " ",
            Color::Reset,
            Color::DarkGray,
        ));
    }

    #[test]
    fn draw_dashboard_snapshot_highlights_selected_verifying_detail() {
        let mut state = sample_dashboard_state();
        state.files.push(DashboardFileRow {
            id: "file-1".to_string(),
            package_id: "pkg-1".to_string(),
            name: "verify.bin".to_string(),
            size: 100,
            downloaded: 40,
            speed: 0,
            status: DashboardFileStatus::Verifying,
            package_label: None,
        });
        state.rows.push(DashboardRow::File {
            package_id: "pkg-1".to_string(),
            file_id: "file-1".to_string(),
        });

        let buffer = render_dashboard_buffer(state, 80, 12, Some(0));

        assert!(buffer_contains_text_with_style(
            &buffer,
            "verify",
            Color::Blue,
            Color::DarkGray,
        ));
    }
}
