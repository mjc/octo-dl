#![allow(clippy::needless_pass_by_value)]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};

use crate::fs::FileFingerprint;

use super::sidecar_store::{ResumeSidecar, reject_symlink, serialize_sidecar};

pub(in crate::download) fn sidecar_tmp_path(path: &Path) -> PathBuf {
    path.with_extension("postcard.tmp")
}

fn fingerprint_part_sync(path: &Path) -> Option<FileFingerprint> {
    let file = std::fs::OpenOptions::new().read(true).open(path).ok()?;
    let metadata = file.metadata().ok()?;
    Some(FileFingerprint::from_metadata(&metadata))
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SidecarFailurePoint {
    Write,
    FileSync,
    Rename,
    DirectorySync,
    WorkerDisconnect,
}

#[cfg(test)]
type SidecarFailureInjection = Arc<Mutex<Option<SidecarFailurePoint>>>;

#[cfg(test)]
fn inject_failure(
    injection: Option<&SidecarFailureInjection>,
    point: SidecarFailurePoint,
) -> io::Result<()> {
    if let Some(injection) = injection {
        let mut configured = injection.lock().unwrap();
        if *configured == Some(point) {
            *configured = None;
            drop(configured);
            return Err(io::Error::other(format!("injected {point:?} failure")));
        }
    }
    Ok(())
}

fn save_sidecar_atomic_sync(
    path: &Path,
    sidecar: &ResumeSidecar,
    #[cfg(test)] injection: Option<&SidecarFailureInjection>,
) -> io::Result<()> {
    let tmp = sidecar_tmp_path(path);
    match std::fs::symlink_metadata(&tmp) {
        Ok(metadata) => reject_symlink(&tmp, &metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let data = serialize_sidecar(sidecar)?;
    let mut file = std::fs::File::create(&tmp)?;
    #[cfg(test)]
    inject_failure(injection, SidecarFailurePoint::Write)?;
    std::io::Write::write_all(&mut file, &data)?;
    std::io::Write::flush(&mut file)?;
    #[cfg(test)]
    inject_failure(injection, SidecarFailurePoint::FileSync)?;
    file.sync_data()?;
    drop(file);
    #[cfg(test)]
    inject_failure(injection, SidecarFailurePoint::Rename)?;
    std::fs::rename(&tmp, path)?;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    #[cfg(test)]
    inject_failure(injection, SidecarFailurePoint::DirectorySync)?;
    crate::fs::sync_directory(parent)?;

    Ok(())
}

#[cfg(test)]
type PersistEventRx = Arc<Mutex<mpsc::Receiver<()>>>;
#[cfg(test)]
type PersistEventTx = mpsc::Sender<()>;
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct SidecarGeneration(u64);

impl SidecarGeneration {
    pub(super) const fn new(value: u64) -> Self {
        Self(value)
    }
}

struct SidecarWriteRequest {
    generation: SidecarGeneration,
    snapshot: ResumeSidecar,
    allow_equal: bool,
}

enum SidecarWriterCommand {
    Persist(SidecarWriteRequest),
    Finish,
}

struct SidecarWriterWorker {
    path: PathBuf,
    part_path: PathBuf,
    last_persisted_generation: Option<SidecarGeneration>,
    #[cfg(test)]
    persist_event_tx: PersistEventTx,
    #[cfg(test)]
    failure_injection: Option<SidecarFailureInjection>,
    abort_requested: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
}

impl SidecarWriterWorker {
    const fn new(
        path: PathBuf,
        part_path: PathBuf,
        #[cfg(test)] persist_event_tx: PersistEventTx,
        #[cfg(test)] failure_injection: Option<SidecarFailureInjection>,
        abort_requested: Arc<AtomicBool>,
        failure: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            path,
            part_path,
            last_persisted_generation: None,
            #[cfg(test)]
            persist_event_tx,
            #[cfg(test)]
            failure_injection,
            abort_requested,
            failure,
        }
    }

    fn persist_snapshot(
        &mut self,
        generation: SidecarGeneration,
        mut snapshot: ResumeSidecar,
        allow_equal: bool,
    ) -> bool {
        let stale = if allow_equal {
            self.last_persisted_generation
                .is_some_and(|last| generation < last)
        } else {
            self.last_persisted_generation
                .is_some_and(|last| generation <= last)
        };
        if stale {
            return true;
        }
        snapshot.part_fingerprint = fingerprint_part_sync(&self.part_path);
        if let Err(err) = save_sidecar_atomic_sync(
            &self.path,
            &snapshot,
            #[cfg(test)]
            self.failure_injection.as_ref(),
        ) {
            let mut failure = self.failure.lock().unwrap();
            if failure.is_none() {
                *failure = Some(format!(
                    "persist resume sidecar {}: {err}",
                    self.path.display()
                ));
            }
            log::warn!(
                "Failed to persist resume sidecar {} after verified chunk sync: {err}",
                self.path.display()
            );
            return false;
        }
        self.last_persisted_generation = Some(generation);
        #[cfg(test)]
        let _ = self.persist_event_tx.send(());
        true
    }

    fn run(mut self, rx: mpsc::Receiver<SidecarWriterCommand>) {
        while let Ok(command) = rx.recv() {
            if self.abort_requested.load(Ordering::Relaxed) {
                break;
            }
            match command {
                SidecarWriterCommand::Persist(request) => {
                    #[cfg(test)]
                    if let Err(error) = inject_failure(
                        self.failure_injection.as_ref(),
                        SidecarFailurePoint::WorkerDisconnect,
                    ) {
                        let mut failure = self.failure.lock().unwrap();
                        if failure.is_none() {
                            *failure = Some(format!("sidecar writer worker disconnected: {error}"));
                        }
                        return;
                    }
                    if !self.persist_snapshot(
                        request.generation,
                        request.snapshot,
                        request.allow_equal,
                    ) {
                        break;
                    }
                }
                SidecarWriterCommand::Finish => break,
            }
        }
    }
}

pub(super) struct LazySidecarWriter {
    tx: Mutex<Option<Sender<SidecarWriterCommand>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    abort_requested: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    #[cfg(test)]
    persist_event_rx: PersistEventRx,
}

impl LazySidecarWriter {
    pub(super) fn new(path: PathBuf, part_path: PathBuf) -> io::Result<Self> {
        #[cfg(test)]
        {
            Self::new_with_failure_injection(path, part_path, None)
        }
        #[cfg(not(test))]
        {
            Self::new_with_failure_injection(path, part_path)
        }
    }

    #[cfg(test)]
    pub(super) fn new_with_failpoint(
        path: PathBuf,
        part_path: PathBuf,
        failpoint: SidecarFailurePoint,
    ) -> io::Result<Self> {
        Self::new_with_failure_injection(
            path,
            part_path,
            Some(Arc::new(Mutex::new(Some(failpoint)))),
        )
    }

    fn new_with_failure_injection(
        path: PathBuf,
        part_path: PathBuf,
        #[cfg(test)] failure_injection: Option<SidecarFailureInjection>,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let abort_requested = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        #[cfg(test)]
        let (persist_event_tx, persist_event_rx) = mpsc::channel();
        #[cfg(test)]
        let persist_event_rx = Arc::new(Mutex::new(persist_event_rx));
        let worker_abort_requested = Arc::clone(&abort_requested);
        let worker_failure = Arc::clone(&failure);
        let worker = std::thread::Builder::new()
            // Keep user-controlled paths out of the OS thread name. In
            // particular, `Builder::name` panics on interior NUL bytes.
            .name("octo-sidecar-writer".to_string())
            .spawn(move || {
                SidecarWriterWorker::new(
                    path,
                    part_path,
                    #[cfg(test)]
                    persist_event_tx,
                    #[cfg(test)]
                    failure_injection,
                    worker_abort_requested,
                    worker_failure,
                )
                .run(rx);
            })?;
        Ok(Self {
            tx: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
            abort_requested,
            failure,
            #[cfg(test)]
            persist_event_rx,
        })
    }

    #[cfg(test)]
    pub(in crate::download) fn persist_event_listener(&self) -> PersistEventRx {
        self.persist_event_rx.clone()
    }

    fn persist_snapshot(
        &self,
        generation: SidecarGeneration,
        snapshot: ResumeSidecar,
        allow_equal: bool,
    ) {
        let tx = self.tx.lock().unwrap().clone();
        let Some(tx) = tx else {
            return;
        };
        if tx
            .send(SidecarWriterCommand::Persist(SidecarWriteRequest {
                generation,
                snapshot,
                allow_equal,
            }))
            .is_err()
        {
            log::warn!("Failed to queue resume sidecar write");
        }
    }

    pub(super) fn persist_verified_snapshot(
        &self,
        generation: SidecarGeneration,
        snapshot: ResumeSidecar,
    ) {
        self.persist_snapshot(generation, snapshot, false);
    }

    pub(super) fn persist_final_snapshot(
        &self,
        generation: SidecarGeneration,
        snapshot: ResumeSidecar,
    ) {
        self.persist_snapshot(generation, snapshot, true);
    }

    pub(super) async fn finish(&self, shutdown: SidecarWriterShutdown) -> io::Result<()> {
        let abort = match &shutdown {
            SidecarWriterShutdown::Abort => true,
            SidecarWriterShutdown::Flush => false,
        };
        if abort {
            self.abort_requested.store(true, Ordering::Relaxed);
        }
        if let Some(tx) = self.tx.lock().unwrap().take()
            && !abort
            && tx.send(SidecarWriterCommand::Finish).is_err()
        {
            let mut failure = self.failure.lock().unwrap();
            if failure.is_none() {
                *failure = Some("queue sidecar writer finish command".to_string());
            }
        }
        let worker = self.worker.lock().unwrap().take();
        if let Some(worker) = worker {
            let joined = tokio::task::spawn_blocking(move || worker.join()).await;
            if joined.is_err() || joined.is_ok_and(|result| result.is_err()) {
                let mut failure = self.failure.lock().unwrap();
                if failure.is_none() {
                    *failure = Some("join sidecar writer worker".to_string());
                }
            }
        }
        self.failure
            .lock()
            .unwrap()
            .take()
            .map_or(Ok(()), |error| Err(io::Error::other(error)))
    }
}

#[cfg(test)]
pub(in crate::download) async fn wait_for_persist_event(events: PersistEventRx) -> bool {
    tokio::task::spawn_blocking(move || {
        events
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok()
    })
    .await
    .expect("sidecar persist wait should not panic")
}

pub(super) enum SidecarWriterShutdown {
    Flush,
    Abort,
}

#[cfg(test)]
mod tests;
