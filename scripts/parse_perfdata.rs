use std::env;
use std::io;
use std::process::{Command, Stdio};

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let mut input = String::from("./perf.data");
    let mut rest = &args[1..];

    // Parse options
    while !rest.is_empty() {
        match rest[0].as_str() {
            "-i" | "--input" => {
                if rest.len() < 2 {
                    eprintln!("Error: --input requires an argument");
                    std::process::exit(1);
                }
                input = rest[1].clone();
                rest = &rest[2..];
            }
            "-h" | "--help" => {
                usage();
                return Ok(());
            }
            s if s.starts_with('-') => {
                eprintln!("Unknown option: {s}");
                usage();
                std::process::exit(1);
            }
            _ => break,
        }
    }

    let command = rest.first().map(|s| s.as_str()).unwrap_or("top");
    let cmd_args = if rest.len() > 1 { &rest[1..] } else { &[] };

    if !std::path::Path::new(&input).exists() {
        eprintln!("Error: {input} not found");
        std::process::exit(1);
    }

    match command {
        "top" => cmd_top(&input, cmd_args),
        "callers" => cmd_callers(&input, cmd_args),
        "annotate" => cmd_annotate(&input, cmd_args),
        "flamegraph" => cmd_flamegraph(&input),
        "dso" => cmd_dso(&input),
        "summary" => cmd_summary(&input),
        _ => {
            eprintln!("Unknown command: {command}");
            usage();
            std::process::exit(1);
        }
    }
}

fn usage() {
    eprintln!("Usage: parse_perfdata [OPTIONS] [command] [args...]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -i, --input <FILE>   Input perf.data file (default: ./perf.data)");
    eprintln!("  -h, --help           Show help");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  top [N]              Top N symbols by overhead (default: 30)");
    eprintln!("  callers <symbol>     Call-graph ancestors for a symbol");
    eprintln!("  annotate <symbol>    Source-annotated disassembly");
    eprintln!("  flamegraph           Generate flamegraph.svg via inferno");
    eprintln!("  dso                  Per-library/DSO breakdown");
    eprintln!("  summary              Combined: dso + top 20 + category breakdown");
}

fn run_perf(args: &[&str]) -> io::Result<String> {
    let output = Command::new("perf")
        .args(args)
        .stderr(Stdio::null())
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn filter_overhead_lines(output: &str, limit: usize) -> Vec<&str> {
    output
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with(|c: char| c.is_ascii_digit())
                || trimmed.starts_with('.')
        })
        .take(limit)
        .collect()
}

fn cmd_top(input: &str, args: &[String]) -> ! {
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(30);
    println!("=== Top {n} symbols by overhead ===\n");

    match run_perf(&[
        "report", "-i", input, "--stdio", "--no-children",
        "--sort=overhead,symbol", "--max-stack=0",
    ]) {
        Ok(output) => {
            for line in filter_overhead_lines(&output, n) {
                println!("{line}");
            }
        }
        Err(e) => eprintln!("perf report failed: {e}"),
    }
    std::process::exit(0);
}

fn cmd_callers(input: &str, args: &[String]) -> ! {
    let symbol = match args.first() {
        Some(s) => s.as_str(),
        None => {
            eprintln!("Usage: callers <symbol>");
            std::process::exit(1);
        }
    };
    println!("=== Call-graph ancestors for: {symbol} ===\n");

    match run_perf(&[
        "report", "-i", input, "--stdio", "-g", "caller",
        &format!("--symbol-filter={symbol}"),
    ]) {
        Ok(output) => print!("{output}"),
        Err(e) => eprintln!("perf report failed: {e}"),
    }
    std::process::exit(0);
}

