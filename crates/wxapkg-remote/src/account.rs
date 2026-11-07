// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! The account model. One installation remembers at most one account.

use serde::{Deserialize, Serialize};

/// Whether a stored session can still be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    /// The last attempt to use the session succeeded.
    Online,
    /// The session expired and must be renewed by scanning again.
    Stale,
    /// Never confirmed, or explicitly deleted.
    Offline,
}

impl AccountState {
    /// Human-readable label, matching what the window shows.
    pub fn label(self, english: bool) -> String {
        match (self, english) {
            (Self::Online, false) => "在线".to_owned(),
            (Self::Online, true) => "online".to_owned(),
            (Self::Stale, false) => "需重新登录".to_owned(),
            (Self::Stale, true) => "sign-in expired".to_owned(),
            (Self::Offline, false) => "离线".to_owned(),
            (Self::Offline, true) => "offline".to_owned(),
        }
    }

    /// Parses a state from storage, defaulting to offline for unknown values.
    pub fn from_wire(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "online" => Self::Online,
            "stale" | "expired" => Self::Stale,
            _ => Self::Offline,
        }
    }
}

/// The one account this installation remembers.
///
/// A new login overwrites the record wholesale: same `openid` means the same person
/// re-authorised (fresh keys), a different `openid` means the account was replaced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// Display name.
    #[serde(default)]
    pub nick_name: String,
    /// Avatar URL.
    #[serde(default)]
    pub avatar: String,
    /// Session state.
    #[serde(default = "default_state")]
    pub state: AccountState,
    /// Identity as the upstream service reports it, when it reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openid: Option<String>,
    /// Device key the session was issued for; reused by silent re-authentication.
    #[serde(default = "default_device")]
    pub device: String,
    /// Unix seconds of the last successful refresh.
    #[serde(default)]
    pub last_refresh_at: i64,
    /// Unix seconds of the last write.
    #[serde(default)]
    pub updated_at: i64,
    /// The iLink session, present once a confirmed scan has been exchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<crate::session::Session>,
}

fn default_state() -> AccountState {
    AccountState::Offline
}

/// The device every fresh login uses; the upstream binds the session to it.
fn default_device() -> String {
    "default".to_owned()
}

impl Default for Account {
    fn default() -> Self {
        Self {
            nick_name: String::new(),
            avatar: String::new(),
            state: default_state(),
            openid: None,
            device: default_device(),
            last_refresh_at: 0,
            updated_at: 0,
            session: None,
        }
    }
}

impl Account {
    /// Whether this record and `other` describe the same upstream user.
    pub fn same_person(&self, other: &Account) -> bool {
        match (self.openid.as_deref(), other.openid.as_deref()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(nick: &str, openid: Option<&str>) -> Account {
        Account {
            nick_name: nick.to_owned(),
            openid: openid.map(str::to_owned),
            ..Account::default()
        }
    }

    #[test]
    fn the_same_openid_is_the_same_person() {
        assert!(account("a", Some("o1")).same_person(&account("b", Some("o1"))));
        assert!(!account("a", Some("o1")).same_person(&account("b", Some("o2"))));
        assert!(!account("a", None).same_person(&account("b", None)));
    }

    #[test]
    fn state_labels_are_stable() {
        assert_eq!(AccountState::Online.label(false), "在线");
        assert_eq!(AccountState::Stale.label(true), "sign-in expired");
        assert_eq!(AccountState::from_wire("expired"), AccountState::Stale);
        assert_eq!(AccountState::from_wire("nonsense"), AccountState::Offline);
    }

    #[test]
    fn the_device_key_defaults_and_survives_round_trips() {
        let text = serde_json::to_string(&account("a", None)).unwrap();
        let back: Account = serde_json::from_str(&text).unwrap();
        assert_eq!(back.device, "default");
    }
}
