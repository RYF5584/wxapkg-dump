// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Enrichment cache: names, avatars and icons of recently used Mini Programs.
//!
//! The usage record carries only original ids. Names and icons come one attribute-sync call
//! per program, so they are resolved once, cached on disk and served from memory afterwards.
//! The file layout matches the one the legacy store used, so an existing cache imports
//! unchanged.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::store::Store;
use crate::wxa::Wxa;

/// One cached program profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProgramMeta {
    /// App id, filled once any call learned it.
    #[serde(default)]
    pub appid: String,
    /// Display name.
    #[serde(default)]
    pub nick_name: String,
    /// Avatar URL.
    #[serde(default)]
    pub avatar: String,
    /// Cached icon file, when the bytes were fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_path: Option<PathBuf>,
    /// Unix seconds of the last refresh.
    #[serde(default)]
    pub fetched_at: i64,
}

impl ProgramMeta {
    /// Whether the record describes a program fully enough to display.
    pub fn is_complete(&self) -> bool {
        !self.nick_name.is_empty() && !self.avatar.is_empty() && !self.appid.is_empty()
    }
}

/// Disk-backed cache under the store's root.
pub struct MetaCache {
    path: PathBuf,
    icons: PathBuf,
    entries: HashMap<String, ProgramMeta>,
}

impl MetaCache {
    /// Loads (or starts) the cache inside `store`.
    pub fn open(store: &Store) -> Self {
        let path = store.root().join("metadata.json");
        let icons = store.root().join("icons");
        let _ = std::fs::create_dir_all(&icons);
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            icons,
            entries,
        }
    }

    /// The cached record of one program, if any.
    pub fn get(&self, username: &str) -> Option<&ProgramMeta> {
        self.entries.get(username)
    }

    /// Every cached record, for bulk display.
    pub fn all(&self) -> &HashMap<String, ProgramMeta> {
        &self.entries
    }

    /// Inserts or replaces one record and persists.
    pub fn put(&mut self, username: &str, meta: ProgramMeta) {
        self.entries.insert(username.to_owned(), meta);
        self.save();
    }

    /// Refreshes entries that are missing parts or a week old, bounded by `budget`.
    ///
    /// Each name costs one attribute-sync call plus one icon fetch, so the work is spread
    /// over a few threads: a 30-entry list that took half a minute now finishes in a couple
    /// of seconds. Names the budget cut short arrive on the next call.
    pub fn enrich(&mut self, wxa: &Wxa, usernames: &[String], budget: usize) {
        let now = crate::store::now();
        let week = 7 * 24 * 3600;
        let pending: Vec<String> = usernames
            .iter()
            .filter(|name| !name.is_empty())
            .filter(|name| match self.entries.get(*name) {
                None => true,
                Some(meta) => !meta.is_complete() || now - meta.fetched_at > week,
            })
            .take(budget)
            .cloned()
            .collect();
        if pending.is_empty() {
            return;
        }

        let previous: HashMap<String, ProgramMeta> = pending
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    self.entries.get(name).cloned().unwrap_or_default(),
                )
            })
            .collect();
        let icons = self.icons.clone();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results: std::sync::Mutex<Vec<(String, ProgramMeta)>> =
            std::sync::Mutex::new(Vec::new());
        let workers = pending.len().min(ENRICH_WORKERS);

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(username) = pending.get(index) else {
                        break;
                    };
                    let Ok(info) = wxa.info("", username) else {
                        continue;
                    };
                    let before = previous.get(username).cloned().unwrap_or_default();
                    let mut meta = ProgramMeta {
                        appid: if info.appid.is_empty() {
                            before.appid
                        } else {
                            info.appid
                        },
                        nick_name: info.nick_name,
                        avatar: info.avatar,
                        icon_path: before.icon_path,
                        fetched_at: crate::store::now(),
                    };
                    if !meta.avatar.is_empty() {
                        if let Some(path) = fetch_icon_in(&icons, &meta.avatar) {
                            meta.icon_path = Some(path);
                        }
                    }
                    results
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push((username.clone(), meta));
                });
            }
        });

        for (username, meta) in results.into_inner().unwrap_or_default() {
            self.entries.insert(username, meta);
        }
        self.save();
    }

    /// Persists the map; a failure only costs the next call a refresh.
    fn save(&self) {
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.entries) {
            let _ = std::fs::write(&self.path, bytes);
        }
    }
}

/// How many metadata lookups run at once. Six keeps the service calm while turning a
/// sequential 30-entry refresh into a couple of seconds.
const ENRICH_WORKERS: usize = 6;

/// Downloads an avatar once, keyed by its address.
fn fetch_icon_in(icons: &std::path::Path, url: &str) -> Option<PathBuf> {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(url.as_bytes());
    let name = hex(&hasher.finalize());
    let path = icons.join(format!("{name}.bin"));
    if path.is_file() {
        return Some(path);
    }
    let mut bytes = Vec::new();
    use std::io::Read as _;
    crate::oauth::agent(std::time::Duration::from_secs(20))
        .ok()?
        .get(url)
        .call()
        .ok()?
        .into_reader()
        .read_to_end(&mut bytes)
        .ok()?;
    std::fs::write(&path, bytes).ok()?;
    Some(path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let inner = Store::at(dir.path().join("remote")).unwrap();
        (dir, inner)
    }

    #[test]
    fn records_round_trip_through_the_file() {
        let (_dir, store) = store();
        let mut cache = MetaCache::open(&store);
        assert!(cache.get("gh_x@app").is_none());
        cache.put(
            "gh_x@app",
            ProgramMeta {
                appid: "wx1".to_owned(),
                nick_name: "Name".to_owned(),
                avatar: "https://a/1".to_owned(),
                icon_path: None,
                fetched_at: 5,
            },
        );

        let reopened = MetaCache::open(&store);
        let meta = reopened.get("gh_x@app").unwrap();
        assert_eq!(meta.nick_name, "Name");
        assert_eq!(meta.appid, "wx1");
        assert!(meta.is_complete());
    }

    #[test]
    fn an_incomplete_record_is_flagged() {
        let meta = ProgramMeta {
            nick_name: "only a name".to_owned(),
            ..ProgramMeta::default()
        };
        assert!(!meta.is_complete());
    }
}
