// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Remote download capability for wxapkg-dump, implemented entirely in Rust.
//!
//! This crate exists so remote download can be added to, or removed from, the toolkit as one
//! piece: nothing outside it knows how an account is stored or how the login handshake works.
//! The application talks to [`Remote`]; the window, the CLI and the HTTP API all sit on that
//! one type.
//!
//! Exactly one account is remembered. A new scan overwrites it — same `openid` means the
//! same person re-authorised, a different one means the account was replaced — and
//! [`Remote::delete_account`] removes it together with every cache tied to it.
//!
//! The layers, bottom to top:
//!
//! * [`oauth`] — the QR login handshake, plain HTTPS against WeChat's Open Platform;
//! * [`ual`] — the access API that turns a confirmed scan into a one-time login buffer;
//! * [`ilink`] — MMTLS, the WPKG envelope, hybrid sealing and the auth exchanges;
//! * [`wxa`] — the authenticated Mini Program calls (usage record, attributes, download);
//! * [`store`] — where the account and its session live: one JSON file, `0600`,
//!   written atomically, under the platform's standard application-data directory.
//!
//! Sessions issued by the legacy shared library import automatically, so switching to the
//! native path costs nobody a re-scan.

pub mod account;
pub mod challenge;
pub mod error;
pub mod ilink;
pub mod meta;
pub mod oauth;
pub mod pb;
pub mod sec;
pub mod session;
pub mod store;
pub mod ual;
pub mod wxa;

pub use account::{Account, AccountState};
pub use error::{Error, Result};
pub use meta::{MetaCache, ProgramMeta};
pub use oauth::{LoginSession, LoginStatus};
pub use session::Session;
pub use store::Store;
pub use wxa::{
    DownloadOptions, DownloadReport, MiniInfo, PackageFile, RecentItem, RecentList, Wxa,
};

/// How many attribute lookups one `recent` call may spend on names and icons.
const ENRICH_BUDGET: usize = 40;

/// How many recent entries one page may request at most.
const RECENT_PAGE_MAX: u32 = 100;

/// Ceiling for a single upstream fetch, however deep the page is.
const RECENT_FETCH_MAX: u32 = 500;

/// Entry point for everything remote.
///
/// Cheap to construct; the store is a directory handle and every network client is created
/// per request, so a caller may hold one for the lifetime of the application.
/// Nickname sanitisation: upstream occasionally returns UTF-16 fragments or control bytes (shown as '%' garbage).
/// Nicknames containing NUL/control characters or only symbols fall back to "WeChat User".
pub fn clean_nick(raw: &str) -> String {
    let filtered: String = raw
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{0}')
        .collect();
    let t = filtered.trim();
    if t.is_empty() || t == "%" {
        return "微信用户".to_owned();
    }
    t.to_owned()
}

#[derive(Debug, Clone)]

pub struct Remote {
    store: Store,
}

impl Remote {
    /// Opens the remote capability at the platform's standard storage location.
    pub fn open() -> Result<Self> {
        Ok(Self {
            store: Store::open()?,
        })
    }

    /// Opens it at an explicit location, which tests and portable installs need.
    pub fn open_at(dir: impl Into<std::path::PathBuf>) -> Result<Self> {
        Ok(Self {
            store: Store::at(dir.into())?,
        })
    }

    /// The account store, for callers that only need persistence.
    pub fn store(&self) -> &Store {
        &self.store
    }

    // ------------------------------------------------------------------ login

    /// Starts a QR login. A confirmed scan replaces whatever account was stored.
    ///
    /// Pending logins whose QR window has already closed are dropped here, so a night of
    /// abandoned modals does not pile up files.
    pub fn begin_login(&self) -> Result<LoginSession> {
        let now = store::now();
        for pending in self.store.pending()? {
            if pending.started_at + pending.expires_in as i64 + 60 < now {
                self.store.forget_pending(&pending.session_id);
            }
        }
        let session = oauth::begin(Some("default"))?;
        self.store.remember_pending(&session)?;
        Ok(session)
    }

    /// Fetches the QR bitmap for a session as bytes.
    pub fn qr_image(&self, session: &LoginSession) -> Result<Vec<u8>> {
        oauth::image(&session.qr_url)
    }

