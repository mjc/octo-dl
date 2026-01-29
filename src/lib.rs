//! octo-dl - A library for downloading files from MEGA.
//!
//! This library provides core functionality for downloading files from MEGA,
//! abstracted from any specific UI or display framework.
//!
//! # Example
//!
//! ```no_run
//! use octo_dl::{Downloader, DownloadConfig, NoProgress};
//!
//! # async fn example() -> octo_dl::Result<()> {
//! // Create a MEGA client
//! let http = reqwest::Client::new();
//! let mut client = mega::Client::builder().build(http)?;
//! client.login("email", "password", None).await?;
//!
//! // Create downloader with default config
//! let downloader = Downloader::new(client, DownloadConfig::default());
//!
//! // Fetch nodes from a URL
//! let nodes = downloader.client().fetch_public_nodes("https://mega.nz/...").await?;
//!
//! // Collect files to download
//! let collected = downloader.collect_files(&nodes).await;
//!
//! // Download with no progress reporting
//! let stats = downloader.download_all(&collected.to_download, &NoProgress, collected.skipped).await?;
//! println!("Downloaded {} files", stats.files_downloaded);
//! # Ok(())
//! # }
//! ```

#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]

pub mod config;
pub mod dlc;
pub mod download;
pub mod error;
pub mod fs;
pub mod stats;

// Re-export main types for convenience
pub use config::DownloadConfig;
pub use dlc::{DlcKeyCache, parse_dlc_file};
pub use download::{CollectedFiles, DownloadItem, DownloadProgress, Downloader, NoProgress};
pub use error::{Error, Result};
pub use fs::{FileSystem, TokioFileSystem};
pub use stats::{FileStats, SessionStats};

// Re-export mega types used in the public API
pub use mega::{Client as MegaClient, Node, Nodes};
