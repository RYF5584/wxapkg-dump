// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Host introspection and housekeeping: environment, caches, reveal-in-file-manager.
//!
//! A caller driving this API from another tool needs to know what this machine looks like
//! before it asks for anything expensive — which directories were searched, whether the
//! App Store bridge is loadable, and how much disk the toolkit has already filled.

use std::path::{Path, PathBuf};

use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use wxapkg_core::discover;

use crate::api::{failure, internal_error, remote_paths, remote_status, success, PROTOCOL};
use crate::guard;
use crate::state::AppState;
use crate::util;

/// Prefix the engine uses for the temp directories it extracts archives into.
const TEMP_PREFIX: &str = "wxapkg-dump-";

/// Routes owned by this module.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/version", get(version))
        .route("/environment", get(environment))
        .route("/cache", get(cache_report).delete(cache_purge))
        .route("/reveal", post(reveal))
}

// ---------------------------------------------------------------------------
// version
// ---------------------------------------------------------------------------

/// `GET /api/version`
async fn version() -> Response {
    success(
        "version",
        json!({ "version": wxapkg_core::VERSION, "protocol": PROTOCOL }),
    )
}

// ---------------------------------------------------------------------------
// environment
// ---------------------------------------------------------------------------

/// `GET /api/environment`
async fn environment(State(state): State<AppState>) -> Response {
    let (storage, config) = remote_paths();
    let (remote_found, remote_path) = remote_status();
    let roots = discover::default_roots();

    // This endpoint must stay cheap: it is called on every window open. Counting programs
    // belongs to `/api/programs`, which caches a bounded walk for exactly that purpose.
    let search_roots: Vec<Value> = roots
        .iter()
        .map(|root| {
            // `read_dir` can block on a TCC-protected or network-backed directory. The
            // environment probe only needs a cheap stat; a scan reports real read failures.
            let exists = root.is_dir();
            json!({
                "path": root.display().to_string(),
                "exists": exists,
                "readable": exists,
            })
        })
        .collect();

    state.events.publish("info", "reporting the environment");

    success(
        "environment",
        json!({
            "version": wxapkg_core::VERSION,
            "protocol": PROTOCOL,
            "target": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "family": std::env::consts::FAMILY,
            "remoteFound": remote_found,
            "remotePath": remote_path,
            "configDir": config,
            "storageDir": storage,
            "workspace": state.workspace.display().to_string(),
            "workspaceExists": state.workspace.is_dir(),
            "searchRoots": search_roots,
            "startedAt": state.started_at,
            "uptimeSeconds": util::now() - state.started_at,
            "cachedAccounts": cached_accounts(),
            // Reporting free space portably needs a filesystem-stats crate, which this
            // build deliberately does not carry.
            "diskSpace": Value::Null,
            "notes": [
                "disk space is not reported: it cannot be read portably without an extra dependency"
            ],
        }),
    )
}

#[cfg(feature = "remote")]
fn cached_accounts() -> Option<usize> {
    Some(
        wxapkg_remote::Remote::open()
            .ok()
            .and_then(|remote| remote.account().ok().flatten().map(|_| 1usize))
            .unwrap_or(0),
    )
}

#[cfg(not(feature = "remote"))]
fn cached_accounts() -> Option<usize> {
    None
}

// ---------------------------------------------------------------------------
// cache
// ---------------------------------------------------------------------------