    /// Polls the pending login a window remembers by id.
    ///
    /// A restart between showing the code and confirming on the phone must not lose the
    /// login, so the session is looked up among the remembered ones first.
    pub fn poll_pending(&self, session_id: &str) -> Result<LoginStatus> {
        let session = self
            .store
            .pending()?
            .into_iter()
            .find(|session| session.session_id == session_id)
            .ok_or_else(|| Error::InvalidArgument("that login session is unknown".to_owned()))?;
        self.poll(&session)
    }

    /// Polls one session; a confirmed scan is finalized into the stored account on the spot.
    ///
    /// The exchange (tokens, login buffer, ManualAuth) runs once here, so a caller that sees
    /// `Authorized` can immediately read the account and find the new session.
    pub fn poll(&self, session: &LoginSession) -> Result<LoginStatus> {
        let status = oauth::poll(session)?;
        if let LoginStatus::Authorized { code, .. } = &status {
            // The scan is spent either way; swallowing the failure would show "logged in"
            // while the stored account stays the old one.
            if let Err(err) = self.finalize_login(session, code) {
                self.store.forget_pending(&session.session_id);
                return Err(Error::Upstream(format!(
                    "login confirmed, but exchanging it failed: {err}"
                )));
            }
            self.store.forget_pending(&session.session_id);
        }
        if matches!(status, LoginStatus::Expired | LoginStatus::Cancelled) {
            self.store.forget_pending(&session.session_id);
        }
        Ok(status)
    }

    /// Turns a confirmed scan into the stored account: tokens, profile, buffer, ManualAuth.
    fn finalize_login(&self, session: &LoginSession, code: &str) -> Result<()> {
        let mut credentials = ual::exchange_code(code, &session.state)?;
        let _ = ual::refresh_token(&mut credentials);
        let profile = ual::user_profile(&credentials).ok();
        let buffer = ual::login_buffer("default", &credentials)?;
        let outcome = ilink::auth::manual_auth(&buffer, "default")?;
        let mut session = outcome.session;
        session.ual = Some(credentials);
        session.login_buffer = buffer;
        let account = Account {
            nick_name: clean_nick(profile.as_ref().map(|p| p.nick_name.as_str()).unwrap_or("")),
            avatar: profile.map(|p| p.avatar.clone()).unwrap_or_default(),
            state: AccountState::Online,
            openid: session
                .ual
                .as_ref()
                .map(|ual| ual.open_id.clone())
                .filter(|openid| !openid.is_empty()),
            device: "default".to_owned(),
            last_refresh_at: store::now(),
            updated_at: store::now(),
            session: Some(session),
        };
        self.store.save(account)
    }

    /// The remembered account, when one is signed in.
    pub fn account(&self) -> Result<Option<Account>> {
        self.store.account()
    }

    /// Whether any account is signed in.
    pub fn is_signed_in(&self) -> Result<bool> {
        Ok(self.store.account()?.is_some())
    }

    /// Resolves the one account a call means.
    pub fn resolve_account(&self) -> Result<Account> {
        self.store.account()?.ok_or_else(|| {
            Error::NotLoggedIn("no account is signed in; scan the login code first".to_owned())
        })
    }

    /// Deletes the account and every cache tied to it.
    pub fn delete_account(&self) -> Result<()> {
        self.store.clear()
    }

    // ------------------------------------------------------------------ data

    /// The account's recently used Mini Programs, one page of `size` entries (1-based),
    /// enriched from the metadata cache.
    ///
    /// The first call spends a bounded budget on names and icons; later calls serve from
    /// the cache and quietly complete what the budget cut short.
    pub fn recent(&self, page: u32, size: u32) -> Result<RecentList> {
        self.recent_inner(page, size, true)
    }

    /// Same page, but never waits on names or icons.
    ///
    /// The interactive UI renders this immediately and fills the rows as
    /// [`Remote::enrich_recent`] resolves them, so a list of thirty programs no longer means
    /// staring at a skeleton for half a minute.
    pub fn recent_cached(&self, page: u32, size: u32) -> Result<RecentList> {
        self.recent_inner(page, size, false)
    }

