// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Path and identifier safety for caller-supplied input.
//!
//! The API hands out read and write access to the user's own machine, so every path that
//! arrives from a request is funnelled through [`ensure_inside`] before it is touched.
//! Canonicalisation is what makes the check sound: it collapses `..` and resolves symlinks,
//! so a path that *textually* looks contained but escapes through a link is still caught.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Canonicalises `path`, tolerating a missing leaf.
///
/// Output directories usually do not exist yet, so canonicalising the nearest existing
/// ancestor and re-appending the tail is the only way to validate them.
fn resolve_allow_missing(path: &Path) -> Option<PathBuf> {
    let mut tail: Vec<OsString> = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(base) = std::fs::canonicalize(cursor) {
            let mut out = base;
            for piece in tail.iter().rev() {
                out.push(piece);
            }
            return Some(out);
        }
        tail.push(cursor.file_name()?.to_os_string());
        cursor = cursor.parent()?;
    }
}

/// Resolves `requested` against `workspace` and refuses anything that escapes it.
///
/// Relative paths are anchored at the workspace root; absolute paths are accepted only
/// when they already live inside it, so `..` and symlink tricks cannot be used to read or
/// delete files the caller was never meant to reach.
pub fn ensure_inside(workspace: &Path, requested: &str) -> Result<PathBuf, String> {
    let trimmed = requested.trim();
    if trimmed.is_empty() {
        return Err("the path is empty".to_owned());
    }
    if trimmed.contains('\0') {
        return Err("the path contains a NUL byte".to_owned());
    }

    let raw = Path::new(trimmed);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        workspace.join(raw)
    };

    let root = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let resolved = resolve_allow_missing(&candidate)
        .ok_or_else(|| format!("cannot resolve {}", candidate.display()))?;
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(format!(
            "{} is outside the workspace {}",
            resolved.display(),
            root.display()
        ))
    }
}

/// Canonicalises a path that the caller explicitly opted out of the workspace guard for.
pub fn resolve_anywhere(requested: &str) -> Result<PathBuf, String> {
    let trimmed = requested.trim();
    if trimmed.is_empty() {
        return Err("the path is empty".to_owned());
    }
    if trimmed.contains('\0') {
        return Err("the path contains a NUL byte".to_owned());
    }
    resolve_allow_missing(Path::new(trimmed)).ok_or_else(|| format!("cannot resolve {trimmed}"))
}

/// Rejects a single path segment that could traverse out of its parent.
///
/// Used for the `{appid}` and `{name}` route parameters, which reach the filesystem or a
/// container index before any canonicalisation can run.
pub fn ensure_segment(value: &str) -> Result<&str, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("the value is empty".to_owned());
    }
    if trimmed.contains("..") || trimmed.contains('/') || trimmed.contains('\\') {
        return Err(format!("{value:?} is not a valid identifier"));
    }
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_stay_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = ensure_inside(dir.path(), "a/b").unwrap();
        assert!(resolved.starts_with(std::fs::canonicalize(dir.path()).unwrap()));
    }

    #[test]
    fn dot_dot_escapes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ensure_inside(dir.path(), "../../etc/passwd").is_err());
        assert!(ensure_inside(dir.path(), "a/../../../../etc/passwd").is_err());
    }

    #[test]
    fn absolute_paths_outside_the_workspace_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ensure_inside(dir.path(), "/etc/passwd").is_err());
    }

    #[test]
    fn segments_cannot_traverse() {
        assert!(ensure_segment("../../etc/passwd").is_err());
        assert!(ensure_segment("a/b").is_err());
        assert!(ensure_segment("wx069ba97219f66d99").is_ok());
    }
}