/// `GET /api/cache`
async fn cache_report(State(state): State<AppState>) -> Response {
    let workspace = state.workspace.clone();
    let result = tokio::task::spawn_blocking(move || {
        let projects = workspace_projects(&workspace);
        let temp = temp_directories();
        let project_bytes: u64 = projects.iter().map(|entry| entry.size).sum();
        let temp_bytes: u64 = temp.iter().map(|entry| entry.size).sum();
        (projects, temp, project_bytes, temp_bytes)
    })
    .await;

    match result {
        Ok((projects, temp, project_bytes, temp_bytes)) => {
            state.events.publish(
                "info",
                format!(
                    "{} project(s) and {} temp director(ies) cached",
                    projects.len(),
                    temp.len()
                ),
            );
            success(
                "cache",
                json!({
                    "workspace": state.workspace.display().to_string(),
                    "projects": projects.iter().map(CacheEntry::to_json).collect::<Vec<_>>(),
                    "tempDirs": temp.iter().map(CacheEntry::to_json).collect::<Vec<_>>(),
                    "projectCount": projects.len(),
                    "tempCount": temp.len(),
                    "projectBytes": project_bytes,
                    "tempBytes": temp_bytes,
                    "totalBytes": project_bytes + temp_bytes,
                }),
            )
        }
        Err(err) => internal_error("cache", &err.to_string()),
    }
}

