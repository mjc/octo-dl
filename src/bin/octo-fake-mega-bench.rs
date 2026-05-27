use std::path::PathBuf;

use octo_dl::fake_mega::{BenchOptions, run_bench};

fn print_usage() {
    eprintln!("Usage: octo-fake-mega-bench [OPTIONS]");
    eprintln!();
    eprintln!("Build a local fake mega.nz file link and download it through octo-dl");
    eprintln!("using the real decrypt + condensed-MAC path with one connection.");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --size-mib <N>                Fixture size in MiB (default: 256)");
    eprintln!("  --output-dir <PATH>          Directory for fixture/output artifacts");
    eprintln!("  --seed <N>                   Deterministic plaintext seed");
    eprintln!("  --chunks-per-file <N>        Download workers / per-file parallelism");
    eprintln!("  --server-worker-threads <N>  Fake MEGA server runtime worker threads");
    eprintln!("  --mega-chunks-per-request <N>");
    eprintln!("                               Adjacent MEGA chunks batched per HTTP request");
    eprintln!("  --keep                       Keep fixture/output files after the run");
    eprintln!("  -h, --help                   Show this help");
}

fn parse_args() -> Result<(BenchOptions, bool), String> {
    let mut options = BenchOptions::default();
    let mut keep = false;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--size-mib" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--size-mib requires a value".to_string())?;
                let mib = value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --size-mib value: {value}"))?;
                options.size_bytes = mib
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| "--size-mib is too large".to_string())?;
            }
            "--output-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--output-dir requires a value".to_string())?;
                options.root_dir = PathBuf::from(value);
            }
            "--seed" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--seed requires a value".to_string())?;
                options.seed = value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --seed value: {value}"))?;
            }
            "--chunks-per-file" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--chunks-per-file requires a value".to_string())?;
                options.chunks_per_file = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --chunks-per-file value: {value}"))?;
            }
            "--server-worker-threads" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--server-worker-threads requires a value".to_string())?;
                options.server_worker_threads = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --server-worker-threads value: {value}"))?;
            }
            "--mega-chunks-per-request" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--mega-chunks-per-request requires a value".to_string())?;
                options.mega_chunks_per_request = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --mega-chunks-per-request value: {value}"))?;
            }
            "--keep" => keep = true,
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok((options, keep))
}

fn bytes_per_second(bytes: u64, elapsed: std::time::Duration) -> u64 {
    let seconds = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
    (bytes as f64 / seconds) as u64
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (options, keep) = parse_args().map_err(|error| {
        eprintln!("Error: {error}");
        eprintln!();
        print_usage();
        std::process::exit(1);
    })?;

    let result = run_bench(&options).await?;
    let throughput = bytes_per_second(result.bytes, result.elapsed);

    println!("Downloaded {}", octo_dl::format_bytes(result.bytes));
    println!("Elapsed: {}", octo_dl::format_duration(result.elapsed));
    println!("Throughput: {}/s", octo_dl::format_bytes(throughput));
    println!("Public URL: {}", result.public_url);

    if keep {
        println!("Artifacts kept in {}", result.root_dir.display());
    } else {
        tokio::fs::remove_dir_all(&result.root_dir).await?;
    }

    Ok(())
}
