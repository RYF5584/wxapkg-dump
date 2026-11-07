// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! The authenticated Mini Program calls: usage record, attribute sync, package download.
//!
//! Every call rides the same `WxaRuntimeTransfer` wrapper — a CGI path plus a sealed inner
//! message — so one helper carries them all. Downloading is an orchestration on top: the
//! transfer yields CDN addresses, plain HTTPS fetches the bytes, and the attribute list is
//! the authoritative set of packages to collect.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

use crate::error::{Error, Result};
use crate::ilink::envelope::{self, RequestHeader};
use crate::ilink::mmtls::MmtlsChannel;
use crate::pb;
use crate::session::{hex_encode, Session};

/// Automatic pass-risk throttle table (appid → last trigger timestamp in seconds).
#[allow(dead_code)] // kept for callers that enable automatic pass-risk
static RISK_GATE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, i64>>> =
    std::sync::OnceLock::new();

#[allow(dead_code)]
fn risk_gate() -> std::sync::MutexGuard<'static, std::collections::HashMap<String, i64>> {
    RISK_GATE
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// At most one automatic pure-computation pass-risk per appid every 15 minutes.
#[allow(dead_code)]
fn risk_gate_open(appid: &str) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut map = risk_gate();
    if let Some(last) = map.get(appid) {
        if now - *last < 15 * 60 {
            return false;
        }
    }
    map.insert(appid.to_owned(), now);
    true
}

/// Thread entry point: rebuild storage/session from the on-disk directory and run pass-risk.
pub fn run_pass_risk_in(
    dir: std::path::PathBuf,
    appid: &str,
) -> std::result::Result<serde_json::Value, String> {
    // Session resolution needs CGI_DUMP_DIR (the device profile and logs share it), so set it first.
    set_cgi_dump_dir(&dir.to_string_lossy());
    let remote = crate::Remote::open_at(dir).map_err(|e| e.to_string())?;
    let acct = remote.resolve_account().map_err(|e| e.to_string())?;
    let session = acct
        .session
        .ok_or_else(|| "stored account has no session".to_owned())?;
    let wxa = Wxa::new(&session);
    wxa.run_pass_risk(appid)
}

/// mmtls channel pool: short-lived channels (early-data POST) skip a full ECDH handshake each time.
/// The server treats a full handshake per CGI as suspicious (412 / risk control);
/// reusing a channel for five minutes halves the POST count and lowers that risk.
static MMTSL_POOL: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, (MmtlsChannel, std::time::Instant)>>,
> = std::sync::OnceLock::new();

fn mmtls_channel(host: &str) -> Result<MmtlsChannel> {
    let pool = MMTSL_POOL.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    {
        let guard = pool.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((ch, at)) = guard.get(host) {
            if at.elapsed() < std::time::Duration::from_secs(300) {
                return Ok(ch.clone());
            }
        }
    }
    let ch = MmtlsChannel::connect(host)?;
    let mut guard = pool.lock().unwrap_or_else(|p| p.into_inner());
    guard.insert(host.to_owned(), (ch.clone(), std::time::Instant::now()));
    Ok(ch)
}

/// Global minimum send interval (20 ms): prevents dozens of startup requests from flooding mmtls,
/// and keeps the server from classifying the burst as risk control.
static PACE: std::sync::OnceLock<std::sync::Mutex<std::time::Instant>> = std::sync::OnceLock::new();

fn pace() {
    let m = PACE.get_or_init(|| std::sync::Mutex::new(std::time::Instant::now()));
    let mut last = m.lock().unwrap_or_else(|p| p.into_inner());
    let gap = std::time::Duration::from_millis(20);
    let elapsed = last.elapsed();
    if elapsed < gap {
        std::thread::sleep(gap - elapsed);
    }
    *last = std::time::Instant::now();
}

/// Endpoint every runtime transfer posts to./// Endpoint every runtime transfer posts to./// Endpoint every runtime transfer posts to./// Endpoint every runtime transfer posts to.
const PATH_RUNTIME_TRANSFER: &str = "/ilink/ilinkapp/mp/wxaruntime_transfer";
/// Usage-record CGI.
const CGI_USAGE_RECORD: &str = "/cgi-bin/mmbiz-bin/wxaapp/getwxausagerecord";
/// Add/update usage-record CGI (fire-and-forget after launching a Mini Program).
const CGI_UPDATE_RECORD: &str = "/cgi-bin/mmbiz-bin/wxaapp/updatewxausagerecord";
/// Attribute-sync CGI.
const CGI_ATTR_SYNC: &str = "/cgi-bin/mmbiz-bin/wxaattr/wxaattrsync";
/// Whole-package address CGI (main package plus subpackages at once).
const CGI_PREGET_URL: &str = "/cgi-bin/mmbiz-bin/wxaapp/pregetwxadownloadurl";
/// Per-package address CGI, used to fill gaps the preget did not cover.
const CGI_BATCHGET_URL: &str = "/cgi-bin/mmbiz-bin/wxaapp/batchgetwxadownloadurl";

// Device parameters of the reference client (Android WMPF capture).
const HOST_APPID: &str = "wxd44977328b36e647";
/// Central device parameters: transport, fingerprint and UA all read from here.
///
/// `<storage_dir>/device_profile.json` may override them (read before every transfer):
/// `{"wx_version":"8.0.80","client_ver":"0x28005034","sub_ver":"0x22313951",
///   "sdk_version":"wx-miniprogram-sdk/3.6.6 (1732454001000 platform/android})"}`
/// Missing fields keep the built-in defaults, so a new version or device only changes one file.
#[derive(Clone, Debug)]
pub struct DeviceProfile {
    /// WeChat version (used by the UA, wx_app_version and the challenge).
    pub wx_version: String,
    /// BaseRequest.clientVersion / transfer.client_ver2
    pub client_ver: u32,
    /// transfer.sub_ver
    pub sub_ver: u32,
    /// BaseRequest.deviceType / transfer.device_type
    pub device_type: String,
    /// qbase_meta.sdk_version (full string).
    pub sdk_version: String,
}

// Device-semantic strings are XOR-encoded in the .so, so `strings` cannot reveal them.
// To change versions, run tools/version_profile.py sync (it re-encodes these constants).
const PROFILE_XK: u8 = 0x5A;
const WX_VER_ENC: &[u8] = &[0x62, 0x74, 0x6a, 0x74, 0x62, 0x6a];
const SDK_VER_ENC: &[u8] = &[
    0x2d, 0x22, 0x77, 0x37, 0x33, 0x34, 0x33, 0x2a, 0x28, 0x35, 0x3d, 0x28, 0x3b, 0x37, 0x77, 0x29,
    0x3e, 0x31, 0x75, 0x69, 0x74, 0x6c, 0x74, 0x6c, 0x7a, 0x72, 0x6b, 0x6d, 0x69, 0x68, 0x6e, 0x6f,
    0x6e, 0x6a, 0x6a, 0x6b, 0x6a, 0x6a, 0x6a, 0x7a, 0x2a, 0x36, 0x3b, 0x2e, 0x3c, 0x35, 0x28, 0x37,
    0x75, 0x3b, 0x34, 0x3e, 0x28, 0x35, 0x33, 0x3e, 0x27, 0x73,
];
const CLIENT_VER_X: u32 = 0x725a0a6e;
const CLIENT_VER_K: u32 = 0x5A5A5A5A;

fn deob(bytes: &[u8]) -> String {
    bytes.iter().map(|b| (b ^ PROFILE_XK) as char).collect()
}

impl Default for DeviceProfile {
    fn default() -> Self {
        DeviceProfile {
            wx_version: deob(WX_VER_ENC),
            client_ver: CLIENT_VER_X ^ CLIENT_VER_K,
            sub_ver: 0x2231_3951,
            device_type: "android".to_owned(),
            sdk_version: deob(SDK_VER_ENC),
        }
    }
}

/// Compile-time device semantics: the only authority for a release, stored in the .so and never read from external JSON.
pub fn compiled_profile() -> DeviceProfile {
    DeviceProfile::default()
}

thread_local! {
    static PROFILE_CACHE: std::cell::RefCell<
        Option<(std::path::PathBuf, Option<std::time::SystemTime>, DeviceProfile)>,
    > = const { std::cell::RefCell::new(None) };
}

/// Read device parameters from `<dir>/device_profile.json` (override only if present; reparse only when mtime changes).
pub fn device_profile() -> DeviceProfile {
    let dir = match CGI_DUMP_DIR.lock() {
        Ok(g) => g.clone(),
        Err(_) => None,
    };
    let Some(dir) = dir else {
        return DeviceProfile::default();
    };
    // Safety gate: device versions come only from compiled_profile() in the .so by default.
    // JSON overrides require an explicit files/config/allow_profile_override marker (debugging only).
    if read_flag("allow_profile_override").is_none() {
        return DeviceProfile::default();
    }
    // Normalisation: prefer files/config/device_profile.json, still accepting the old files/device_profile.json.
    let mut path = dir.join("config").join("device_profile.json");
    if !path.exists() {
        path = dir.join("device_profile.json");
    }
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    PROFILE_CACHE.with(|c| {
        if let Some((p, t, prof)) = c.borrow().as_ref() {
            if *p == path && *t == mtime {
                return prof.clone();
            }
        }
        let prof = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .map(|v| {
                let d = DeviceProfile::default();
                DeviceProfile {
                    wx_version: v
                        .get("wx_version")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_owned())
                        .unwrap_or(d.wx_version),
                    client_ver: v
                        .get("client_ver")
                        .and_then(|x| x.as_str())
                        .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                        .unwrap_or(d.client_ver),
                    sub_ver: v
                        .get("sub_ver")
                        .and_then(|x| x.as_str())
                        .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                        .unwrap_or(d.sub_ver),
                    device_type: v
                        .get("device_type")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_owned())
                        .unwrap_or(d.device_type),
                    sdk_version: v
                        .get("sdk_version")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_owned())
                        .unwrap_or(d.sdk_version),
                }
            })
            .unwrap_or_default();
        c.borrow_mut().replace((path.clone(), mtime, prof.clone()));
        prof
    })
}

/// Normalizes a caller-supplied original id: `gh_…` without the `@app` suffix still names
/// the Mini Program, so the suffix is appended instead of failing the lookup.
pub fn normalize_username(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.starts_with("gh_") && !trimmed.ends_with("@app") {
        format!("{trimmed}@app")
    } else {
        trimmed.to_owned()
    }
}

/// Default transfer scene.
const SCENE_DEFAULT: u32 = 1029;

/// One recently used Mini Program.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecentItem {
    /// Original id, channel suffix included (`gh_…@app`).
    pub username: String,
    /// Unix seconds of the last use.
    pub update_time: i64,
    /// Release channel the record refers to.
    pub version_type: u32,
    /// Sort position.
    pub order: i64,
    /// Whether the record came from outside the channel.
    pub is_from_outer: bool,
    /// One-line description, when the service sends one.
    pub app_desc: String,
    /// Which list the item came from.
    pub source: &'static str,
}

/// The two lists the usage record returns.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RecentList {
    /// Pinned entries.
    pub starred: Vec<RecentItem>,
    /// Everything else, newest first.
    pub history: Vec<RecentItem>,
}

impl RecentList {
    /// Both lists concatenated, starred first.
    pub fn iter_all(&self) -> impl Iterator<Item = &RecentItem> {
        self.starred.iter().chain(self.history.iter())
    }
}

/// The flattened profile of one Mini Program.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MiniInfo {
    /// App id, resolved from either input form.
    pub appid: String,
    /// Original id, when known.
    pub username: String,
    /// Display name.
    pub nick_name: String,
    /// Signature line.
    pub signature: String,
    /// Avatar URL.
    pub avatar: String,
    /// Current release version.
    pub version: u32,
    /// Client-side version stamp.
    pub client_version: u32,
    /// Unix seconds of the last release.
    pub update_time: i64,
    /// Every attribute the service returned, values parsed where they are JSON.
    pub attrs: serde_json::Map<String, serde_json::Value>,
}

/// One package of a download.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PackageFile {
    /// Subpackage module name, empty for the main package.
    pub module: String,
    /// Package class (widgets, plugins use non-zero).
    pub package_type: u32,
    /// Release version.
    pub version: u32,
    /// CDN address the bytes came from.
    pub url: String,
    /// Whether the transfer was zstd compressed.
    pub is_zstd: bool,
    /// Bytes on disk.
    pub size: u64,
    /// Marked non-essential at startup by the service.
    pub deferred: bool,
    /// Resource widget the caller did not ask for; nothing was downloaded.
    pub skipped: bool,
    /// Where the package landed, empty on failure.
    pub saved_path: String,
    /// MD5 of the saved bytes.
    pub md5: String,
    /// MD5 the service expected, when it stated one.
    pub expected_md5: String,
    /// Human-readable failure, empty on success.
    pub error: String,
}

/// The outcome of one download.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DownloadReport {
    /// App id the packages belong to.
    pub appid: String,
    /// Original id, when the caller addressed the program by it.
    pub username: String,
    /// Release version the service handed out.
    pub version: u32,
    /// Directory the packages landed in.
    pub out_dir: String,
    /// The packages, downloaded or failed.
    pub packages: Vec<PackageFile>,
    /// Non-host packages (plugins) whose addresses were listed but not fetched.
    pub other_packages: Vec<PackageFile>,
    /// Subpackage roots the config declared but no address covered.
    pub missing_sub_packages: Vec<String>,
    /// Diagnostic lines from the gap-filling passes.
    pub fetch_log: Vec<String>,
    /// Path of the written manifest, when it was written.
    pub manifest_path: String,
}

impl DownloadReport {
    /// How many packages landed successfully.
    pub fn downloaded(&self) -> usize {
        self.packages
            .iter()
            .filter(|package| package.error.is_empty() && !package.saved_path.is_empty())
            .count()
    }

