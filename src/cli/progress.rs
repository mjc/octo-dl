//! Progress bar and summary reporting for CLI downloads.

use indicatif::{ProgressBar, ProgressStyle};
use crate::{DownloadItem, SessionStats, format_bytes, format_duration};

const SEPARATOR: &str = "────────────────────────────────────────────────────────────";

/// Creates a progress bar for a single file download.
pub fn make_progress_bar(size: u64, name: &str) -> ProgressBar {
    let bar = ProgressBar::new(size);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} [{bar:40.cyan/blue}] {bytes}/{total_bytes} @ {bytes_per_sec} - {msg}",
        )
        .expect("progress template is valid")
        .progress_chars("━━╌"),
    );
    bar.set_message(name.to_string());
    bar
}

/// Creates a progress bar for total download progress.
pub fn make_total_progress_bar(size: u64) -> ProgressBar {
    let bar = ProgressBar::new(size);
    bar.set_style(
        ProgressStyle::with_template(
            "Total [{bar:40.green/white}] {bytes}/{total_bytes} @ {bytes_per_sec}",
        )
        .expect("template valid")
        .progress_chars("━━╌"),
    );
    bar
}

/// Prints the list of files to be downloaded.
pub fn print_file_list(files: &[DownloadItem], skipped: usize, partial: usize) {
    if files.is_empty() && skipped == 0 {
        println!("No files found.");
        return;
    }

    let total_size: u64 = files.iter().map(|i| i.node.size()).sum();

    println!("\n{SEPARATOR}");
    println!("Files to download:");
    println!("{SEPARATOR}");

    for item in files {
        println!("  {} ({})", item.path, format_bytes(item.node.size()));
    }

    println!("{SEPARATOR}");
    println!(
        "  {} file(s), {} total",
        files.len(),
        format_bytes(total_size)
    );
    if skipped > 0 {
        println!("  {skipped} file(s) skipped (already exist)");
    }
    if partial > 0 {
        println!("  {partial} file(s) with partial downloads (will re-download)");
    }
    println!("{SEPARATOR}\n");
}

/// Prints a summary of download statistics.
pub fn print_summary(stats: &SessionStats) {
    if stats.files_downloaded == 0 && stats.files_skipped == 0 {
        return;
    }

    println!("\n{SEPARATOR}");
    println!("Download Summary");
    println!("{SEPARATOR}");

    if stats.files_downloaded > 0 {
        println!("  Files downloaded:  {}", stats.files_downloaded);
        println!("  Total size:        {}", format_bytes(stats.total_bytes));
        println!("  Total time:        {}", format_duration(stats.elapsed));
        println!(
            "  Average speed:     {}/s",
            format_bytes(stats.average_speed())
        );
        println!("  Peak speed:        {}/s", format_bytes(stats.peak_speed));
        if let Some(ramp) = stats.average_ramp_up {
            println!(
                "  Avg ramp-up:       {} to 80% of peak",
                format_duration(ramp)
            );
        }
    }

    if stats.files_skipped > 0 {
        println!("  Files skipped:     {}", stats.files_skipped);
    }

    println!("{SEPARATOR}");
}
