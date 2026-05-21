use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use crate::core::SessionSnapshot;

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
}

impl SessionPersistence {
    pub(crate) fn new() -> Self {
        let (request_tx, request_rx) = mpsc::channel();
        let (error_tx, error_rx) = mpsc::channel();

        thread::Builder::new()
            .name("octo-session-persistence".to_string())
            .spawn(move || session_persistence_worker(request_rx, error_tx))
            .expect("session persistence worker should start");

        Self {
            request_tx,
            error_rx,
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
}

impl Default for SessionPersistence {
    fn default() -> Self {
        Self::new()
    }
}

fn session_persistence_worker(
    request_rx: mpsc::Receiver<SessionPersistenceRequest>,
    error_tx: mpsc::Sender<SessionPersistenceError>,
) {
    while let Ok(request) = request_rx.recv() {
        match request {
            SessionPersistenceRequest::Save { session, path } => {
                let id = session.id.clone();
                if let Err(error) = session.save_to_path(&path) {
                    let _ = error_tx.send(SessionPersistenceError::Save { id, error });
                }
            }
            SessionPersistenceRequest::Remove(path) => {
                if let Err(error) = std::fs::remove_file(&path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    let _ = error_tx.send(SessionPersistenceError::Remove { path, error });
                }
            }
            SessionPersistenceRequest::Flush(done_tx) => {
                let _ = done_tx.send(());
            }
        }
    }
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
}
