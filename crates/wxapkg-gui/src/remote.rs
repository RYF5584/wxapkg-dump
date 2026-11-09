// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Authorized remote download, implemented natively.
//!
//! The window drives [`wxapkg_remote::Remote`] directly: no shared library, no HTTP hop.
//! Exactly one account is remembered; a confirmed scan replaces it, and deleting it clears
//! every cache tied to it. A scan that landed on a restart is resumed from the pending
//! logins the store keeps.

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use wxapkg_remote::{Account, AccountState, DownloadOptions, Remote};

use crate::run_job;

/// The signed-in account, in the shape the window uses.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    nick_name: String,
    avatar: String,
    status: String,
    online: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    openid: Option<String>,
    last_refresh_at: i64,
}

impl From<&Account> for AccountView {
    fn from(account: &Account) -> Self {
        Self {
            nick_name: account.nick_name.clone(),
            avatar: account.avatar.clone(),
            status: account.state.label(false),
            online: account.state == AccountState::Online,
            openid: account.openid.clone(),
            last_refresh_at: account.last_refresh_at,
        }
    }
}

/// One recently used Mini Program.
///
/// The icon is deliberately not inlined: a recent list holds hundreds of entries. The window
/// asks for the icons it actually paints through [`ilink_icon`].
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentView {
    username: String,
    appid: String,
    nick_name: String,
    icon_url: String,
    last_used_at: i64,
    source: String,
}

/// Everything the account view needs at once.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct YybStatus {
    /// Always true now: the native implementation needs no library.
    available: bool,
    reason: Option<String>,
    /// Which implementation answers ("native" for the built-in engine).
    implementation: Option<String>,
    /// The one signed-in account, or `null` when nobody is.
    account: Option<AccountView>,
    /// A session that looks expired, phrased for a person.
    warnings: Vec<String>,
}

/// Opens the remote capability.
fn remote() -> Result<Remote, String> {
    Remote::open().map_err(|err| format!("{err}"))
}

/// Reports the capability and the signed-in account.
#[tauri::command]
pub async fn ilink_status(app: AppHandle) -> Result<YybStatus, String> {
    run_job(&app, "remote status", move || {
        let remote = remote()?;
        let account = remote.account().map_err(|err| err.to_string())?;
        let warnings = account
            .as_ref()
            .filter(|account| account.state == AccountState::Stale)
            .map(|_| "登录已过期，请重新扫码登录".to_owned())
            .into_iter()
            .collect();
        Ok(YybStatus {
            available: true,
            reason: None,
            implementation: Some("native".to_owned()),
            account: account.as_ref().map(AccountView::from),
            warnings,
        })
    })
    .await
}

/// A QR code ready to display, plus the token used to poll it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginQrView {
    session_id: String,
    qr_content: String,
    /// Image source for the window: the upstream login QR is served as an image, so this is
    /// that URL. Only a payload that is not a URL is rendered locally as a QR bitmap.
    qr_image: String,
    /// Whether `qr_image` is a remote image (the window may want to show a hint on failure).
    qr_is_url: bool,
    expires_in: u32,
}

/// Opens a login session and renders the QR code. A confirmed scan replaces the account.
#[tauri::command]
pub async fn ilink_login_begin(app: AppHandle) -> Result<LoginQrView, String> {
    run_job(&app, "remote login", move || {
        let remote = remote()?;
        let session = remote.begin_login().map_err(|err| err.to_string())?;
        // Upstream returns the address of a rendered QR image, not a payload to encode. If we
        // encoded it, scanning would open a page containing another QR — one scan too deep.
        let is_url =
            session.qr_url.starts_with("http://") || session.qr_url.starts_with("https://");
        let image = if is_url {
            // Inline the bitmap so the window never depends on webview networking (CSP,
            // proxies and referer rules all differ from a browser). A failure falls back to
            // the URL.
            remote
                .qr_image(&session)
                .ok()
                .and_then(|bytes| crate::image_data_url_from_bytes(&session.qr_url, &bytes))
                .unwrap_or_else(|| session.qr_url.clone())
        } else {
            crate::qr_data_url(&session.qr_url)?
        };
        Ok(LoginQrView {
            session_id: session.session_id.clone(),
            qr_content: session.qr_url.clone(),
            qr_image: image,
            qr_is_url: is_url,
            expires_in: session.remaining(),
        })
    })
    .await
}