/// `DELETE /api/cache?scope=projects|all&confirm=true`
async fn cache_purge(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let pairs = util::parse_query(raw.as_deref().unwrap_or_default());
    let scope = util::query_value(&pairs, "scope")
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let confirm = util::query_flag(&pairs, "confirm", false);

    match scope.as_str() {
        "projects" => {}
        "all" if confirm => {}
        "all" => {
            return failure(
                "cache purge",
                "confirmation_required",
                "scope=all also deletes transient directories and must be confirmed",
                Some("repeat the request with ?confirm=true"),
                StatusCode::BAD_REQUEST,
            )
        }
        other => {
            return failure(
                "cache purge",
                "invalid_argument",
                &format!("unknown scope {other:?}"),
                Some("use scope=projects or scope=all&confirm=true"),
                StatusCode::BAD_REQUEST,
            )
        }
    }

    let include_temp = scope == "all";
    let workspace = state.workspace.clone();
    state
        .events
        .publish("info", format!("purging the cache (scope={scope})"));

    let result = tokio::task::spawn_blocking(move || purge(&workspace, include_temp)).await;
    match result {
        Ok(Ok((removed, freed))) => {
            state.events.publish(
                "info",
                format!("removed {} director(ies), {freed} bytes", removed.len()),
            );
            success(
                "cache purge",
                json!({
                    "scope": scope,
                    "removed": removed.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
                    "removedCount": removed.len(),
                    "freedBytes": freed,
                }),
            )
        }
        Ok(Err(message)) => failure(
            "cache purge",
            "io_error",
            &message,
            None,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        Err(err) => internal_error("cache purge", &err.to_string()),
    }
}

/// One directory the server owns, with everything `/api/cache` needs to describe it.
struct CacheEntry {
    path: PathBuf,
    size: u64,
    modified_at: Option<i64>,
}

impl CacheEntry {
    fn to_json(&self) -> Value {
        json!({
            "path": self.path.display().to_string(),
            "size": self.size,
            "modifiedAt": self.modified_at,
        })
    }
}

/// Top-level directories of the workspace that look like produced projects.
///
/// The workspace belongs to the user, so only directories this server's own exports could
/// have created are considered cache; anything else is left strictly alone.
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
            size: util::dir_size(&path),
            modified_at: util::modified_at(&path),
            path,
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
            .map(|n| n.to_string_lossy().into_owned())
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
            size: util::dir_size(&path),
            modified_at: util::modified_at(&path),
            path,
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Deletes the cached directories, never the workspace root and never anything outside it.
fn purge(workspace: &Path, include_temp: bool) -> Result<(Vec<PathBuf>, u64), String> {
    let root = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let temp = std::env::temp_dir();
    let temp_root = std::fs::canonicalize(&temp).unwrap_or(temp);
    let mut targets: Vec<CacheEntry> = workspace_projects(workspace);
    if include_temp {
        targets.extend(temp_directories());
    }

    let mut removed = Vec::new();
    let mut freed = 0u64;
    for entry in targets {
        // Defence in depth: `workspace_projects` and `temp_directories` already only yield
        // directories this server could have made, but re-check before deleting anything.
        if entry.path == root {
            continue;
        }
        let resolved = std::fs::canonicalize(&entry.path).unwrap_or_else(|_| entry.path.clone());
        let inside_workspace = resolved.starts_with(&root);
        let inside_temp = include_temp && resolved.starts_with(&temp_root);
        if !inside_workspace && !inside_temp {
            continue;
        }
        std::fs::remove_dir_all(&resolved)
            .map_err(|err| format!("cannot delete {}: {err}", resolved.display()))?;
        freed += entry.size;
        removed.push(resolved);
    }
    Ok((removed, freed))
}

// ---------------------------------------------------------------------------
// reveal
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevealBody {
    path: String,
}

/// `POST /api/reveal`
async fn reveal(State(state): State<AppState>, Json(body): Json<RevealBody>) -> Response {
    let path = match guard::ensure_inside(&state.workspace, &body.path) {
        Ok(path) => path,
        Err(message) => {
            return failure(
                "reveal",
                "forbidden",
                &message,
                Some("only paths inside the workspace can be revealed"),
                StatusCode::FORBIDDEN,
            )
        }
    };
    if !path.exists() {
        return failure(
            "reveal",
            "not_found",
            &format!("{} does not exist", path.display()),
            None,
            StatusCode::NOT_FOUND,
        );
    }

    state
        .events
        .publish("info", format!("revealing {}", path.display()));

    let target = path.clone();
    match tokio::task::spawn_blocking(move || reveal_in_file_manager(&target)).await {
        Ok(Ok(())) => success(
            "reveal",
            json!({ "revealed": wxapkg_core::paths::display_path(&path) }),
        ),
        Ok(Err(err)) => failure(
            "reveal",
            "io_error",
            &format!("the file manager could not be launched: {err}"),
            None,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        Err(err) => internal_error("reveal", &err.to_string()),
    }
}

/// Runs the platform command that reveals `path` in the graphical file manager.
///
/// Windows needs its own handling: `explorer.exe` returns exit code 1 even when it opened
/// the window, so its status cannot be used as a success signal, and the `/select,` verb
/// wants the path quoted because the split happens inside Explorer itself.
pub fn reveal_in_file_manager(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!("open exited with {status}")));
        }
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        let mut command = std::process::Command::new("explorer.exe");
        if path.is_dir() {
            command.arg(path);
        } else {
            command.arg(format!("/select,\"{}\"", path.display()));
        }
        // Spawn only: Explorer keeps running and its exit code means nothing.
        command.spawn()?;
        Ok(())
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // Desktop Linux has no "select this file" verb, so open the containing directory.
        let parent = path.parent().unwrap_or(path);
        let status = std::process::Command::new("xdg-open")
            .arg(parent)
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "xdg-open exited with {status}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appid_detection_is_strict() {
        assert!(is_appid("wx0000000000000001"));
        assert!(!is_appid("wx000000000000001"));
        assert!(!is_appid("wy069ba97219f66d99"));
        assert!(!is_appid("project"));
    }

    #[test]
    fn projects_are_recognised_by_shape_or_name() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().join("wx0000000000000001");
        std::fs::create_dir_all(&named).unwrap();
        assert!(looks_like_project(&named));

        let shaped = dir.path().join("scratch");
        std::fs::create_dir_all(shaped.join("project")).unwrap();
        assert!(looks_like_project(&shaped));

        let unrelated = dir.path().join("notes");
        std::fs::create_dir_all(&unrelated).unwrap();
        assert!(!looks_like_project(&unrelated));
    }

    #[test]
    fn purge_only_removes_directories_it_created() {
        let workspace = tempfile::tempdir().unwrap();
        let project = workspace
            .path()
            .join("wx0000000000000001")
            .join("1539")
            .join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("app.json"), "{}").unwrap();
        let user_dir = workspace.path().join("notes");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(user_dir.join("keep.txt"), "keep me").unwrap();

        let (removed, freed) = purge(workspace.path(), false).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(freed > 0);
        assert!(!workspace.path().join("wx0000000000000001").exists());
        assert!(user_dir.join("keep.txt").is_file());
        // The workspace root always survives.
        assert!(workspace.path().is_dir());
    }
}