    /// How many entries the caller skipped on purpose.
    pub fn skipped(&self) -> usize {
        self.packages
            .iter()
            .filter(|package| package.skipped)
            .count()
    }
}

/// Progress callback for one download batch.
pub type DownloadProgress = std::sync::Arc<dyn Fn(usize, usize, &str) + Send + Sync>;

/// Default and upper bound of the download worker count.
pub const DEFAULT_CONCURRENCY: usize = 3;
pub const CONCURRENCY_MAX: usize = 10;

/// Parameters of one download.
#[derive(Clone, Default)]
pub struct DownloadOptions {
    /// App id (`wx…`), or empty when addressing by original id.
    pub appid: String,
    /// Original id (`gh_…`), or empty when addressing by app id.
    pub username: String,
    /// Directory to write into; empty derives one from the app id.
    pub out_dir: PathBuf,
    /// Also download resource widgets (types 0/8/16/22/24/28), which can be huge.
    pub all_packages: bool,
    /// How many packages to fetch at once (1-10); the default is 3.
    pub concurrency: usize,
    /// Called once per package with (done, total, module); `total` covers the batch.
    pub progress: Option<DownloadProgress>,
    /// 0 release (default), 1 trial, 2 develop, 6 audit, 999 preview.
    pub version_type: u32,
    /// Explicit version, 0 for latest.
    pub version: u32,
    /// Set to true by another thread to stop the download between (and inside) packages.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl std::fmt::Debug for DownloadOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadOptions")
            .field("appid", &self.appid)
            .field("username", &self.username)
            .field("out_dir", &self.out_dir)
            .field("all_packages", &self.all_packages)
            .field("version_type", &self.version_type)
            .field("version", &self.version)
            .field("progress", &self.progress.as_ref().map(|_| ".."))
            .field("cancel", &self.cancel.is_some())
            .finish()
    }
}

/// A caller bound to one session.
pub struct Wxa<'a> {
    session: &'a Session,
}

impl<'a> Wxa<'a> {
    /// Binds the calls to a stored session.
    pub fn new(session: &'a Session) -> Self {
        Self { session }
    }

    /// Write one usage record (add/update wxapp record).
    ///
    /// The host calls this asynchronously after opening a Mini Program; failure never affects the main flow.
    /// The CGI belongs to the same family as the read call: `wxaapp/updatewxausagerecord`.
    ///
    /// Fields match yyb-go `AppService.AddWxAppRecord` one by one:
    /// scene=1047, recordType=2, opType=1, rea=1; cook is a timestamped hash string;
    /// the transfer appid is empty, matching `SendWxaRuntimeTransfer(authData, "", ...)`.
    pub fn add_record(&self, username: &str, appid: &str) -> Result<()> {
        let _ = appid; // 记录归属只看 username（gh_…@app），appid 仅用于宿主侧兜底解析
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let client_ver = if self.session.client_ver != 0 {
            self.session.client_ver
        } else {
            device_profile().client_ver
        };
        let inner = pb::UpdateWxaUsageRecordRequest {
            base_request: Some(self.base_request()),
            scene: Some(1047),
            is_from_background: Some(0),
            version_type: Some(0),
            record_type: Some(2),
            op_type: Some(1),
            username: Some(username.to_owned()),
            rea: Some(1),
            cook: Some(format!(
                "hash=1544042925&ts={ts}&host=&version={client_ver}&device=2"
            )),
        };
        let _ = self.transfer("", CGI_UPDATE_RECORD, &encode(&inner))?;
        Ok(())
    }

    /// The usage record: the account's recently used Mini Programs.
    pub fn recent(&self, limit: u32) -> Result<RecentList> {
        let inner = pb::GetWxAppRecordRequest {
            base_request: Some(self.base_request()),
            reason: Some(0),
            condition: Some(0),
            max_updatetime: Some(0),
            history_count: Some(limit.min(200) as i32),
        };
        let plain = self.transfer("", CGI_USAGE_RECORD, &encode(&inner))?;
        let answer = decode::<pb::GetWxAppRecordResponse>(&plain)?;
        let base_ret = answer
            .base_response
            .as_ref()
            .and_then(|r| r.ret)
            .unwrap_or(0);
        if base_ret != 0 {
            return Err(Error::Upstream(format!(
                "the usage record was refused (ret {base_ret})"
            )));
        }
        Ok(RecentList {
            starred: answer.star_list.iter().map(|i| item(i, "star")).collect(),
            history: answer
                .history_list
                .iter()
                .map(|i| item(i, "history"))
                .collect(),
        })
    }

    /// The attribute sync: profile, version and package inventory of one program.
    pub fn info(&self, appid: &str, username: &str) -> Result<MiniInfo> {
        let appid = appid.trim();
        let username = username.trim();
        if appid.is_empty() && username.is_empty() {
            return Err(Error::InvalidArgument(
                "either appid or username is required".to_owned(),
            ));
        }
        // The attrs call keys on appid; an original id without one resolves first.
        if appid.is_empty() {
            let guessed = crate::wxa::normalize_username(username);
            let info = self.info_inner("", &guessed)?;
            if info.appid.is_empty() {
                return Ok(info);
            }
            return self.info_inner(&info.appid, &guessed);
        }
        self.info_inner(appid, username)
    }

    /// The attribute sync itself, against a resolved appid.
    fn info_inner(&self, appid: &str, username: &str) -> Result<MiniInfo> {
        let inner = pb::WxaAttrSyncRequest {
            base_request: Some(self.base_request()),
            username: (!username.trim().is_empty()).then(|| username.to_owned()),
            appid: (!appid.trim().is_empty()).then(|| appid.to_owned()),
            ..pb::WxaAttrSyncRequest::default()
        };
        let plain = self.transfer(appid, CGI_ATTR_SYNC, &encode(&inner))?;
        let answer = decode::<pb::WxaAttrSyncResponse>(&plain)?;
        let base_ret = answer
            .base_response
            .as_ref()
            .and_then(|r| r.ret)
            .unwrap_or(0);
        if base_ret != 0 {
            // -10005 is how the service rejects an unusable session on this CGI.
            if base_ret == -10005 {
                return Err(Error::SessionExpired(
                    "the service refused the attribute sync; sign in again".to_owned(),
                ));
            }
            return Err(Error::Upstream(format!(
                "the attribute sync was refused (ret {base_ret})"
            )));
        }

        let mut info = MiniInfo {
            appid: appid.to_owned(),
            username: username.to_owned(),
            ..MiniInfo::default()
        };
        let attrs = attrs_map(&answer);
        tracing::debug!(
            target: "wxapkg-dump::remote",
            username = %username,
            appid = %appid,
            keys = ?attrs.keys().collect::<Vec<_>>(),
            "attribute sync returned these attr keys"
        );
        if let Some(value) = attrs.get("appId").and_then(value::as_str) {
            info.appid = value.to_owned();
        }
        if let Some(value) = attrs.get("username").and_then(value::as_str) {
            info.username = value.to_owned();
        }
        if let Some(value) = attrs.get("UserName").and_then(value::as_str) {
            if value.contains("@app") {
                info.username = value.to_owned();
            }
        }
        if let Some(value) = attrs.get("NickName").and_then(value::as_str) {
            info.nick_name = value.to_owned();
        }
        if let Some(value) = attrs.get("Signature").and_then(value::as_str) {
            info.signature = value.to_owned();
        }
        for key in ["SmallHeadImgUrl", "BrandIconURL", "BigHeadImgUrl"] {
            if let Some(url) = attrs.get(key).and_then(value::as_str) {
                if !url.is_empty() {
                    info.avatar = url.to_owned();
                    break;
                }
            }
        }
        for key in ["WxaAppInfo", "wxaAppInfo"] {
            let Some(entry) = attrs.get(key).and_then(value::as_object) else {
                continue;
            };
            let nick = entry.get("nickName").and_then(value::as_str).unwrap_or("");
            if info.nick_name.is_empty() && !nick.is_empty() {
                info.nick_name = nick.to_owned();
            }
            let avatar = entry.get("avatar").and_then(value::as_str).unwrap_or("");
            if info.avatar.is_empty() && !avatar.is_empty() {
                info.avatar = avatar.to_owned();
            }
            let appid = entry
                .get("Appid")
                .or_else(|| entry.get("appId"))
                .and_then(value::as_str)
                .unwrap_or("");
            if info.appid.is_empty() && !appid.is_empty() {
                info.appid = appid.to_owned();
            }
        }
        for key in ["WxaAppVersionInfo", "wxaAppVersionInfo"] {
            let Some(entry) = attrs.get(key).and_then(value::as_object) else {
                continue;
            };
            let read_u64 = |names: &[&str]| -> u64 {
                names
                    .iter()
                    .find_map(|name| entry.get(*name).and_then(value::as_u64))
                    .unwrap_or(0)
            };
            info.version = read_u64(&["AppVersion", "version"]) as u32;
            info.client_version = read_u64(&["ClientVersion", "clientVersion"]) as u32;
            info.update_time = entry
                .get("UpdateTime")
                .or_else(|| entry.get("updateTime"))
                .and_then(value::as_i64)
                .unwrap_or(0);
        }
        info.attrs = attrs;
        Ok(info)
    }

    /// Downloads every package of one Mini Program into `options.out_dir`.
    ///
    /// The release channel resolves addresses in one preget round; non-release channels and
    /// gaps the preget left are filled per package through batchget. A manifest is written
    /// next to the packages.
    pub fn download(&self, options: &DownloadOptions) -> Result<DownloadReport> {
        if cancelled(options) {
            return Err(Error::Cancelled);
        }
        if options.appid.trim().is_empty() && options.username.trim().is_empty() {
            return Err(Error::InvalidArgument(
                "either appid or username is required".to_owned(),
            ));
        }
        if options.version_type != 0 || options.version != 0 {
            return self.download_by_identity(options);
        }

        let inner = pb::PreGetWxaDownloadUrlRequest {
            base_request: Some(self.base_request()),
            appid: (!options.appid.is_empty()).then(|| options.appid.clone()),
            username: (!options.username.is_empty()).then(|| options.username.clone()),
            local_version: Some(0),
            field6: Some(0),
            field7: Some(true),
            ..pb::PreGetWxaDownloadUrlRequest::default()
        };
        let plain = self.transfer(&options.appid, CGI_PREGET_URL, &encode(&inner))?;
        let answer = decode::<pb::PreGetWxaDownloadUrlResponse>(&plain)?;
        let base_ret = answer
            .base_response
            .as_ref()
            .and_then(|r| r.ret)
            .unwrap_or(0);
        if base_ret != 0 {
            return Err(Error::Upstream(format!(
                "the address request was refused (ret {base_ret})"
            )));
        }
        if answer.package_info_list.is_empty() {
            return Err(Error::Upstream(
                "the service listed no packages for this program".to_owned(),
            ));
        }

        let appid = if options.appid.is_empty() {
            answer.appid.clone().unwrap_or_default()
        } else {
            options.appid.clone()
        };
        let mut report = DownloadReport {
            appid: appid.clone(),
            username: options.username.clone(),
            version: answer.version.unwrap_or(0),
            ..DownloadReport::default()
        };

        // Prefer the plain variant of each (module, package_type); record foreign appids.
        let mut plain_urls: HashMap<(String, u32), pb::WxaPackageInfo> = HashMap::new();
        let mut zstd_urls: HashMap<(String, u32), pb::WxaPackageInfo> = HashMap::new();
        for entry in &answer.package_info_list {
            let url = entry.url.clone().unwrap_or_default();
            if url.is_empty() {
                continue;
            }
            let owner = entry.appid.clone().unwrap_or_default();
            if !owner.is_empty() && !appid.is_empty() && owner != appid {
                report.other_packages.push(PackageFile {
                    module: entry.module.clone().unwrap_or_default(),
                    package_type: entry.package_type.unwrap_or(0),
                    version: entry.version.unwrap_or(0),
                    url,
                    is_zstd: entry.is_zstd.unwrap_or(false),
                    size: entry.size.unwrap_or(0) as u64,
                    deferred: entry.field20.unwrap_or(false),
                    ..PackageFile::default()
                });
                continue;
            }
            let key = (
                entry.module.clone().unwrap_or_default(),
                entry.package_type.unwrap_or(0),
            );
            let slot = if entry.is_zstd.unwrap_or(false) {
                &mut zstd_urls
            } else {
                &mut plain_urls
            };
            slot.entry(key).or_insert_with(|| entry.clone());
        }
        let mut ordered: Vec<pb::WxaPackageInfo> = plain_urls.into_values().collect();
        for (key, entry) in zstd_urls {
            if !ordered.iter().any(|candidate| {
                (
                    candidate.module.clone().unwrap_or_default(),
                    candidate.package_type.unwrap_or(0),
                ) == key
            }) {
                ordered.push(entry);
            }
        }
        ordered.sort_by(|a, b| {
            let am = a.module.clone().unwrap_or_default();
            let bm = b.module.clone().unwrap_or_default();
            am.cmp(&bm).then(
                a.package_type
                    .unwrap_or(0)
                    .cmp(&b.package_type.unwrap_or(0)),
            )
        });
        let deferred: HashMap<String, bool> = answer
            .package_info_list
            .iter()
            .map(|entry| {
                (
                    entry.url.clone().unwrap_or_default(),
                    entry.field20.unwrap_or(false),
                )
            })
            .collect();

        let out_dir = self.prepare_out_dir(&options.out_dir, &appid, &options.username)?;
        report.out_dir = out_dir.display().to_string();
        for entry in &ordered {
            let url = entry.url.clone().unwrap_or_default();
            let mut package = PackageFile {
                module: entry.module.clone().unwrap_or_default(),
                package_type: entry.package_type.unwrap_or(0),
                version: entry.version.unwrap_or(0),
                url: url.clone(),
                is_zstd: entry.is_zstd.unwrap_or(false),
                size: entry.size.unwrap_or(0) as u64,
                deferred: deferred.get(&url).copied().unwrap_or(false),
                ..PackageFile::default()
            };
            fetch_into(
                &url,
                &out_dir,
                &appid,
                &mut package,
                options.cancel.as_deref(),
            );
            report.packages.push(package);
        }
        if cancelled(options) {
            return Err(Error::Cancelled);
        }

        // The attribute list is authoritative; fill what the preget missed.
        self.fill_from_attrs(&out_dir, options, &appid, &mut report);
        if cancelled(options) {
            return Err(Error::Cancelled);
        }
        // Last resort: subpackage roots the main package's config declares.
        self.fill_from_config(&out_dir, options, &appid, &mut report);

        write_manifest(&out_dir, &mut report);
        Ok(report)
    }

