// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Uniform output handling shared by every subcommand.
//!
//! Two renderings exist for every result:
//!
//! * **human** — aligned tables and colour, meant for a terminal;
//! * **json** — a stable envelope (`{"ok":true,"command":...,"data":...}`), meant for
//!   scripts and for AI agents that drive the CLI.
//!
//! Errors always use the envelope with `ok: false`, so a caller can parse the same
//! shape regardless of the exit path.

use serde::Serialize;

use crate::exit::ExitCode;

/// JSON envelope returned on success.
#[derive(Debug, Serialize)]
pub struct Success<'a, T: Serialize> {
    /// Always `true`.
    pub ok: bool,
    /// Subcommand that produced the payload.
    pub command: &'a str,
    /// Toolkit version.
    pub version: &'a str,
    /// Command-specific payload.
    pub data: T,
}

/// JSON envelope returned on failure.
#[derive(Debug, Serialize)]
pub struct Failure<'a> {
    /// Always `false`.
    pub ok: bool,
    /// Subcommand that failed.
    pub command: &'a str,
    /// Toolkit version.
    pub version: &'a str,
    /// Structured error.
    pub error: ErrorBody,
}

/// Structured error payload.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    /// Stable machine readable code.
    pub code: String,
    /// Human readable message.
    pub message: String,
    /// Optional remediation hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Emits a success result, either as JSON or through the supplied human renderer.
pub fn emit_success<T, F>(command: &str, json: bool, data: T, human: F) -> ExitCode
where
    T: Serialize,
    F: FnOnce(&T),
{
    if json {
        let envelope = Success {
            ok: true,
            command,
            version: wxapkg_core::VERSION,
            data,
        };
        match serde_json::to_string_pretty(&envelope) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("failed to serialise result: {err}");
                return ExitCode::Software;
            }
        }
    } else {
        human(&data);
    }
    ExitCode::Success
}

/// Emits a failure result and returns the matching exit code.
pub fn emit_failure(
    command: &str,
    json: bool,
    code: &str,
    message: &str,
    hint: Option<&str>,
) -> ExitCode {
    if json {
        let envelope = Failure {
            ok: false,
            command,
            version: wxapkg_core::VERSION,
            error: ErrorBody {
                code: code.to_owned(),
                message: message.to_owned(),
                hint: hint.map(str::to_owned),
            },
        };
        if let Ok(text) = serde_json::to_string_pretty(&envelope) {
            println!("{text}");
        }
    } else {
        // Failures deserve attention: bold red for the message, dim for the hint.
        eprintln!(
            "{}",
            console::style(format!("error: {message}")).red().bold()
        );
        if let Some(hint) = hint {
            eprintln!("{}", console::style(format!("hint:  {hint}")).dim());
        }
    }
    ExitCode::from_error_code(code)
}

/// Renders a byte count using binary units.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// Truncates `text` in the middle so the tail stays visible.
pub fn ellipsize(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_owned();
    }
    let head = max.saturating_sub(3) / 2;
    let tail = max.saturating_sub(3) - head;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}…{end}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_units_are_binary() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(1024), "1.0KiB");
        assert_eq!(human_bytes(1024 * 1024 * 3 / 2), "1.5MiB");
    }

    #[test]
    fn ellipsize_keeps_the_tail() {
        assert_eq!(ellipsize("abcdef", 10), "abcdef");
        assert!(ellipsize("/a/very/long/path/that/keeps/going", 16).ends_with("going"));
    }
}
