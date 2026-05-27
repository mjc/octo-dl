use std::path::PathBuf;
use std::sync::mpsc;
#[cfg(test)]
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use crate::core::SessionSnapshot;

// Keep session saves responsive while collapsing bursts of same-path writes that
// would otherwise serialize the full TOML snapshot over and over during add-file storms.
#[cfg(not(test))]
const SESSION_SAVE_DEBOUNCE: Duration = Duration::from_millis(100);
#[cfg(test)]
const SESSION_SAVE_DEBOUNCE: Duration = Duration::from_millis(10);

#[cfg(not(test))]
const SESSION_SAVE_MAX_DELAY: Duration = Duration::from_millis(500);
#[cfg(test)]
const SESSION_SAVE_MAX_DELAY: Duration = Duration::from_millis(50);

#[cfg(test)]
type SaveCallCount = Arc<AtomicUsize>;
#[cfg(not(test))]
type SaveCallCount = ();

enum SessionPersistenceRequest {
    Save {
        session: SessionSnapshot,
        path: PathBuf,
    },
    Remove(PathBuf),
    Flush(mpsc::Sender<()>),
}

pub(crate) enum SessionPersistenceError {
    Save {
        id: String,
        error: std::io::Error,
    },
    Remove {
        path: PathBuf,
        error: std::io::Error,
    },
}

pub(crate) struct SessionPersistence {
    request_tx: mpsc::Sender<SessionPersistenceRequest>,
    error_rx: mpsc::Receiver<SessionPersistenceError>,
    #[cfg(test)]
    save_call_count: SaveCallCount,
}

impl SessionPersistence {
    pub(crate) fn new() -> Self {
        let (request_tx, request_rx) = mpsc::channel();
        let (error_tx, error_rx) = mpsc::channel();
        #[cfg(test)]
        let save_call_count = Arc::new(AtomicUsize::new(0));
        #[cfg(test)]
        let worker_save_call_count = save_call_count.clone();

        thread::Builder::new()
            .name("octo-session-persistence".to_string())
            .spawn(move || {
                session_persistence_worker(
                    request_rx,
                    error_tx,
                    #[cfg(test)]
                    worker_save_call_count,
                    #[cfg(not(test))]
                    (),
                )
            })
            .expect("session persistence worker should start");

        Self {
            request_tx,
            error_rx,
            #[cfg(test)]
            save_call_count,
        }
    }

    pub(crate) fn save(&self, session: SessionSnapshot, path: PathBuf) {
        let _ = self
            .request_tx
            .send(SessionPersistenceRequest::Save { session, path });
    }

    pub(crate) fn remove(&self, path: PathBuf) {
        let _ = self
            .request_tx
            .send(SessionPersistenceRequest::Remove(path));
    }

    pub(crate) fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        if self
            .request_tx
            .send(SessionPersistenceRequest::Flush(tx))
            .is_ok()
        {
            let _ = rx.recv();
        }
    }

    pub(crate) fn drain_errors(&self) -> Vec<SessionPersistenceError> {
        let mut errors = Vec::new();
        while let Ok(error) = self.error_rx.try_recv() {
            errors.push(error);
        }
        errors
    }

    #[cfg(test)]
    fn reset_save_call_count(&self) {
        self.save_call_count.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn save_call_count(&self) -> usize {
        self.save_call_count.load(Ordering::Relaxed)
    }
}

impl Default for SessionPersistence {
    fn default() -> Self {
        Self::new()
    }
}

fn session_persistence_worker(
    request_rx: mpsc::Receiver<SessionPersistenceRequest>,
    error_tx: mpsc::Sender<SessionPersistenceError>,
    save_call_count: SaveCallCount,
) {
    let mut next_request = None;
    loop {
        let request = if let Some(request) = next_request.take() {
            request
        } else {
            let Ok(request) = request_rx.recv() else {
                break;
            };
            request
        };
        match request {
            SessionPersistenceRequest::Save { session, path } => {
                next_request = persist_latest_save(
                    session,
                    path,
                    &request_rx,
                    &error_tx,
                    &save_call_count,
                );
            }
            SessionPersistenceRequest::Remove(path) => {
                remove_snapshot(path, &error_tx);
            }
            SessionPersistenceRequest::Flush(done_tx) => {
                let _ = done_tx.send(());
            }
        }
    }
}

