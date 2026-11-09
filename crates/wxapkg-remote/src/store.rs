// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Where remote state lives on disk.
//!
//! One directory under the platform's application-data root, one JSON file per concern,
//! `0600` permissions (sessions are credentials), atomic replacement so a crash cannot
//! truncate a write. Exactly one account is remembered: `account.json` holds it, a new
//! login overwrites it, and clearing the account also wipes the caches tied to it.
//!
//! ```text
//! <data dir>/wxapkg-dump/remote/
//!   account.json         the signed-in account, schema-versioned (or absent)
//!   pending/<uuid>.json  logins waiting for a scan, resumable after a restart
//!   metadata.json        Mini Program names / icon URLs, keyed by original id
//!   icons/<sha256>.bin   cached launcher icons
//! ```
//!
//! `WXAPKG_REMOTE_HOME` overrides the directory entirely, which tests and portable installs
//! rely on.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::account::Account;
use crate::error::{Error, Result};
use crate::oauth::LoginSession;

/// Environment variable that relocates the whole remote state directory.
pub const HOME_ENV: &str = "WXAPKG_REMOTE_HOME";

/// Schema version of `account.json`; bump only with a migration.
const SCHEMA: u32 = 2;

/// The account file.
#[derive(Debug, Serialize, Deserialize)]
struct AccountFile {
    schema: u32,
    #[serde(default)]
    account: Option<Account>,
}

/// The multi-account layout this store replaced; read once during migration.
#[derive(Debug, Deserialize)]
struct LegacyAccountFile {
    #[serde(default)]
    accounts: Vec<Account>,
}

