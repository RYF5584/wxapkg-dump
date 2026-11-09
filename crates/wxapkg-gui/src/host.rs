// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Host introspection and housekeeping: environment, caches, reveal-in-file-manager.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::AppHandle;

use crate::run_job;

/// Prefix the engine uses for the temp directories it extracts archives into.
const TEMP_PREFIX: &str = "wxapkg-dump-";

/// Root directory that output directories resolve against.
///
/// Mirrors the CLI and the API so all three surfaces write to the same place by default.
pub fn workspace() -> PathBuf {
    if let Some(explicit) = std::env::var_os("WXAPKG_WORKSPACE") {
        return PathBuf::from(explicit);
    }
    directories::UserDirs::new()
        .and_then(|dirs| dirs.document_dir().map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
        .join("wxapkg-dump")
}

/// Resolves a caller-supplied output directory against the workspace.
pub fn resolve_output(requested: Option<&str>, fallback: &str) -> PathBuf {
    match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(explicit) => {
            let path = PathBuf::from(explicit);
            if path.is_absolute() {
                path
            } else {
                workspace().join(path)
            }
        }
        None => workspace().join(fallback),
    }
}

/// One directory the scan walks.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RootInfo {
    /// Absolute path.
    path: String,
    /// Whether the directory is present on this machine.
    exists: bool,
    /// Whether this process may read it.
    readable: bool,
}

/// What the environment panel shows.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    version: &'static str,
    target: String,
    os: String,
    arch: String,
    workspace: String,
    workspace_exists: bool,
    config_dir: Option<String>,
    storage_dir: Option<String>,
    /// Whether the native remote store is present.
    remote_found: bool,
    remote_path: Option<String>,
    node_found: bool,
    cached_accounts: Option<usize>,
    roots: Vec<RootInfo>,
    /// `ok`, `denied` (the system refused a root) or `missing` (no WeChat data found).
    permission: String,
}

/// Reports the toolkit's environment, without walking the whole corpus.
///
/// The program list is the local view's job; this only answers "what am I running on", so it
/// stays instant even on a machine with hundreds of installed Mini Programs.
#[tauri::command]
pub async fn environment() -> Result<Environment, String> {
    let root = workspace();
    let (remote_found, remote_path) = match crate::platform::storage_dir() {
        Some(path) => (path.is_dir(), Some(path.display().to_string())),
        None => (false, None),
    };
    let node_found = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);

    let roots: Vec<RootInfo> = wxapkg_core::discover::default_roots()
        .into_iter()
        .map(|path| {
            let exists = path.is_dir();
            // Do not enumerate here: a protected or network-backed root can block, and the
            // environment panel must paint immediately. A scan reports real read failures.
            let readable = exists;
            RootInfo {
                path: path.display().to_string(),
                exists,
                readable,
            }
        })
        .collect();
    // A denied root is the one condition the user can fix, so it is called out separately
    // from "WeChat was never used on this machine".
    let permission = if roots.iter().any(|root| root.exists && !root.readable) {
        "denied"
    } else if roots.iter().all(|root| !root.exists) {
        "missing"
    } else {
        "ok"
    };

    Ok(Environment {
        version: wxapkg_core::VERSION,
        target: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        workspace_exists: root.is_dir(),
        workspace: root.display().to_string(),
        config_dir: crate::platform::config_dir().map(|path| path.display().to_string()),
        storage_dir: crate::platform::storage_dir().map(|path| path.display().to_string()),
        remote_found,
        remote_path,
        node_found,
        cached_accounts: crate::platform::cached_accounts(),
        roots,
        permission: permission.to_owned(),
    })
}