/// Login state, in the shape the window uses.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStatusView {
    status: String,
    nick_name: Option<String>,
    avatar: Option<String>,
    message: Option<String>,
}

/// Polls a login session; a confirmed scan is exchanged into the account on the spot.
#[tauri::command]
pub async fn ilink_login_poll(
    app: AppHandle,
    session_id: String,
) -> Result<LoginStatusView, String> {
    run_job(&app, "remote login check", move || {
        let remote = remote()?;
        let status = remote
            .poll_pending(&session_id)
            .map_err(|err| err.to_string())?;
        let (state, message) = match &status {
            wxapkg_remote::LoginStatus::Waiting => ("waiting", None),
            wxapkg_remote::LoginStatus::Scanned => ("scanned", None),
            wxapkg_remote::LoginStatus::Authorized { .. } => ("confirmed", None),
            wxapkg_remote::LoginStatus::Expired => {
                ("expired", Some("二维码已过期，请重新获取".to_owned()))
            }
            wxapkg_remote::LoginStatus::Cancelled => {
                ("cancelled", Some("已在手机上取消登录".to_owned()))
            }
        };
        let account = if state == "confirmed" {
            remote.account().ok().flatten()
        } else {
            None
        };
        Ok(LoginStatusView {
            status: state.to_owned(),
            nick_name: account.as_ref().map(|account| account.nick_name.clone()),
            avatar: account.as_ref().map(|account| account.avatar.clone()),
            message,
        })
    })
    .await
}

/// Deletes the signed-in account together with every cache tied to it.
#[tauri::command]
pub async fn ilink_account_delete(app: AppHandle) -> Result<(), String> {
    run_job(&app, "remote delete", move || {
        let remote = remote()?;
        remote.delete_account().map_err(|err| err.to_string())
    })
    .await
}

/// Returns the cached icon of one Mini Program, as a `data:` URL.
#[tauri::command]
pub async fn ilink_icon(username: String) -> Result<Option<String>, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let remote = Remote::open().map_err(|err| err.to_string())?;
        Ok(remote
            .icon(&username)
            .and_then(|path| crate::image_data_url(path.as_path())))
    })
    .await
    .map_err(|err| format!("the icon job could not be scheduled: {err}"))?;
    result
}

/// Recently used Mini Programs, one page of the signed-in account.
#[tauri::command]
pub async fn ilink_recent(
    app: AppHandle,
    page: Option<u32>,
    size: Option<u32>,
) -> Result<Vec<RecentView>, String> {
    let page = page.unwrap_or(1).max(1);
    let size = size.unwrap_or(50).clamp(1, 100);
    run_job(&app, "remote recent", move || {
        let remote = remote()?;
        // Never block on metadata here: the window paints the rows at once and fills in
        // names, appids and icons through `ilink_enrich_recent`.
        let list = remote
            .recent_cached(page, size)
            .map_err(|err| err.to_string())?;
        let views: Vec<RecentView> = list
            .iter_all()
            .map(|item| {
                let meta = remote.program_meta(&item.username);
                RecentView {
                    username: item.username.clone(),
                    appid: meta.as_ref().map(|m| m.appid.clone()).unwrap_or_default(),
                    nick_name: meta
                        .as_ref()
                        .map(|m| m.nick_name.clone())
                        .unwrap_or_default(),
                    icon_url: meta.as_ref().map(|m| m.avatar.clone()).unwrap_or_default(),
                    last_used_at: item.update_time,
                    source: item.source.to_owned(),
                }
            })
            .collect();
        Ok(views)
    })
    .await
}

