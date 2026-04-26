use indexmap::IndexMap;
use serde::Serialize;

use crate::DownloadConfig;

use super::{App, FileEntry, FileStatus, Popup};

#[derive(Serialize)]
struct RunTotals {
    run_total_bytes: u64,
    run_completed_bytes: u64,
    run_file_total: usize,
    run_file_completed: usize,
    displayed_network_rate_bps: u64,
}

#[derive(Serialize)]
struct SnapshotFile<'a> {
    id: &'a str,
    name: &'a str,
    size: u64,
    downloaded: u64,
    speed: u64,
    status: &'a FileStatus,
}

#[derive(Serialize)]
struct Snapshot<'a> {
    authenticated: bool,
    paused: bool,
    logging_in: bool,
    login_error: Option<&'a str>,
    popup: Popup,
    packages: Vec<serde_json::Value>,
    files: Vec<SnapshotFile<'a>>,
    total_downloaded: u64,
    total_size: u64,
    files_completed: usize,
    files_total: usize,
    current_speed: u64,
    displayed_network_rate_bps: u64,
    run_totals: RunTotals,
    cpu_usage: f32,
    memory_rss: u64,
    api_port: u16,
    config: &'a DownloadConfig,
}

pub(super) fn snapshot_packages(app: &App) -> Vec<serde_json::Value> {
    if !app.core_state.packages.is_empty() {
        return app
            .core_state
            .packages
            .values()
            .map(|package| {
                serde_json::json!({
                    "id": package.id,
                    "source_url": package.source_url,
                    "display_name": package.display_name,
                    "status": package.status,
                    "file_ids": package.file_ids,
                })
            })
            .collect();
    }

    let mut packages = IndexMap::<String, Vec<&FileEntry>>::new();
    for file in &app.files {
        let package_id = file.source_url.clone().unwrap_or_else(|| file.id.clone());
        packages.entry(package_id).or_default().push(file);
    }

    packages
        .into_iter()
        .map(|(package_id, files)| {
            let status = if files
                .iter()
                .any(|file| matches!(file.status, FileStatus::Error(_)))
            {
                "failed"
            } else if files
                .iter()
                .any(|file| matches!(file.status, FileStatus::Downloading))
            {
                "downloading"
            } else if files.iter().all(|file| matches!(file.status, FileStatus::Complete)) {
                "complete"
            } else if files.iter().any(|file| matches!(file.status, FileStatus::Complete)) {
                "partial"
            } else {
                "queued"
            };
            serde_json::json!({
                "id": package_id,
                "source_url": files[0].source_url.clone().unwrap_or_else(|| files[0].id.clone()),
                "display_name": files[0].source_url.clone().unwrap_or_else(|| files[0].name.clone()),
                "status": status,
                "file_ids": files.iter().map(|file| file.id.clone()).collect::<Vec<_>>(),
            })
        })
        .collect()
}

fn run_totals(app: &App) -> RunTotals {
    if !app.core_state.files.is_empty() {
        RunTotals {
            run_total_bytes: app.core_state.totals.run_total_bytes,
            run_completed_bytes: app.core_state.totals.run_completed_bytes,
            run_file_total: app.core_state.totals.run_file_total,
            run_file_completed: app.core_state.totals.run_file_completed,
            displayed_network_rate_bps: app.current_speed,
        }
    } else {
        RunTotals {
            run_total_bytes: app.total_size,
            run_completed_bytes: app.total_downloaded,
            run_file_total: app.files_total,
            run_file_completed: app.files_completed,
            displayed_network_rate_bps: app.current_speed,
        }
    }
}

pub(super) fn to_json(app: &App) -> String {
    let snapshot = Snapshot {
        authenticated: app.authenticated,
        paused: app.paused,
        logging_in: app.login.logging_in,
        login_error: app.login.error.as_deref(),
        popup: app.popup,
        packages: snapshot_packages(app),
        files: app
            .files
            .iter()
            .map(|file| SnapshotFile {
                id: &file.id,
                name: &file.name,
                size: file.size,
                downloaded: file.downloaded,
                speed: app.file_speed(&file.id),
                status: &file.status,
            })
            .collect(),
        total_downloaded: app.total_downloaded,
        total_size: app.total_size,
        files_completed: app.files_completed,
        files_total: app.files_total,
        current_speed: app.current_speed,
        displayed_network_rate_bps: app.current_speed,
        run_totals: run_totals(app),
        cpu_usage: app.cpu_usage,
        memory_rss: app.memory_rss,
        api_port: app.api_port,
        config: &app.config.config,
    };

    serde_json::to_string(&snapshot).unwrap_or_default()
}
