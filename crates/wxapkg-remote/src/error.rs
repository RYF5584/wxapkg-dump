// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Errors remote download can report.

use std::fmt;

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong while talking to the remote service.
#[derive(Debug)]
pub enum Error {
    /// The platform has no application-data directory.
    NoDataDirectory,
    /// A filesystem operation failed.
    Io {
        /// What was being done.
        action: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A stored file is not valid JSON, or does not match the expected schema.
    Corrupt {
        /// Path that failed to parse.
        path: String,
        /// The parser's complaint.
        reason: String,
    },
    /// A network request failed.
    Network(String),
    /// The response was not what the protocol requires.
    Protocol(String),
    /// The service understood the request and refused it.
    Upstream(String),
    /// The stored session is no longer accepted; sign in again.
    SessionExpired(String),
    /// No account is signed in.
    NotLoggedIn(String),
    /// A caller-supplied value is not usable.
    InvalidArgument(String),
    /// The caller asked to stop the download.
    Cancelled,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDataDirectory => {
                write!(f, "the platform reports no application data directory")
            }
            Self::Io { action, source } => write!(f, "{action}: {source}"),
            Self::Corrupt { path, reason } => write!(f, "{path} is not readable: {reason}"),
            Self::Network(detail) => write!(f, "network error: {detail}"),
            Self::Protocol(detail) => write!(f, "unexpected response: {detail}"),
            Self::Upstream(detail) => write!(f, "the service refused the request: {detail}"),
            Self::SessionExpired(detail) => {
                write!(
                    f,
                    "登录已过期，请重新扫码登录 / session expired; scan again（{detail}）"
                )
            }
            Self::NotLoggedIn(detail) => {
                write!(f, "尚未登录，请先扫码登录 / not signed in（{detail}）")
            }
            Self::InvalidArgument(detail) => write!(f, "invalid argument: {detail}"),
            Self::Cancelled => write!(f, "下载已取消 / download cancelled"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Error {
    /// Wraps an I/O failure with the action that produced it.
    pub fn io(action: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            action: action.into(),
            source,
        }
    }

    /// Stable wire code, shared by the CLI, the HTTP API and the window.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoDataDirectory => "no_data_directory",
            Self::Io { .. } => "io_error",
            Self::Corrupt { .. } => "storage_corrupt",
            Self::Network(_) => "network",
            Self::Protocol(_) => "protocol",
            Self::Upstream(_) => "upstream_rejected",
            Self::SessionExpired(_) => "session_expired",
            Self::NotLoggedIn(_) => "not_logged_in",
            Self::InvalidArgument(_) => "invalid_argument",
            Self::Cancelled => "cancelled",
        }
    }
}