    /// Resolves names, appids and icons for `usernames`, in parallel, and returns the fresh
    /// records. The cache is updated as a side effect, so the next list is complete already.
    pub fn enrich_recent(&self, usernames: &[String]) -> Result<Vec<(String, ProgramMeta)>> {
        let account = self.resolve_account()?;
        let session = self.session_of(&account)?;
        let wxa = wxa::Wxa::new(&session);
        let mut cache = MetaCache::open(&self.store);
        cache.enrich(&wxa, usernames, usernames.len());
        Ok(usernames
            .iter()
            .filter_map(|username| {
                cache
                    .get(username)
                    .cloned()
                    .map(|meta| (username.clone(), meta))
            })
            .collect())
    }

    fn recent_inner(&self, page: u32, size: u32, enrich: bool) -> Result<RecentList> {
        let page = page.max(1);
        let size = size.clamp(1, RECENT_PAGE_MAX);
        let account = self.resolve_account()?;
        let session = self.session_of(&account)?;
        // Starred entries come first in the combined list, so the page window is cut from it.
        let fetch = (page * size).min(RECENT_FETCH_MAX);
        let list = wxa::Wxa::new(&session).recent(fetch)?;

        if enrich {
            let mut cache = MetaCache::open(&self.store);
            let usernames: Vec<String> =
                list.iter_all().map(|item| item.username.clone()).collect();
            cache.enrich(&wxa::Wxa::new(&session), &usernames, ENRICH_BUDGET);
        }

        let skip = ((page - 1) * size) as usize;
        let star_end = list.starred.len();
        let mut starred = Vec::new();
        let mut history = Vec::new();
        for (index, item) in list.iter_all().enumerate() {
            if index < skip {
                continue;
            }
            if starred.len() + history.len() >= size as usize {
                break;
            }
            if index < star_end {
                starred.push(item.clone());
            } else {
                history.push(item.clone());
            }
        }
        Ok(RecentList { starred, history })
    }

    /// One Mini Program's profile, resolved by app id or original id.
    pub fn info(&self, appid: Option<&str>, username: Option<&str>) -> Result<MiniInfo> {
        self.info_inner(appid, username).inspect_err(|err| {
            if matches!(err, Error::SessionExpired(_)) {
                let _ = self.mark_stale();
            }
        })
    }

    fn info_inner(&self, appid: Option<&str>, username: Option<&str>) -> Result<MiniInfo> {
        let account = self.resolve_account()?;
        let session = self.session_of(&account)?;
        let username = username.map(wxa::normalize_username);
        let info = wxa::Wxa::new(&session).info(
            appid.unwrap_or_default().trim(),
            username.as_deref().unwrap_or_default(),
        )?;

        // Remember what was learned; the recent list displays it later.
        if !info.username.is_empty() {
            let mut cache = MetaCache::open(&self.store);
            let previous = cache.get(&info.username).cloned().unwrap_or_default();
            cache.put(
                &info.username,
                ProgramMeta {
                    appid: if info.appid.is_empty() {
                        previous.appid.clone()
                    } else {
                        info.appid.clone()
                    },
                    nick_name: {
                        let cleaned = clean_nick(&info.nick_name);
                        if cleaned == info.nick_name {
                            info.nick_name.clone()
                        } else if info.nick_name.is_empty() {
                            previous.nick_name.clone()
                        } else {
                            cleaned
                        }
                    },
                    avatar: if info.avatar.is_empty() {
                        previous.avatar.clone()
                    } else {
                        info.avatar.clone()
                    },
                    icon_path: previous.icon_path,
                    fetched_at: store::now(),
                },
            );
        }
        Ok(info)
    }

    /// Downloads every package of one Mini Program. See [`wxa::Wxa::download`].
    pub fn download(&self, options: &DownloadOptions) -> Result<DownloadReport> {
        self.download_inner(options).inspect_err(|err| {
            if matches!(err, Error::SessionExpired(_)) {
                let _ = self.mark_stale();
            }
        })
    }

