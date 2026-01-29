//! Error types for the octo-dl library.

use thiserror::Error;

/// Errors that can occur during download operations.
#[derive(Error, Debug)]
pub enum Error {
    /// Error from the MEGA API.
    #[error("MEGA API error: {0}")]
    Mega(#[from] mega::Error),

    /// DLC file parsing failed.
    #[error("DLC parsing failed: {0}")]
    Dlc(String),

    /// I/O error during file operations.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// File already exists and force overwrite is disabled.
    #[error("File already exists: {path}")]
    FileExists {
        /// Path to the existing file.
        path: String,
    },

    /// Download operation failed.
    #[error("Download failed: {0}")]
    Download(String),

    /// HTTP request error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

/// A specialized `Result` type for octo-dl operations.
pub type Result<T> = std::result::Result<T, Error>;
