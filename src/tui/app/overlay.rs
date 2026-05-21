use super::{App, FileEntry, FileStatus, TransientRow, VisibleFileContext};
use crate::core::FileId;

impl App {
    pub(crate) fn package_label_for_file(&self, file_id: &FileId) -> Option<String> {
        if let Some(core_file) = self.core_state.files.get(file_id) {
            let configured = self
                .core_state
                .packages
                .get(&core_file.package_id)
                .map(|package| package.display_name.clone());
            if configured.as_deref().is_some_and(|label| {
                !label.starts_with("http://") && !label.starts_with("https://")
            }) {
                return configured;
            }
            return Some(folder_label_from_path(&core_file.path));
        }

        self.overlay_files.get(file_id).map(|file| {
            file.source_url()
                .filter(|label| !label.starts_with("http://") && !label.starts_with("https://"))
                .map_or_else(|| folder_label_from_path(&file.file().name), str::to_string)
        })
    }

    pub(crate) fn upsert_overlay_file(
        &mut self,
        file: FileEntry,
        source_url: Option<String>,
        _counts_toward_progress: bool,
    ) {
        let row = match source_url {
            Some(source_url) => TransientRow::PendingUrl { file, source_url },
            None => TransientRow::UiError { file },
        };
        self.overlay_files.insert(row.file().id.clone(), row);
        self.sync_visible_files();
    }

    pub(crate) fn overlay_file_mut(&mut self, id: &FileId) -> Option<&mut FileEntry> {
        self.overlay_files.get_mut(id).map(TransientRow::file_mut)
    }

    pub(crate) fn visible_file_context(&self, id: &FileId) -> Option<VisibleFileContext> {
        if let Some(core_file) = self.core_state.files.get(id) {
            let status = match &core_file.lifecycle {
                crate::core::FileLifecycle::Planned | crate::core::FileLifecycle::Queued => {
                    FileStatus::Queued
                }
                crate::core::FileLifecycle::Downloading => FileStatus::Downloading,
                crate::core::FileLifecycle::Complete => FileStatus::Complete,
                crate::core::FileLifecycle::Failed { message } => {
                    FileStatus::Error(message.clone())
                }
            };
            return Some(VisibleFileContext {
                id: core_file.id.clone(),
                status,
                source_url: Some(core_file.source_url.clone()),
                artifact_path: core_file.path.clone(),
                size: core_file.size,
                counts_toward_progress: matches!(
                    core_file.accounting,
                    crate::core::FileAccounting::CurrentRun
                ),
            });
        }

        if let Some(overlay) = self.overlay_files.get(id) {
            return Some(VisibleFileContext {
                id: overlay.file().id.clone(),
                status: overlay.file().status.clone(),
                source_url: None,
                artifact_path: overlay.file().name.clone(),
                size: overlay.file().size,
                counts_toward_progress: false,
            });
        }

        None
    }

    pub(crate) fn show_overlay_error(
        &mut self,
        id: &FileId,
        name: &str,
        error: &str,
        counts_toward_progress: bool,
    ) {
        self.cancellation_tokens.remove(id);
        if self.core_state.files.contains_key(id) {
            // Core-backed rows are projected back into the TUI view.
        } else if let Some(file) = self.overlay_file_mut(id) {
            file.status = FileStatus::Error(error.to_string());
            file.name = name.to_string();
            self.sync_visible_files();
        } else {
            self.upsert_overlay_file(
                FileEntry {
                    id: id.clone(),
                    name: name.to_string(),
                    size: 0,
                    downloaded: 0,
                    status: FileStatus::Error(error.to_string()),
                },
                None,
                counts_toward_progress,
            );
        }
        self.reset_file_ui_rate(id);
    }

    pub(crate) fn show_ui_error_only(&mut self, name: &str, error: &str) {
        self.show_overlay_error(&FileId::from(name), name, error, false);
    }
}

fn folder_label_from_path(path: &str) -> String {
    path.split('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(path)
        .to_string()
}