fn persist_latest_save(
    mut session: SessionSnapshot,
    path: PathBuf,
    request_rx: &mpsc::Receiver<SessionPersistenceRequest>,
    error_tx: &mpsc::Sender<SessionPersistenceError>,
    save_call_count: &SaveCallCount,
) -> Option<SessionPersistenceRequest> {
    let first_queued_at = Instant::now();
    loop {
        let max_remaining =
            SESSION_SAVE_MAX_DELAY.saturating_sub(first_queued_at.elapsed());
        let wait_for = SESSION_SAVE_DEBOUNCE.min(max_remaining);
        if wait_for.is_zero() {
            save_snapshot(session, path, error_tx, save_call_count);
            return None;
        }

        match request_rx.recv_timeout(wait_for) {
            Ok(SessionPersistenceRequest::Save {
                session: next_session,
                path: next_path,
            }) if next_path == path => {
                session = next_session;
            }
            Ok(SessionPersistenceRequest::Remove(remove_path)) if remove_path == path => {
                return Some(SessionPersistenceRequest::Remove(remove_path));
            }
            Ok(SessionPersistenceRequest::Flush(done_tx)) => {
                save_snapshot(session, path, error_tx, save_call_count);
                let _ = done_tx.send(());
                return None;
            }
            Ok(request) => {
                save_snapshot(session, path, error_tx, save_call_count);
                return Some(request);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                save_snapshot(session, path, error_tx, save_call_count);
                return None;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                save_snapshot(session, path, error_tx, save_call_count);
                return None;
            }
        }
    }
}

fn remove_snapshot(path: PathBuf, error_tx: &mpsc::Sender<SessionPersistenceError>) {
    if let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        let _ = error_tx.send(SessionPersistenceError::Remove { path, error });
    }
}

fn save_snapshot(
    session: SessionSnapshot,
    path: PathBuf,
    error_tx: &mpsc::Sender<SessionPersistenceError>,
    _save_call_count: &SaveCallCount,
) {
    #[cfg(test)]
    _save_call_count.fetch_add(1, Ordering::Relaxed);
    let id = session.id.clone();
    if let Err(error) = session.save_to_path(&path) {
        let _ = error_tx.send(SessionPersistenceError::Save { id, error });
    }
}

#[cfg(test)]
fn wait_for_path(path: &std::path::Path) -> bool {
    let deadline =
        Instant::now() + SESSION_SAVE_MAX_DELAY + SESSION_SAVE_DEBOUNCE + Duration::from_millis(50);
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    path.exists()
}