    /// Non-release channels: every package is addressed by identity through batchget.
    fn download_by_identity(&self, options: &DownloadOptions) -> Result<DownloadReport> {
        let dev_uin = (self.session.uin & 0xffff_ffff) as u32;
        let main = self.batchget(options, "", 0, options.version, dev_uin)?;
        let main = main
            .into_iter()
            .find(|entry| !entry.url.is_empty() && entry.error.is_empty())
            .ok_or_else(|| Error::Upstream("no address came back for this channel".to_owned()))?;

        let appid = if options.appid.is_empty() {
            options.username.replace('/', "_")
        } else {
            options.appid.clone()
        };
        let out_dir = self.prepare_out_dir(&options.out_dir, &appid, &options.username)?;
        let mut report = DownloadReport {
            appid: options.appid.clone(),
            username: options.username.clone(),
            version: main.version,
            out_dir: out_dir.display().to_string(),
            ..DownloadReport::default()
        };

        let mut main_package = main.clone();
        main_package.module = String::new();
        fetch_into(
            &main.url,
            &out_dir,
            &appid,
            &mut main_package,
            options.cancel.as_deref(),
        );
        let saved = main_package.saved_path.clone();
        report.packages.push(main_package);

        if !saved.is_empty() {
            if let Some(roots) = subpackage_roots(Path::new(&saved)) {
                let done: Vec<String> = report
                    .packages
                    .iter()
                    .filter(|p| !p.saved_path.is_empty())
                    .map(|p| norm_module(&p.module))
                    .collect();
                for root in roots {
                    let normalized = norm_module(&root);
                    if normalized.is_empty() || done.contains(&normalized) {
                        continue;
                    }
                    self.fill_module(&out_dir, options, &appid, &normalized, &mut report);
                }
            }
        }

        write_manifest(&out_dir, &mut report);
        Ok(report)
    }

    /// Fills packages the attribute inventory lists but the preget did not return.
    fn fill_from_attrs(
        &self,
        out_dir: &Path,
        options: &DownloadOptions,
        appid: &str,
        report: &mut DownloadReport,
    ) {
        let info = match self.info(appid, &options.username) {
            Ok(info) => info,
            Err(err) => {
                report
                    .fetch_log
                    .push(format!("attribute sync failed: {err}"));
                return;
            }
        };
        let Some(version_info) = info.attrs.get("WxaAppVersionInfo") else {
            return;
        };
        let version = version_info
            .get("AppVersion")
            .and_then(value::as_u64)
            .unwrap_or(0) as u32;
        if version > 0 {
            report.version = version;
        }

        let mut done: Vec<String> = report
            .packages
            .iter()
            .filter(|p| !p.saved_path.is_empty())
            .map(|p| format!("{}#{}", norm_module(&p.module), p.package_type))
            .collect();

        let mut candidates: Vec<(String, u32, String)> = Vec::new();
        if let Some(modules) = version_info.get("module_list").and_then(value::as_array) {
            for module in modules {
                let Some(name) = module.get("name").and_then(value::as_str) else {
                    continue;
                };
                for widget in module
                    .get("widget_list")
                    .and_then(value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let ptype = widget
                        .get("package_type")
                        .and_then(value::as_u64)
                        .unwrap_or(0);
                    let md5 = widget
                        .get("wxapkg_md5")
                        .and_then(value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    candidates.push((name.to_owned(), ptype as u32, md5));
                }
            }
        }
        for widget in version_info
            .get("widget_list")
            .and_then(value::as_array)
            .into_iter()
            .flatten()
        {
            let ptype = widget
                .get("package_type")
                .and_then(value::as_u64)
                .unwrap_or(0);
            let md5 = widget
                .get("wxapkg_md5")
                .and_then(value::as_str)
                .unwrap_or("")
                .to_owned();
            candidates.push((String::new(), ptype as u32, md5));
        }

        let concurrency = options.concurrency.clamp(1, CONCURRENCY_MAX);
        let progress = options.progress.clone();

        // Partition first: resource widgets are reported without touching the CDN.
        let mut work: Vec<(String, u32, String)> = Vec::new();
        for (module, ptype, expected_md5) in candidates {
            if !matches!(ptype, 4 | 23) && !options.all_packages {
                report.packages.push(PackageFile {
                    module: norm_module(&module),
                    package_type: ptype,
                    version,
                    expected_md5: expected_md5.clone(),
                    skipped: true,
                    error: format!(
                        "resource widget (type {ptype}) skipped; pass all-packages to include it"
                    ),
                    ..PackageFile::default()
                });
                continue;
            }
            let key = format!("{}#{}", norm_module(&module), ptype);
            if done.contains(&key) {
                for package in &mut report.packages {
                    if norm_module(&package.module) == norm_module(&module)
                        && package.package_type == ptype
                    {
                        package.expected_md5 = expected_md5.clone();
                    }
                }
                continue;
            }
            work.push((module, ptype, expected_md5));
        }

        // Progress counts only what this run actually fetches: skipped resource packages
        // are reported separately and must not inflate the denominator.
        let total = work.len();
        // Resolve addresses one by one (a small CGI each, the service prefers a calm
        // pattern here), then fetch the bytes through a bounded worker pool — the CDN
        // is happy with parallel pulls, and this is where serial downloads waste time.
        let mut resolved: Vec<Option<PackageFile>> = Vec::with_capacity(work.len());
        for (index, (module, ptype, _)) in work.iter().enumerate() {
            if cancelled(options) {
                return;
            }
            if let Some(progress) = &progress {
                progress(index, total, module);
            }
            // The address lookup is a small CGI too; a transient failure here must not
            // turn into a "no download address" package.
            let mut package = None;
            for attempt in 1..=2 {
                package = self
                    .batchget(options, module, *ptype, version, 0)
                    .ok()
                    .and_then(|entries| {
                        entries
                            .into_iter()
                            .find(|entry| !entry.url.is_empty() && entry.error.is_empty())
                    });
                if package.is_some() {
                    break;
                }
                if attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
            }
            resolved.push(package);
        }

        let workers = concurrency.min(resolved.len().max(1));
        let fetched: std::sync::Mutex<Vec<(usize, PackageFile)>> =
            std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            // The channel lives inside the scope: seeding here and dropping the senders
            // when the closure ends is what releases the workers after the last index.
            let (tx, rx) = std::sync::mpsc::channel::<usize>();
            let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
            for _ in 0..workers {
                let rx = rx.clone();
                let fetched = &fetched;
                let work = &work;
                let resolved = &resolved;
                scope.spawn(move || loop {
                    let index = {
                        let guard = rx.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        guard.recv()
                    };
                    let Ok(index) = index else { break };
                    if cancelled(options) {
                        break;
                    }
                    let (module, ptype, expected_md5) = &work[index];
                    let mut package = resolved[index].clone().unwrap_or_default();
                    package.module = norm_module(module);
                    package.package_type = *ptype;
                    package.version = version;
                    package.expected_md5 = expected_md5.clone();
                    let url = package.url.clone();
                    fetch_into(
                        &url,
                        out_dir,
                        appid,
                        &mut package,
                        options.cancel.as_deref(),
                    );
                    fetched
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push((index, package));
                });
            }
            for index in 0..resolved.len() {
                if tx.send(index).is_err() {
                    break;
                }
            }
        });