/// Handle to the on-disk state.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Opens the store at the platform's standard location.
    pub fn open() -> Result<Self> {
        let root = match std::env::var_os(HOME_ENV) {
            Some(explicit) if !explicit.is_empty() => PathBuf::from(explicit),
            _ => {
                let base = directories::BaseDirs::new().ok_or(Error::NoDataDirectory)?;
                base.data_dir().join("wxapkg-dump").join("remote")
            }
        };
        Self::at(root)
    }

    /// Opens the store at an explicit directory, creating it with `0700`.
    pub fn at(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .map_err(|err| Error::io(format!("creating {}", root.display()), err))?;
        restrict(&root, 0o700);
        std::fs::create_dir_all(root.join("pending"))
            .map_err(|err| Error::io("creating the pending directory", err))?;
        std::fs::create_dir_all(root.join("icons"))
            .map_err(|err| Error::io("creating the icon directory", err))?;
        Ok(Self { root })
    }

    /// Directory backing this store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn account_path(&self) -> PathBuf {
        self.root.join("account.json")
    }

    fn pending_path(&self, session_id: &str) -> PathBuf {
        let safe: String = session_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join("pending").join(format!("{safe}.json"))
    }

    /// The one remembered account, newest migrations already folded in.
    pub fn account(&self) -> Result<Option<Account>> {
        self.adopt_legacy_layout()?;
        let path = self.account_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::io(format!("reading {}", path.display()), err)),
        };
        match serde_json::from_str::<AccountFile>(&text) {
            Ok(file) => Ok(file.account),
            Err(err) => {
                // Keep the evidence: a user can recover by signing in again, but only if the
                // old file is still there to inspect.
                let backup = path.with_extension(format!("corrupt-{}", crate::store::now()));
                let _ = std::fs::rename(&path, &backup);
                tracing::warn!(
                    target: "wxapkg-dump::remote",
                    path = %path.display(),
                    backup = %backup.display(),
                    error = %err,
                    "account.json was unreadable; starting from no account"
                );
                Ok(None)
            }
        }
    }

    /// Saves the account, replacing whatever was remembered before.
    pub fn save(&self, account: Account) -> Result<()> {
        let file = AccountFile {
            schema: SCHEMA,
            account: Some(account),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(|err| Error::Corrupt {
            path: self.account_path().display().to_string(),
            reason: err.to_string(),
        })?;
        write_atomic(&self.account_path(), &bytes)
    }

    /// Deletes the account and every cache tied to it. Missing files are fine: the goal
    /// is "gone".
    pub fn clear(&self) -> Result<()> {
        let _ = std::fs::remove_file(self.account_path());
        let _ = std::fs::remove_file(self.root.join("metadata.json"));
        let _ = std::fs::remove_dir_all(self.root.join("icons"));
        let _ = std::fs::create_dir_all(self.root.join("icons"));
        for session in self.pending()? {
            self.forget_pending(&session.session_id);
        }
        Ok(())
    }

    /// Marks the stored session as no longer usable; a missing account is not an error.
    pub fn mark_stale(&self) -> Result<()> {
        if let Some(mut account) = self.account()? {
            account.state = crate::account::AccountState::Stale;
            account.updated_at = now();
            self.save(account)?;
        }
        Ok(())
    }

    /// Remembers a login that is waiting for the user to scan.
    pub fn remember_pending(&self, session: &LoginSession) -> Result<()> {
        let path = self.pending_path(&session.session_id);
        write_atomic(
            &path,
            &serde_json::to_vec(session).map_err(|err| Error::Corrupt {
                path: path.display().to_string(),
                reason: err.to_string(),
            })?,
        )
    }

    /// Drops a pending login.
    pub fn forget_pending(&self, session_id: &str) {
        let _ = std::fs::remove_file(self.pending_path(session_id));
    }

    /// Pending logins, so a restart can resume polling instead of stranding the user.
    pub fn pending(&self) -> Result<Vec<LoginSession>> {
        let dir = self.root.join("pending");
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(out);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            match serde_json::from_str::<LoginSession>(&text) {
                Ok(session) => out.push(session),
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        out.sort_by_key(|session| session.started_at);
        Ok(out)
    }

    /// Moves a multi-account `accounts.json` (the layout this store replaced) aside,
    /// keeping its newest record as the single account. Runs at most once: the legacy
    /// file is renamed on success.
    fn adopt_legacy_layout(&self) -> Result<()> {
        let legacy = self.root.join("accounts.json");
        if !legacy.is_file() || self.account_path().is_file() {
            return Ok(());
        }
        let text = std::fs::read_to_string(&legacy)
            .map_err(|err| Error::io(format!("reading {}", legacy.display()), err))?;
        let parsed = serde_json::from_str::<LegacyAccountFile>(&text);
        let moved = legacy.with_extension("json.legacy");
        std::fs::rename(&legacy, &moved)
            .map_err(|err| Error::io(format!("renaming {}", legacy.display()), err))?;
        if let Ok(file) = parsed {
            let mut accounts = file.accounts;
            accounts.sort_by_key(|account| std::cmp::Reverse(account.updated_at));
            if let Some(newest) = accounts.into_iter().next() {
                tracing::info!(
                    target: "wxapkg-dump::remote",
                    backup = %moved.display(),
                    "adopted the newest of the legacy multi-account records"
                );
                self.save(newest)?;
            }
        }
        Ok(())
    }
}

/// Writes a small file atomically with `0600`, so an interrupted write cannot corrupt state.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Protocol(format!("{} has no parent", path.display())))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".to_owned());
    let temp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    std::fs::write(&temp, bytes)
        .map_err(|err| Error::io(format!("writing {}", temp.display()), err))?;
    restrict(&temp, 0o600);
    std::fs::rename(&temp, path).map_err(|err| {
        let _ = std::fs::remove_file(&temp);
        Error::io(format!("replacing {}", path.display()), err)
    })
}

