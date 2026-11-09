// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Output path rules.
//!
//! Container entry names are not file system paths. They are package-absolute names that
//! can contain characters Windows forbids — `plugin-private://wx…/components/x` puts a
//! `:` in the first segment — and names Windows reserves (`con`, `nul`, `com1`, …).
//!
//! The rules below therefore run on every platform, not only on Windows: one container
//! must restore to the same tree on macOS, Linux and Windows, otherwise a project built
//! on one machine cannot be copied to another without breaking its include paths.
//!
//! Two more things happen here: `..` is neutralised so a hostile container cannot write
//! outside the output directory, and Windows paths are given the verbatim (`\\?\`) prefix
//! so an appid deep enough to pass `MAX_PATH` still restores.

use std::path::{Path, PathBuf};

/// Characters Windows rejects in a file name.
const FORBIDDEN: [char; 7] = ['<', '>', ':', '"', '|', '?', '*'];

/// Device names that cannot be used as a file name on Windows, with or without extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Makes one path segment safe to create on any of the three platforms.
///
/// The result is never empty, never ends in a `.` or a space (Windows silently strips
/// those, which would make two entries collide), and is not a reserved device name.
pub fn sanitize_segment(segment: &str) -> String {
    let mut name: String = segment
        .chars()
        .map(|ch| {
            if ch.is_control() || FORBIDDEN.contains(&ch) {
                '_'
            } else {
                ch
            }
        })
        .collect();

    while name.ends_with('.') || name.ends_with(' ') {
        name.pop();
    }
    if name.is_empty() {
        return "_".to_owned();
    }

    let stem = name.split('.').next().unwrap_or(&name);
    if RESERVED
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        name.insert(0, '_');
    }
    name
}

/// Turns a container entry name into a relative path with `/` separators.
///
/// Empty and `.` segments are dropped because entry names routinely begin with `/` or
/// contain `//`; `..` becomes `_` so nothing can escape the output directory. Returns an
/// empty string when no segment survives.
pub fn sanitize_relative(rel: &str) -> String {
    rel.replace('\\', "/")
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .map(sanitize_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Resolves a relative output path under `out`, with the platform's own separator and,
/// on Windows, the verbatim prefix that lifts the `MAX_PATH` limit.
pub fn native(out: &Path, rel: &str) -> PathBuf {
    let rel = sanitize_relative(rel);
    let joined = out.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    verbatim(joined)
}

#[cfg(windows)]
fn verbatim(path: PathBuf) -> PathBuf {
    use std::path::Prefix;

    let path = if path.is_absolute() {
        path
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => return path,
        }
    };
    // Verbatim paths do not accept `/` as a separator, and callers may have built the
    // base path with forward slashes (for example from a JSON request), so normalise.
    let text = path.to_string_lossy().replace('/', "\\");

    // Already verbatim, or a device path we must not rewrite.
    if text.starts_with(r"\\?\") || text.starts_with(r"\\.\") {
        return path;
    }
    if let Some(rest) = text.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{rest}"));
    }
    match path.components().next() {
        Some(std::path::Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_)) => {
            PathBuf::from(format!(r"\\?\{text}"))
        }
        _ => path,
    }
}

#[cfg(not(windows))]
fn verbatim(path: PathBuf) -> PathBuf {
    path
}

/// Renders a path for people: `canonicalize` adds the Windows verbatim prefix, which is
/// noise in API responses and log lines.
pub fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return rest.to_owned();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real entry name from a WeChat container: the scheme survives the split as
    // `plugin-private:`, which no Windows file system will accept.
    #[test]
    fn scheme_prefix_loses_its_colon() {
        assert_eq!(
            sanitize_relative("plugin-private://wx0000000000000003/components/playcanvas/index.js"),
            "plugin-private_/wx0000000000000003/components/playcanvas/index.js"
        );
    }

    #[test]
    fn traversal_cannot_escape_the_output_directory() {
        assert_eq!(sanitize_relative("../../etc/passwd"), "_/_/etc/passwd");
        assert_eq!(sanitize_relative("a/../../b"), "a/_/_/b");
    }

    #[test]
    fn windows_reserved_names_are_prefixed() {
        assert_eq!(sanitize_segment("con"), "_con");
        assert_eq!(sanitize_segment("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_segment("Lpt9"), "_Lpt9");
        assert_eq!(sanitize_segment("console"), "console");
    }

    #[test]
    fn trailing_dots_and_spaces_are_removed() {
        assert_eq!(sanitize_segment("name."), "name");
        assert_eq!(sanitize_segment("name  "), "name");
        assert_eq!(sanitize_segment("..."), "_");
    }

    #[test]
    fn forbidden_characters_become_underscores() {
        assert_eq!(sanitize_segment("a<b>c:d\"e|f?g*h"), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize_segment("tab\there"), "tab_here");
    }

    #[test]
    fn separator_flavours_are_normalised() {
        assert_eq!(
            sanitize_relative("\\wx1\\pages\\index.js"),
            "wx1/pages/index.js"
        );
        assert_eq!(sanitize_relative("/app-config.json"), "app-config.json");
        assert_eq!(sanitize_relative("a//b"), "a/b");
        assert_eq!(sanitize_relative(""), "");
    }

    #[test]
    fn native_joins_under_the_output_directory() {
        let out = Path::new("/tmp/out");
        let joined = native(out, "plugin-private://wx1/a.js");
        // Windows makes the path absolute and adds a verbatim prefix; compare the tail
        // with one separator flavour so the assertion is platform-neutral.
        let normalised = display_path(&joined).replace('\\', "/");
        assert!(
            normalised.ends_with("out/plugin-private_/wx1/a.js"),
            "{normalised}"
        );
    }
}