        if cancelled(options) {
            return;
        }
        let mut fetched = fetched.into_inner().unwrap_or_default();
        fetched.sort_by_key(|(index, _)| *index);
        for (index, package) in fetched {
            let key = format!("{}#{}", norm_module(&work[index].0), work[index].1);
            if !package.saved_path.is_empty() {
                done.push(key);
            }
            report.packages.push(package);
            if let Some(progress) = &progress {
                progress(index + 1, total, &work[index].0);
            }
        }
        if let Some(progress) = &progress {
            progress(total, total, "done");
        }
    }

    /// Fills subpackages the main package's `app-config.json` declares but nothing covered.
    fn fill_from_config(
        &self,
        out_dir: &Path,
        options: &DownloadOptions,
        appid: &str,
        report: &mut DownloadReport,
    ) {
        let Some(main) = report
            .packages
            .iter()
            .find(|package| !package.saved_path.is_empty() && is_main_module(&package.module))
        else {
            return;
        };
        let Some(roots) = subpackage_roots(Path::new(&main.saved_path)) else {
            return;
        };
        let done: Vec<String> = report
            .packages
            .iter()
            .filter(|p| !p.saved_path.is_empty())
            .map(|p| norm_module(&p.module))
            .collect();
        let missing: Vec<String> = roots
            .iter()
            .map(|root| norm_module(root))
            .filter(|normalized| !normalized.is_empty() && !done.contains(normalized))
            .collect();
        let total = missing.len();
        for (index, normalized) in missing.into_iter().enumerate() {
            if let Some(progress) = &options.progress {
                progress(index, total, &normalized);
            }
            self.fill_module(out_dir, options, appid, &normalized, report);
        }
        if total > 0 {
            if let Some(progress) = &options.progress {
                progress(total, total, "done");
            }
        }
    }

    /// Fetches one subpackage module, trying the name forms the service recognizes.
    fn fill_module(
        &self,
        out_dir: &Path,
        options: &DownloadOptions,
        appid: &str,
        module: &str,
        report: &mut DownloadReport,
    ) {
        if cancelled(options) {
            return;
        }
        let variants = [
            module.to_owned(),
            format!("/{module}/"),
            format!("{module}/"),
        ];
        for variant in &variants {
            let Ok(entries) = self.batchget(options, variant, 0, options.version, 0) else {
                continue;
            };
            let Some(mut package) = entries
                .into_iter()
                .find(|entry| !entry.url.is_empty() && entry.error.is_empty())
            else {
                continue;
            };
            package.module = module.to_owned();
            let url = package.url.clone();
            fetch_into(
                &url,
                out_dir,
                appid,
                &mut package,
                options.cancel.as_deref(),
            );
            let ok = !package.saved_path.is_empty();
            report.packages.push(package);
            if ok {
                return;
            }
        }
        report.missing_sub_packages.push(module.to_owned());
    }

    /// One batchget round; the caller picks the entry it wants.
    fn batchget(
        &self,
        options: &DownloadOptions,
        module: &str,
        package_type: u32,
        version: u32,
        dev_uin: u32,
    ) -> Result<Vec<PackageFile>> {
        let appid = options.appid.as_str();
        let username = options.username.as_str();
        let identity = pb::WxaPkgIdentity {
            package_type: Some(package_type),
            version_type: Some(options.version_type),
            appid: (!appid.is_empty()).then(|| appid.to_owned()),
            username: (!username.is_empty()).then(|| username.to_owned()),
            module: Some(module.to_owned()),
        };
        let item = pb::WxaDownloadUrlReqItem {
            identity: Some(identity),
            version: Some(version),
            need_latest_version: Some(version == 0),
            need_zstd: Some(false),
            dev_uin: Some(dev_uin),
            ..pb::WxaDownloadUrlReqItem::default()
        };
        let inner = pb::BatchGetWxaDownloadUrlRequest {
            base_request: Some(self.base_request()),
            source: Some(pb::WxaCgiSource {
                source_type: Some(1),
                ..pb::WxaCgiSource::default()
            }),
            req_list: vec![item],
            ..pb::BatchGetWxaDownloadUrlRequest::default()
        };
        let plain = self.transfer(appid, CGI_BATCHGET_URL, &encode(&inner))?;
        let answer = decode::<pb::BatchGetWxaDownloadUrlResponse>(&plain)?;
        Ok(answer
            .resp_list
            .into_iter()
            .map(|entry| {
                let ret = entry.ret.unwrap_or(-1);
                PackageFile {
                    module: module.to_owned(),
                    package_type,
                    version: entry.field24.unwrap_or(0) as u32,
                    url: entry.url.clone().unwrap_or_default(),
                    is_zstd: entry.is_zstd.unwrap_or(false),
                    error: if ret == 0 && !entry.url.clone().unwrap_or_default().is_empty() {
                        String::new()
                    } else {
                        format!("the service answered ret {ret}")
                    },
                    ..PackageFile::default()
                }
            })
            .collect())
    }

    /// Full pure-computation risk challenge: start → transfer → end → transfer (SM4/HMAC-SM3 fingerprint).
    pub fn run_pass_risk(&self, appid: &str) -> std::result::Result<serde_json::Value, String> {
        let profile = device_profile();
        let sdk_full = profile.sdk_version.clone();
        let sdk_bare = sdk_full
            .split('/')
            .nth(1)
            .unwrap_or("3.16.2")
            .split(' ')
            .next()
            .unwrap_or("3.16.2")
            .to_owned();
        let sdk_ts: u64 = 1709110880000;
        let wx_version = profile.wx_version.clone();

        let pair = crate::challenge::build_pair(appid, &sdk_bare, sdk_ts, &wx_version);
        let cgi = "/cgi-bin/mmbiz-bin/js-operatewxdata";

        let start_pb = crate::challenge::inner_pb(appid, &pair.inner_start);
        let start_plain = self
            .transfer(appid, cgi, &start_pb)
            .map_err(|e| e.to_string())?;
        let start_text = crate::challenge::resp_f3_json(&start_plain)
            .unwrap_or_else(|| String::from_utf8_lossy(&start_plain).into_owned());
        let start_err = crate::challenge::extract_errcode(&start_text).unwrap_or(-1);

        let end_pb = crate::challenge::inner_pb(appid, &pair.inner_end);
        let end_plain = self
            .transfer(appid, cgi, &end_pb)
            .map_err(|e| e.to_string())?;
        let end_text = crate::challenge::resp_f3_json(&end_plain)
            .unwrap_or_else(|| String::from_utf8_lossy(&end_plain).into_owned());
        let end_err = crate::challenge::extract_errcode(&end_text).unwrap_or(-1);

        let out = serde_json::json!({
            "risk_passed": end_err == 0,
            "start_errcode": start_err,
            "end_errcode": end_err,
            "chlid": pair.chlid,
            "ua": pair.ua,
            "system": pair.system,
            "sdk_meta": pair.sdk_meta,
        });
        wxa_log(&format!("pass-risk {appid}: {out}"));
        Ok(out)
    }

    /// The inner request header every CGI body carries.
    fn base_request(&self) -> pb::BaseRequest {
        pb::BaseRequest {
            session_key: Some(
                self.session
                    .hex(&self.session.app_session_key)
                    .unwrap_or_default(),
            ),
            uin: Some((self.session.uin & 0xffff_ffff) as u32),
            device_id: Some(mac_of(self.session).into_bytes()),
            client_version: Some(if self.session.client_ver != 0 {
                self.session.client_ver
            } else {
                device_profile().client_ver
            }),
            device_type: Some(device_profile().device_type.clone()),
            scene: Some(0),
        }
    }

    /// Wraps an inner message in the runtime transfer and unwraps the answer.
    /// Public: exposed to FFI so a host app can relay arbitrary CGI traffic.
    pub fn transfer(&self, appid: &str, cgi_path: &str, inner: &[u8]) -> Result<Vec<u8>> {
        self.transfer_inner(appid, cgi_path, inner, false)
    }

    /// Same as `transfer`, but returns the outer `WxaRuntimeTransferResponse` bytes
    /// (base_response + resp_data) instead of peeling the inner payload.
    /// WMPF's synchronous TDI channel expects this outer envelope.
    pub fn transfer_raw(&self, appid: &str, cgi_path: &str, inner: &[u8]) -> Result<Vec<u8>> {
        self.transfer_inner(appid, cgi_path, inner, true)
    }

    /// Relays an already-serialized `WxaRuntimeTransferRequest` built by the WMPF
    /// render layer. Uses a hand-rolled tolerant parser: only fields 2 (cgi_path),
    /// 5 (req_data) and 6 (appid) are extracted; every other field is skipped
    /// regardless of its wire type (WMPF's writer encodes some ints as
    /// length-delimited, which strict prost decoding rejects).
    pub fn relay(&self, outer: &[u8]) -> Result<Vec<u8>> {
        let mut cgi_path = String::new();
        let mut appid = String::new();
        let mut inner: Vec<u8> = Vec::new();
        let mut i = 0usize;
        while i < outer.len() {
            let (tag, ni) = pb_read_varint(outer, i)?;
            i = ni;
            let field = (tag >> 3) as u32;
            let wt = (tag & 7) as u32;
            match (field, wt) {
                (2, 2) | (5, 2) | (6, 2) => {
                    let (len, ni2) = pb_read_varint(outer, i)?;
                    i = ni2;
                    let end = i + len as usize;
                    if end > outer.len() {
                        return Err(Error::Protocol("relay: truncated field".to_owned()));
                    }
                    let slice = &outer[i..end];
                    match field {
                        2 => cgi_path = String::from_utf8_lossy(slice).into_owned(),
                        5 => inner = slice.to_vec(),
                        _ => appid = String::from_utf8_lossy(slice).into_owned(),
                    }
                    i = end;
                }
                (_, 2) => {
                    let (len, ni2) = pb_read_varint(outer, i)?;
                    i = ni2 + len as usize;
                }
                (_, 0) => {
                    let (_, ni2) = pb_read_varint(outer, i)?;
                    i = ni2;
                }
                (_, 5) => i += 4,
                (_, 1) => i += 8,
                (_, w) => {
                    return Err(Error::Protocol(format!("relay: bad wire type {w}")));
                }
            }
        }
        if inner.is_empty() {
            return Err(Error::Protocol(
                "relay: request has empty req_data".to_owned(),
            ));
        }
        wxa_log(&format!(
            "relay: cgi_path={cgi_path} appid={appid} inner_len={}",
            inner.len()
        ));
        cgi_dump("relay_REQ", &cgi_path, &appid, &inner);
        // js-login / jslogin follows yyb-go JSLoginAPI: it uses the wxaruntime transfer channel
        // (transfer_inner), not a separate H5 POST, because an H5 response is not an iLink envelope and fails the magic check.
        if cgi_path.contains("h5transfer") || cgi_path.contains("getoauthcode") {
            return self.relay_h5(&cgi_path, &inner);
        }
        self.transfer_inner(&appid, &cgi_path, &inner, true)
    }

    /// Dispatch one CGI according to the WMPF `lu.d` transport URL.
    ///
    /// WMPF has two TDI transports:
    ///   * `/ilink/ilinkapp/mp/wxaruntime_transfer`: the business cgi_path lives in envelope field 2,
    ///     the body is a `WxaRuntimeTransferRequest`, and the response must be unwrapped as `WxaRuntimeTransferResponse`.
    ///   * `/ilink/ilinkapp/mm/...` (ilinkgeta8key and friends): the body is the bare business request
    ///     (`GetA8KeyReq`) and the response is a bare `GetA8KeyResp`; it must **not** be wrapped in a transfer envelope,
    ///     or `om5.p()` / `respClazz.parseFrom()` will parse the envelope as garbage.
    ///
    /// An empty `transport_url` falls back to envelope parsing for older callers that pass only a body.
    pub fn relay_cgi(&self, transport_url: &str, cmd_id: u32, body: &[u8]) -> Result<Vec<u8>> {
        if transport_url.contains("/ilink/ilinkapp/mm/") {
            return self.relay_mm(transport_url, cmd_id, body);
        }
        self.relay(body)
    }

    /// Direct mm CGI relay: replace the WMPF bare request BaseRequest with **our signed-in** one, then POST it.
    ///
    /// This is the A8Key relay point. WMPF has no WeChat session of its own, so the `GetA8KeyReq.base_request`
    /// it builds has an empty session_key and uin=0, which the server rejects (the client sees TRANSFER(-10005)).
    /// Only field 1 is replaced; every other field (op_code / req_url / scene / code_type ...) passes through untouched,
    /// and the bare `GetA8KeyResp` bytes are returned unchanged in WMPF format, not yyb format.
    fn relay_mm(&self, cgi_path: &str, cmd_id: u32, body: &[u8]) -> Result<Vec<u8>> {
        let br = encode(&self.base_request());
        let plain = replace_field1(body, &br);
        wxa_log(&format!(
            "relay_mm: cgi={cgi_path} cmd_id={cmd_id} body_len={} -> injected_len={}",
            body.len(),
            plain.len()
        ));
        cgi_dump("relay_mm_REQ", cgi_path, "", &plain);

        let session_key = self
            .session
            .hex(&self.session.app_session_key)
            .map_err(|err| Error::SessionExpired(err.to_string()))?;
        if session_key.is_empty() {
            return Err(Error::SessionExpired(
                "the stored session has no key".to_owned(),
            ));
        }
        let server_id = self.session.hex(&self.session.server_id)?;
        let sealed = envelope::seal_body(&session_key, &plain)?;
        let packet = envelope::wrap_packet(
            &sealed,
            self.session.uin,
            &self.session.ilink_app_id,
            &server_id,
            envelope::ENCRYPT_TYPE_SESSION_AES,
        );

        pace();
        let mut channel = mmtls_channel(self.session.host())?;
        let response = channel.post(cgi_path, &packet)?;
        let parsed = envelope::parse_packet(&response)?;
        let sealed_resp = match parsed.into_body() {
            Ok(b) => b,
            Err(err) => {
                return Err(match err {
                    Error::Protocol(ref detail)
                        if detail.contains("app_ret -2") || detail.contains("app_ret -14") =>
                    {
                        Error::SessionExpired(
                            "the service no longer accepts this session".to_owned(),
                        )
                    }
                    other => other,
                });
            }
        };
        if sealed_resp.is_empty() {
            cgi_dump("relay_mm_RESP(EMPTY)", cgi_path, "", &[]);
            return Ok(Vec::new());
        }
        let opened = envelope::open_body(&session_key, &sealed_resp).or_else(|_| {
            envelope::open_body(
                &self.session.hex(&self.session.auto_auth_enc_key)?,
                &sealed_resp,
            )
        })?;
        wxa_log(&format!(
            "relay_mm: cgi={cgi_path} resp_len={}",
            opened.len()
        ));
        cgi_dump("relay_mm_RESP", cgi_path, "", &opened);
        // Return the bare response to WMPF: om5.p() calls respClazz.parseFrom(bytes).
        Ok(opened)
    }

    /// H5Transfer passthrough: `inner` is the body bytes of IlinkH5TransferRequest;
    /// wrap it in a session envelope, POST to the H5 CGI, and return the decrypted outer result.
    fn relay_h5(&self, cgi_path: &str, inner: &[u8]) -> Result<Vec<u8>> {
        cgi_dump("relay_h5_REQ", cgi_path, "", inner);
        let session_key = self
            .session
            .hex(&self.session.app_session_key)
            .map_err(|err| Error::SessionExpired(err.to_string()))?;
        let server_id = self.session.hex(&self.session.server_id)?;

        let plain = inner.to_vec();
        let sealed = envelope::seal_body(&session_key, &plain)?;
        let packet = envelope::wrap_packet(
            &sealed,
            self.session.uin,
            &self.session.ilink_app_id,
            &server_id,
            envelope::ENCRYPT_TYPE_SESSION_AES,
        );

        pace();
        let mut channel = mmtls_channel(self.session.host())?;
        let response = channel.post(cgi_path, &packet)?;
        let parsed = envelope::parse_packet(&response)?;
        let body = match parsed.into_body() {
            Ok(b) => b,
            Err(err) => {
                return Err(match err {
                    Error::Protocol(ref d)
                        if d.contains("app_ret -2") || d.contains("app_ret -14") =>
                    {
                        Error::SessionExpired(
                            "the service no longer accepts this session".to_owned(),
                        )
                    }
                    other => other,
                });
            }
        };
        if body.is_empty() {
            return Ok(Vec::new());
        }
        let opened = envelope::open_body(&session_key, &body).or_else(|_| {
            envelope::open_body(&self.session.hex(&self.session.auto_auth_enc_key)?, &body)
        })?;
        wxa_log(&format!(
            "relay_h5: cgi={cgi_path} resp_len={}",
            opened.len()
        ));
        cgi_dump("relay_h5_RESP", cgi_path, "", &opened);
        // The runtime hands every async TDI answer to a WxaRuntimeTransferResponse parser,
        // whichever CGI produced it. Returning the bare inner body here makes the render
        // layer read a js-login answer as a transfer envelope, which breaks the container as
        // soon as a Mini Program calls wx.login. Wrap it the same way the wxaruntime path
        // does, so both channels speak one shape.
        Ok(wrap_transfer_response(&opened))
    }

    fn transfer_inner(
        &self,
        appid: &str,
        cgi_path: &str,
        inner: &[u8],
        raw: bool,
    ) -> Result<Vec<u8>> {
        cgi_dump(
            &format!("transfer_inner_REQ(raw={raw})"),
            cgi_path,
            appid,
            inner,
        );
        // Inner BaseRequest injection: **on by default** (disable with no_inner_br).
        // WMPF-generated inner requests carry an empty BaseRequest (empty session_key, uin=0)
        // because it has no WeChat session; the server then reports busy or rejects based on that inner session.
        // This started as the inject_inner_br diagnostic switch; on-device tests of launchwxaapp/jsoperatewxdata
        // only became stable with it, so it became the default after the PopMart retest on 2026-11-06.
        let inner_owned;
        let mut inner: &[u8] = if !flag_on("no_inner_br") {
            let br = encode(&self.base_request());
            inner_owned = replace_field1(inner, &br);
            wxa_log(&format!(
                "inner_br: cgi={cgi_path} {} -> {} bytes",
                inner.len(),
                inner_owned.len()
            ));
            cgi_dump("transfer_inner_REQ_INJECTED", cgi_path, appid, &inner_owned);
            &inner_owned
        } else {
            inner
        };
        // launchwxaapp instance-id rewriting: **on by default** (disable with no_spoof_iid;
        // the spoof_iid file selects hash/host/both/all, default all, matching yyb-go).
        // It replaces the "IoT hardware host (hash=0) + host appid" fingerprint with yyb-go's
        // "hash=<real uin> + empty host + ts=seconds + version=<clientVer>",
        // otherwise the server returns ActionCode=2 (the H5 fallback page saying the app cannot open in this software).
        let iid_owned;
        {
            let spoof_mode = if flag_on("no_spoof_iid") {
                None
            } else {
                Some(read_flag("spoof_iid").unwrap_or_else(|| "all".to_owned()))
            };
            if let Some(mode) = spoof_mode {
                if cgi_path.contains("launchwxaapp") {
                    if let Some(raw) = pb_field_first(inner, 11) {
                        let src = String::from_utf8_lossy(raw).into_owned();
                        let fixed = spoof_instance_id(
                            &src,
                            &mode,
                            self.session.uin,
                            if self.session.client_ver != 0 {
                                self.session.client_ver
                            } else {
                                device_profile().client_ver
                            },
                        );
                        iid_owned = replace_field(inner, 11, fixed.as_bytes());
                        wxa_log(&format!("spoof_iid({mode}): {src}  ->  {fixed}"));
                        inner = &iid_owned;
                        cgi_dump("transfer_inner_REQ_SPOOFED", cgi_path, appid, inner);
                    }
                }
            }
        }
        // Version alignment now lives in source (dt6 systemInfo / yc1 UA / 3.6.6 base library),
        // so the qbase body is no longer rewritten by default (it breaks the signature → 103006). Only an explicit files/sdk_fix
        // performs the old three text replacements, for controlled comparisons.
        let js_owned;
        if cgi_path.contains("js-operatewxdata") && flag_on("sdk_fix") {
            if let Some(raw) = pb_field_first(inner, 3) {
                let json_src = String::from_utf8_lossy(raw).into_owned();
                // Normalise in order: headers (UA/version) → SDK fingerprint → wx_app_version; write back on any hit.
                let mut cur = json_src.clone();
                if let Some(v) = fix_qbase_headers(&cur) {
                    cur = v;
                }
                if let Some(v) = fix_sdk_fingerprint(&cur) {
                    cur = v;
                }
                if let Some(v) = fix_wx_app_version(&cur) {
                    cur = v;
                }
                if cur != json_src {
                    js_owned = replace_field(inner, 3, cur.as_bytes());
                    inner = &js_owned;
                    wxa_log(&format!(
                        "sdk_fix: js-operatewxdata {} -> {} bytes (ua/sdk/wx -> profile)",
                        json_src.len(),
                        cur.len()
                    ));
                    cgi_dump("transfer_inner_REQ_SDKFIX", cgi_path, appid, inner);
                }
            }
        }
        let request = pb::WxaRuntimeTransferRequest {
            base_request: Some(self.base_request()),
            cgi_path: Some(cgi_path.to_owned()),
            host_appid: Some(HOST_APPID.to_owned()),
            field4: Some(5),
            req_data: Some(inner.to_vec()),
            appid: Some(appid.to_owned()),
            scene: Some(SCENE_DEFAULT),
            client_ver2: Some(device_profile().client_ver),
            device_type: Some(device_profile().device_type.clone()),
            sub_ver: Some(device_profile().sub_ver),
        };
        let plain = encode(&request);
        let session_key = self
            .session
            .hex(&self.session.app_session_key)
            .map_err(|err| Error::SessionExpired(err.to_string()))?;
        if session_key.is_empty() {
            return Err(Error::SessionExpired(
                "the stored session has no key".to_owned(),
            ));
        }
        let device_id = self.session.hex(&self.session.device_id)?;
        let server_id = self.session.hex(&self.session.server_id)?;

        let sealed = envelope::seal_body(&session_key, &plain)?;
        let header = RequestHeader {
            uin: Some(self.session.uin),
            cmd_id: 0,
            session_key: Some(session_key.as_slice()),
            device_id: &device_id,
            client_ver: if self.session.client_ver != 0 {
                self.session.client_ver
            } else {
                crate::session::ILINK_CLIENT_VERSION
            },
            encrypt_type: envelope::ENCRYPT_TYPE_SESSION_AES,
        };
        // In this transport the app header stays off the wire: the WPKG header carries
        // its fields, and the payload after it is the sealed body alone.
        let _ = &header;
        let packet = envelope::wrap_packet(
            &sealed,
            self.session.uin,
            &self.session.ilink_app_id,
            &server_id,
            envelope::ENCRYPT_TYPE_SESSION_AES,
        );

        pace();
        let mut channel = mmtls_channel(self.session.host())?;
        let response = channel.post(PATH_RUNTIME_TRANSFER, &packet)?;
        let parsed = envelope::parse_packet(&response)?;
        let body = match parsed.into_body() {
            Ok(body) => body,
            Err(err) => {
                // -2 and -14 are how the service says the session is no longer welcome.
                return Err(match err {
                    Error::Protocol(ref detail)
                        if detail.contains("app_ret -2") || detail.contains("app_ret -14") =>
                    {
                        Error::SessionExpired(
                            "the service no longer accepts this session".to_owned(),
                        )
                    }
                    other => other,
                });
            }
        };
        if body.is_empty() {
            return Ok(Vec::new());
        }
        let opened = envelope::open_body(&session_key, &body).or_else(|_| {
            envelope::open_body(&self.session.hex(&self.session.auto_auth_enc_key)?, &body)
        })?;
        if opened.is_empty() {
            cgi_dump(
                &format!("transfer_inner_RESP(raw={raw},EMPTY)"),
                cgi_path,
                appid,
                &opened,
            );
            return Ok(Vec::new());
        }
        cgi_dump(
            &format!("transfer_inner_RESP(raw={raw})"),
            cgi_path,
            appid,
            &opened,
        );
        log_launch_verdict(cgi_path, &opened);
        // checkwebviewurl returns its verdict in base_response.ret: when non-zero WMPF calls rl1 (failure),
        // and rl1.a() only logs a line and **never calls ud1** → the WebView never loadUrl → blank page.
        // Without a WeChat session the server always returns ret=1 here, so every H5 fallback page stays blank.
        // With an empty open_url the success callback ql1 invokes (original_url, original_url), allowing the original address,
        // so zeroing ret turns this chain from "failure blocks" into "failure allows".
        let mut opened = opened;
        if cgi_path.contains("checkwebviewurl") {
            let n = zero_base_response_ret(&mut opened);
            wxa_log(&format!(
                "checkwebviewurl: forced base_response.ret=0 in {n} place(s)"
            ));
            if n > 0 {
                cgi_dump("transfer_inner_RESP_RET0", cgi_path, appid, &opened);
            }
        }
        // Diagnostic switch force_launch_ok: change launchwxaapp ActionCode 2 (open the H5 fallback)
        // to 1 (normal launch), so programs the server rejects still start natively.
        if cgi_path.contains("launchwxaapp") && flag_on("force_launch_ok") {
            if force_launch_action_ok(&mut opened) {
                wxa_log("force_launch_ok: ActionCode 2 -> 1");
                cgi_dump("transfer_inner_RESP_FORCED", cgi_path, appid, &opened);
            } else {
                wxa_log("force_launch_ok: nothing to change (ActionCode != 2)");
            }
            log_launch_verdict(cgi_path, &opened);
        }
        // raw mode: return the outer envelope as-is (WMPF sync channel expects it)
        if raw {
            return Ok(opened.to_vec());
        } // raw mode: return the outer envelope as-is (WMPF sync channel expects it)
        if raw {
            return Ok(opened.to_vec());
        }
        // The wrapper answer may nest the real payload one level down.
        if let Ok(wrapper) = decode::<pb::WxaRuntimeTransferResponse>(opened.as_slice()) {
            if let Some(data) = wrapper.resp_data {
                if !data.is_empty() {
                    cgi_dump("transfer_inner_RESP_inner", cgi_path, appid, &data);
                    return Ok(data);
                }
            }
        }
        Ok(opened)
    }

    /// Resolves (and creates) the output directory, defaulting under the system temp root.
    fn prepare_out_dir(&self, asked: &Path, appid: &str, username: &str) -> Result<PathBuf> {
        if !asked.as_os_str().is_empty() {
            std::fs::create_dir_all(asked)
                .map_err(|err| Error::io(format!("creating {}", asked.display()), err))?;
            return Ok(asked.to_path_buf());
        }
        let id = if appid.is_empty() {
            username.replace('/', "_")
        } else {
            appid.to_owned()
        };
        let dir = std::env::temp_dir().join("wxapkg").join(id);
        std::fs::create_dir_all(&dir)
            .map_err(|err| Error::io(format!("creating {}", dir.display()), err))?;
        Ok(dir)
    }
}