/// Resolves names and icons for a freshly listed page, emitting one event per program.
///
/// The list command deliberately does not wait for this: each row appears immediately and
/// is patched as soon as its metadata lands, which is what makes a 30-row page feel instant.
#[tauri::command]
pub async fn ilink_enrich_recent(app: AppHandle, usernames: Vec<String>) -> Result<usize, String> {
    let event_app = app.clone();
    let outcome = run_job(&app, "remote enrich", move || {
        let remote = remote()?;
        let rows = remote
            .enrich_recent(&usernames)
            .map_err(|err| err.to_string())?;
        for (username, meta) in &rows {
            let icon = meta
                .icon_path
                .as_ref()
                .and_then(|path| crate::image_data_url(path.as_path()));
            let _ = event_app.emit(
                "wxapkg-recent-meta",
                serde_json::json!({
                    "username": username,
                    "appid": meta.appid,
                    "nickName": meta.nick_name,
                    "icon": icon,
                }),
            );
        }
        Ok(rows.len())
    })
    .await;
    outcome
}

/// Cancellation flags for downloads that are currently running, keyed by the id the window
/// uses for the live row.
fn cancel_registry() -> &'static std::sync::Mutex<
    std::collections::HashMap<String, std::sync::Arc<std::sync::atomic::AtomicBool>>,
> {
    static REGISTRY: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<String, std::sync::Arc<std::sync::atomic::AtomicBool>>,
        >,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Asks a running download to stop. Returns false when nothing with that id is running.
#[tauri::command]
pub fn ilink_cancel_download(id: String) -> bool {
    let flag = cancel_registry()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&id).cloned());
    match flag {
        Some(flag) => {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Basic attributes of one Mini Program.
#[tauri::command]
pub async fn ilink_info(
    app: AppHandle,
    appid: Option<String>,
    username: Option<String>,
) -> Result<serde_json::Value, String> {
    run_job(&app, "remote info", move || {
        let remote = remote()?;
        let info = remote
            .info(appid.as_deref(), username.as_deref())
            .map_err(|err| err.to_string())?;
        Ok(serde_json::json!({
            "appid": info.appid,
            "username": info.username,
            "nickName": info.nick_name,
            "signature": info.signature,
            "avatar": info.avatar,
            "version": info.version,
            "clientVersion": info.client_version,
            "updateTime": info.update_time,
            "modules": info
                .attrs
                .get("WxaAppVersionInfo")
                .and_then(|v| v.get("module_list"))
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        }))
    })
    .await
}

/// One downloaded package, in the shape the window uses.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageView {
    name: String,
    module: String,
    package_type: u32,
    version: u32,
    path: String,
    size: u64,
    error: String,
    skipped: bool,
}

/// What a remote download produced.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportView {
    appid: String,
    username: String,
    out_dir: String,
    zip: Option<String>,
    unpacked_dir: Option<String>,
    packages: Vec<PackageView>,
}

/// What a remote download produced, plus where reconstruction landed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadView {
    report: ReportView,
    /// Project directory when the GUI reconstructed the sources itself.
    project_dir: Option<String>,
}