#[cfg(test)]
fn wait_for_save_calls(persistence: &SessionPersistence, expected: usize) -> bool {
    let deadline =
        Instant::now() + SESSION_SAVE_MAX_DELAY + SESSION_SAVE_DEBOUNCE + Duration::from_millis(50);
    while Instant::now() < deadline {
        if persistence.save_call_count() == expected {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    persistence.save_call_count() == expected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SessionSnapshot;
    use crate::test_support::{FileFixtureStatus, UrlFixtureStatus, push_file, session_snapshot};

    #[test]
    fn save_and_flush_persists_session_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        let persistence = SessionPersistence::new();
        persistence.reset_save_call_count();
        let mut session = session_snapshot(vec![(
            "https://mega.nz/file/root",
            UrlFixtureStatus::Fetched,
        )]);
        push_file(
            &mut session,
            0,
            "episode-1.mkv",
            128,
            FileFixtureStatus::Pending,
        );

        persistence.save(session.clone(), path.clone());
        persistence.flush();

        let loaded = SessionSnapshot::load(&path).unwrap();
        assert_eq!(loaded.id, session.id);
        let loaded_paths = loaded
            .iter_files()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(loaded_paths, vec!["episode-1.mkv".to_string()]);
        assert!(persistence.drain_errors().is_empty());
        assert_eq!(persistence.save_call_count(), 1);
    }

    #[test]
    fn remove_and_flush_deletes_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        let session = session_snapshot(vec![(
            "https://mega.nz/file/root",
            UrlFixtureStatus::Fetched,
        )]);
        session.save_to_path(&path).unwrap();
        let persistence = SessionPersistence::new();

        persistence.remove(path.clone());
        persistence.flush();

        assert!(!path.exists());
        assert!(persistence.drain_errors().is_empty());
    }

    #[test]
    fn save_burst_persists_latest_snapshot_before_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        let persistence = SessionPersistence::new();
        persistence.reset_save_call_count();
        let mut first = session_snapshot(vec![(
            "https://mega.nz/file/root",
            UrlFixtureStatus::Fetched,
        )]);
        push_file(
            &mut first,
            0,
            "episode-1.mkv",
            128,
            FileFixtureStatus::Pending,
        );
        let mut latest = first.clone();
        let file = latest.find_file_mut("episode-1.mkv").unwrap();
        file.path = "episode-2.mkv".to_string();
        file.id = "episode-2.mkv".into();

        persistence.save(first, path.clone());
        persistence.save(latest, path.clone());
        persistence.flush();

        let loaded = SessionSnapshot::load(&path).unwrap();
        assert_eq!(
            loaded
                .iter_files()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["episode-2.mkv"]
        );
        assert!(persistence.drain_errors().is_empty());
        assert_eq!(persistence.save_call_count(), 1);
    }

    #[test]
    fn remove_cancels_debounced_save_for_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        let persistence = SessionPersistence::new();
        persistence.reset_save_call_count();
        let session = session_snapshot(vec![(
            "https://mega.nz/file/root",
            UrlFixtureStatus::Fetched,
        )]);

        persistence.save(session, path.clone());
        persistence.remove(path.clone());
        persistence.flush();

        assert!(!path.exists());
        assert_eq!(persistence.save_call_count(), 0);
        assert!(persistence.drain_errors().is_empty());
    }

    #[test]
    fn remove_other_path_does_not_cancel_debounced_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        let other_path = dir.path().join("other-session.toml");
        let persistence = SessionPersistence::new();
        persistence.reset_save_call_count();
        let session = session_snapshot(vec![(
            "https://mega.nz/file/root",
            UrlFixtureStatus::Fetched,
        )]);

        persistence.save(session.clone(), path.clone());
        persistence.remove(other_path);
        persistence.flush();

        let loaded = SessionSnapshot::load(&path).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(persistence.save_call_count(), 1);
        assert!(persistence.drain_errors().is_empty());
    }

    #[test]
    fn drop_flushes_pending_save_on_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        let session = session_snapshot(vec![(
            "https://mega.nz/file/root",
            UrlFixtureStatus::Fetched,
        )]);

        {
            let persistence = SessionPersistence::new();
            persistence.reset_save_call_count();
            persistence.save(session.clone(), path.clone());
        }

        assert!(wait_for_path(&path));
        let loaded = SessionSnapshot::load(&path).unwrap();
        assert_eq!(loaded.id, session.id);
    }

    #[test]
    fn steady_same_path_save_stream_flushes_within_max_delay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        let persistence = SessionPersistence::new();
        persistence.reset_save_call_count();
        let base = session_snapshot(vec![(
            "https://mega.nz/file/root",
            UrlFixtureStatus::Fetched,
        )]);

        for index in 0..10 {
            let mut session = base.clone();
            session.id = format!("session-{index}");
            persistence.save(session, path.clone());
            thread::sleep(SESSION_SAVE_DEBOUNCE / 2);
        }

        assert!(wait_for_save_calls(&persistence, 1));
        persistence.flush();
        assert_eq!(persistence.save_call_count(), 1);
    }
}