/// Opens the platform's folder picker and returns the chosen directory.
#[tauri::command]
pub async fn pick_folder(app: AppHandle, title: Option<String>) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt as _;

    let (sender, receiver) = std::sync::mpsc::channel();
    let mut dialog = app.dialog().file();
    if let Some(title) = title.filter(|value| !value.trim().is_empty()) {
        dialog = dialog.set_title(&title);
    }
    dialog.pick_folder(move |choice| {
        let _ = sender.send(choice);
    });

    // The picker runs on the main thread and answers through the channel; waiting here
    // keeps the command async without blocking the window.
    tauri::async_runtime::spawn_blocking(move || receiver.recv().ok().flatten())
        .await
        .map_err(|err| format!("the folder picker failed: {err}"))?
        .map(|path| {
            path.into_path()
                .map(|path| path.display().to_string())
                .map_err(|err| format!("unusable folder choice: {err}"))
        })
        .transpose()
}

/// Opens the operating system's file-access settings, where the user can grant access.
///
/// Returns whether the platform has such a page: on macOS the WeChat container can be
/// protected by Full Disk Access, and this is the page that grants it.
#[tauri::command]
pub async fn open_privacy_settings() -> Result<bool, String> {
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
            .status()
            .map_err(|err| format!("cannot open System Settings: {err}"))?;
        Ok(status.success())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(false)
    }
}

/// One directory the toolkit owns.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    path: String,
    size: u64,
}

/// What the cache panel shows.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheReport {
    workspace: String,
    projects: Vec<CacheEntry>,
    temp_dirs: Vec<CacheEntry>,
    total_bytes: u64,
}

/// Lists the directories an export could have produced, plus the engine's temp directories.
#[tauri::command]
pub async fn cache_report(app: AppHandle) -> Result<CacheReport, String> {
    run_job(&app, "cache report", || {
        let root = workspace();
        let projects = workspace_projects(&root);
        let temp_dirs = temp_directories();
        let total_bytes = projects
            .iter()
            .chain(temp_dirs.iter())
            .map(|e| e.size)
            .sum();
        Ok(CacheReport {
            workspace: root.display().to_string(),
            projects,
            temp_dirs,
            total_bytes,
        })
    })
    .await
}

/// How much a purge removed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeResult {
    removed: usize,
    freed_bytes: u64,
}

/// Deletes the cached directories, never the workspace root and never anything outside it.
#[tauri::command]
pub async fn cache_purge(app: AppHandle, include_temp: bool) -> Result<PurgeResult, String> {
    run_job(&app, "cache purge", move || {
        let dir = workspace();
        let root = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        let temp = std::env::temp_dir();
        let temp_root = std::fs::canonicalize(&temp).unwrap_or(temp);
        let mut targets = workspace_projects(&dir);
        if include_temp {
            targets.extend(temp_directories());
        }

        let mut removed = 0usize;
        let mut freed = 0u64;
        for entry in targets {
            let path = PathBuf::from(&entry.path);
            // Defence in depth: both collectors already only yield directories the toolkit
            // could have made, but re-check before deleting anything.
            if path == root {
                continue;
            }
            let resolved = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let inside_workspace = resolved.starts_with(&root);
            let inside_temp = include_temp && resolved.starts_with(&temp_root);
            if !inside_workspace && !inside_temp {
                continue;
            }
            std::fs::remove_dir_all(&resolved)
                .map_err(|err| format!("cannot delete {}: {err}", resolved.display()))?;
            freed += entry.size;
            removed += 1;
        }
        Ok(PurgeResult {
            removed,
            freed_bytes: freed,
        })
    })
    .await
}

/// Appends a UI-side error to `logs/ui.log`.
///
/// A JS exception never closes a webview, so without this a broken view leaves no trace on
/// disk — the same reason Rust panics get their own log.
#[tauri::command]
pub fn log_js_error(message: String) {
    let Some(dir) = crate::platform::logs_dir() else {
        return;
    };
    let _ = crate::platform::ensure_dir(&dir);
    let stamp = crate::platform::now_unix();
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ui.log"))
    {
        let _ = writeln!(file, "[{stamp}] {message}");
    }
}

