// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Desktop application for wxapkg.
//!
//! The GUI is a standalone program: every capability the CLI offers is implemented here as a
//! Tauri command that calls `wxapkg-core` / `wxapkg-remote` directly. No listener is opened for
//! the window's own work, and nothing here ever asks for an access key.
//!
//! The public HTTP API is a separate concern that the operator may start from the service
//! panel (or with `wxapkg-dump --gui --server`): it is hosted in-process by `wxapkg-server` and
//! is the only listener this application can open.
//!
//! This crate lives outside the Cargo workspace so that `cargo build --workspace` never
//! requires the Tauri toolchain.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod host;
mod local;
mod platform;
#[cfg(feature = "remote")]
mod remote;
#[cfg(not(feature = "remote"))]
#[path = "remote_off.rs"]
mod remote;
mod service;
mod settings;

use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};

/// Event channel the window's log drawer subscribes to.
pub const LOG_EVENT: &str = "wxapkg-log";

/// Event channel carrying long-running job progress.
pub const JOB_EVENT: &str = "wxapkg-job";

/// One progress update for a long-running job.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobEvent {
    /// Job identifier, so the window can tell updates apart.
    pub id: String,
    /// Human readable name of the job.
    pub label: String,
    /// `start`, `progress`, `done` or `failed`.
    pub phase: String,
    /// Units completed, when the job can count them.
    pub done: Option<usize>,
    /// Units in total, when the job can count them.
    pub total: Option<usize>,
    /// Result summary or failure reason.
    pub message: Option<String>,
}

/// One line shown in the log drawer.
#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    /// `info`, `warn` or `error`.
    pub kind: String,
    /// Human readable text.
    pub line: String,
}

/// Publishes a line to the log drawer; a failure only means the window is gone.
pub fn log(app: &AppHandle, kind: &str, line: impl Into<String>) {
    let _ = app.emit(
        LOG_EVENT,
        LogLine {
            kind: kind.to_owned(),
            line: line.into(),
        },
    );
}

/// Publishes one job progress update; the drawers ignore updates for a finished job.
pub fn job(app: &AppHandle, id: &str, label: &str, phase: &str) {
    job_with(app, id, label, phase, None, None, None);
}

/// Publishes a job update carrying counters and a message.
pub fn job_with(
    app: &AppHandle,
    id: &str,
    label: &str,
    phase: &str,
    done: Option<usize>,
    total: Option<usize>,
    message: Option<String>,
) {
    let _ = app.emit(
        JOB_EVENT,
        JobEvent {
            id: id.to_owned(),
            label: label.to_owned(),
            phase: phase.to_owned(),
            done,
            total,
            message,
        },
    );
}

/// State shared with the commands.
#[derive(Default)]
pub struct Shell {
    /// The optional public API, started from the service panel or by `--gui --server`.
    public: Mutex<Option<service::Listener>>,
}

impl Shell {
    /// The public listener slot, owned by the service panel.
    pub fn public(&self) -> &Mutex<Option<service::Listener>> {
        &self.public
    }
}

/// Records a panic instead of losing it.
///
/// The release profile unwinds, so a panic no longer kills the process; this hook makes sure
/// the reason survives the toast that replaced it.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info.to_string();
        if let Some(dir) = crate::platform::logs_dir() {
            let _ = crate::platform::ensure_dir(&dir);
            let path = dir.join("panic.log");
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_secs())
                .unwrap_or(0);
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "[{stamp}] {message}");
            }
        }
        default(info);
    }));
}