/// Downloads the packages of one Mini Program, optionally reconstructing the sources.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn ilink_download(
    app: AppHandle,
    appid: Option<String>,
    username: Option<String>,
    out: Option<String>,
    beautify: bool,
    unpack: bool,
    all_packages: bool,
    id: Option<String>,
) -> Result<DownloadView, String> {
    let base = match out
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(explicit) => crate::host::resolve_output(Some(explicit), ""),
        None => crate::host::workspace(),
    };
    let label = appid
        .clone()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| username.clone())
        .unwrap_or_default();
    let key = id
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| label.clone());
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if !key.is_empty() {
        if let Ok(mut registry) = cancel_registry().lock() {
            registry.insert(key.clone(), cancel.clone());
        }
    }
    crate::job(&app, "download", "下载小程序", "start");
    let progress_app = app.clone();
    let progress_label = key.clone();
    let progress: wxapkg_remote::wxa::DownloadProgress =
        std::sync::Arc::new(move |done, total, module| {
            let _ = progress_app.emit(
                "wxapkg-download-progress",
                serde_json::json!({
                    "id": progress_label,
                    "done": done,
                    "total": total,
                    "module": module,
                }),
            );
            crate::job_with(
                &progress_app,
                "download",
                "下载小程序",
                "progress",
                Some(done),
                Some(total),
                Some(format!("{done}/{total}")),
            );
        });
    let outcome = run_job(&app, "remote download", move || {
        let remote = remote()?;
        let requested_appid = appid
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_default();
        let requested_username = username
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_default();
        // The folder is named after the appid even when the caller addressed the program by
        // its original id: resolve it before anything is written.
        let resolved_appid = if requested_appid.is_empty() && !requested_username.is_empty() {
            remote
                .info(None, Some(requested_username.as_str()))
                .ok()
                .map(|info| info.appid)
                .filter(|value| !value.is_empty())
                .unwrap_or_default()
        } else {
            requested_appid.clone()
        };
        let folder = if resolved_appid.is_empty() {
            requested_username.replace('/', "_")
        } else {
            resolved_appid.clone()
        };
        let dir = base
            .join("original_wxapkg")
            .join(wxapkg_core::paths::sanitize_segment(&folder))
            .join("wxapkg");
        let options = DownloadOptions {
            appid: resolved_appid.clone(),
            username: requested_username.clone(),
            out_dir: dir.clone(),
            all_packages,
            progress: Some(progress),
            cancel: Some(cancel.clone()),
            ..DownloadOptions::default()
        };
        let report = remote.download(&options).map_err(|err| err.to_string())?;

        let project_dir = if unpack {
            let paths: Vec<std::path::PathBuf> = report
                .packages
                .iter()
                .filter(|package| package.error.is_empty() && !package.saved_path.is_empty())
                .map(|package| std::path::PathBuf::from(&package.saved_path))
                .collect();
            if paths.is_empty() {
                None
            } else {
                // Containers live in `<out>/original_wxapkg`; the project lands next to
                // that archive, in `<out>/<appid>` — the same shape a local restore uses.
                let mut request = wxapkg_core::pipeline::UnpackRequest::new(
                    wxapkg_core::pipeline::Source::Paths(paths.clone()),
                    dir.clone(),
                );
                if !report.appid.is_empty() {
                    request.project_folder = Some(format!("unpack_{}", report.appid));
                }
                request.clean = true;
                request.wxid = (!report.appid.is_empty()).then(|| report.appid.clone());
                request.manifest = wxapkg_core::pipeline::find_manifest(&paths);
                if beautify {
                    request.options.format = Some(wxapkg_core::beautify::FormatOptions::default());
                }
                let build =
                    wxapkg_core::pipeline::unpack(&request).map_err(|err| err.to_string())?;
                Some(build.out_dir)
            }
        } else {
            None
        };

        Ok(DownloadView {
            report: ReportView {
                appid: report.appid.clone(),
                username: report.username.clone(),
                out_dir: report.out_dir.clone(),
                zip: None,
                unpacked_dir: project_dir.clone(),
                packages: report
                    .packages
                    .iter()
                    .map(|package| PackageView {
                        name: package
                            .saved_path
                            .rsplit('/')
                            .next()
                            .unwrap_or(&package.saved_path)
                            .to_owned(),
                        module: package.module.clone(),
                        package_type: package.package_type,
                        version: package.version,
                        path: package.saved_path.clone(),
                        size: package.size,
                        error: package.error.clone(),
                        skipped: package.skipped,
                    })
                    .collect(),
            },
            project_dir,
        })
    })
    .await;

    if !key.is_empty() {
        if let Ok(mut registry) = cancel_registry().lock() {
            registry.remove(&key);
        }
    }
    match &outcome {
        Ok(view) => crate::job_with(
            &app,
            "download",
            "下载小程序",
            "done",
            Some(view.report.packages.len()),
            Some(view.report.packages.len()),
            Some(format!(
                "{} 个包，{} 个失败",
                view.report.packages.len(),
                view.report
                    .packages
                    .iter()
                    .filter(|package| !package.error.is_empty() && !package.skipped)
                    .count()
            )),
        ),
        Err(err) => crate::job_with(
            &app,
            "download",
            "下载小程序",
            "failed",
            None,
            None,
            Some(err.clone()),
        ),
    }
    outcome
}
