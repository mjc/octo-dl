use super::{App, FileEntry, FileStatus, OverlayFile, VisibleFileContext};

impl App {
    pub(crate) fn counted_overlay_files(
        &self,
    ) -> impl Iterator<Item = (&str, &OverlayFile)> + '_ {
        self.overlay_files.iter().filter_map(|(id, overlay)| {
            (!self.core_state.files.contains_key(id) && overlay.counts_toward_progress)
                .then_some((id.as_str(), overlay))
        })
    }

    fn seed_overlay_from_visible(&mut self) {
        super::super::visible::seed_overlay_from_visible(
            &self.files,
            &self.core_state,
            &self.deleted_files,
            &mut self.overlay_files,
        );
    }

    pub(crate) fn package_label_for_file(&self, file_id: &str) -> Option<String> {
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
            file.source_url
                .as_deref()
                .filter(|label| !label.starts_with("http://") && !label.starts_with("https://"))
                .map_or_else(|| folder_label_from_path(&file.file.name), str::to_string)
        })
    }

    pub(crate) fn upsert_overlay_file(
        &mut self,
        file: FileEntry,
        source_url: Option<String>,
        counts_toward_progress: bool,
    ) {
        self.overlay_files.insert(
            file.id.clone(),
            OverlayFile {
                file,
                source_url,
                counts_toward_progress,
            },
        );
        self.sync_visible_files();
    }

    pub(crate) fn overlay_file_mut(&mut self, id: &str) -> Option<&mut FileEntry> {
        if !self.overlay_files.contains_key(id) {
            self.seed_overlay_from_visible();
        }
        self.overlay_files.get_mut(id).map(|file| &mut file.file)
    }

    pub(crate) fn remove_overlay_file(&mut self, id: &str) -> Option<FileEntry> {
        if !self.overlay_files.contains_key(id) {
            self.seed_overlay_from_visible();
        }
        let removed = self.overlay_files.shift_remove(id).map(|file| file.file);
        self.sync_visible_files();
        removed
    }

    pub(crate) fn drop_overlay_file(&mut self, id: &str) -> Option<FileEntry> {
        self.deleted_files.insert(id.to_string());
        let removed = self.overlay_files.shift_remove(id).map(|file| file.file);
        self.sync_visible_files();
        self.deleted_files.remove(id);
        removed
    }

    pub(crate) fn visible_file_context(&self, id: &str) -> Option<VisibleFileContext> {
        self.files.iter().find(|file| file.id == id).map(|file| {
            let source_url = self
                .core_state
                .files
                .get(id)
                .and_then(|core_file| core_file.source_url.clone())
                .or_else(|| {
                    self.overlay_files
                        .get(id)
                        .and_then(|overlay| overlay.source_url.clone())
                });
            let counts_toward_progress = self
                .core_state
                .files
                .get(id)
                .map(|file| file.runtime.counts_in_run_totals && !file.runtime.preexisting_complete)
                .or_else(|| {
                    self.overlay_files
                        .get(id)
                        .map(|overlay| overlay.counts_toward_progress)
                })
                .unwrap_or(true);
            VisibleFileContext {
                id: file.id.clone(),
                status: file.status.clone(),
                artifact_path: file.name.clone(),
                size: file.size,
                counts_toward_progress,
                source_url,
            }
        })
    }

    pub(crate) fn mark_visible_file_complete(&mut self, id: &str, name: &str) {
        self.cancellation_tokens.remove(id);
        if !self.core_state.files.contains_key(id)
            && let Some(file) = self.overlay_file_mut(id)
        {
            file.name = name.to_string();
            file.status = FileStatus::Complete;
            file.downloaded = file.size;
            self.sync_visible_files();
        }
        self.reset_file_ui_rate(id);

        self.recompute_totals();
        self.sync_session_after_file_complete(id);
        self.update_download_status_message();
    }

    pub(crate) fn show_overlay_error(
        &mut self,
        id: &str,
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
                    id: id.to_string(),
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

    pub(crate) fn mark_visible_file_error(&mut self, id: &str, name: &str, error: &str) {
        self.show_overlay_error(id, name, error, true);
        self.note_file_error(id, error);
    }

    pub(crate) fn show_ui_error_only(&mut self, name: &str, error: &str) {
        self.show_overlay_error(name, name, error, false);
    }
}

fn folder_label_from_path(path: &str) -> String {
    path.split('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(path)
        .to_string()
}
