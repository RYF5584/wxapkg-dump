// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Server-wide state shared by every handler.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::events::EventBus;
use crate::inventory::ProgramsCache;

/// Default workspace used when a request omits an output directory.
pub fn default_workspace() -> PathBuf {
    if let Some(explicit) = std::env::var_os("WXAPKG_WORKSPACE") {
        return PathBuf::from(explicit);
    }
    directories::UserDirs::new()
        .and_then(|dirs| dirs.document_dir().map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
        .join("wxapkg-dump")
}

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    /// Token required in `x-admin-key` when the server is exposed remotely.
    pub admin_key: Option<String>,
    /// Root directory that output directories resolve against.
    pub workspace: PathBuf,
    /// Server-sent event bus feeding a front-end's log drawer.
    pub events: EventBus,
    /// Unix timestamp of the process start.
    pub started_at: i64,
    /// Pending login sessions, keyed by the token handed to the client.
    pub logins: Arc<Mutex<HashMap<String, LoginSession>>>,
    /// Last Mini Program listing, reused for a few seconds.
    pub programs_cache: Arc<Mutex<ProgramsCache>>,
}

/// A login attempt that is currently being polled by a client.
#[derive(Debug, Clone)]
pub struct LoginSession {
    /// Account identifier being bound.
    /// Unix timestamp of creation.
    pub created_at: i64,
}

impl AppState {
    /// Builds the state, creating the workspace directory.
    pub fn new(admin_key: Option<String>, workspace: Option<PathBuf>) -> anyhow::Result<Self> {
        let workspace = workspace.unwrap_or_else(default_workspace);
        std::fs::create_dir_all(&workspace).map_err(|err| {
            anyhow::anyhow!("cannot create the workspace {}: {err}", workspace.display())
        })?;

        let state = Self {
            admin_key: admin_key.filter(|key| !key.is_empty()),
            workspace,
            events: EventBus::new(),
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            logins: Arc::new(Mutex::new(HashMap::new())),
            programs_cache: Arc::new(Mutex::new(ProgramsCache::default())),
        };
        state
            .events
            .publish("info", format!("workspace: {}", state.workspace.display()));
        Ok(state)
    }

    /// Resolves a caller-supplied input path against the workspace.
    ///
    /// Relative paths are anchored at the workspace; absolute paths are honoured as-is,
    /// matching [`Self::resolve_output`].
    pub fn resolve_input(&self, requested: &str) -> PathBuf {
        let path = PathBuf::from(requested.trim());
        if path.is_absolute() {
            path
        } else {
            self.workspace.join(path)
        }
    }

    /// Resolves a caller-supplied output directory against the workspace.
    ///
    /// Relative paths stay inside the workspace; absolute paths are honoured as-is so
    /// users keep full control over where their files land.
    pub fn resolve_output(&self, requested: Option<&str>, fallback_name: &str) -> PathBuf {
        match requested.map(str::trim).filter(|value| !value.is_empty()) {
            Some(explicit) => {
                let path = PathBuf::from(explicit);
                if path.is_absolute() {
                    path
                } else {
                    self.workspace.join(path)
                }
            }
            None => self.workspace.join(fallback_name),
        }
    }
}
