// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Error types shared across the `wxapkg-core` engine.
//!
//! Every failure mode that can surface to a CLI / HTTP / GUI caller is modelled
//! as a variant of [`Error`]. Variants carry enough context to render a precise,
//! actionable message without the caller having to guess what went wrong.

use std::path::PathBuf;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Engine error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem access failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying operating system error.
        #[source]
        source: std::io::Error,
    },

    /// Generic I/O error without an associated path.
    #[error("I/O error: {0}")]
    PlainIo(#[from] std::io::Error),

    /// The input is not a valid WeChat package container.
    #[error("invalid wxapkg-dump at {path}: {reason}")]
    InvalidContainer {
        /// Package path (or a synthetic name for in-memory buffers).
        path: String,
        /// Human readable explanation of the structural violation.
        reason: String,
    },

    /// The package is `V1MMWX`-encrypted and no usable key was available.
    #[error("encrypted wxapkg-dump at {path} requires a key: {reason}")]
    MissingDecryptionKey {
        /// Package path.
        path: String,
        /// Why the automatic key discovery failed.
        reason: String,
    },

    /// Automatic decryption produced bytes that are still not a valid container.
    #[error("decryption of {path} produced unusable output: {reason}")]
    DecryptionFailed {
        /// Package path.
        path: String,
        /// Diagnostic detail (derived key, observed first byte, ...).
        reason: String,
    },

    /// A ZIP archive could not be read.
    #[error("failed to read archive {path}: {reason}")]
    Archive {
        /// Archive path.
        path: String,
        /// Underlying cause.
        reason: String,
    },

    /// JSON (de)serialisation failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A regular expression literal failed to compile.
    #[error("invalid pattern {pattern}: {reason}")]
    Regex {
        /// The offending pattern.
        pattern: String,
        /// Compiler diagnostic.
        reason: String,
    },

    /// The requested entry does not exist inside the package.
    #[error("entry {name:?} not found in {path}")]
    EntryNotFound {
        /// Package path.
        path: String,
        /// Requested entry name.
        name: String,
    },

    /// A requested appid / program is not present on this machine.
    ///
    /// Carries the two facts needed to build the sentence rather than a finished sentence:
    /// a rendered explanation could only ever be in one language, and it would be spliced
    /// into the localised message to produce a bilingual one.
    #[error("mini program {appid} was not found{}", match .version {
        Some(v) => format!(" at version {v}"),
        None => String::new(),
    })]
    ProgramNotFound {
        /// Requested appid or original id.
        appid: String,
        /// Requested version directory, when the lookup named one.
        version: Option<String>,
    },

    /// The caller supplied mutually incompatible options.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// A feature was requested that is not available in this build.
    #[error("unsupported feature: {0}")]
    Unsupported(String),
}

impl Error {
    /// Build an [`Error::Io`] from a path and an OS error.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Build an [`Error::InvalidContainer`].
    pub fn invalid_container(path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::InvalidContainer {
            path: path.into(),
            reason: reason.into(),
        }
    }

    /// Build an [`Error::Regex`] from a compile failure.
    pub fn regex(pattern: &str, err: &regex::Error) -> Self {
        Self::Regex {
            pattern: pattern.to_owned(),
            reason: err.to_string(),
        }
    }

    /// Stable machine readable code, used by the CLI / HTTP layers.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } | Self::PlainIo(_) => "io_error",
            Self::InvalidContainer { .. } => "invalid_container",
            Self::MissingDecryptionKey { .. } => "missing_decryption_key",
            Self::DecryptionFailed { .. } => "decryption_failed",
            Self::Archive { .. } => "archive_error",
            Self::Json(_) => "json_error",
            Self::Regex { .. } => "regex_error",
            Self::EntryNotFound { .. } => "entry_not_found",
            Self::ProgramNotFound { .. } => "program_not_found",
            Self::InvalidArgument(_) => "invalid_argument",
            Self::Unsupported(_) => "unsupported",
        }
    }
}

/// Convenience trait for attaching path context to [`std::io::Result`].
pub trait IoContext<T> {
    /// Wrap the error with the path that produced it.
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T> {
        let path = path.into();
        self.map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })
    }
}