/// Opens an https link in the user's default browser.
///
/// Only https is accepted: on Windows the URL is handed to `cmd /C start`, and an unchecked
/// string there would be a command-injection hole.
#[tauri::command]
pub async fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("only https links can be opened".to_owned());
    }
    run_job(&app, "open url", move || {
        open_in_browser(&url).map_err(|err| format!("could not open the browser: {err}"))
    })
    .await
}

fn open_in_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt as _;

        // ShellExecuteW is the OS verb behind "open with the default handler": no console
        // window, no browser picker, works with whichever browser is installed. Declared
        // directly to avoid pulling a Windows crate into this build.
        #[link(name = "shell32")]
        extern "system" {
            fn ShellExecuteW(
                hwnd: *mut core::ffi::c_void,
                operation: *const u16,
                file: *const u16,
                parameters: *const u16,
                directory: *const u16,
                show: i32,
            ) -> *mut core::ffi::c_void;
        }
        const SW_SHOWNORMAL: i32 = 1;

        let wide: Vec<u16> = std::ffi::OsStr::new(url)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let verb: Vec<u16> = "open\0".encode_utf16().collect();
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                wide.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        } as isize;
        if result <= 32 {
            return Err(std::io::Error::other(format!(
                "ShellExecuteW returned {result}"
            )));
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

/// Selects a path in the platform's file manager.
#[tauri::command]
pub async fn reveal(app: AppHandle, path: String) -> Result<(), String> {
    run_job(&app, "reveal", move || {
        let path = PathBuf::from(&path);
        if !path.exists() {
            return Err(format!("{} does not exist", path.display()));
        }
        wxapkg_server::system::reveal_in_file_manager(&path)
            .map_err(|err| format!("the file manager could not be launched: {err}"))
    })
    .await
}

/// Top-level directories of the workspace that look like produced projects.
fn workspace_projects(workspace: &Path) -> Vec<CacheEntry> {
    let Ok(reader) = std::fs::read_dir(workspace) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in reader.flatten() {
        let path = entry.path();
        if !path.is_dir() || !looks_like_project(&path) {
            continue;
        }
        out.push(CacheEntry {
            size: dir_size(&path),
            path: path.display().to_string(),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Whether a directory is something an export produced.
fn looks_like_project(path: &Path) -> bool {
    if is_appid(
        &path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    ) {
        return true;
    }
    ["project", "raw"]
        .iter()
        .any(|name| path.join(name).is_dir())
        || path.join("_unpack_report.json").is_file()
        || path.join("app.json").is_file()
}

/// An appid is `wx` followed by exactly 16 hex digits.
fn is_appid(name: &str) -> bool {
    name.len() == 18 && name.starts_with("wx") && name[2..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Temp directories left behind by archive extraction, identified by the engine's prefix.
fn temp_directories() -> Vec<CacheEntry> {
    let root = std::env::temp_dir();
    let Ok(reader) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out: Vec<CacheEntry> = reader
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .map(|path| CacheEntry {
            size: dir_size(&path),
            path: path.display().to_string(),
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Total size of a directory tree, ignoring entries that cannot be read.
pub fn dir_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(path),
                Ok(kind) if kind.is_file() => {
                    total += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                }
                _ => {}
            }
        }
    }
    total
}

/// Opens the platform file picker (`.wxapkg` and `.wxapkg2` preselected) and returns the
/// chosen file.
#[tauri::command]
pub async fn pick_wxapkg(app: AppHandle, title: Option<String>) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt as _;
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut dialog = app
        .dialog()
        .file()
        .add_filter("wxapkg", &["wxapkg", "wxapkg2"]);
    if let Some(title) = title.filter(|value| !value.trim().is_empty()) {
        dialog = dialog.set_title(&title);
    }
    dialog.pick_file(move |choice| {
        let _ = sender.send(choice);
    });
    tauri::async_runtime::spawn_blocking(move || receiver.recv().ok().flatten())
        .await
        .map_err(|err| format!("the file picker failed: {err}"))?
        .map(|path| {
            path.into_path()
                .map(|path| path.display().to_string())
                .map_err(|err| format!("unusable file choice: {err}"))
        })
        .transpose()
}
