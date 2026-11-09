// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Platform paths.
//!
//! Everything the window needs from the remote crate is funnelled through here, so a build
//! without remote download is `--no-default-features` rather than a set of `#[cfg]` scattered
//! across the codebase.

use std::path::PathBuf;

/// Application data directory: settings, accounts, sessions.
pub fn config_dir() -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    Some(base.data_dir().join("wxapkg-dump"))
}

/// Directory for regenerable data (scan snapshots, icons).
pub fn cache_dir() -> Option<PathBuf> {
    Some(config_dir()?.join("cache"))
}

/// Directory the native remote store lives in.
pub fn storage_dir() -> Option<PathBuf> {
    Some(config_dir()?.join("remote"))
}

/// Directory for crash and diagnostic logs.
pub fn logs_dir() -> Option<PathBuf> {
    Some(config_dir()?.join("logs"))
}

/// Creates a directory when missing.
pub fn ensure_dir(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Unix seconds.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| i64::try_from(value.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

/// How many accounts the native store remembers.
pub fn cached_accounts() -> Option<usize> {
    #[cfg(feature = "remote")]
    {
        let remote = wxapkg_remote::Remote::open().ok()?;
        remote.account().ok().flatten().map(|_| 1usize)
    }
    #[cfg(not(feature = "remote"))]
    {
        None
    }
}
