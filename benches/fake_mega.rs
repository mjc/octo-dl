use std::path::PathBuf;

use octo_dl::fake_mega::{BenchOptions, run_bench};
use uuid::Uuid;

const SIZE_MIB_ENV: &str = "OCTO_FAKE_MEGA_SIZE_MIB";
const OUTPUT_DIR_ENV: &str = "OCTO_FAKE_MEGA_OUTPUT_DIR";
const SEED_ENV: &str = "OCTO_FAKE_MEGA_SEED";
const CHUNKS_PER_FILE_ENV: &str = "OCTO_FAKE_MEGA_CHUNKS_PER_FILE";
const SERVER_WORKERS_ENV: &str = "OCTO_FAKE_MEGA_SERVER_WORKER_THREADS";
const MEGA_CHUNKS_ENV: &str = "OCTO_FAKE_MEGA_MEGA_CHUNKS_PER_REQUEST";
const KEEP_ENV: &str = "OCTO_FAKE_MEGA_KEEP";

#[derive(Debug, Clone)]
struct BenchHarnessOptions {
    base_output_dir: PathBuf,
    keep_artifacts: bool,
    size_bytes: u64,
    seed: u64,
    chunks_per_file: usize,
    server_worker_threads: usize,
    mega_chunks_per_request: usize,
}

impl BenchHarnessOptions {
    fn from_env() -> Result<Self, String> {
        let defaults = BenchOptions::default();
        let size_mib = parse_env_u64(SIZE_MIB_ENV)?.unwrap_or(defaults.size_bytes / (1024 * 1024));
        let size_bytes = size_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| format!("{SIZE_MIB_ENV} is too large"))?;
        Ok(Self {
            base_output_dir: parse_env_path(OUTPUT_DIR_ENV)
                .unwrap_or_else(|| std::env::temp_dir().join("octo-fake-mega-bench")),
            keep_artifacts: parse_env_bool(KEEP_ENV)?.unwrap_or(false),
            size_bytes,
            seed: parse_env_u64(SEED_ENV)?.unwrap_or(defaults.seed),
            chunks_per_file: parse_env_usize(CHUNKS_PER_FILE_ENV)?
                .unwrap_or(defaults.chunks_per_file),
            server_worker_threads: parse_env_usize(SERVER_WORKERS_ENV)?
                .unwrap_or(defaults.server_worker_threads),
            mega_chunks_per_request: parse_env_usize(MEGA_CHUNKS_ENV)?
                .unwrap_or(defaults.mega_chunks_per_request),
        })
    }

    fn bench_options(&self) -> BenchOptions {
        BenchOptions {
            root_dir: self.base_output_dir.join(format!("run-{}", Uuid::new_v4())),
            file_name: BenchOptions::default().file_name,
            size_bytes: self.size_bytes,
            seed: self.seed,
            chunks_per_file: self.chunks_per_file,
            server_worker_threads: self.server_worker_threads,
            mega_chunks_per_request: self.mega_chunks_per_request,
        }
    }
}

fn parse_env_u64(name: &str) -> Result<Option<u64>, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .map(Some)
            .map_err(|_| format!("invalid {name} value: {value}")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid unicode")),
    }
}

fn parse_env_usize(name: &str) -> Result<Option<usize>, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .map(Some)
            .map_err(|_| format!("invalid {name} value: {value}")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid unicode")),
    }
}

fn parse_env_bool(name: &str) -> Result<Option<bool>, String> {
    match std::env::var(name) {
        Ok(value) => match value.as_str() {
            "0" | "false" | "FALSE" | "False" => Ok(Some(false)),
            "1" | "true" | "TRUE" | "True" => Ok(Some(true)),
            _ => Err(format!("invalid {name} value: {value}")),
        },
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid unicode")),
    }
}

fn parse_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn main() {
    divan::main();
}

#[divan::bench]
fn fake_mega_memory_path() {
    let options = BenchHarnessOptions::from_env().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime
        .block_on(async {
            let bench_options = options.bench_options();
            let result = run_bench(&bench_options).await.unwrap();
            if !options.keep_artifacts {
                tokio::fs::remove_dir_all(&result.root_dir).await.unwrap();
            }
            result
        })
        .bytes;
    divan::black_box(result);
}