// ---------------------------------------------------------------- internals

/// Replace the message's original field 1 with `new_f1`; every other field is preserved byte for byte.
///
/// Only wire type 2 is accepted (BaseRequest is always length-delimited); an original field 1
/// with another wire type is kept as an ordinary field rather than guessed at. Protobuf does not require order,
/// so the new field 1 can simply go first.
fn replace_field1(msg: &[u8], new_f1: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(msg.len() + new_f1.len() + 4);
    out.push(0x0a); // field 1, wire type 2
    let mut n = new_f1.len();
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
    out.extend_from_slice(new_f1);

    let mut i = 0usize;
    while i < msg.len() {
        let start = i;
        let (tag, ni) = match pb_read_varint(msg, i) {
            Ok(v) => v,
            Err(_) => break,
        };
        i = ni;
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u32;
        let end = match wt {
            0 => match pb_read_varint(msg, i) {
                Ok((_, ni2)) => ni2,
                Err(_) => break,
            },
            1 => i + 8,
            5 => i + 4,
            2 => {
                let (len, ni2) = match pb_read_varint(msg, i) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                ni2 + len as usize
            }
            _ => break,
        };
        if end > msg.len() {
            break;
        }
        i = end;
        if field == 1 && wt == 2 {
            continue; // 旧的 BaseRequest 丢掉
        }
        out.extend_from_slice(&msg[start..end]);
    }
    out
}

/// Diagnostic switch: create an empty file with the same name in the app-private directory (adb can toggle it without a rebuild).
fn flag_on(name: &str) -> bool {
    read_flag(name).is_some()
}

/// Read a diagnostic switch file (used by mmtls and other modules).
pub(crate) fn flag_value(name: &str) -> Option<String> {
    read_flag(name)
}

/// Read a diagnostic switch file's content with surrounding whitespace removed; None when absent.
fn read_flag(name: &str) -> Option<String> {
    let dir = match CGI_DUMP_DIR.lock() {
        Ok(g) => g.clone(),
        Err(_) => None,
    };
    let dir = dir?;
    // Normalisation: prefer files/config/<name>, still accepting the old files/<name>.
    for p in [dir.join("config").join(name), dir.join(name)] {
        if let Ok(raw) = std::fs::read_to_string(&p) {
            return Some(raw.trim().to_owned());
        }
    }
    None
}

/// Rewrite the instance-id string (field 11) in the launchwxaapp inner request.
///
/// WMPF builds it with `WxaIoTPVReportIDFactory` (et6); note the **IoT** in the class name:
///
/// ```text
/// `hash=<uin>&ts=<ms>&host=<host-appid>&version=<wmpf-version>&device=2`
/// ```
///
/// We have no WeChat session, so `hash=0`; `host=wxd44977328b36e647` is the WMPF/yyb host
/// appid. The server reads that as "anonymous IoT hardware host" and returns ActionCode=2
/// for some programs (the H5 fallback page saying to open it with mobile WeChat).
///
/// Compare the same string in yyb-go `services/app.go`:
/// `hash=<real-uin>&ts=<seconds>&host=&version=<clientVer>&device=2`: **host is empty**,
/// **hash is the real uin**, and `device=2` matches on both sides. The difference is hash/host, not device.
///
/// Mode values (the content of the spoof_iid switch file):
///   `hash` replaces only hash with our uin; `host` only clears host;
///   `both` (or empty) changes both; `all` also switches ts to seconds and version to our client_ver.
fn spoof_instance_id(src: &str, mode: &str, uin: u64, client_ver: u32) -> String {
    let do_hash = mode.is_empty() || mode.contains("hash") || mode == "both" || mode == "all";
    let do_host = mode.is_empty() || mode.contains("host") || mode == "both" || mode == "all";
    let do_all = mode == "all";
    let mut out: Vec<String> = Vec::new();
    for part in src.split('&') {
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => (part, ""),
        };
        let nv = match k {
            "hash" if do_hash => uin.to_string(),
            "host" if do_host => String::new(),
            "version" if do_all => client_ver.to_string(),
            "ts" if do_all => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_else(|_| v.to_owned()),
            _ => v.to_owned(),
        };
        out.push(format!("{k}={nv}"));
    }
    out.join("&")
}

/// Align the SDK fingerprint in the js-operatewxdata inner JSON.
///
/// The Mini Program SDK embedded in WMPF reports its own fingerprint in cloud calls (tcbapi_*):
/// `wx-miniprogram-sdk/2.29.2 (1709110880000 platform/pyramid backend/wasm version/1.0.0)`
/// `platform=pyramid` is the IoT/wasm runtime fingerprint; when validation fails the cloud service returns
/// `errcode 103006 "system error"` (the "system busy" seen by the PopMart blind-box machine).
/// yyb-go's Android path uses `wx-miniprogram-sdk/3.16.2 (1709110880000 platform/android})`
/// (the trailing `}` is present in the real capture). Replace the whole sdk_version value,
/// keeping JSON-escaped `\/` sequences as they are.
/// Rewrite `wx_app_version` in the inner JSON to 8.0.71 (paired with client_ver 0x28004734).
/// The WMPF runtime reports 8.0.28, but the cloud service (get_service_info / tcbapi_*) checks the version
/// and returns errcode 103006 "system error" on a mismatch (the blind-box machine's "system busy").
/// Quotes occur in two forms: escaped `\"wx_app_version\":\"` and plain `"wx_app_version":"`.
/// Normalise the qbase_req plaintext headers: the WeChat version/build in the UA, remove WMPF's Luggage-IoT marker,
/// and replace X-Request-Sdk-Version with the local base-library version. DeviceProfile drives all of it.
/// First turn JSON `\/` into `/` (valid JSON, same meaning) to simplify matching.
/// Replace the string value after the first ':' following the key (handling JSON-escaped quotes).
fn replace_value_after(s: &str, key: &str, val: &str) -> Option<String> {
    let pos = s.find(key)?;
    let bytes = s.as_bytes();
    let mut i = pos + key.len();
    while i < bytes.len() && bytes[i] != b':' {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    i += 1;
    while i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\\' || bytes[i] == b' ') {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i] != b'"' && bytes[i] != b'\\' {
        i += 1;
    }
    let end = i;
    if &s[start..end] == val {
        return None;
    }
    Some(format!("{}{}{}", &s[..start], val, &s[end..]))
}

