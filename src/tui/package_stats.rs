use super::app::App;
use crate::core::{FileLifecycle, FileState, PackageId, PackageStatus};

#[derive(Clone, Copy)]
pub(super) struct PackageDisplayStats<'a> {
    pub(super) source_url: &'a str,
    pub(super) present_files: usize,
    pub(super) completed_files: usize,
    pub(super) downloaded_bytes: u64,
    pub(super) total_bytes: u64,
    pub(super) downloading: bool,
    pub(super) verifying: bool,
    pub(super) folder_label: Option<&'a str>,
    pub(super) folder_conflict: bool,
}

impl<'a> PackageDisplayStats<'a> {
    pub(super) const fn empty() -> Self {
        Self {
            source_url: "",
            present_files: 0,
            completed_files: 0,
            downloaded_bytes: 0,
            total_bytes: 0,
            downloading: false,
            verifying: false,
            folder_label: None,
            folder_conflict: false,
        }
    }

    pub(super) fn new(app: &'a App, package_id: PackageId) -> Self {
        Self::from_files(app, app.core_state.package_files(&package_id))
    }

    pub(super) fn from_files(app: &'a App, files: impl IntoIterator<Item = &'a FileState>) -> Self {
        #[cfg(test)]
        super::dashboard::record_package_stats_call();

        let mut stats = Self::empty();
        for file in files {
            stats.record_file(app, file);
        }
        stats
    }

    pub(super) fn record_file(&mut self, app: &'a App, file: &'a FileState) {
        if self.source_url.is_empty() {
            self.source_url = &file.source_url;
        }
        self.downloading |= matches!(file.lifecycle, FileLifecycle::Downloading);
        self.verifying |= app.is_verification_active(&file.id);

        let folder = file.path.split('/').next().filter(|part| !part.is_empty());
        match (self.folder_label, folder) {
            (None, Some(folder)) => self.folder_label = Some(folder),
            (Some(existing), Some(folder)) if existing == folder => {}
            (Some(_), Some(_)) => self.folder_conflict = true,
            _ => {}
        }

        let complete = matches!(file.lifecycle, FileLifecycle::Complete);
        let visible = if complete {
            file.size
        } else {
            crate::core::visible_completed_bytes_for_display(file)
        };
        self.present_files += 1;
        self.completed_files += usize::from(complete);
        self.downloaded_bytes = self.downloaded_bytes.saturating_add(visible);
        self.total_bytes = self.total_bytes.saturating_add(file.size);
    }

    pub(super) const fn active(&self) -> bool {
        self.downloading || self.verifying
    }

    pub(super) const fn activity_label(&self, status: PackageStatus) -> &'static str {
        if self.verifying {
            "verify"
        } else if self.downloading || matches!(status, PackageStatus::Downloading) {
            "active"
        } else {
            ""
        }
    }

    pub(super) fn folder_label(&self) -> Option<&'a str> {
        (!self.folder_conflict)
            .then_some(self.folder_label)
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shared_package_stats_starts_empty() {
        let stats = super::PackageDisplayStats::empty();

        assert_eq!(stats.present_files, 0);
    }
}