fn main() {
    install_panic_hook();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Shell::default())
        .invoke_handler(tauri::generate_handler![
            // host
            host::environment,
            host::cache_report,
            host::cache_purge,
            host::reveal,
            host::open_url,
            host::log_js_error,
            host::pick_folder,
            host::pick_wxapkg,
            host::open_privacy_settings,
            // settings
            settings::get_settings,
            settings::save_settings,
            // local
            local::scan_programs,
            local::cached_scan,
            local::program_icon,
            local::inspect,
            local::raw_export,
            local::decrypt_folder,
            local::unpack_files,
            local::unpack,
            local::export_batch,
            local::beautify,
            local::verify,
            local::pack,
            // remote
            remote::ilink_status,
            remote::ilink_login_begin,
            remote::ilink_login_poll,
            remote::ilink_account_delete,
            remote::ilink_recent,
            remote::ilink_enrich_recent,
            remote::ilink_icon,
            remote::ilink_info,
            remote::ilink_download,
            remote::ilink_cancel_download,
            // service panel
            service::server_status,
            service::server_start,
            service::server_stop,
        ])
        .setup(|app| {
            // `wxapkg-dump --gui --server` hands the public address over through the
            // environment, so the combined form works without touching the panel.
            if let Ok(bind) = std::env::var("WXAPKG_PUBLIC_API") {
                let bind = bind.trim().to_owned();
                if !bind.is_empty() {
                    let key = std::env::var("WXAPKG_PUBLIC_KEY").ok();
                    let listener = service::start_api(bind.clone(), key);
                    if service::wait_until_ready(&bind, std::time::Duration::from_secs(3)) {
                        *app.state::<Shell>().public().lock().expect("public lock") =
                            Some(listener);
                        log(
                            app.handle(),
                            "info",
                            format!("public API listening on http://{bind} (docs at /docs)"),
                        );
                    } else {
                        log(
                            app.handle(),
                            "error",
                            format!("cannot publish the API on {bind}"),
                        );
                    }
                }
            }

            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("wxapkg-dump")
                .inner_size(1180.0, 780.0)
                .min_inner_size(900.0, 560.0)
                .center()
                .build()?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build the desktop application");

    app.run(move |app, event| {
        if let RunEvent::Exit = event {
            if let Some(listener) = app
                .state::<Shell>()
                .public()
                .lock()
                .expect("public lock")
                .take()
            {
                let _ = listener.stop.send(());
            }
        }
    });
}

/// Runs a blocking engine call off the UI thread and logs its start and outcome.
///
/// Every capability command funnels through here so the window never freezes and the log
/// drawer always tells the operator what happened.
pub async fn run_job<T, F>(app: &AppHandle, name: &str, job: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    log(app, "info", format!("{name}: started"));
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        // A panic inside the engine must surface as a failed command, not as a vanished app.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).unwrap_or_else(|panic| {
            let detail = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                .unwrap_or_else(|| "unknown panic".to_owned());
            Err(format!("内部错误：{detail}"))
        })
    })
    .await;
    match outcome {
        Ok(Ok(value)) => {
            log(app, "info", format!("{name}: finished"));
            Ok(value)
        }
        Ok(Err(message)) => {
            log(app, "error", format!("{name}: {message}"));
            Err(message)
        }
        Err(err) => {
            let message = format!("the job could not be scheduled: {err}");
            log(app, "error", format!("{name}: {message}"));
            Err(message)
        }
    }
}

/// Renders a QR code as a PNG `data:` URL for the login panel.
///
/// A bitmap keeps the window free of injected markup: the panel only ever sets `img.src`.
pub fn qr_data_url(content: &str) -> Result<String, String> {
    use base64::Engine as _;
    use image::{ImageFormat, Luma};

    let code = qrcode::QrCode::new(content.as_bytes())
        .map_err(|err| format!("cannot encode the QR content: {err}"))?;
    let image = code
        .render::<Luma<u8>>()
        .quiet_zone(true)
        .module_dimensions(6, 6)
        .dark_color(Luma([11u8]))
        .light_color(Luma([255u8]))
        .build();
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|err| format!("cannot encode the QR image: {err}"))?;
    Ok(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    ))
}

/// Wraps image bytes as a `data:` URL, sniffing the type from the signature.
///
/// Used for the login QR, which arrives as a remote bitmap: inlining it keeps the window
/// independent of webview networking.
pub fn image_data_url_from_bytes(_source: &str, bytes: &[u8]) -> Option<String> {
    use base64::Engine as _;
    let mime = if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else {
        return None;
    };
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// Reads an image file as a `data:` URL, for icons the remote store cached on disk.
pub fn image_data_url(path: &std::path::Path) -> Option<String> {
    use base64::Engine as _;
    let bytes = std::fs::read(path).ok()?;
    let mime = if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") {
        "image/webp"
    } else {
        "application/octet-stream"
    };
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}
