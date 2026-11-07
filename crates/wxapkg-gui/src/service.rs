// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! The optional public HTTP API, hosted in-process on the operator's request.
//!
//! This is the only listener the desktop application opens, and it is never required: the
//! window works without it, and the GUI never talks to it. `wxapkg-dump --gui --server` starts it
//! at launch; the service panel starts and stops it interactively.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::State;
use tokio::sync::oneshot;

use crate::Shell;

/// A listener started by this process.
pub struct Listener {
    /// Address it was asked to bind.
    pub address: String,
    /// Fires the graceful shutdown.
    pub stop: oneshot::Sender<()>,
}

/// What the window's service panel shows.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    /// Whether the public API is listening.
    pub running: bool,
    /// Bound address, empty when stopped.
    pub address: String,
    /// Documentation URL, empty when stopped.
    pub docs: String,
}

impl ServiceStatus {
    fn stopped() -> Self {
        Self {
            running: false,
            address: String::new(),
            docs: String::new(),
        }
    }

    fn running(address: String) -> Self {
        Self {
            docs: format!("http://{address}/docs"),
            address,
            running: true,
        }
    }
}

/// Starts the API on `bind`, returning the handle that stops it.
pub fn start_api(bind: String, key: Option<String>) -> Listener {
    let options = wxapkg_server::Options {
        bind: bind.clone(),
        admin_key: key.filter(|value| !value.trim().is_empty()),
        // The operator explicitly asked for this listener, including a non-loopback one.
        allow_remote: true,
        ..wxapkg_server::Options::default()
    };

    let (stop, stopped) = oneshot::channel();
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::error!(target: "wxapkg-dump::gui", error = %err, "cannot start the runtime");
                return;
            }
        };
        if let Err(err) = runtime.block_on(wxapkg_server::serve(options, Some(stopped))) {
            tracing::error!(target: "wxapkg-dump::gui", error = %err, "the API stopped");
        }
    });

    Listener {
        address: bind,
        stop,
    }
}

/// Blocks until the listener accepts connections or the deadline passes.
pub fn wait_until_ready(address: &str, timeout: Duration) -> bool {
    let Ok(target) = address.parse() else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&target, Duration::from_millis(150)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Current state of the public API.
#[tauri::command]
pub fn server_status(shell: State<'_, Shell>) -> ServiceStatus {
    match shell.public().lock().expect("public lock").as_ref() {
        Some(listener) => ServiceStatus::running(listener.address.clone()),
        None => ServiceStatus::stopped(),
    }
}

/// Starts the public API on `host:port`, optionally guarded by `key`.
///
/// A non-loopback host is accepted without further confirmation: the caller is the person
/// running the application, and the panel states what the exposure means.
#[tauri::command]
pub fn server_start(
    shell: State<'_, Shell>,
    host: String,
    port: u16,
    key: Option<String>,
) -> Result<ServiceStatus, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("host must not be empty".to_owned());
    }
    if port == 0 {
        return Err("port must be between 1 and 65535".to_owned());
    }

    let bind = format!("{host}:{port}");
    if shell.public().lock().expect("public lock").is_some() {
        return Err("the API is already running; stop it first".to_owned());
    }

    let listener = start_api(bind.clone(), key);
    if !wait_until_ready(&bind, Duration::from_secs(3)) {
        let _ = listener.stop.send(());
        return Err(format!("cannot listen on {bind}"));
    }

    *shell.public().lock().expect("public lock") = Some(listener);
    Ok(ServiceStatus::running(bind))
}

/// Stops the public API. The window keeps working: nothing in it depends on this listener.
#[tauri::command]
pub fn server_stop(shell: State<'_, Shell>) -> ServiceStatus {
    if let Some(listener) = shell.public().lock().expect("public lock").take() {
        let _ = listener.stop.send(());
    }
    ServiceStatus::stopped()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// Small raw HTTP GET; the test only needs the status line and a slice of the body.
    fn get(address: &str, path: &str, key: Option<&str>) -> String {
        let mut stream = TcpStream::connect(address).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let auth = key
            .map(|value| format!("x-admin-key: {value}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {address}\r\n{auth}Connection: close\r\n\r\n"
        )
        .expect("request");
        let mut text = String::new();
        stream.read_to_string(&mut text).ok();
        text
    }

    // What the service panel starts must be the real API, not a listening socket that
    // answers nothing — the panel says "running" as soon as the port accepts.
    #[test]
    fn the_panel_starts_a_working_api() {
        let port = portpicker::pick_unused_port().expect("free port");
        let bind = format!("127.0.0.1:{port}");
        let listener = start_api(bind.clone(), None);
        assert!(wait_until_ready(&bind, Duration::from_secs(5)));
        let response = get(&bind, "/api/health", None);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"ok\":true"), "{response}");
        assert!(
            response.to_ascii_lowercase().contains("x-api-protocol: 1"),
            "{response}"
        );
        let _ = listener.stop.send(());
    }

    #[test]
    fn the_panel_key_is_enforced() {
        let port = portpicker::pick_unused_port().expect("free port");
        let bind = format!("127.0.0.1:{port}");
        let listener = start_api(bind.clone(), Some("secret".to_owned()));
        assert!(wait_until_ready(&bind, Duration::from_secs(5)));
        let refused = get(&bind, "/api/health", None);
        assert!(
            refused.starts_with("HTTP/1.1 401") || refused.starts_with("HTTP/1.1 403"),
            "{refused}"
        );
        let allowed = get(&bind, "/api/health", Some("secret"));
        assert!(allowed.starts_with("HTTP/1.1 200"), "{allowed}");
        let _ = listener.stop.send(());
    }
}