    fn download_inner(&self, options: &DownloadOptions) -> Result<DownloadReport> {
        // A cancel that arrived before the job started must not require a login first.
        if options
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
        {
            return Err(Error::Cancelled);
        }
        let account = self.resolve_account()?;
        let session = self.session_of(&account)?;
        let mut options = options.clone();
        options.username = wxa::normalize_username(&options.username);
        let report = wxa::Wxa::new(&session).download(&options)?;

        // The download is the authoritative source of the app id for an original id.
        if !report.appid.is_empty() && !options.username.is_empty() {
            let mut cache = MetaCache::open(&self.store);
            if let Some(entry) = cache.all().get(&options.username).cloned() {
                if entry.appid != report.appid {
                    cache.put(
                        &options.username,
                        ProgramMeta {
                            appid: report.appid.clone(),
                            ..entry
                        },
                    );
                }
            }
        }
        Ok(report)
    }

    /// The cached profile of one Mini Program, if any.
    pub fn program_meta(&self, username: &str) -> Option<ProgramMeta> {
        let cache = MetaCache::open(&self.store);
        cache.get(username).cloned()
    }

    /// The cached icon of one Mini Program, if it was ever fetched.
    pub fn icon(&self, username: &str) -> Option<std::path::PathBuf> {
        let cache = MetaCache::open(&self.store);
        cache
            .get(username)
            .and_then(|meta| meta.icon_path.as_ref())
            .filter(|path| path.is_file())
            .cloned()
    }

    /// Marks the stored session as no longer usable.
    pub fn mark_stale(&self) -> Result<()> {
        self.store.mark_stale()
    }

    /// The session of the account, rejecting accounts that never completed an exchange.
    fn session_of(&self, account: &Account) -> Result<Session> {
        account
            .session
            .clone()
            .filter(|session| session.is_usable())
            .ok_or_else(|| {
                Error::SessionExpired("the stored session is incomplete; sign in again".to_owned())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote() -> (tempfile::TempDir, Remote) {
        let dir = tempfile::tempdir().unwrap();
        let inner = Remote::open_at(dir.path().join("remote")).unwrap();
        (dir, inner)
    }

    fn account(nick: &str) -> Account {
        Account {
            nick_name: nick.to_owned(),
            state: AccountState::Online,
            updated_at: 1,
            ..Account::default()
        }
    }

    #[test]
    fn an_empty_store_has_no_account() {
        let (_dir, remote) = remote();
        assert!(matches!(
            remote.resolve_account(),
            Err(Error::NotLoggedIn(_))
        ));
        assert!(!remote.is_signed_in().unwrap());
    }

    #[test]
    fn the_single_account_is_always_the_one() {
        let (_dir, remote) = remote();
        remote.store().save(account("only")).unwrap();
        assert_eq!(
            remote.resolve_account().unwrap().nick_name,
            "only",
            "no naming, no ambiguity"
        );
    }

    #[test]
    fn a_new_login_replaces_the_previous_account() {
        let (_dir, remote) = remote();
        remote.store().save(account("first")).unwrap();
        remote.store().save(account("second")).unwrap();
        assert_eq!(remote.resolve_account().unwrap().nick_name, "second");
    }

    #[test]
    fn deleting_the_account_clears_everything() {
        let (_dir, remote) = remote();
        remote.store().save(account("a")).unwrap();
        remote.delete_account().unwrap();
        assert!(!remote.is_signed_in().unwrap());
        // Deleting again is fine: the goal is "gone".
        remote.delete_account().unwrap();
    }

    #[test]
    fn a_cancelled_download_stops_before_any_network_call() {
        let (_dir, remote) = remote();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let options = crate::wxa::DownloadOptions {
            appid: "wx0123456789abcdef".to_owned(),
            cancel: Some(flag),
            ..crate::wxa::DownloadOptions::default()
        };
        let err = remote.download(&options).unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{err}");
        assert_eq!(err.code(), "cancelled");
    }

    #[test]
    fn an_account_without_a_session_cannot_serve_data_calls() {
        let (_dir, remote) = remote();
        remote.store().save(account("shell")).unwrap();
        let err = remote.recent(1, 20).unwrap_err();
        assert!(matches!(err, Error::SessionExpired(_)), "{err}");
    }
}