fn cmd_annotate(input: &str, args: &[String]) -> ! {
    let symbol = match args.first() {
        Some(s) => s.as_str(),
        None => {
            eprintln!("Usage: annotate <symbol>");
            std::process::exit(1);
        }
    };
    println!("=== Annotated disassembly: {symbol} ===\n");

    match run_perf(&[
        "annotate", "-i", input, "--stdio", &format!("--symbol={symbol}"),
    ]) {
        Ok(output) => print!("{output}"),
        Err(e) => eprintln!("perf annotate failed: {e}"),
    }
    std::process::exit(0);
}

fn cmd_flamegraph(input: &str) -> ! {
    println!("=== Generating flamegraph.svg ===");

    // Check for inferno tools
    let has_collapse = Command::new("inferno-collapse-perf")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    let has_flamegraph = Command::new("inferno-flamegraph")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();

    if !has_collapse || !has_flamegraph {
        eprintln!("Error: inferno tools not found. Install with: cargo install inferno");
        std::process::exit(1);
    }

    let perf_script = Command::new("perf")
        .args(["script", "-i", input])
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn();

    match perf_script {
        Ok(ps) => {
            let collapse = Command::new("inferno-collapse-perf")
                .stdin(ps.stdout.unwrap())
                .stdout(Stdio::piped())
                .spawn();

            match collapse {
                Ok(col) => {
                    let fg_out = std::fs::File::create("flamegraph.svg")
                        .expect("Failed to create flamegraph.svg");
                    let status = Command::new("inferno-flamegraph")
                        .stdin(col.stdout.unwrap())
                        .stdout(fg_out)
                        .status();

                    match status {
                        Ok(s) if s.success() => println!("Wrote flamegraph.svg"),
                        Ok(s) => eprintln!("inferno-flamegraph exited with: {s}"),
                        Err(e) => eprintln!("inferno-flamegraph failed: {e}"),
                    }
                }
                Err(e) => eprintln!("inferno-collapse-perf failed: {e}"),
            }
        }
        Err(e) => eprintln!("perf script failed: {e}"),
    }
    std::process::exit(0);
}

fn cmd_dso(input: &str) -> ! {
    println!("=== Per-library/DSO breakdown ===\n");

    match run_perf(&[
        "report", "-i", input, "--stdio", "--no-children",
        "--sort=dso,overhead",
    ]) {
        Ok(output) => {
            for line in filter_overhead_lines(&output, 30) {
                println!("{line}");
            }
        }
        Err(e) => eprintln!("perf report failed: {e}"),
    }
    std::process::exit(0);
}

fn cmd_summary(input: &str) -> ! {
    println!("========================================");
    println!("  Performance Summary");
    println!("========================================\n");

    // DSO breakdown
    match run_perf(&[
        "report", "-i", input, "--stdio", "--no-children",
        "--sort=dso,overhead",
    ]) {
        Ok(output) => {
            println!("=== Per-library/DSO breakdown ===\n");
            for line in filter_overhead_lines(&output, 30) {
                println!("{line}");
            }
        }
        Err(e) => eprintln!("perf report (dso) failed: {e}"),
    }

    println!("\n----------------------------------------\n");

    // Top 20
    match run_perf(&[
        "report", "-i", input, "--stdio", "--no-children",
        "--sort=overhead,symbol", "--max-stack=0",
    ]) {
        Ok(output) => {
            println!("=== Top 20 symbols by overhead ===\n");
            for line in filter_overhead_lines(&output, 20) {
                println!("{line}");
            }
        }
        Err(e) => eprintln!("perf report (top) failed: {e}"),
    }

    // Try to run parse_flamegraph for category breakdown
    let exe_path = env::current_exe().ok();
    let script_dir = exe_path
        .as_ref()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf());

    if let Some(dir) = script_dir {
        let fg_bin = dir.join("parse_flamegraph");
        if fg_bin.exists() && std::path::Path::new("flamegraph.svg").exists() {
            println!("\n----------------------------------------\n");
            let _ = Command::new(&fg_bin)
                .args(["flamegraph.svg", "summary"])
                .status();
        }
    }

    std::process::exit(0);
}
