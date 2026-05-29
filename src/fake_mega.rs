use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle as ThreadJoinHandle;

use aes::Aes128;
use aes::cipher::{BlockEncryptMut, KeyIvInit, StreamCipher, StreamCipherSeek};
use axum::extract::{Path as AxumPath, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, http::StatusCode};
use base64::Engine;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use bytes::Bytes;
use cbc::Encryptor;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::{Error, Result};
use reqwest::Url;

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

const FIXTURE_BUFFER_BYTES: usize = 1024 * 1024;
const DEFAULT_FILE_NAME: &str = "bench.bin";

#[derive(Debug, Clone)]
pub struct FakeMegaFixture {
    root_dir: PathBuf,
    ciphertext_path: PathBuf,
    ciphertext: Bytes,
    file_name: String,
    handle: String,
    public_key: String,
    size: u64,
    seed: u64,
    attr: String,
}

impl FakeMegaFixture {
    #[must_use]
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    #[must_use]
    pub fn ciphertext_path(&self) -> &Path {
        &self.ciphertext_path
    }

    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    #[must_use]
    pub fn handle(&self) -> &str {
        &self.handle
    }

    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub fn public_url(&self) -> String {
        format!("https://mega.nz/file/{}#{}", self.handle, self.public_key)
    }

    pub fn fill_plaintext(&self, offset: u64, buf: &mut [u8]) {
        fill_deterministic_plaintext(self.seed, offset, buf);
    }
}

#[derive(Debug)]
pub struct FakeMegaServer {
    origin: Url,
    fixture: FakeMegaFixture,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_thread: Option<ThreadJoinHandle<io::Result<()>>>,
}

impl FakeMegaServer {
    pub fn spawn(fixture: FakeMegaFixture, worker_threads: usize) -> Result<Self> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let origin = Url::parse(&format!("http://{addr}/"))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let state = FakeMegaState {
            handle: fixture.handle.clone(),
            attr: fixture.attr.clone(),
            size: fixture.size,
            ciphertext: fixture.ciphertext.clone(),
            download_base_url: format!("http://{addr}/download/{}", fixture.handle),
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_thread = std::thread::spawn(move || -> io::Result<()> {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads.max(1))
                .enable_all()
                .build()
                .map_err(io::Error::other)?;

            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener)?;
                let app = Router::new()
                    .route("/cs", post(handle_command_batch))
                    .route("/download/{handle}/{range}", get(handle_download_range))
                    .with_state(state);
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
            })
        });

        Ok(Self {
            origin,
            fixture,
            shutdown_tx: Some(shutdown_tx),
            server_thread: Some(server_thread),
        })
    }

    #[must_use]
    pub fn origin(&self) -> &Url {
        &self.origin
    }

    #[must_use]
    pub fn fixture(&self) -> &FakeMegaFixture {
        &self.fixture
    }

    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.server_thread.take() {
            let result = tokio::task::spawn_blocking(move || thread.join())
                .await
                .map_err(|error| {
                    io::Error::other(format!("fake MEGA server join failed: {error}"))
                })?
                .map_err(|_| io::Error::other("fake MEGA server thread panicked"))?;
            result?;
        }
        Ok(())
    }
}

impl Drop for FakeMegaServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchOptions {
    pub root_dir: PathBuf,
    pub file_name: String,
    pub size_bytes: u64,
    pub seed: u64,
    pub chunks_per_file: usize,
    pub server_worker_threads: usize,
    pub mega_chunks_per_request: usize,
}

