// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! `wxapkg-server` — the HTTP API.
//!
//! The server exists so that other tools (and AI agents) can drive the toolkit without
//! shelling out: every capability the CLI exposes is reachable over HTTP, using the same
//! request and response shapes. It serves no interface of its own beyond the interactive
//! documentation at `/docs`; the desktop GUI is a separate binary that does not depend on
//! this service.
//!
//! ```text
//! wxapkg-dump --server --bind 127.0.0.1:8788
//! ```
//!
//! ## Response envelope
//!
//! ```json
//! { "ok": true,  "command": "scan", "version": "1.0.0", "data": { } }
//! { "ok": false, "command": "scan", "version": "1.0.0", "error": { "code": "…", "message": "…", "hint": "…" } }
//! ```
//!
//! ## Security
//!
//! The API can read arbitrary directories, write into arbitrary directories and drive an
//! authenticated App Store session, so it binds to loopback by default. Binding to a
//! non-loopback address additionally requires `--allow-remote`; running that way without an
//! `--admin-key` is possible but draws a loud warning, because anyone who can reach the
//! listener can then act as the operator.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::module_name_repetitions,
    clippy::too_many_lines,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::uninlined_format_args,
    clippy::needless_pass_by_value
)]

mod api;
mod artifacts;
mod events;
mod guard;
mod inventory;
mod state;
pub mod system;
mod util;

use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;

/// Flags accepted by `wxapkg-dump --server`.
#[derive(Debug, Parser)]
#[command(name = "wxapkg-dump", version, about = "HTTP API for wxapkg-dump")]
struct Args {
    /// Address to bind. Defaults to loopback only.
    #[arg(long, default_value = "127.0.0.1:8788", env = "WXAPKG_BIND")]
    bind: String,

    /// Required `x-admin-key` value when binding to a non-loopback address.
    #[arg(long, env = "WXAPKG_ADMIN_KEY")]
    admin_key: Option<String>,

    /// Explicitly allow binding to a non-loopback address.
    #[arg(long)]
    allow_remote: bool,

    /// Directory that unpacked projects are written into when a request omits one.
    #[arg(long, env = "WXAPKG_WORKSPACE")]
    workspace: Option<std::path::PathBuf>,

    /// Log level (`error`, `warn`, `info`, `debug`, `trace`).
    #[arg(long, default_value = "info", env = "WXAPKG_LOG")]
    log: String,
}

/// Options for the HTTP API.
#[derive(Debug, Clone)]
pub struct Options {
    /// Address to bind.
    pub bind: String,
    /// Required `x-admin-key` value when binding to a non-loopback address.
    pub admin_key: Option<String>,
    /// Allow binding to a non-loopback address.
    pub allow_remote: bool,
    /// Where unpacked projects are written when a request omits a directory.
    pub workspace: Option<std::path::PathBuf>,
    /// Log filter for this process.
    pub log: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8788".to_owned(),
            admin_key: None,
            allow_remote: false,
            workspace: None,
            log: "info".to_owned(),
        }
    }
}

/// Parses the server flags, for callers that own the process' argument vector.
///
/// Kept here rather than in the binary so `wxapkg-dump --server` and the standalone binary cannot
/// drift into accepting different flags.
pub fn parse(argv: Vec<String>) -> Options {
    // `parse_from` treats the first element as the program name, and callers pass flags only.
    let args = Args::parse_from(std::iter::once("wxapkg-dump".to_owned()).chain(argv));
    Options {
        bind: args.bind,
        admin_key: args.admin_key,
        allow_remote: args.allow_remote,
        workspace: args.workspace,
        log: args.log,
    }
}

/// Runs the API until a shutdown signal arrives.
///
/// # Errors
///
/// Returns an error when the address is unusable, when binding beyond loopback is attempted
/// without the required opt-in, or when the state directory cannot be created.
pub async fn run(options: Options) -> anyhow::Result<()> {
    serve(options, None).await
}

/// Runs the API until `stop` fires or the process receives a termination signal.
///
/// The desktop shell and other embedders use this to run a server they can shut down
/// without terminating the process that hosts it.
///
/// # Errors
///
/// Same conditions as [`run`].
pub async fn serve(
    options: Options,
    stop: Option<tokio::sync::oneshot::Receiver<()>>,
) -> anyhow::Result<()> {
    let args = options;

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(format!("wxapkg-dump={}", args.log))
            }),
        )
        .with_target(false)
        .finish();
    // Embedders may already have installed a subscriber; a second one is not an error.
    let _ = tracing::subscriber::set_global_default(subscriber);

    let addr: SocketAddr = args
        .bind
        .parse()
        .map_err(|err| anyhow::anyhow!("cannot parse --bind {}: {err}", args.bind))?;

    // The API is powerful enough that exposing it beyond loopback must be deliberate.
    if !addr.ip().is_loopback() {
        if !args.allow_remote {
            anyhow::bail!(
                "refusing to bind {addr}: the API can read and write arbitrary paths. Pass \
                 --allow-remote to expose it, preferably together with --admin-key."
            );
        }
        if args.admin_key.as_deref().unwrap_or_default().is_empty() {
            tracing::warn!(
                target: "wxapkg-dump::server",
                "binding a non-loopback address without --admin-key: anyone who can reach \
                 {addr} can read and write files on this machine"
            );
        }
    }

    let state = state::AppState::new(args.admin_key.clone(), args.workspace.clone())?;
    let router = api::router(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    tracing::info!(target: "wxapkg-dump::server", address = %local, "listening");
    println!(
        "wxapkg-dump {} listening on http://{local}",
        wxapkg_core::VERSION
    );
    println!("api docs:     http://{local}/docs");
    println!("api root:     http://{local}/api/health");

    // Hyper waits for every open connection to finish, and a client holding an SSE stream
    // open would otherwise keep the process alive forever — which a service manager can
    // only break with SIGKILL. The grace period is the backstop behind closing the bus.
    const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

    let drain = state.clone();
    let (notify_drain_started, drain_started) = tokio::sync::oneshot::channel::<()>();
    let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
        match stop {
            Some(receiver) => {
                let _ = receiver.await;
                tracing::info!(target: "wxapkg-dump::server", "stop requested");
                drain.events.close();
                drain.events.publish("info", "server is shutting down");
            }
            None => shutdown_signal(drain).await,
        }
        let _ = notify_drain_started.send(());
    });

    tokio::select! {
        result = serve => result?,
        () = async {
            let _ = drain_started.await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => {
            tracing::warn!(
                target: "wxapkg-dump::server",
                grace = ?SHUTDOWN_GRACE,
                "connections were still open when the grace period expired; exiting anyway"
            );
            state.events.publish(
                "warn",
                "shutdown grace period expired with connections still open",
            );
        }
    }
    Ok(())
}

/// Entry point used by the `wxapkg-dump` binary for `--server`.
///
/// # Errors
///
/// Propagates whatever [`run`] returns.
pub async fn command_line(argv: Vec<String>) -> anyhow::Result<()> {
    run(parse(argv)).await
}

async fn shutdown_signal(state: state::AppState) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!(target: "wxapkg-dump::server", "shutting down");
    state.events.close();
    // Publishing after closing is what wakes an idle subscriber, which then notices the
    // closed bus on its next poll and completes its response.
    state.events.publish("info", "server is shutting down");
}