fn fix_qbase_headers(json: &str) -> Option<String> {
    let p = device_profile();
    // Do not normalise every `\/` to `/` here: the qbase_req body and signature are computed
    // over the original bytes, and a global unescape changes length/content → TCB returns 103006.
    let mut cur = json.to_owned();
    let mut changed = false;

    // UA: MicroMessenger/8.0.28.48(0x28001c30) → MicroMessenger/8.0.80.48(0x28005034)
    if let Some(pos) = cur.find("MicroMessenger/") {
        let start = pos + "MicroMessenger/".len();
        if let Some(rel) = cur[start..].find(')') {
            let end = start + rel;
            let build = p.client_ver & 0xff;
            let new = format!("{}.{}(0x{:08x})", p.wx_version, build, p.client_ver);
            cur = format!("{}{}{}", &cur[..start], new, &cur[end + 1..]);
            changed = true;
        }
    }
    // Real-device UAs carry no Luggage marker.
    for tok in [
        " Luggage/2.2.0.86552af0-IoT",
        "Luggage/2.2.0.86552af0-IoT ",
        "Luggage/2.2.0.86552af0-IoT",
    ] {
        if cur.contains(tok) {
            cur = cur.replace(tok, "");
            changed = true;
        }
    }
    // X-Request-Sdk-Version value → bare base-library version (locating it does not depend on escaping).
    let sdk_full = p.sdk_version.clone();
    let sdk_bare = sdk_full
        .split('/')
        .nth(1)
        .unwrap_or("3.16.2")
        .split(' ')
        .next()
        .unwrap_or("3.16.2")
        .to_owned();
    if let Some(v) = replace_value_after(&cur, "X-Request-Sdk-Version", &sdk_bare) {
        cur = v;
        changed = true;
    }
    if changed {
        Some(cur)
    } else {
        None
    }
}

fn fix_wx_app_version(json: &str) -> Option<String> {
    // Two forms: escaped \"wx_app_version\":\"8.0.28\" and plain "wx_app_version":"8.0.28".
    let marks: [&str; 2] = [r#"\"wx_app_version\":\""#, r#""wx_app_version":"#];
    let mut cur = json.to_owned();
    let mut changed = false;
    for mark in marks {
        let Some(pos) = cur.find(mark) else { continue };
        let start = pos + mark.len();
        let Some(q) = cur[start..].find('"') else {
            continue;
        };
        let mut end = start + q;
        // In the escaped form the closing quote is \"; the backslash is part of the JSON delimiter and must stay.
        if end > start && cur.as_bytes()[end - 1] == b'\\' {
            end -= 1;
        }
        let new_val = device_profile().wx_version;
        let candidate = format!("{}{}{}", &cur[..start], new_val, &cur[end..]);
        if candidate != cur {
            cur = candidate;
            changed = true;
        }
    }
    if changed {
        Some(cur)
    } else {
        None
    }
}

fn fix_sdk_fingerprint(json: &str) -> Option<String> {
    // The runtime (including a cached base library) may still report 2.29.2/pyramid; replace it with the profile's Android string.
    let marks: [&str; 2] = [
        r#"\"sdk_version\":\"wx-miniprogram-sdk\"#,
        r#""sdk_version":"wx-miniprogram-sdk"#,
    ];
    let sdk = device_profile().sdk_version; // 形如 wx-miniprogram-sdk/3.16.2 (1709110880000 platform/android})
    let mut cur = json.to_owned();
    let mut changed = false;
    for mark in marks {
        let Some(pos) = cur.find(mark) else { continue };
        // `mark` already contains "wx-miniprogram-sdk"; replace after it (only the /3.16.2 (...) part).
        let vstart = pos + mark.len();
        // The value ends at the next unescaped quote (in escaped form, at \").
        let Some(q) = cur[vstart..].find('"') else {
            continue;
        };
        let mut vend = vstart + q;
        if vend > vstart && cur.as_bytes()[vend - 1] == b'\\' {
            vend -= 1;
        }
        // `mark` already includes the "wx-miniprogram-sdk" prefix; replace only its tail (/3.16.2 (...)).
        let tail = sdk.trim_start_matches("wx-miniprogram-sdk");
        let esc = mark.starts_with('\\');
        let val = if esc {
            tail.replace("/", "\\/")
        } else {
            tail.to_owned()
        };
        let candidate = format!("{}{}{}", &cur[..vstart], val, &cur[vend..]);
        if candidate != cur {
            cur = candidate;
            changed = true;
        }
    }
    if changed {
        Some(cur)
    } else {
        None
    }
}

/// Replace one length-delimited field wholesale with `new_bytes`, preserving every other field.
fn replace_field(msg: &[u8], want: u32, new_bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(msg.len() + new_bytes.len());
    let mut i = 0usize;
    let mut replaced = false;
    while i < msg.len() {
        let start = i;
        let (tag, ni) = match pb_read_varint(msg, i) {
            Ok(v) => v,
            Err(_) => break,
        };
        i = ni;
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u32;
        let end = match wt {
            0 => match pb_read_varint(msg, i) {
                Ok((_, ni2)) => ni2,
                Err(_) => break,
            },
            1 => i + 8,
            5 => i + 4,
            2 => {
                let (len, ni2) = match pb_read_varint(msg, i) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                ni2 + len as usize
            }
            _ => break,
        };
        if end > msg.len() {
            break;
        }
        i = end;
        if field == want && wt == 2 && !replaced {
            replaced = true;
            out.push(((want << 3) | 2) as u8);
            let mut n = new_bytes.len();
            while n >= 0x80 {
                out.push((n as u8) | 0x80);
                n >>= 7;
            }
            out.push(n as u8);
            out.extend_from_slice(new_bytes);
            continue;
        }
        out.extend_from_slice(&msg[start..end]);
    }
    out
}

/// Return the content of the first length-delimited field with this number.
fn pb_field_first(msg: &[u8], want: u32) -> Option<&[u8]> {
    let mut i = 0usize;
    while i < msg.len() {
        let (tag, ni) = pb_read_varint(msg, i).ok()?;
        i = ni;
        let field = (tag >> 3) as u32;
        match (tag & 7) as u32 {
            0 => {
                let (_, ni2) = pb_read_varint(msg, i).ok()?;
                i = ni2;
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let (len, ni2) = pb_read_varint(msg, i).ok()?;
                let end = ni2 + len as usize;
                if end > msg.len() {
                    return None;
                }
                if field == want {
                    return Some(&msg[ni2..end]);
                }
                i = end;
            }
            _ => return None,
        }
    }
    None
}

/// launchwxaapp returns its verdict in the response: `f2.f2.f1` is ActionCode and `f2.f2.f2` the H5 fallback URL.
/// ActionCode=1 → normal native launch; =2 → the server sends the client to that URL (secitptpagepub scan page).
/// Print it on every relay so a 6 KB package no longer has to be decoded by hand.
fn log_launch_verdict(cgi_path: &str, resp: &[u8]) {
    if !cgi_path.contains("launchwxaapp") {
        return;
    }
    let Some((code, url)) = launch_action(resp) else {
        wxa_log("launch_verdict: no action block in response");
        return;
    };
    wxa_log(&format!(
        "launch_verdict: ActionCode={} url={}",
        code.map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_owned()),
        url.unwrap_or_default()
    ));
}

/// Read `f2.f2` (LaunchAction): ActionCode (f1) and H5 URL (f2).
fn launch_action(resp: &[u8]) -> Option<(Option<u64>, Option<String>)> {
    let biz = pb_field(resp, 2)?;
    let action = pb_field(biz, 2)?;
    Some((
        pb_varint(action, 1),
        pb_field(action, 2).map(|v| String::from_utf8_lossy(v).into_owned()),
    ))
}

