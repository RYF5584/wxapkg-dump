// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Remote download commands for builds without the feature.
//!
//! The window is the same either way: it asks for status, and if remote download is not
//! compiled in, the status says so and every action explains itself instead of failing
//! silently. Keep this file in step with `remote.rs` — the command names are the contract.

use serde::Serialize;

/// Answer to "can this build download remotely".
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct YybStatusView {
    available: bool,
    reason: Option<String>,
    implementation: Option<String>,
    account: Option<AccountView>,
    warnings: Vec<String>,
}

/// One account, in the shape the window reads.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    nick_name: String,
    avatar: String,
    status: String,
    online: bool,
    last_refresh_at: i64,
}

const REASON: &str = "本次构建未包含远程下载（编译时关闭了 `remote` 特性）";

fn disabled() -> String {
    REASON.to_owned()
}

/// Reports that remote download is not compiled in.
#[tauri::command]
pub async fn ilink_status() -> Result<YybStatusView, String> {
    Ok(YybStatusView {
        available: false,
        reason: Some(disabled()),
        implementation: None,
        account: None,
        warnings: vec![disabled()],
    })
}

/// Remote download needs a transport; this build has none.
#[tauri::command]
pub async fn ilink_login_begin() -> Result<(), String> {
    Err(disabled())
}

/// Remote download needs a transport; this build has none.
#[tauri::command]
pub async fn ilink_login_poll(session_id: String) -> Result<(), String> {
    let _ = session_id;
    Err(disabled())
}

/// Remote download needs a transport; this build has none.
#[tauri::command]
pub async fn ilink_account_delete() -> Result<(), String> {
    Err(disabled())
}

/// Remote download needs a transport; this build has none.
#[tauri::command]
pub async fn ilink_recent(page: Option<u32>, size: Option<u32>) -> Result<(), String> {
    let _ = (page, size);
    Err(disabled())
}

/// No transport means no cached remote icons.
#[tauri::command]
pub async fn ilink_icon(username: String) -> Result<Option<String>, String> {
    let _ = username;
    Ok(None)
}

/// Remote download needs a transport; this build has none.
#[tauri::command]
pub async fn ilink_info(appid: Option<String>, username: Option<String>) -> Result<(), String> {
    let _ = (appid, username);
    Err(disabled())
}

/// Remote download needs a transport; this build has none.
#[tauri::command]
pub async fn ilink_download(
    appid: Option<String>,
    username: Option<String>,
    out: Option<String>,
    unpack: bool,
    beautify: bool,
    all_packages: bool,
    id: Option<String>,
) -> Result<(), String> {
    let _ = (appid, username, out, unpack, beautify, all_packages, id);
    Err(disabled())
}

/// Metadata enrichment needs the transport this build does not have.
#[tauri::command]
pub async fn ilink_enrich_recent(usernames: Vec<String>) -> Result<usize, String> {
    let _ = usernames;
    Err(disabled())
}

/// Nothing is downloading in a build without the transport.
#[tauri::command]
pub fn ilink_cancel_download(id: String) -> bool {
    let _ = id;
    false
}
