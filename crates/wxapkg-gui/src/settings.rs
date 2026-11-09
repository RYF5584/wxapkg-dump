// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Persisted preferences.
//!
//! One small JSON file next to the account cache. Everything has a default, so a missing or
//! damaged file is never fatal: the application starts with the defaults and rewrites it on
//! the next save.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::host;

/// User preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Directory exports default to. Empty means "ask every time".
    pub default_output: String,
    /// Pre-select the beautify toggle for new exports.
    pub beautify: bool,
    /// Pre-select the verify toggle for new exports.
    pub verify: bool,
    /// `system`, `light` or `dark`; mirrored in the window for first paint.
    pub theme: String,
    /// Parallel package downloads for remote fetches (1-10).
    pub concurrency: u32,
    /// Width in pixels of each resizable table column, keyed by column name.
    pub columns: std::collections::BTreeMap<String, u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_output: host::workspace().display().to_string(),
            beautify: true,
            verify: true,
            theme: "system".to_owned(),
            concurrency: 3,
            columns: std::collections::BTreeMap::new(),
        }
    }
}

impl Settings {
    /// Path of the settings file.
    pub fn path() -> Result<PathBuf, String> {
        let dir =
            crate::platform::config_dir().ok_or_else(|| "no configuration directory".to_owned())?;
        crate::platform::ensure_dir(&dir).map_err(|err| err.to_string())?;
        Ok(dir.join("settings.json"))
    }

    /// Reads the file, falling back to defaults for anything missing or malformed.
    pub fn load() -> Self {
        let Ok(path) = Self::path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Writes the file atomically enough for a preferences file.
    pub fn save(&self) -> Result<(), String> {
        let path = Self::path()?;
        let text = serde_json::to_string_pretty(self).map_err(|err| err.to_string())?;
        std::fs::write(&path, text).map_err(|err| format!("cannot write {}: {err}", path.display()))
    }
}

/// Returns the current preferences.
#[tauri::command]
pub async fn get_settings(app: AppHandle) -> Result<Settings, String> {
    crate::run_job(&app, "settings", || Ok(Settings::load())).await
}

/// Stores the preferences and returns what was written.
#[tauri::command]
pub async fn save_settings(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    crate::run_job(&app, "settings save", move || {
        settings.save()?;
        Ok(settings)
    })
    .await
}