/// Bypass "the server refuses a native launch": change launchwxaapp ActionCode from 2 (open the H5 fallback)
/// to 1 (normal launch).
///
/// This is based on a measured comparison: the PopMart blind-box machine (wx15f182efdb0ba66c) and JD Takeaway
/// (wxad169f0e9e78fe47), which opens normally, have **identical launchwxaapp response structure**: BaseResponse ret=0,
/// commlib download URL (f5), capability JSON (f7), appid/username (f8/f9) and experiment data (f16) are all present;
/// the only difference is the f2 LaunchAction: {1, -, 1} versus {2, secitptpagepub url, 0}.
/// The server supplies everything needed to launch, plus one extra instruction: "do not run, open this page".
/// WMPF's `WxaLaunchErrorAction$a` reads the URL and builds an H5 action only when ActionCode==2,
/// so changing that single byte to 1 is enough: an in-place same-length edit leaves packet length and offsets intact.
fn force_launch_action_ok(buf: &mut [u8]) -> bool {
    let Some((bs, be)) = pb_field_span(buf, 2) else {
        return false;
    };
    let Some((as_, ae)) = pb_field_span(&buf[bs..be], 2) else {
        return false;
    };
    let base = bs + as_;
    let end = bs + ae;
    let mut i = base;
    while i < end {
        let (tag, ni) = match pb_read_varint(buf, i) {
            Ok(v) => v,
            Err(_) => return false,
        };
        match (tag & 7) as u32 {
            0 => {
                let (v, ni2) = match pb_read_varint(buf, ni) {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                if (tag >> 3) == 1 && ni2 == ni + 1 {
                    if v == 2 {
                        buf[ni] = 1;
                        return true;
                    }
                    return false;
                }
                i = ni2;
            }
            2 => {
                let (len, ni2) = match pb_read_varint(buf, ni) {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                i = ni2 + len as usize;
                if i > end {
                    return false;
                }
            }
            1 => i = ni + 8,
            5 => i = ni + 4,
            _ => return false,
        }
    }
    false
}

/// Set `base_response.ret` to 0 in place and return how many sites changed.
///
/// Only same-length edits are made (when the original varint is one byte), so packet length, offsets
/// and envelope length prefixes need no recalculation. ret=1 is exactly one byte, covering the real cases.
///
/// Handle both layers: the outer `WxaRuntimeTransferResponse.base_response` and the business
/// response's own `base_response` inside `resp_data`; WMPF checks both.
fn zero_base_response_ret(buf: &mut [u8]) -> usize {
    let mut n = zero_ret_in(buf);
    // resp_data is outer field 2 and contains another business response.
    if let Some(span) = pb_field_span(buf, 2) {
        n += zero_ret_in(&mut buf[span.0..span.1]);
    }
    n
}

/// Find `field 1 (BaseResponse)` in a message and zero its `field 1 (ret)`.
fn zero_ret_in(buf: &mut [u8]) -> usize {
    let Some((bs, be)) = pb_field_span(buf, 1) else {
        return 0;
    };
    let mut i = bs;
    while i < be {
        let (tag, ni) = match pb_read_varint(buf, i) {
            Ok(v) => v,
            Err(_) => return 0,
        };
        let field = (tag >> 3) as u32;
        match (tag & 7) as u32 {
            0 => {
                let (v, ni2) = match pb_read_varint(buf, ni) {
                    Ok(v) => v,
                    Err(_) => return 0,
                };
                if field == 1 && ni2 == ni + 1 {
                    if v != 0 {
                        buf[ni] = 0;
                        return 1;
                    }
                    return 0;
                }
                i = ni2;
            }
            2 => {
                let (len, ni2) = match pb_read_varint(buf, ni) {
                    Ok(v) => v,
                    Err(_) => return 0,
                };
                i = ni2 + len as usize;
                if i > be {
                    return 0;
                }
            }
            1 => i = ni + 8,
            5 => i = ni + 4,
            _ => return 0,
        }
    }
    0
}

/// Return the byte range of a length-delimited field **content** (excluding tag and length prefix).
fn pb_field_span(msg: &[u8], want: u32) -> Option<(usize, usize)> {
    let mut i = 0usize;
    while i < msg.len() {
        let (tag, ni) = pb_read_varint(msg, i).ok()?;
        i = ni;
        let field = (tag >> 3) as u32;
        match (tag & 7) as u32 {
            0 => {
                let (_, ni2) = pb_read_varint(msg, i).ok()?;
                i = ni2;
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let (len, ni2) = pb_read_varint(msg, i).ok()?;
                let end = ni2 + len as usize;
                if end > msg.len() {
                    return None;
                }
                if field == want {
                    return Some((ni2, end));
                }
                i = end;
            }
            _ => return None,
        }
    }
    None
}

/// Return the **last** occurrence of a length-delimited field (protobuf allows repeats; the latter wins).
fn pb_field(msg: &[u8], want: u32) -> Option<&[u8]> {
    let mut found = None;
    let mut i = 0usize;
    while i < msg.len() {
        let (tag, ni) = pb_read_varint(msg, i).ok()?;
        i = ni;
        let field = (tag >> 3) as u32;
        match (tag & 7) as u32 {
            0 => {
                let (_, ni2) = pb_read_varint(msg, i).ok()?;
                i = ni2;
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let (len, ni2) = pb_read_varint(msg, i).ok()?;
                let end = ni2 + len as usize;
                if end > msg.len() {
                    return found;
                }
                if field == want {
                    found = Some(&msg[ni2..end]);
                }
                i = end;
            }
            _ => return found,
        }
    }
    found
}

/// Return a varint field value.
fn pb_varint(msg: &[u8], want: u32) -> Option<u64> {
    let mut i = 0usize;
    while i < msg.len() {
        let (tag, ni) = pb_read_varint(msg, i).ok()?;
        i = ni;
        let field = (tag >> 3) as u32;
        match (tag & 7) as u32 {
            0 => {
                let (v, ni2) = pb_read_varint(msg, i).ok()?;
                if field == want {
                    return Some(v);
                }
                i = ni2;
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let (len, ni2) = pb_read_varint(msg, i).ok()?;
                i = ni2 + len as usize;
                if i > msg.len() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

mod value {
    // serde_json::Value's methods resolve through Deref; these free functions keep call
    // sites short (`value::as_str`) without importing the enum everywhere.
    pub(crate) use serde_json::Value;
    pub(crate) fn as_str(v: &Value) -> Option<&str> {
        v.as_str()
    }
    pub(crate) fn as_u64(v: &Value) -> Option<u64> {
        v.as_u64()
    }
    pub(crate) fn as_i64(v: &Value) -> Option<i64> {
        v.as_i64()
    }
    pub(crate) fn as_object(v: &Value) -> Option<&serde_json::Map<String, Value>> {
        v.as_object()
    }
    pub(crate) fn as_array(v: &Value) -> Option<&Vec<Value>> {
        v.as_array()
    }
}

// ---- tolerant protobuf field reader (for WMPF relay) ----

fn pb_read_varint(buf: &[u8], mut i: usize) -> Result<(u64, usize)> {
    let mut v: u64 = 0;
    let mut shift = 0;
    loop {
        if i >= buf.len() {
            return Err(Error::Protocol("relay: truncated varint".to_owned()));
        }
        let b = buf[i];
        v |= ((b & 0x7f) as u64) << shift;
        i += 1;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return Err(Error::Protocol("relay: varint too long".to_owned()));
        }
    }
    Ok((v, i))
}

fn encode<M: prost::Message + Default>(message: &M) -> Vec<u8> {
    prost::Message::encode_to_vec(message)
}

fn decode<M: prost::Message + Default>(bytes: &[u8]) -> Result<M> {
    M::decode(bytes).map_err(|err| Error::Protocol(format!("the answer is malformed: {err}")))
}

/// Wraps an already-unwrapped inner answer in the `WxaRuntimeTransferResponse` envelope
/// the render layer parses, so channels that bypass `wxaruntime_transfer` (js-login,
/// h5transfer, getoauthcode) still answer in the shape the runtime expects.
fn wrap_transfer_response(inner: &[u8]) -> Vec<u8> {
    // An answer that is already an envelope (base_response present) is passed through.
    if let Ok(wrapper) = decode::<pb::WxaRuntimeTransferResponse>(inner) {
        if wrapper.base_response.is_some() || wrapper.resp_data.is_some() {
            return inner.to_vec();
        }
    }
    encode(&pb::WxaRuntimeTransferResponse {
        base_response: Some(pb::BaseResponse {
            ret: Some(0),
            err_msg: Some("ok".to_owned()),
        }),
        resp_data: Some(inner.to_vec()),
    })
}

fn item(entry: &pb::WxaAppItem, source: &'static str) -> RecentItem {
    RecentItem {
        username: entry.username.clone().unwrap_or_default(),
        update_time: entry.update_time.unwrap_or(0) as i64,
        version_type: entry.version_type.unwrap_or(0),
        order: entry.order.unwrap_or(0),
        is_from_outer: entry.is_from_outer.unwrap_or(false),
        app_desc: entry.app_desc.clone().unwrap_or_default(),
        source,
    }
}

/// Flattens the attribute entries, parsing JSON values.
fn attrs_map(answer: &pb::WxaAttrSyncResponse) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for entry in &answer.attrs {
        let (Some(key), value) = (entry.key.clone(), entry.value.clone().unwrap_or_default())
        else {
            continue;
        };
        let parsed = if value.starts_with('{') || value.starts_with('[') {
            serde_json::from_str::<serde_json::Value>(&value)
                .unwrap_or(serde_json::Value::String(value))
        } else {
            serde_json::Value::String(value)
        };
        out.insert(key, parsed);
    }
    out
}

/// A MAC-shaped device string for the inner header, stable per session.
fn mac_of(session: &Session) -> String {
    let id = session.hex(&session.device_id).unwrap_or_default();
    let mut base = [0u8; 6];
    for (index, byte) in id.iter().take(6).enumerate() {
        base[index] = *byte;
    }
    base[0] = (base[0] | 0x02) & 0xfe;
    base.iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join("-")
}

/// Strips the slashes module names carry.
fn norm_module(module: &str) -> String {
    module.trim().trim_matches('/').to_owned()
}

/// Whether a module name refers to the main package.
fn is_main_module(module: &str) -> bool {
    let normalized = norm_module(module);
    normalized.is_empty() || normalized.eq_ignore_ascii_case("__APP__")
}

/// Downloads one URL, decompresses and validates it, and writes it under its stable name.
fn fetch_into(
    url: &str,
    out_dir: &Path,
    appid: &str,
    package: &mut PackageFile,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) {
    if url.is_empty() {
        package.error = "no download address".to_owned();
        return;
    }
    // A dropped connection or a truncated body is worth another try before the package is
    // reported as failed: the CDN is far more reliable on the second attempt than the report
    // makes it look, and one bad package used to cost the whole restore.
    let mut attempt = 0usize;
    let bytes = loop {
        attempt += 1;
        match http_get(url, cancel) {
            Ok(raw) => {
                let bytes = if package.is_zstd || raw.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
                    match zstd_stream_decode(&raw) {
                        Ok(plain) => {
                            package.is_zstd = true;
                            plain
                        }
                        Err(err) => {
                            if attempt < FETCH_ATTEMPTS {
                                std::thread::sleep(std::time::Duration::from_millis(
                                    300 * attempt as u64,
                                ));
                                continue;
                            }
                            package.error = format!("zstd decompression failed: {err}");
                            return;
                        }
                    }
                } else {
                    raw
                };
                match check_wxapkg(&bytes) {
                    Ok(()) => break bytes,
                    Err(err) if attempt < FETCH_ATTEMPTS => {
                        let _ = err;
                        std::thread::sleep(std::time::Duration::from_millis(300 * attempt as u64));
                    }
                    Err(err) => {
                        package.error = err;
                        return;
                    }
                }
            }
            Err(Error::Cancelled) => {
                package.error = "cancelled".to_owned();
                return;
            }
            Err(err) if attempt < FETCH_ATTEMPTS => {
                let _ = err;
                std::thread::sleep(std::time::Duration::from_millis(300 * attempt as u64));
            }
            Err(err) => {
                package.error = format!("download failed after {attempt} attempts: {err}");
                return;
            }
        }
    };
    let name = package_name(appid, &package.module, package.package_type);
    let path = out_dir.join(name);
    if let Err(err) = std::fs::write(&path, &bytes) {
        package.error = format!("write failed: {err}");
        return;
    }
    use md5::Digest as _;
    let mut hasher = md5::Md5::new();
    hasher.update(&bytes);
    package.md5 = hex_encode(&hasher.finalize());
    package.size = bytes.len() as u64;
    package.saved_path = path.display().to_string();
}

/// How many times one package body is fetched before it is reported as failed.
const FETCH_ATTEMPTS: usize = 3;

/// Plain HTTPS fetch with the downloader's user agent.
///
/// Reads in chunks so a cancellation flag is honoured even in the middle of a large body.
fn http_get(url: &str, cancel: Option<&std::sync::atomic::AtomicBool>) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let response = crate::oauth::agent(std::time::Duration::from_secs(120))?
        .get(url)
        .set("User-Agent", "MicroMessenger Client")
        .set("Accept", "*/*")
        .call()
        .map_err(|err| Error::Network(err.to_string()))?;
    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
            return Err(Error::Cancelled);
        }
        let read = reader
            .read(&mut chunk)
            .map_err(|err| Error::Network(err.to_string()))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(bytes)
}

/// Whether the caller set the cancel flag.
fn cancelled(options: &DownloadOptions) -> bool {
    options
        .cancel
        .as_ref()
        .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
}

/// Decompresses a zstd stream in one shot.
fn zstd_stream_decode(bytes: &[u8]) -> std::result::Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut out = Vec::new();
    zstd::stream::read::Decoder::new(bytes)
        .map_err(|err| err.to_string())?
        .read_to_end(&mut out)
        .map_err(|err| err.to_string())?;
    Ok(out)
}

/// Validates the standard wxapkg marks.
fn check_wxapkg(bytes: &[u8]) -> std::result::Result<(), String> {
    if bytes.len() < 18 {
        return Err(format!("too small to be a wxapkg ({} bytes)", bytes.len()));
    }
    if bytes[0] != 0xBE {
        return Err(format!(
            "first mark 0x{:02x}, not a standard wxapkg",
            bytes[0]
        ));
    }
    if bytes[13] != 0xED {
        return Err(format!(
            "last mark 0x{:02x}, not a standard wxapkg",
            bytes[13]
        ));
    }
    Ok(())
}

/// Stable package file name: `{appid}[_{module}][_t{type}].wxapkg`.
fn package_name(appid: &str, module: &str, package_type: u32) -> String {
    let mut name = if appid.is_empty() { "pkg" } else { appid }.to_owned();
    if !module.is_empty() {
        name.push('_');
        name.push_str(&module.replace(['/', '\\'], "__"));
    }
    if package_type != 0 {
        name.push_str(&format!("_t{package_type}"));
    }
    format!("{name}.wxapkg")
}

/// Reads `subPackages[].root` out of the `app-config.json` inside a downloaded package.
fn subpackage_roots(pkg_path: &Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(pkg_path).ok()?;
    if bytes.len() < 18 || bytes[0] != 0xBE {
        return None;
    }
    let file_count = u32::from_be_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]);
    let mut offset = 18usize;
    for _ in 0..file_count {
        if offset + 4 > bytes.len() {
            break;
        }
        let name_len = u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as usize;
        offset += 4;
        if offset + name_len + 8 > bytes.len() {
            break;
        }
        let name = String::from_utf8_lossy(&bytes[offset..offset + name_len]).into_owned();
        offset += name_len;
        let file_off = u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as usize;
        let file_size = u32::from_be_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        offset += 8;
        if name != "/app-config.json" && name != "app-config.json" {
            continue;
        }
        if file_off + file_size > bytes.len() {
            return None;
        }
        let config: serde_json::Value =
            serde_json::from_slice(&bytes[file_off..file_off + file_size]).ok()?;
        let mut roots = Vec::new();
        for key in ["subPackages", "subpackages"] {
            for entry in config
                .get(key)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(root) = entry.get("root").and_then(|v| v.as_str()) {
                    if !root.is_empty() {
                        roots.push(root.to_owned());
                    }
                }
            }
        }
        return Some(roots);
    }
    None
}

/// Writes the manifest beside the packages.
fn write_manifest(out_dir: &Path, report: &mut DownloadReport) {
    let Ok(bytes) = serde_json::to_vec_pretty(report) else {
        return;
    };
    let path = out_dir.join("manifest.json");
    if std::fs::write(&path, bytes).is_ok() {
        report.manifest_path = path.display().to_string();
    }
}

