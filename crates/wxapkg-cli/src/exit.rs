// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Process exit codes.
//!
//! The mapping is deliberately distinct per failure class so that scripts and CI can
//! branch on the exit status alone.

/// Exit status returned by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// Everything succeeded.
    Success = 0,
    /// Completed with warnings (`--strict` escalates them).
    Warnings = 1,
    /// The invocation or the input was invalid.
    Usage = 2,
    /// The operation failed at runtime.
    Software = 3,
    /// A required login session is missing or expired.
    Unauthorized = 4,
    /// An upstream service rejected the request.
    Upstream = 5,
}

impl ExitCode {
    /// Maps a stable error code onto an exit status.
    pub fn from_error_code(code: &str) -> Self {
        match code {
            "invalid_argument"
            | "invalid_container"
            | "missing_decryption_key"
            | "program_not_found" => Self::Usage,
            "not_logged_in" | "session_expired" => Self::Unauthorized,
            "upstream_rejected" | "upstream_unavailable" => Self::Upstream,
            _ => Self::Software,
        }
    }

    /// Numeric status.
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

impl From<ExitCode> for i32 {
    fn from(value: ExitCode) -> Self {
        value.as_i32()
    }
}