impl Default for BenchOptions {
    fn default() -> Self {
        Self {
            root_dir: std::env::temp_dir().join(format!("octo-fake-mega-{}", Uuid::new_v4())),
            file_name: DEFAULT_FILE_NAME.to_string(),
            size_bytes: 256 * 1024 * 1024,
            seed: 0x0c70_d1_5eed,
            chunks_per_file: 1,
            server_worker_threads: 1,
            mega_chunks_per_request: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchResult {
    pub root_dir: PathBuf,
    pub public_url: String,
    pub output_path: PathBuf,
    pub bytes: u64,
    pub elapsed: std::time::Duration,
}

pub async fn run_bench(options: &BenchOptions) -> Result<BenchResult> {
    let fixture_dir = options.root_dir.join("fixture");
    let download_dir = options.root_dir.join("download");
    tokio::fs::create_dir_all(&fixture_dir).await?;
    tokio::fs::create_dir_all(&download_dir).await?;

    let fixture = create_fake_mega_fixture(
        &fixture_dir,
        &options.file_name,
        options.size_bytes,
        options.seed,
    )
    .await?;
    let server = FakeMegaServer::spawn(fixture.clone(), options.server_worker_threads)?;

    let http = reqwest::Client::builder().build()?;
    let bench_http = http.clone();
    let client = mega::Client::builder()
        .origin(server.origin().clone())
        .build(http)
        .map_err(Error::Mega)?;
    let public_url = fixture.public_url();
    let nodes = client
        .fetch_public_nodes(&public_url)
        .await
        .map_err(Error::Mega)?;
    let node = nodes.get_node_by_handle(fixture.handle()).ok_or_else(|| {
        Error::Download("fake MEGA node missing from fetched metadata".to_string())
    })?;
    let output_path = download_dir.join(fixture.file_name());
    let download_base_url = format!("{}download/{}", server.origin(), fixture.handle());

    let start = std::time::Instant::now();
    benchmark_parallel_memory_download(
        &bench_http,
        &download_base_url,
        node,
        options.chunks_per_file,
        options.mega_chunks_per_request,
    )
    .await?;
    let elapsed = start.elapsed();
    drop(client);
    server.shutdown().await?;

    Ok(BenchResult {
        root_dir: options.root_dir.clone(),
        public_url,
        output_path,
        bytes: fixture.size(),
        elapsed,
    })
}

pub async fn run_single_connection_bench(options: &BenchOptions) -> Result<BenchResult> {
    let mut single_connection = options.clone();
    single_connection.chunks_per_file = 1;
    single_connection.server_worker_threads = 1;
    single_connection.mega_chunks_per_request = 1;
    run_bench(&single_connection).await
}

pub async fn create_fake_mega_fixture(
    root_dir: &Path,
    file_name: &str,
    size: u64,
    seed: u64,
) -> Result<FakeMegaFixture> {
    tokio::fs::create_dir_all(root_dir).await?;
    let handle = BASE64_URL_SAFE_NO_PAD.encode(&Uuid::new_v4().as_bytes()[..8]);
    let ciphertext_path = root_dir.join("ciphertext.bin");
    let aes_key = derive_bytes::<16>(seed, 0xA5A5_A5A5_A5A5_A5A5);
    let aes_iv = derive_bytes::<8>(seed, 0x5A5A_5A5A_5A5A_5A5A);
    let (ciphertext, condensed_mac) =
        build_ciphertext_fixture(size, seed, &aes_key, &aes_iv).map_err(io::Error::other)?;
    tokio::fs::write(&ciphertext_path, ciphertext.as_ref()).await?;
    let attr = pack_public_attributes(file_name, &aes_key)?;

    let mut public_key = [0u8; 32];
    public_key[..16].copy_from_slice(&aes_key);
    public_key[16..24].copy_from_slice(&aes_iv);
    public_key[24..].copy_from_slice(&condensed_mac);
    merge_public_key(&mut public_key);

    Ok(FakeMegaFixture {
        root_dir: root_dir.to_path_buf(),
        ciphertext_path,
        ciphertext,
        file_name: file_name.to_string(),
        handle,
        public_key: BASE64_URL_SAFE_NO_PAD.encode(public_key),
        size,
        seed,
        attr,
    })
}

#[derive(Clone)]
struct FakeMegaState {
    handle: String,
    attr: String,
    size: u64,
    ciphertext: Bytes,
    download_base_url: String,
}

#[derive(Debug, Deserialize)]
struct ApiRequest {
    #[serde(rename = "a")]
    action: String,
    #[serde(default)]
    p: Option<String>,
    #[serde(default)]
    n: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiDownloadResponse {
    #[serde(rename = "g")]
    download_url: String,
    #[serde(rename = "s")]
    size: u64,
    #[serde(rename = "at")]
    attr: String,
}

#[derive(Debug, Serialize)]
struct PublicNodeAttributes<'a> {
    #[serde(rename = "n")]
    name: &'a str,
}

async fn handle_command_batch(
    State(state): State<FakeMegaState>,
    Json(requests): Json<Vec<ApiRequest>>,
) -> Response {
    let Some(request) = requests.first() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let requested_handle = request.p.as_deref().or(request.n.as_deref());
    if request.action != "g" || requested_handle != Some(state.handle.as_str()) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    Json(vec![ApiDownloadResponse {
        download_url: state.download_base_url,
        size: state.size,
        attr: state.attr,
    }])
    .into_response()
}

async fn handle_download_range(
    State(state): State<FakeMegaState>,
    AxumPath((handle, range)): AxumPath<(String, String)>,
) -> Response {
    if handle != state.handle {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some((offset, length)) = parse_inclusive_range(&range, state.size) else {
        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    };

    state
        .ciphertext
        .slice(offset..offset + length)
        .into_response()
}

fn parse_inclusive_range(spec: &str, size: u64) -> Option<(usize, usize)> {
    let (start, end) = spec.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    if end < start || end >= size {
        return None;
    }
    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?;
    Some((start, end.checked_sub(start)?.checked_add(1)?))
}

fn build_ciphertext_fixture(
    size: u64,
    seed: u64,
    aes_key: &[u8; 16],
    aes_iv: &[u8; 8],
) -> Result<(Bytes, [u8; 8])> {
    let mut plain = vec![0u8; FIXTURE_BUFFER_BYTES];
    let mut cipher = vec![0u8; FIXTURE_BUFFER_BYTES];
    let ciphertext_capacity = usize::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "fixture size exceeds usize"))?;
    let mut ciphertext = Vec::with_capacity(ciphertext_capacity);
    let mut ctr_iv = [0u8; 16];
    ctr_iv[..8].copy_from_slice(aes_iv);
    let mut ctr = Aes128Ctr::new(aes_key.into(), (&ctr_iv).into());
    let mut condensed = mega::MegaCondensedMac::new(aes_key);
    let mut offset = 0u64;

    for chunk in mega::mega_chunk_boundaries(size) {
        let mut chunk_mac = mega::MegaChunkMac::new(aes_key, aes_iv);
        let mut remaining = chunk.length;
        while remaining > 0 {
            let step = remaining.min(FIXTURE_BUFFER_BYTES as u64) as usize;
            let plain_slice = &mut plain[..step];
            fill_deterministic_plaintext(seed, offset, plain_slice);
            chunk_mac.update(plain_slice);

            let cipher_slice = &mut cipher[..step];
            cipher_slice.copy_from_slice(plain_slice);
            ctr.apply_keystream(cipher_slice);
            ciphertext.extend_from_slice(cipher_slice);

            offset += step as u64;
            remaining -= step as u64;
        }
        condensed.update_chunk_mac(&chunk_mac.finalize());
    }

    Ok((Bytes::from(ciphertext), condensed.finalize()))
}

#[derive(Clone, Copy)]
struct MegaChunkRun {
    start: usize,
    end: usize,
}

impl MegaChunkRun {
    fn first(self, chunks: &[mega::MegaChunk]) -> mega::MegaChunk {
        chunks[self.start]
    }

    fn last(self, chunks: &[mega::MegaChunk]) -> mega::MegaChunk {
        chunks[self.end - 1]
    }

    fn total_length(self, chunks: &[mega::MegaChunk]) -> u64 {
        let first = self.first(chunks);
        let last = self.last(chunks);
        last.offset + last.length - first.offset
    }
}

#[derive(Default)]
struct ChunkClaimCursor {
    next: Mutex<usize>,
}

impl ChunkClaimCursor {
    fn claim(
        &self,
        chunks: &[mega::MegaChunk],
        max_chunks_per_request: usize,
    ) -> Option<MegaChunkRun> {
        let mut next = self.next.lock();
        if *next >= chunks.len() {
            return None;
        }
        let start = *next;
        let end = chunks.len().min(start + max_chunks_per_request.max(1));
        *next = end;
        Some(MegaChunkRun { start, end })
    }
}

async fn benchmark_parallel_memory_download(
    http: &reqwest::Client,
    base_url: &str,
    node: &mega::Node,
    worker_count: usize,
    max_chunks_per_request: usize,
) -> Result<()> {
    if !node.kind().is_file() {
        return Err(Error::Mega(mega::Error::NotAFileNode));
    }

    let aes_key = *node.aes_key();
    let aes_iv_8 = node
        .aes_iv()
        .copied()
        .ok_or(mega::Error::MissingNodeAesIv)
        .map_err(Error::Mega)?;
    let expected_mac = node
        .condensed_mac()
        .copied()
        .ok_or(mega::Error::MissingCondensedMac)
        .map_err(Error::Mega)?;
    let mut aes_iv_16 = [0u8; 16];
    aes_iv_16[..8].copy_from_slice(&aes_iv_8);

    let chunks = Arc::<[mega::MegaChunk]>::from(mega::mega_chunk_boundaries(node.size()));
    let worker_count = worker_count.max(1).min(chunks.len().max(1));
    let claim_cursor = Arc::new(ChunkClaimCursor::default());
    let mac = Arc::new(mega::ParallelMacProcessor::new(
        node.size(),
        &aes_key,
        &aes_iv_8,
    ));
    let base_url: Arc<str> = Arc::from(base_url);

    let workers = (0..worker_count)
        .map(|_| {
            let http = http.clone();
            let chunks = Arc::clone(&chunks);
            let claim_cursor = Arc::clone(&claim_cursor);
            let mac = Arc::clone(&mac);
            let base_url = Arc::clone(&base_url);

            tokio::spawn(async move {
                let mut url = String::with_capacity(base_url.len() + 48);

                while let Some(run) = claim_cursor.claim(&chunks, max_chunks_per_request) {
                    let first = run.first(&chunks);
                    let last = run.last(&chunks);
                    url.clear();
                    url.push_str(&base_url);
                    url.push('/');
                    url.push_str(&format!(
                        "{}-{}",
                        first.offset,
                        last.offset + last.length - 1
                    ));

                    let body = http
                        .get(url.as_str())
                        .send()
                        .await?
                        .error_for_status()?
                        .bytes()
                        .await?;
                    let mut body = body.to_vec();
                    let expected_len =
                        usize::try_from(run.total_length(&chunks)).map_err(io::Error::other)?;
                    if body.len() != expected_len {
                        return Err(Error::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!(
                                "unexpected range size for {}: expected {} bytes, got {}",
                                url,
                                expected_len,
                                body.len()
                            ),
                        )));
                    }
                    for chunk in &chunks[run.start..run.end] {
                        let start = usize::try_from(chunk.offset - first.offset)
                            .map_err(io::Error::other)?;
                        let end = start
                            .checked_add(usize::try_from(chunk.length).map_err(io::Error::other)?)
                            .ok_or_else(|| io::Error::other("chunk range overflow"))?;
                        decrypt_range(&aes_key, &aes_iv_16, chunk.offset, &mut body[start..end]);
                        let chunk_mac =
                            mega::compute_mega_chunk_mac(&body[start..end], &aes_key, &aes_iv_8);
                        mac.set_chunk_mac(chunk.index as usize, chunk_mac);
                    }
                }

                Ok::<(), Error>(())
            })
        })
        .collect::<Vec<_>>();