/// Applies a permission mode on Unix; a no-op elsewhere.
fn restrict(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

/// Unix seconds, or zero when the clock is before the epoch.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| i64::try_from(value.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountState;

    fn account(nick: &str, updated: i64) -> Account {
        Account {
            nick_name: nick.to_owned(),
            state: AccountState::Online,
            updated_at: updated,
            ..Account::default()
        }
    }

    #[test]
    fn an_account_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("remote")).unwrap();
        assert!(store.account().unwrap().is_none());
        store.save(account("Eternal", 1)).unwrap();

        let reopened = Store::at(dir.path().join("remote")).unwrap();
        let account = reopened.account().unwrap().expect("one account");
        assert_eq!(account.nick_name, "Eternal");
    }

    #[test]
    fn a_new_login_replaces_the_old_account() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("remote")).unwrap();
        store.save(account("first", 1)).unwrap();
        store.save(account("second", 2)).unwrap();
        assert_eq!(store.account().unwrap().unwrap().nick_name, "second");
    }

    #[test]
    fn clearing_removes_the_account_and_its_caches() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("remote")).unwrap();
        store.save(account("a", 1)).unwrap();
        std::fs::write(store.root.join("metadata.json"), b"{}").unwrap();
        std::fs::create_dir_all(store.root.join("icons")).unwrap();
        std::fs::write(store.root.join("icons/x.bin"), b"x").unwrap();

        store.clear().unwrap();
        assert!(store.account().unwrap().is_none());
        assert!(!store.root.join("metadata.json").exists());
        assert!(!store.root.join("icons/x.bin").exists());
    }

    #[test]
    fn a_corrupt_file_is_moved_aside_instead_of_being_lost() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("remote");
        let store = Store::at(root.clone()).unwrap();
        std::fs::write(root.join("account.json"), b"{not json").unwrap();

        assert!(store.account().unwrap().is_none());
        let backups: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains("corrupt"))
            .collect();
        assert_eq!(backups.len(), 1, "the unreadable file must be preserved");
    }

    #[cfg(unix)]
    #[test]
    fn credentials_are_readable_only_by_the_owner() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("remote");
        let store = Store::at(root.clone()).unwrap();
        store.save(account("a", 1)).unwrap();

        let mode = std::fs::metadata(root.join("account.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "account file must not be world readable");
        let dir_mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "state directory is private");
    }

    #[test]
    fn pending_logins_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("remote")).unwrap();
        let session = LoginSession {
            session_id: "abc-123".to_owned(),
            login_id: None,
            qr_url: "https://example/qr".to_owned(),
            state: "state".to_owned(),
            started_at: 10,
            expires_in: 240,
        };
        store.remember_pending(&session).unwrap();
        let resumed = Store::at(dir.path().join("remote"))
            .unwrap()
            .pending()
            .unwrap();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].session_id, "abc-123");

        store.forget_pending("abc-123");
        assert!(store.pending().unwrap().is_empty());
    }

    #[test]
    fn the_legacy_multi_account_file_adopts_its_newest_record() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("remote");
        let store = Store::at(root.clone()).unwrap();
        let legacy = serde_json::json!({
            "schema": 1,
            "accounts": [
                { "login_id": "old", "nick_name": "Old", "updated_at": 100 },
                { "login_id": "new", "nick_name": "Newest", "updated_at": 200 },
            ]
        });
        std::fs::write(root.join("accounts.json"), legacy.to_string()).unwrap();

        let account = store.account().unwrap().expect("adopted");
        assert_eq!(account.nick_name, "Newest");
        assert!(root.join("accounts.json.legacy").is_file());
        // A second open must not re-adopt or lose the account.
        let again = Store::at(root).unwrap().account().unwrap().unwrap();
        assert_eq!(again.nick_name, "Newest");
    }

    #[test]
    fn marking_stale_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("remote");
        let store = Store::at(root.clone()).unwrap();
        store.save(account("a", 1)).unwrap();
        store.mark_stale().unwrap();
        assert_eq!(
            Store::at(root).unwrap().account().unwrap().unwrap().state,
            AccountState::Stale
        );
    }
}
