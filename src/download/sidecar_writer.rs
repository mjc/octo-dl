use std::io;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{self, Sender};

use crate::fs::FileFingerprint;

use super::sidecar_store::ResumeSidecar;

pub(in crate::download) fn sidecar_tmp_path(path: &Path) -> PathBuf {
    path.with_extension("postcard.tmp")
}

fn fingerprint_part_sync(path: &Path) -> Option<FileFingerprint> {
    let file = std::fs::OpenOptions::new().read(true).open(path).ok()?;
    let metadata = file.metadata().ok()?;
    Some(FileFingerprint::from_metadata(&metadata))
}

fn save_sidecar_atomic_sync(path: &Path, sidecar: &ResumeSidecar) -> io::Result<()> {
    let tmp = sidecar_tmp_path(path);
    let data = postcard::to_stdvec(sidecar).map_err(io::Error::other)?;
    let mut file = std::fs::File::create(&tmp)?;
    std::io::Write::write_all(&mut file, &data)?;
    std::io::Write::flush(&mut file)?;
    file.sync_data()?;
    drop(file);
    std::fs::rename(&tmp, path)?;

    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
    }

    Ok(())
}

#[cfg(test)]
type PersistEventRx = Arc<Mutex<mpsc::Receiver<()>>>;
#[cfg(test)]
type PersistEventTx = mpsc::Sender<()>;
#[cfg(not(test))]
type PersistEventTx = ();

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
    persist_event_tx: PersistEventTx,
}

impl SidecarWriterWorker {
    fn new(path: PathBuf, part_path: PathBuf, persist_event_tx: PersistEventTx) -> Self {
        Self {
            path,
            part_path,
            last_persisted_generation: None,
            persist_event_tx,
        }
    }

    fn persist_snapshot(
        &mut self,
        generation: SidecarGeneration,
        mut snapshot: ResumeSidecar,
        allow_equal: bool,
    ) {
        let stale = if allow_equal {
            self.last_persisted_generation
                .is_some_and(|last| generation < last)
        } else {
            self.last_persisted_generation
                .is_some_and(|last| generation <= last)
        };
        if stale {
            return;
        }
        snapshot.part_fingerprint = fingerprint_part_sync(&self.part_path);
        if let Err(err) = save_sidecar_atomic_sync(&self.path, &snapshot) {
            log::warn!(
                "Failed to persist resume sidecar {} after verified chunk sync: {err}",
                self.path.display()
            );
            return;
        }
        self.last_persisted_generation = Some(generation);
        #[cfg(test)]
        let _ = self.persist_event_tx.send(());
    }

    fn run(mut self, rx: mpsc::Receiver<SidecarWriterCommand>) {
        while let Ok(command) = rx.recv() {
            match command {
                SidecarWriterCommand::Persist(request) => {
                    self.persist_snapshot(request.generation, request.snapshot, request.allow_equal)
                }
                SidecarWriterCommand::Finish => break,
            }
        }
    }
}

pub(super) struct LazySidecarWriter {
    tx: Mutex<Option<Sender<SidecarWriterCommand>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    #[cfg(test)]
    persist_event_rx: PersistEventRx,
}

impl LazySidecarWriter {
    pub(super) fn new(path: PathBuf, part_path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        #[cfg(test)]
        let (persist_event_tx, persist_event_rx) = mpsc::channel();
        #[cfg(test)]
        let persist_event_rx = Arc::new(Mutex::new(persist_event_rx));
        let worker = std::thread::Builder::new()
            .name(format!("sidecar-writer:{}", path.display()))
            .spawn(move || {
                SidecarWriterWorker::new(
                    path,
                    part_path,
                    #[cfg(test)]
                    persist_event_tx,
                    #[cfg(not(test))]
                    (),
                )
                .run(rx)
            })
            .expect("spawn sidecar writer thread");
        Self {
            tx: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
            #[cfg(test)]
            persist_event_rx,
        }
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

    pub(super) async fn finish(&self, _shutdown: SidecarWriterShutdown) {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(SidecarWriterCommand::Finish);
        }
        let worker = self.worker.lock().unwrap().take();
        if let Some(worker) = worker {
            let _ = tokio::task::spawn_blocking(move || worker.join()).await;
        }
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