    for worker in workers {
        worker.await.map_err(|error| {
            Error::Io(io::Error::other(format!(
                "benchmark worker failed: {error}"
            )))
        })??;
    }

    let actual_mac = mac
        .finalize()
        .ok_or(mega::Error::CondensedMacMismatch)
        .map_err(Error::Mega)?;
    if actual_mac != expected_mac {
        return Err(Error::Mega(mega::Error::CondensedMacMismatch));
    }

    Ok(())
}

fn decrypt_range(key: &[u8; 16], iv: &[u8; 16], offset: u64, data: &mut [u8]) {
    let mut cipher = ctr::Ctr128BE::<Aes128>::new(key.into(), iv.into());
    cipher.seek(offset);
    cipher.apply_keystream(data);
}

fn pack_public_attributes(file_name: &str, aes_key: &[u8; 16]) -> Result<String> {
    let mut buffer = b"MEGA".to_vec();
    serde_json::to_writer(&mut buffer, &PublicNodeAttributes { name: file_name })
        .map_err(io::Error::other)?;
    let padding_len = (16 - buffer.len() % 16) % 16;
    buffer.extend(std::iter::repeat_n(0, padding_len));

    let iv = [0u8; 16];
    let mut cbc = Encryptor::<Aes128>::new(aes_key.into(), (&iv).into());
    for chunk in buffer.chunks_exact_mut(16) {
        cbc.encrypt_block_mut(chunk.into());
    }

    Ok(BASE64_URL_SAFE_NO_PAD.encode(buffer))
}

fn merge_public_key(key: &mut [u8; 32]) {
    let (first, second) = key.split_at_mut(16);
    for (dst, src) in first.iter_mut().zip(second.iter()) {
        *dst ^= *src;
    }
}

fn fill_deterministic_plaintext(seed: u64, offset: u64, buf: &mut [u8]) {
    let mut cursor = 0usize;
    let mut absolute = offset;
    while cursor < buf.len() {
        let block_index = absolute / 8;
        let block = splitmix64(seed ^ block_index).to_le_bytes();
        let skip = (absolute % 8) as usize;
        let take = (8 - skip).min(buf.len() - cursor);
        buf[cursor..cursor + take].copy_from_slice(&block[skip..skip + take]);
        absolute += take as u64;
        cursor += take;
    }
}

fn derive_bytes<const N: usize>(seed: u64, domain: u64) -> [u8; N] {
    let mut out = [0u8; N];
    let mut counter = 0u64;
    let mut cursor = 0usize;
    while cursor < N {
        let bytes = splitmix64(seed ^ domain ^ counter).to_le_bytes();
        let take = (N - cursor).min(bytes.len());
        out[cursor..cursor + take].copy_from_slice(&bytes[..take]);
        cursor += take;
        counter += 1;
    }
    out
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}