fn cgi_dump_file(tag: &str, cgi_path: &str, appid: &str, body: &[u8]) {
    let dir = match CGI_DUMP_DIR.lock() {
        Ok(g) => g.clone(),
        Err(_) => None,
    };
    let Some(dir) = dir else { return };
    // If the no_cgi_dump switch file exists, stop writing dumps (the log once grew past 800 MB).
    if read_flag("no_cgi_dump").is_some() {
        return;
    }
    let path = dir.join("cgi_dump.log");
    let line = format!(
        "{}\t{}\t{}\t{}\t{}\n",
        tag,
        cgi_path,
        appid,
        body.len(),
        cgi_b64(body)
    );
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

#[allow(dead_code)]
fn cgi_ascii(b: &[u8]) -> String {
    // By default only a short summary is logged; for debugging, create files/log_full to log full packets (capped at 3000
    // characters, since logd truncates long lines). The full packet is always in cgi_dump.log.
    let cap = if read_flag("log_full").is_some() {
        3000
    } else {
        220
    };
    let mut s = String::with_capacity(b.len().min(cap));
    for &c in b.iter().take(cap) {
        if c.is_ascii_graphic() || c == b' ' {
            s.push(c as char);
        } else {
            s.push('.');
        }
    }
    if b.len() > cap {
        s.push_str(&format!("...(total {})", b.len()));
    }
    s
}

/// Log one summary line to logcat and persist the full packet as base64.
///
/// This used to log chunked base64 to logcat, but one large response (76 chunks for wxajsapiinfo)
/// could flush the whole ring buffer and destroy earlier verdict logs. The full packet is already
/// in cgi_dump.log, so logcat keeps only the summary.
///
/// Disabled on desktop by default: these lines carry raw request bodies and must not appear in a user terminal.
/// The Android bridge sets the dump directory at init and keeps the old behaviour; on desktop, set
/// `WXAPKG_CGI_DUMP=1`。
/// Release builds (WAD_PUBLIC=1) emit no request summaries or dumps, so logs cannot leak implementation details.
#[inline]
fn public_build() -> bool {
    matches!(option_env!("WAD_PUBLIC"), Some("1"))
}

#[allow(dead_code)]
fn cgi_dump(tag: &str, cgi_path: &str, appid: &str, body: &[u8]) {
    if public_build() {
        return;
    }
    let dumping = CGI_DUMP_DIR
        .lock()
        .map(|dir| dir.is_some())
        .unwrap_or(false);
    if !dumping && std::env::var_os("WXAPKG_CGI_DUMP").is_none() {
        return;
    }
    wxa_log(&format!(
        "{tag} cgi_path={cgi_path} appid={appid} len={} ascii={}",
        body.len(),
        cgi_ascii(body)
    ));
    cgi_dump_file(tag, cgi_path, appid, body);
}

// ---- Android logcat helpers (for bridge diagnostics) ----
#[allow(dead_code)]
pub(crate) fn wxa_log(msg: &str) {
    if public_build() {
        return;
    }
    #[cfg(target_os = "android")]
    {
        use std::ffi::CString;
        unsafe extern "C" {
            #[link_name = "__android_log_print"]
            fn alp(prio: i32, tag: *const u8, fmt: *const u8, ...) -> i32;
        }
        let tag = CString::new("WxaRelay").unwrap();
        let m = CString::new(msg.replace('\n', " ")).unwrap();
        unsafe { alp(4, tag.as_ptr(), b"%s\0".as_ptr(), m.as_ptr()) };
    }
    #[cfg(not(target_os = "android"))]
    {
        // Desktop builds stay quiet unless the operator explicitly asks for CGI diagnostics;
        // request summaries must never appear on stdout, and stderr should stay clean too.
        if std::env::var_os("WXAPKG_CGI_DUMP").is_some() {
            eprintln!("[wxa] {msg}");
        }
    }
}
// ---- Full CGI diagnostic log helpers ----
#[allow(dead_code)]
fn cgi_b64(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// Android truncates one logcat message to ~1000 characters, so chunked base64 can never rebuild a full body.
/// During diagnosis the full packet goes straight to the app-private directory, to be pulled and parsed later.
static CGI_DUMP_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Set the CGI dump directory (the FFI layer passes the app files directory at init).
pub fn set_cgi_dump_dir(dir: &str) {
    if let Ok(mut guard) = CGI_DUMP_DIR.lock() {
        *guard = if dir.is_empty() {
            None
        } else {
            Some(PathBuf::from(dir))
        };
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_username;

    #[test]
    fn a_bare_gh_id_gains_the_app_suffix() {
        assert_eq!(normalize_username("gh_7d2ae877326d"), "gh_7d2ae877326d@app");
        assert_eq!(
            normalize_username("gh_7d2ae877326d@app"),
            "gh_7d2ae877326d@app"
        );
        assert_eq!(normalize_username(" wxabc "), "wxabc");
    }

    use super::*;

    #[test]
    fn module_names_normalize_and_classify() {
        assert_eq!(norm_module("/sub/pkg/"), "sub/pkg");
        assert!(is_main_module(""));
        assert!(is_main_module("__APP__"));
        assert!(!is_main_module("/pages/detail/"));
    }

    /// The real 14-byte `checkwebviewurl` answer captured off the device:
    /// outer base_response.ret=1 plus resp_data carrying its own ret=1.
    /// Both have to become 0, and the total length must not move — WMPF reads
    /// this buffer through length prefixes it already parsed.
    #[test]
    fn checkwebviewurl_verdict_is_forced_to_zero_in_place() {
        let mut buf = vec![
            0x0a, 0x06, 0x08, 0x01, 0x12, 0x02, 0x0a,
            0x00, // f1 BaseResponse{ret=1, errMsg{""}}
            0x12, 0x04, 0x0a, 0x02, 0x08, 0x01, //            f2 resp_data = kx6{ret=1}
        ];
        let before = buf.len();
        assert_eq!(zero_base_response_ret(&mut buf), 2);
        assert_eq!(buf.len(), before);
        assert_eq!(
            buf,
            vec![
                0x0a, 0x06, 0x08, 0x00, 0x12, 0x02, 0x0a, 0x00, 0x12, 0x04, 0x0a, 0x02, 0x08, 0x00,
            ]
        );
    }

    /// An already-successful answer must be left completely alone.
    #[test]
    fn a_zero_verdict_is_left_untouched() {
        let mut buf = vec![0x0a, 0x02, 0x08, 0x00, 0x12, 0x00];
        let original = buf.clone();
        assert_eq!(zero_base_response_ret(&mut buf), 0);
        assert_eq!(buf, original);
    }

    /// A multi-byte ret cannot be shrunk without moving every following offset,
    /// so it is deliberately left as-is rather than corrupting the packet.
    #[test]
    fn a_wide_ret_is_not_shortened() {
        let mut buf = vec![0x0a, 0x03, 0x08, 0x80, 0x01];
        let original = buf.clone();
        assert_eq!(zero_base_response_ret(&mut buf), 0);
        assert_eq!(buf, original);
    }

    /// Only field 1 is swapped; every other field survives byte for byte and in order.
    #[test]
    fn base_request_injection_keeps_the_rest_of_the_message() {
        // GetA8KeyReq-ish: f1 old BaseRequest, f2 op_code=2, f7 req_url="http://x"
        let msg = vec![
            0x0a, 0x02, 0x08, 0x09, // f1 = old BaseRequest{ret-ish 9}
            0x10, 0x02, //           f2 varint 2
            0x3a, 0x07, b'h', b't', b't', b'p', b':', b'/', b'/', // f7 "http://"
        ];
        let br = vec![0x10, 0x2a]; // new BaseRequest{f2=42}
        let out = replace_field1(&msg, &br);
        assert_eq!(&out[..4], &[0x0a, 0x02, 0x10, 0x2a]); // new f1 first
        assert_eq!(&out[4..], &msg[4..]); // old f1 dropped, tail verbatim
        let new_f1 = pb_field(&out, 1).expect("f1 present");
        assert_eq!(pb_varint(new_f1, 2), Some(42));
        assert_eq!(pb_varint(&out, 2), Some(2)); // op_code untouched
        assert_eq!(
            pb_field(&out, 7).map(|v| String::from_utf8_lossy(v).into_owned()),
            Some("http://".to_owned())
        );
    }

    /// The instance id WMPF actually sent: anonymous hash plus the WMPF host appid.
    /// `both` has to turn it into the yyb-go shape — real uin, empty host — and leave
    /// ts/version/device alone, because device=2 is what yyb-go sends too.
    #[test]
    fn instance_id_spoof_matches_the_yyb_shape() {
        let src = "hash=0&ts=1793307073881&host=wxd44977328b36e647&version=570556656&device=2";
        assert_eq!(
            spoof_instance_id(src, "both", 536776121, 0x2800_4734),
            "hash=536776121&ts=1793307073881&host=&version=570556656&device=2"
        );
        assert_eq!(
            spoof_instance_id(src, "hash", 536776121, 0x2800_4734),
            "hash=536776121&ts=1793307073881&host=wxd44977328b36e647&version=570556656&device=2"
        );
        assert_eq!(
            spoof_instance_id(src, "host", 536776121, 0x2800_4734),
            "hash=0&ts=1793307073881&host=&version=570556656&device=2"
        );
        // `all` also swaps ts to unix seconds and version to our own client_ver
        let full = spoof_instance_id(src, "all", 536776121, 0x2800_4734);
        assert!(full.starts_with("hash=536776121&ts="), "{full}");
        assert!(
            full.contains(&format!("&host=&version={}&device=2", 0x2800_4734u32)),
            "{full}"
        );
        assert_eq!(ts_of(&full).len(), 10, "ts should be unix seconds: {full}");
    }

    fn ts_of(spoofed: &str) -> String {
        spoofed
            .split('&')
            .find(|p| p.starts_with("ts="))
            .map(|p| p[3..].to_owned())
            .unwrap_or_default()
    }

    /// Substituting a longer string must keep every other field byte-identical.
    #[test]
    fn field_replacement_survives_a_length_change() {
        // f2 varint 7, f11 "ab", f13 varint 9
        let msg = vec![0x10, 0x07, 0x5a, 0x02, b'a', b'b', 0x68, 0x09];
        let out = replace_field(&msg, 11, b"much-longer");
        assert_eq!(
            pb_field_first(&out, 11).map(|v| v.to_vec()),
            Some(b"much-longer".to_vec())
        );
        assert_eq!(pb_varint(&out, 2), Some(7));
        assert_eq!(pb_varint(&out, 13), Some(9));
    }

    /// Shape of the real 6447-byte answer, reduced to the fields that matter:
    /// envelope{ base_response, biz{ base_response, action{code,url,f3}, appid } }.
    fn fake_launch_resp(code: u8) -> Vec<u8> {
        let url = b"https://mp.weixin.qq.com/publicpoc/secitptpagepub?x=1";
        let mut action = vec![0x08, code, 0x12, url.len() as u8];
        action.extend_from_slice(url);
        action.extend_from_slice(&[0x18, 0x00]); // f3 = 0
        let base_resp = vec![0x0a, 0x02, 0x08, 0x00]; // BaseResponse{ret=0}
        let mut biz = base_resp.clone();
        biz.extend_from_slice(&[0x12, action.len() as u8]);
        biz.extend_from_slice(&action);
        biz.extend_from_slice(&[0x42, 0x03, b'w', b'x', b'1']); // f8 appid
        let mut out = base_resp;
        out.extend_from_slice(&[0x12, biz.len() as u8]);
        out.extend_from_slice(&biz);
        out
    }

    /// ActionCode 2 -> 1, same length, everything else byte-identical.
    #[test]
    fn an_h5_verdict_is_turned_into_a_native_launch() {
        let mut buf = fake_launch_resp(2);
        let before = buf.clone();
        assert!(force_launch_action_ok(&mut buf));
        assert_eq!(buf.len(), before.len());
        assert_eq!(buf.iter().zip(&before).filter(|(a, b)| a != b).count(), 1);
        let (code, url) = launch_action(&buf).expect("action");
        assert_eq!(code, Some(1));
        // the url stays in the packet; WMPF only reads it when the code is 2
        assert!(url.unwrap().contains("secitptpagepub"));
    }

    /// An already-native launch must not be touched, or a working Mini Program
    /// would start reporting a bogus verdict.
    #[test]
    fn a_native_verdict_is_left_alone() {
        let mut buf = fake_launch_resp(1);
        let before = buf.clone();
        assert!(!force_launch_action_ok(&mut buf));
        assert_eq!(buf, before);
    }

    #[test]
    fn launch_verdict_reads_the_action_code() {
        // f2{ f2{ f1=2, f2="https://h5" } }
        let url = b"https://h5";
        let mut action = vec![0x08, 0x02, 0x12, url.len() as u8];
        action.extend_from_slice(url);
        let mut biz = vec![0x12, action.len() as u8];
        biz.extend_from_slice(&action);
        let mut resp = vec![0x12, biz.len() as u8];
        resp.extend_from_slice(&biz);
        let a = pb_field(&resp, 2).and_then(|b| pb_field(b, 2));
        assert_eq!(a.and_then(|x| pb_varint(x, 1)), Some(2));
        assert_eq!(
            a.and_then(|x| pb_field(x, 2))
                .map(|v| String::from_utf8_lossy(v).into_owned()),
            Some("https://h5".to_owned())
        );
    }

    #[test]
    fn package_names_carry_module_and_type() {
        assert_eq!(package_name("wxabc", "", 0), "wxabc.wxapkg");
        assert_eq!(
            package_name("wxabc", "/sub/pkg/", 0),
            "wxabc___sub__pkg__.wxapkg"
        );
        assert_eq!(package_name("wxabc", "", 8), "wxabc_t8.wxapkg");
    }

    #[test]
    fn wxapkg_marks_are_validated() {
        let mut pkg = vec![0u8; 18];
        pkg[0] = 0xBE;
        pkg[13] = 0xED;
        assert!(check_wxapkg(&pkg).is_ok());
        pkg[0] = 0x00;
        assert!(check_wxapkg(&pkg).is_err());
        assert!(check_wxapkg(&[0xBE]).is_err());
    }

    #[test]
    fn the_mac_shape_follows_the_reference_format() {
        let session = Session {
            device_id: hex_encode(&[0x18, 0xd7, 0x75, 0xe4, 0xaf, 0xa4, 1, 2]),
            ..Session::default()
        };
        let mac = mac_of(&session);
        assert_eq!(mac, "1A-D7-75-E4-AF-A4");
        let first = u8::from_str_radix(&mac[..2], 16).unwrap();
        assert!(first & 0x02 != 0, "locally administered bit set");
    }

    #[test]
    fn subpackage_roots_read_from_a_real_shaped_package() {
        let config = br#"{"subPackages":[{"root":"pkgA"},{"root":"pkgB"}]}"#;
        let mut pkg: Vec<u8> = Vec::new();
        pkg.push(0xBE);
        pkg.extend_from_slice(&[0; 4]); // unknown info version
        pkg.extend_from_slice(&1u32.to_be_bytes()); // index length, fixed below
        pkg.extend_from_slice(&0u32.to_be_bytes()); // body length placeholder
        pkg.push(0xED);
        pkg.extend_from_slice(&1u32.to_be_bytes()); // one file
        let _ = &mut pkg;

        let name = b"/app-config.json";
        let index_start = pkg.len();
        pkg.extend_from_slice(&(name.len() as u32).to_be_bytes());
        pkg.extend_from_slice(name);
        let file_off = 18usize + (name.len() + 12); // after the single index entry
        pkg.extend_from_slice(&(file_off as u32).to_be_bytes());
        pkg.extend_from_slice(&(config.len() as u32).to_be_bytes());
        let index_len = pkg.len() - index_start;
        pkg[5..9].copy_from_slice(&(index_len as u32).to_be_bytes());
        pkg.extend_from_slice(config);
        pkg[9..13].copy_from_slice(&(config.len() as u32).to_be_bytes());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.wxapkg");
        std::fs::write(&path, &pkg).unwrap();
        let roots = subpackage_roots(&path).expect("roots");
        assert_eq!(roots, vec!["pkgA".to_owned(), "pkgB".to_owned()]);
    }
}
