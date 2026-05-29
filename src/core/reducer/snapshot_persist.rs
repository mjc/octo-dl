#[cfg(test)]
use std::cell::Cell;

use super::CoreEvent;
use crate::core::model::DownloadState;
use crate::core::session::{FileSnapshot, PackageSnapshot, SessionSnapshot, SessionUrlSnapshot};

#[cfg(test)]
use crate::core::model::{
    FileAccounting, FileId, FileLifecycle, FileProgressState, FileState, PackageId, PackageKey,
    PackageProgressState, PackageState,
};

#[cfg(test)]
thread_local! {
    static SNAPSHOT_FROM_STATE_CALLS: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn should_persist_session(event: &CoreEvent) -> bool {
    !matches!(
        event,
        CoreEvent::FileProgress { .. }
            | CoreEvent::FileQueued { .. }
            | CoreEvent::FileStarted { .. }
            | CoreEvent::FileResumeStarted { .. }
            | CoreEvent::FileReuseDetected { .. }
            | CoreEvent::FileResumeReverified { .. }
            | CoreEvent::FileVerificationStarted { .. }
            | CoreEvent::FileVerificationProgress { .. }
            | CoreEvent::FileVerificationCompleted { .. }
            | CoreEvent::Tick { .. }
    )
}

#[cfg(test)]
pub(crate) fn reset_snapshot_from_state_call_count() {
    SNAPSHOT_FROM_STATE_CALLS.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn snapshot_from_state_call_count() -> usize {
    SNAPSHOT_FROM_STATE_CALLS.with(Cell::get)
}

pub fn snapshot_from_state(state: &DownloadState) -> SessionSnapshot {
    #[cfg(test)]
    SNAPSHOT_FROM_STATE_CALLS.with(|count| count.set(count.get().saturating_add(1)));
    let mut url_errors = state.url_errors.clone();
    let mut remaining_files = state.files.values().peekable();
    let mut packages = Vec::with_capacity(state.packages.len());
    for package in state.packages.values() {
        let mut files = Vec::with_capacity(package.progress.file_count());
        while remaining_files
            .peek()
            .is_some_and(|file| file.package_id == package.id)
        {
            let file = remaining_files
                .next()
                .expect("peeked file should remain available");
            if let Some(error) = package.error.as_ref()
                && !url_errors.contains_key(&file.source_url)
            {
                url_errors.insert(file.source_url.clone(), error.clone());
            }
            files.push(FileSnapshot {
                id: file.id.clone(),
                package_id: file.package_id,
                source_url: file.source_url.clone(),
                path: file.path.clone(),
                size: file.size,
                lifecycle: file.lifecycle.clone(),
                progress: file.progress.clone(),
                accounting: file.accounting,
            });
        }
        if files.is_empty() {
            continue;
        }
        packages.push(PackageSnapshot {
            id: package.id,
            key: package.key.clone(),
            display_name: package.display_name.clone(),
            files,
            error: package.error.clone(),
        });
    }
    debug_assert!(
        remaining_files.next().is_none(),
        "snapshot_from_state expects files grouped in package order"
    );
    SessionSnapshot {
        version: 6,
        id: state.session_meta.session_id.clone(),
        created: state.session_meta.created,
        status: state.session_meta.status,
        urls: state
            .url_order
            .iter()
            .map(|url| SessionUrlSnapshot {
                url: url.clone(),
                error: url_errors.get(url).cloned(),
            })
            .collect(),
        packages,
        config: state.session_meta.config.clone(),
        credentials: state.session_meta.credentials.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package_id(raw: &str, source_url: &str) -> PackageId {
        PackageId::parse_or_key(raw, &PackageKey::new(source_url))
    }

    #[test]
    fn snapshot_from_state_preserves_package_file_order() {
        let pkg_id = package_id("pkg", "pkg");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.packages.insert(
            pkg_id,
            PackageState {
                id: pkg_id,
                key: PackageKey::new("pkg"),
                display_name: "pkg".to_string(),
                progress: PackageProgressState {
                    queued: 2,
                    ..PackageProgressState::default()
                },
                error: None,
            },
        );
        state.files.insert(
            "b.bin".into(),
            FileState {
                id: "b.bin".into(),
                package_id: pkg_id,
                source_url: "pkg".to_string(),
                path: "b.bin".to_string(),
                size: 20,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );
        state.files.insert(
            "a.bin".into(),
            FileState {
                id: "a.bin".into(),
                package_id: pkg_id,
                source_url: "pkg".to_string(),
                path: "a.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );

        let snapshot = snapshot_from_state(&state);
        let files = &snapshot.packages[0].files;
        assert_eq!(
            files
                .iter()
                .map(|file| file.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b.bin", "a.bin"]
        );
    }

    #[test]
    fn snapshot_from_state_projects_package_errors_onto_url_rows() {
        let pkg_id = package_id("pkg", "https://mega.nz/folder/pkg");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.url_order = vec!["https://mega.nz/folder/pkg".to_string()];
        state.packages.insert(
            pkg_id,
            PackageState {
                id: pkg_id,
                key: PackageKey::new("https://mega.nz/folder/pkg"),
                display_name: "pkg".to_string(),
                progress: PackageProgressState {
                    failed: 1,
                    ..PackageProgressState::default()
                },
                error: Some("boom".to_string()),
            },
        );
        state.files.insert(
            "file.bin".into(),
            FileState {
                id: "file.bin".into(),
                package_id: pkg_id,
                source_url: "https://mega.nz/folder/pkg".to_string(),
                path: "file.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Failed {
                    message: "boom".to_string(),
                },
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );

        let snapshot = snapshot_from_state(&state);

        assert_eq!(snapshot.urls.len(), 1);
        assert_eq!(snapshot.urls[0].url, "https://mega.nz/folder/pkg");
        assert_eq!(snapshot.urls[0].error.as_deref(), Some("boom"));
    }

    #[test]
    fn snapshot_from_state_handles_distinct_equal_file_ids() {
        let pkg_id = package_id("pkg", "pkg");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        let state_file_id = FileId::from(String::from("file.bin"));
        state.packages.insert(
            pkg_id,
            PackageState {
                id: pkg_id,
                key: PackageKey::new("pkg"),
                display_name: "pkg".to_string(),
                progress: PackageProgressState {
                    queued: 1,
                    ..PackageProgressState::default()
                },
                error: None,
            },
        );
        state.files.insert(
            state_file_id.clone(),
            FileState {
                id: state_file_id,
                package_id: pkg_id,
                source_url: "pkg".to_string(),
                path: "file.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );

        let snapshot = snapshot_from_state(&state);
        assert_eq!(snapshot.packages.len(), 1);
        assert_eq!(snapshot.packages[0].files.len(), 1);
        assert_eq!(snapshot.packages[0].files[0].id, "file.bin");
    }

    #[test]
    fn snapshot_from_state_call_count_tracks_invocations() {
        reset_snapshot_from_state_call_count();
        assert_eq!(snapshot_from_state_call_count(), 0);

        let state = DownloadState::new(crate::core::SessionMeta::default());
        let _ = snapshot_from_state(&state);

        assert_eq!(snapshot_from_state_call_count(), 1);
    }
}
