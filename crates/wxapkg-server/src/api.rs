// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! HTTP routing and handlers.
//!
//! Every handler returns the same envelope the CLI emits, so a caller written against one
//! surface works against the other:
//!
//! ```json
//! { "ok": true,  "command": "scan", "version": "1.0.0", "protocol": 1, "data": { } }
//! { "ok": false, "command": "scan", "version": "1.0.0", "error": { "code": "…" } }
//! ```
//!
//! The newer endpoints live in [`crate::inventory`], [`crate::artifacts`] and
//! [`crate::system`]; the helpers below are shared with them.

use std::convert::Infallible;
use std::path::PathBuf;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_stream::StreamExt as _;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use wxapkg_core::beautify::FormatOptions;
use wxapkg_core::pipeline::{
    beautify_tree, inspect, raw_extract, scan, unpack, Source, UnpackRequest,
};
use wxapkg_core::project::{BuildOptions, PluginsMode};

use utoipa::openapi::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::state::AppState;
use crate::util;

/// Envelope version carried in every response, so a client can negotiate before it parses.
pub const PROTOCOL: u32 = 1;

/// Builds the application router.
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/config", get(config))
        .route("/scan", get(scan_programs))
        .route("/inspect", post(inspect_containers))
        .route("/unpack", post(unpack_request))
        .route("/raw", post(raw_dump))
        .route("/decrypt", post(decrypt_folder))
        .route("/beautify", post(beautify_tree_route))
        .route("/verify", post(verify_route))
        .route("/events", get(events))
        .merge(crate::inventory::router())
        .merge(crate::artifacts::router())
        .merge(crate::system::router())
        .merge(remote_routes())
        .with_state(state.clone());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .nest("/api", api)
        .route("/", get(service_index))
        .merge(SwaggerUi::new("/docs").url("/openapi.json", openapi()))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn_with_state(
            state,
            require_admin_key,
        ))
        .layer(axum::middleware::from_fn(add_protocol_header))
}

/// The OpenAPI document served at `/openapi.json`.
fn openapi() -> OpenApi {
    serde_json::from_str(include_str!("openapi.json"))
        .expect("the embedded OpenAPI document is valid JSON")
}

/// Root route: the service exposes only the API and its documentation.
async fn service_index() -> Response {
    success(
        "index",
        json!({
            "name": "wxapkg-dump",
            "docs": "/docs",
            "openapi": "/openapi.json",
            "health": "/api/health",
        }),
    )
}

/// Advertises the envelope version on every response, including error and asset ones.
async fn add_protocol_header(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert("x-api-protocol", axum::http::HeaderValue::from_static("1"));
    response
}

/// Middleware enforcing the admin token when one is configured.
///
/// Only `/api/` is guarded. The token is accepted as a header *or* a query parameter
/// because `EventSource` and `<img>` cannot set headers.
async fn require_admin_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(expected) = &state.admin_key else {
        return next.run(request).await;
    };

    if !request.uri().path().starts_with("/api/") {
        return next.run(request).await;
    }

    let supplied = headers
        .get("x-admin-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            let pairs = util::parse_query(request.uri().query().unwrap_or_default());
            util::query_value(&pairs, "key").map(str::to_owned)
        });

    if supplied.as_deref() == Some(expected.as_str()) {
        next.run(request).await
    } else {
        failure(
            "auth",
            "unauthorized",
            "missing or invalid admin key",
            Some("pass the token in the x-admin-key header, or as ?key= for streamed and image responses"),
            StatusCode::UNAUTHORIZED,
        )
    }
}

// ---------------------------------------------------------------------------
// response helpers
// ---------------------------------------------------------------------------

/// Builds the success envelope. `protocol` is stamped here so no handler can forget it.
pub(crate) fn success<T: Serialize>(command: &str, data: T) -> Response {
    Json(json!({
        "ok": true,
        "command": command,
        "version": wxapkg_core::VERSION,
        "protocol": PROTOCOL,
        "data": data,
    }))
    .into_response()
}

pub(crate) fn failure(
    command: &str,
    code: &str,
    message: &str,
    hint: Option<&str>,
    status: StatusCode,
) -> Response {
    let mut error = json!({ "code": code, "message": message });
    if let Some(hint) = hint {
        error["hint"] = Value::String(hint.to_owned());
    }
    (
        status,
        Json(json!({
            "ok": false,
            "command": command,
            "version": wxapkg_core::VERSION,
            "protocol": PROTOCOL,
            "error": error,
        })),
    )
        .into_response()
}

/// A handler-side failure that no engine error models.
pub(crate) fn internal_error(command: &str, message: &str) -> Response {
    failure(
        command,
        "internal_error",
        message,
        None,
        StatusCode::INTERNAL_SERVER_ERROR,
    )
}

/// Maps an engine error onto an HTTP response.
pub(crate) fn engine_failure(command: &str, err: &wxapkg_core::Error) -> Response {
    let status = match err.code() {
        "invalid_argument" | "invalid_container" | "program_not_found" => StatusCode::BAD_REQUEST,
        "missing_decryption_key" => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    // Same catalogue as the command line: a second table here is how two surfaces drift apart.
    let lang = wxapkg_core::i18n::Lang::resolve(None);
    let hint = err.hint(lang);
    failure(
        command,
        err.code(),
        &err.message(lang),
        hint.as_deref(),
        status,
    )
}

#[cfg(feature = "remote")]
fn remote_failure(command: &str, err: &wxapkg_remote::Error) -> Response {
    let status = match err.code() {
        "invalid_argument" | "unknown_account" => StatusCode::BAD_REQUEST,
        "not_logged_in" | "session_expired" => StatusCode::UNAUTHORIZED,
        "upstream_rejected" => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let hint = match err {
        wxapkg_remote::Error::SessionExpired(_) | wxapkg_remote::Error::NotLoggedIn(_) => {
            Some("请重新扫码登录 / scan the QR code again")
        }
        _ => None,
    };
    failure(command, err.code(), &err.to_string(), hint, status)
}

#[cfg(not(feature = "remote"))]
fn remote_failure(command: &str, _err: &std::convert::Infallible) -> Response {
    failure(
        command,
        "unsupported",
        "this build was compiled without the remote download feature",
        None,
        StatusCode::NOT_IMPLEMENTED,
    )
}

// ---------------------------------------------------------------------------
// local operations
// ---------------------------------------------------------------------------

async fn health(State(state): State<AppState>) -> Response {
    success(
        "health",
        json!({
            "version": wxapkg_core::VERSION,
            "target": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "startedAt": state.started_at,
            "uptimeSeconds": now() - state.started_at,
        }),
    )
}

/// Body of `POST /api/decrypt`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DecryptBody {
    /// Folder to walk for containers.
    folder: String,
    /// Decryption keys (appids); each container tries them in turn, then a path guess.
    #[serde(default)]
    keys: Vec<String>,
    /// Output directory; defaults to `<workspace>/decrypted`.
    out: Option<String>,
}

/// `POST /api/decrypt` — decrypt every container in a folder, rotating the given keys.
async fn decrypt_folder(State(state): State<AppState>, Json(body): Json<DecryptBody>) -> Response {
    let out_dir = state.resolve_output(body.out.as_deref(), "decrypted");
    let events = state.events.clone();
    let result = tokio::task::spawn_blocking(move || {
        let progress = move |done: usize, total: usize, name: &str| {
            events.publish("info", format!("decrypting {done}/{total}: {name}"));
        };
        wxapkg_core::pipeline::decrypt_folder(
            std::path::Path::new(&body.folder),
            &body.keys,
            std::path::Path::new(&out_dir),
            Some(&progress),
        )
    })
    .await;
    match result {
        Ok(Ok(report)) => {
            state.events.publish(
                "info",
                format!(
                    "decrypted {} file(s), {} plain, {} failed",
                    report.decrypted, report.plain, report.failed
                ),
            );
            success("decrypt", report)
        }
        Ok(Err(err)) => engine_failure("decrypt", &err),
        Err(err) => internal_error("decrypt", &err.to_string()),
    }
}

async fn config(State(state): State<AppState>) -> Response {
    let storage = remote_paths().0;
    let (remote_found, remote_path) = remote_status();
    success(
        "config",
        json!({
            "configDir": remote_paths().1,
            "storageDir": storage,
            "workspace": state.workspace,
            "remoteFound": remote_found,
            "remotePath": remote_path,
            "searchRoots": wxapkg_core::discover::default_roots()
                .into_iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        }),
    )
}

/// Returns `(remote_store, config_dir)` as strings, tolerating an unavailable home.
pub(crate) fn remote_paths() -> (String, String) {
    #[cfg(feature = "remote")]
    {
        let config = wxapkg_remote::Store::open()
            .map(|store| store.root().display().to_string())
            .unwrap_or_default();
        let parent = std::path::Path::new(&config)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        (config, parent)
    }
    #[cfg(not(feature = "remote"))]
    {
        (String::new(), String::new())
    }
}

/// Returns `(found, path)` describing the native remote store.
pub(crate) fn remote_status() -> (bool, Option<String>) {
    #[cfg(feature = "remote")]
    {
        match wxapkg_remote::Store::open() {
            Ok(store) => (true, Some(store.root().display().to_string())),
            Err(_) => (false, None),
        }
    }
    #[cfg(not(feature = "remote"))]
    {
        (false, None)
    }
}

#[derive(Debug, Deserialize)]
struct ScanQuery {
    appid: Option<String>,
    limit: Option<usize>,
}

async fn scan_programs(State(state): State<AppState>, Query(query): Query<ScanQuery>) -> Response {
    state
        .events
        .publish("info", "scanning the local WeChat data directories");
    let appid = query.appid.clone();
    let limit = query.limit;
    let result = tokio::task::spawn_blocking(move || scan(None, appid.as_deref(), limit)).await;

    match result {
        Ok(Ok(programs)) => {
            state.events.publish(
                "info",
                format!("scan finished: {} program(s)", programs.len()),
            );
            success("scan", json!({ "programs": programs }))
        }
        Ok(Err(err)) => engine_failure("scan", &err),
        Err(err) => failure(
            "scan",
            "internal_error",
            &format!("the scan task failed: {err}"),
            None,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

#[derive(Debug, Deserialize)]
struct InspectBody {
    #[serde(default)]
    paths: Vec<String>,
    wxid: Option<String>,
}

async fn inspect_containers(
    State(state): State<AppState>,
    Json(body): Json<InspectBody>,
) -> Response {
    if body.paths.is_empty() {
        return failure(
            "inspect",
            "invalid_argument",
            "paths must contain at least one entry",
            None,
            StatusCode::BAD_REQUEST,
        );
    }
    let paths: Vec<PathBuf> = body.paths.iter().map(PathBuf::from).collect();
    let wxid = body.wxid.clone();
    let result = tokio::task::spawn_blocking(move || inspect(&paths, wxid.as_deref())).await;

    match result {
        Ok(Ok(containers)) => {
            state.events.publish(
                "info",
                format!("inspected {} container(s)", containers.len()),
            );
            success("inspect", json!({ "containers": containers }))
        }
        Ok(Err(err)) => engine_failure("inspect", &err),
        Err(err) => failure(
            "inspect",
            "internal_error",
            &format!("the inspect task failed: {err}"),
            None,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct UnpackOptionsBody {
    keep_raw: Option<bool>,
    decompile_wxml: Option<bool>,
    rewrite_requires: Option<bool>,
    prune: Option<bool>,
    synthesize: Option<bool>,
    plugin_clean: Option<bool>,
    plugins: Option<String>,
    wxss_suffix: Option<String>,
    auto_fix: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UnpackBody {
    #[serde(default)]
    paths: Vec<String>,
    appid: Option<String>,
    version: Option<String>,
    out: Option<String>,
    wxid: Option<String>,
    main: Option<String>,
    clean: Option<bool>,
    #[serde(default)]
    options: UnpackOptionsBody,
    beautify: Option<FormatOptions>,
}

async fn unpack_request(State(state): State<AppState>, Json(body): Json<UnpackBody>) -> Response {
    let has_paths = !body.paths.is_empty();
    let has_appid = body.appid.as_deref().is_some_and(|a| !a.trim().is_empty());
    if !has_paths && !has_appid {
        return failure(
            "unpack",
            "invalid_argument",
            "provide `paths`, or `appid` to locate the package automatically",
            None,
            StatusCode::BAD_REQUEST,
        );
    }

    let default_name = body
        .appid
        .as_deref()
        .map(|a| a.trim().to_owned())
        .or_else(|| {
            body.paths.first().and_then(|p| {
                PathBuf::from(p)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
        })
        .unwrap_or_else(|| "project".to_owned());
    let out_dir = state.resolve_output(body.out.as_deref(), &default_name);

    let source = if has_appid {
        Source::AppId {
            appid: body.appid.clone().unwrap_or_default().trim().to_owned(),
            version: body.version.clone(),
            roots: None,
        }
    } else {
        Source::Paths(body.paths.iter().map(PathBuf::from).collect())
    };

    let mut request = UnpackRequest::new(source, out_dir.clone());
    request.wxid = body.wxid.clone();
    request.main = body.main.clone();
    request.clean = body.clean.unwrap_or(true);
    request.options = build_options(&body.options);
    request.options.format = body.beautify.clone();
    if has_paths {
        request.manifest = wxapkg_core::pipeline::find_manifest(
            &body.paths.iter().map(PathBuf::from).collect::<Vec<_>>(),
        );
    }

    state
        .events
        .publish("info", format!("unpacking into {}", out_dir.display()));

    // Per-container progress reaches API consumers through the event stream.
    {
        let events = state.events.clone();
        request.progress = Some(std::sync::Arc::new(move |index, total, name| {
            events.publish("info", format!("reconstructing {index}/{total}: {name}"));
        }));
    }

    match tokio::task::spawn_blocking(move || wxapkg_core::pipeline::unpack_grouped(&request)).await
    {
        // One program keeps the historical shape: the report itself is the payload.
        Ok(Ok(mut reports)) if reports.len() == 1 => {
            let report = reports.remove(0);
            publish_report(&state, &report);
            success("unpack", report)
        }
        // A selection spanning several programs: one project per appid.
        Ok(Ok(reports)) => {
            for report in &reports {
                publish_report(&state, report);
            }
            success("unpack", serde_json::json!({ "projects": reports }))
        }
        Ok(Err(err)) => engine_failure("unpack", &err),
        Err(err) => failure(
            "unpack",
            "internal_error",
            &format!("the unpack task failed: {err}"),
            None,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

pub(crate) fn publish_report(state: &AppState, report: &wxapkg_core::BuildReport) {
    state.events.publish(
        "info",
        format!(
            "unpack finished: {} file(s), {} warning(s)",
            report.total_files(),
            report.warnings.len()
        ),
    );
    for note in report.notes.iter().take(12) {
        state.events.publish("info", note.clone());
    }
    for warning in report.warnings.iter().take(12) {
        state.events.publish("warn", warning.clone());
    }
}

fn build_options(body: &UnpackOptionsBody) -> BuildOptions {
    let auto_fix = body.auto_fix.unwrap_or(true);
    BuildOptions {
        keep_raw: body.keep_raw.unwrap_or(true),
        decompile_wxml: body.decompile_wxml.unwrap_or(true),
        rewrite_requires: body.rewrite_requires.unwrap_or(true),
        prune: auto_fix && body.prune.unwrap_or(true),
        synthesize: auto_fix && body.synthesize.unwrap_or(true),
        plugin_clean: auto_fix && body.plugin_clean.unwrap_or(true),
        plugins_mode: match body.plugins.as_deref() {
            Some("placeholder") => PluginsMode::Placeholder,
            Some("strip") => PluginsMode::Strip,
            _ => PluginsMode::Keep,
        },
        wxss_suffix: body.wxss_suffix.clone().unwrap_or_default(),
        ..BuildOptions::default()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBody {
    #[serde(default)]
    paths: Vec<String>,
    out: Option<String>,
    wxid: Option<String>,
    auto_inflate: Option<bool>,
}

async fn raw_dump(State(state): State<AppState>, Json(body): Json<RawBody>) -> Response {
    if body.paths.is_empty() {
        return failure(
            "raw",
            "invalid_argument",
            "paths must contain at least one entry",
            None,
            StatusCode::BAD_REQUEST,
        );
    }
    let out_dir = state.resolve_output(body.out.as_deref(), "raw");
    let paths: Vec<PathBuf> = body.paths.iter().map(PathBuf::from).collect();
    let wxid = body.wxid.clone();
    let auto_inflate = body.auto_inflate.unwrap_or(false);

    match tokio::task::spawn_blocking(move || {
        raw_extract(&paths, &out_dir, wxid.as_deref(), auto_inflate)
    })
    .await
    {
        Ok(Ok(report)) => {
            state.events.publish(
                "info",
                format!("dumped {} file(s) to {}", report.files, report.out_dir),
            );
            success("raw", report)
        }
        Ok(Err(err)) => engine_failure("raw", &err),
        Err(err) => failure(
            "raw",
            "internal_error",
            &format!("the extraction task failed: {err}"),
            None,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BeautifyBody {
    dir: String,
    #[serde(default)]
    options: Option<FormatOptions>,
}

/// Request body of `POST /api/verify`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifyBody {
    #[serde(default)]
    dir: String,
    /// Run a real JavaScript syntax check when Node is available.
    #[serde(default)]
    node: bool,
}

/// `POST /api/verify`
///
/// Answers "would the DevTools compile this project", which no other endpoint can: export
/// reports what it wrote, not whether the result is coherent.
async fn verify_route(State(state): State<AppState>, Json(body): Json<VerifyBody>) -> Response {
    if body.dir.trim().is_empty() {
        return failure(
            "verify",
            "invalid_argument",
            "dir is required",
            None,
            StatusCode::BAD_REQUEST,
        );
    }
    let dir = state.resolve_input(&body.dir);
    if !dir.is_dir() {
        return failure(
            "verify",
            "not_found",
            &format!("{} is not a directory", dir.display()),
            None,
            StatusCode::NOT_FOUND,
        );
    }
    let node = body.node;

    match tokio::task::spawn_blocking(move || {
        let mut report = wxapkg_core::verify::verify_project(
            &dir,
            &wxapkg_core::verify::VerifyOptions::default(),
        )?;
        if node {
            match wxapkg_core::verify::node_syntax_check(&dir) {
                Ok(failures) => {
                    wxapkg_core::pipeline::merge_syntax_failures(&mut report, &failures);
                }
                Err(err) => report.warnings.push(wxapkg_core::verify::Issue {
                    code: "node_unavailable".to_owned(),
                    path: dir.display().to_string(),
                    message: format!("the JavaScript syntax check was skipped: {err}"),
                }),
            }
        }
        Ok::<_, wxapkg_core::Error>(report)
    })
    .await
    {
        Ok(Ok(report)) => {
            state.events.publish(
                "info",
                format!(
                    "verify: {} error(s), {} warning(s)",
                    report.errors.len(),
                    report.warnings.len()
                ),
            );
            success("verify", report)
        }
        Ok(Err(err)) => engine_failure("verify", &err),
        Err(err) => internal_error("verify", &err.to_string()),
    }
}

async fn beautify_tree_route(
    State(state): State<AppState>,
    Json(body): Json<BeautifyBody>,
) -> Response {
    let dir = state.resolve_input(&body.dir);
    if body.dir.trim().is_empty() {
        return failure(
            "beautify",
            "invalid_argument",
            "dir is required",
            None,
            StatusCode::BAD_REQUEST,
        );
    }
    let options = body.options.unwrap_or_default();
    match tokio::task::spawn_blocking(move || beautify_tree(&dir, &options)).await {
        Ok(Ok(report)) => {
            state.events.publish(
                "info",
                format!("beautified {} file(s)", report.formatted.len()),
            );
            success("beautify", report)
        }
        Ok(Err(err)) => engine_failure("beautify", &err),
        Err(err) => failure(
            "beautify",
            "internal_error",
            &format!("the beautify task failed: {err}"),
            None,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

async fn events(
    State(state): State<AppState>,
) -> Sse<impl futures_core::Stream<Item = Result<SseEvent, Infallible>>> {
    // Lagged messages are dropped rather than closing the stream: the drawer is a progress
    // log, not an audit trail.
    let stream = crate::events::UntilClosed::new(
        tokio_stream::wrappers::BroadcastStream::new(state.events.subscribe()).filter_map(
            |result| match result {
                Ok(event) => Some(Ok(SseEvent::default()
                    .data(serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_owned())))),
                Err(_) => None,
            },
        ),
        state.events.closed_handle(),
    );
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// App Store bridge
// ---------------------------------------------------------------------------

#[cfg(feature = "remote")]
fn remote_routes() -> Router<AppState> {
    Router::new()
        .route("/ilink/status", get(ilink_status))
        .route("/ilink/account", delete(ilink_account_delete))
        .route("/ilink/login/qr", post(ilink_login_qr))
        .route("/ilink/login/check", post(ilink_login_check))
        .route("/ilink/qr.svg", get(ilink_qr_svg))
        .route("/ilink/recent", get(ilink_recent))
        .route("/ilink/info", post(ilink_info))
        .route("/ilink/download", post(ilink_download))
        .route("/ilink/icons/{name}", get(ilink_icon))
}

#[cfg(not(feature = "remote"))]
fn remote_routes() -> Router<AppState> {
    Router::new()
}

#[cfg(feature = "remote")]
mod remote_impl {
    use super::*;
    use wxapkg_remote::{DownloadOptions, DownloadReport, Remote};

    /// Every handler opens the store; it is a directory handle plus a one-time legacy import.
    pub(super) fn client() -> Result<Remote, wxapkg_remote::Error> {
        Remote::open()
    }

    /// The one remembered account, in the shape every caller reads. `null` when none.
    fn account_view(remote: &Remote) -> Value {
        remote
            .account()
            .ok()
            .flatten()
            .map(|account| {
                json!({
                    "nickName": account.nick_name,
                    "avatar": account.avatar,
                    "status": account.state.label(false),
                            "statusEn": account.state.label(true),
                    "online": account.state == wxapkg_remote::AccountState::Online,
                    "openid": account.openid,
                    "lastRefreshAt": account.last_refresh_at,
                })
            })
            .unwrap_or(Value::Null)
    }

    pub(super) async fn status() -> Response {
        match tokio::task::spawn_blocking(client).await {
            Ok(Ok(remote)) => {
                let stale = remote
                    .account()
                    .ok()
                    .flatten()
                    .is_some_and(|account| account.state == wxapkg_remote::AccountState::Stale);
                success(
                    "ilink status",
                    json!({
                        "implementation": "native",
                        "account": account_view(&remote),
                        "warnings": if stale {
                            vec!["登录已过期，请重新扫码登录".to_owned()]
                        } else {
                            Vec::new()
                        },
                    }),
                )
            }
            Ok(Err(err)) => remote_failure("ilink status", &err),
            Err(err) => internal_error("ilink status", &err.to_string()),
        }
    }

    pub(super) async fn account_delete() -> Response {
        match tokio::task::spawn_blocking(move || {
            let remote = client()?;
            remote.delete_account()
        })
        .await
        {
            Ok(Ok(())) => success("ilink account delete", json!({ "deleted": true })),
            Ok(Err(err)) => remote_failure("ilink account delete", &err),
            Err(err) => internal_error("ilink account delete", &err.to_string()),
        }
    }

    pub(super) async fn login_qr(
        State(state): State<AppState>,
        Json(body): Json<LoginQrBody>,
    ) -> Response {
        // The body is accepted but empty: exactly one account exists, so a login has no
        // identity to choose. Extra fields are ignored for compatibility.
        let _ = &body;
        match tokio::task::spawn_blocking(move || {
            let remote = client()?;
            let session = remote.begin_login()?;
            // Hand the caller the login image inline; a headless consumer can still show it.
            let image = remote.qr_image(&session).ok();
            Ok::<_, wxapkg_remote::Error>((session, image))
        })
        .await
        {
            Ok(Ok((session, image))) => {
                if let Ok(mut logins) = state.logins.lock() {
                    logins.insert(
                        session.session_id.clone(),
                        crate::state::LoginSession { created_at: now() },
                    );
                }
                state.events.publish("info", "login QR code issued");
                let image_url = image
                    .map(|bytes| {
                        let mime = if bytes.starts_with(&[0xff, 0xd8]) {
                            "image/jpeg"
                        } else {
                            "image/png"
                        };
                        format!("data:{mime};base64,{}", base64_display(&bytes))
                    })
                    .unwrap_or_default();
                success(
                    "ilink login qr",
                    json!({
                        "sessionId": session.session_id,
                        "qrContent": session.qr_url,
                        "qrImage": image_url,
                        "expiresIn": session.remaining(),
                    }),
                )
            }
            Ok(Err(err)) => remote_failure("ilink login qr", &err),
            Err(err) => internal_error("ilink login qr", &err.to_string()),
        }
    }

    pub(super) async fn login_check(
        State(state): State<AppState>,
        Json(body): Json<LoginCheckBody>,
    ) -> Response {
        let session_id = body.session_id.clone();
        let pending_id = session_id.clone();
        match tokio::task::spawn_blocking(move || {
            let remote = client()?;
            remote.poll_pending(&session_id)
        })
        .await
        {
            Ok(Ok(status)) => {
                let confirmed = matches!(status, wxapkg_remote::LoginStatus::Authorized { .. });
                if confirmed {
                    let session = state
                        .logins
                        .lock()
                        .ok()
                        .and_then(|mut logins| logins.remove(&pending_id));
                    match session {
                        Some(session) => state.events.publish(
                            "info",
                            format!("login confirmed after {}s", now() - session.created_at),
                        ),
                        None => state.events.publish("info", "login confirmed"),
                    };
                }
                let (state_name, message): (&str, Option<String>) = match status {
                    wxapkg_remote::LoginStatus::Waiting => ("waiting", None),
                    wxapkg_remote::LoginStatus::Scanned => ("scanned", None),
                    wxapkg_remote::LoginStatus::Authorized { .. } => ("confirmed", None),
                    wxapkg_remote::LoginStatus::Expired => {
                        ("expired", Some("二维码已过期".to_owned()))
                    }
                    wxapkg_remote::LoginStatus::Cancelled => {
                        ("cancelled", Some("已取消".to_owned()))
                    }
                };
                let account = if confirmed {
                    client()
                        .ok()
                        .and_then(|remote| remote.account().ok().flatten())
                } else {
                    None
                };
                success(
                    "ilink login check",
                    json!({
                        "status": state_name,
                        "nickName": account.as_ref().map(|a| a.nick_name.clone()),
                        "avatar": account.as_ref().map(|a| a.avatar.clone()),
                        "openid": account.as_ref().and_then(|a| a.openid.clone()),
                        "message": message,
                    }),
                )
            }
            Ok(Err(err)) => remote_failure("ilink login check", &err),
            Err(err) => internal_error("ilink login check", &err.to_string()),
        }
    }

    pub(super) async fn qr_svg(Query(query): Query<QrSvgQuery>) -> Response {
        let payload = match query.content {
            Some(content) if !content.is_empty() => content,
            _ => {
                return failure(
                    "ilink qr",
                    "invalid_argument",
                    "pass the QR payload as `content`; it is returned by POST /api/ilink/login/qr",
                    None,
                    StatusCode::BAD_REQUEST,
                );
            }
        };
        match tokio::task::spawn_blocking(move || render_svg(&payload)).await {
            Ok(Ok(svg)) => (
                StatusCode::OK,
                [
                    ("content-type", "image/svg+xml; charset=utf-8"),
                    ("cache-control", "no-store"),
                ],
                svg,
            )
                .into_response(),
            Ok(Err(err)) => remote_failure("ilink qr", &err),
            Err(err) => internal_error("ilink qr", &err.to_string()),
        }
    }

    fn render_svg(content: &str) -> Result<String, wxapkg_remote::Error> {
        let code = qrcode::QrCode::new(content.as_bytes()).map_err(|err| {
            wxapkg_remote::Error::InvalidArgument(format!("cannot encode a QR code: {err}"))
        })?;
        Ok(code
            .render()
            .quiet_zone(true)
            .dark_color(qrcode::render::svg::Color("#111111"))
            .light_color(qrcode::render::svg::Color("#ffffff"))
            .build())
    }

    pub(super) async fn icon(Path(name): Path<String>) -> Response {
        // Only files inside the native icon cache may be served.
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return failure(
                "ilink icon",
                "invalid_argument",
                "invalid icon name",
                None,
                StatusCode::BAD_REQUEST,
            );
        }
        let Ok(remote) = client() else {
            return failure(
                "ilink icon",
                "io_error",
                "the remote store is unavailable",
                None,
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        };
        let path = remote.store().root().join("icons").join(&name);
        match std::fs::read(&path) {
            Ok(bytes) => (
                StatusCode::OK,
                [
                    ("content-type", "image/png"),
                    ("cache-control", "public, max-age=604800"),
                ],
                bytes,
            )
                .into_response(),
            Err(_) => failure(
                "ilink icon",
                "not_found",
                "the icon is not cached",
                None,
                StatusCode::NOT_FOUND,
            ),
        }
    }

    pub(super) async fn recent(
        State(state): State<AppState>,
        Query(query): Query<RecentQuery>,
    ) -> Response {
        let page = query.page.unwrap_or(1).max(1);
        let size = query.size.or(query.limit).unwrap_or(50).max(1);
        match tokio::task::spawn_blocking(move || {
            let remote = client()?;
            let list = remote.recent(page, size)?;
            let items: Vec<Value> = list
                .iter_all()
                .map(|item| {
                    let meta = remote.program_meta(&item.username);
                    json!({
                        "username": item.username,
                        "appid": meta.as_ref().map(|m| m.appid.clone()).unwrap_or_default(),
                        "nickName": meta.as_ref().map(|m| m.nick_name.clone()).unwrap_or_default(),
                        "iconUrl": meta.as_ref().map(|m| m.avatar.clone()).unwrap_or_default(),
                        "lastUsedAt": item.update_time,
                        "source": item.source,
                    })
                })
                .collect();
            // The page size is known even when the page itself is empty.
            let total_fetched = page.saturating_sub(1).saturating_mul(size) + items.len() as u32;
            Ok::<_, wxapkg_remote::Error>((total_fetched, items))
        })
        .await
        {
            Ok(Ok((total, items))) => {
                state
                    .events
                    .publish("info", format!("{} recent Mini Program(s)", items.len()));
                success(
                    "ilink recent",
                    json!({ "total": total, "page": page, "size": size, "items": items }),
                )
            }
            Ok(Err(err)) => remote_failure("ilink recent", &err),
            Err(err) => internal_error("ilink recent", &err.to_string()),
        }
    }

    pub(super) async fn info(Json(body): Json<YybTargetBody>) -> Response {
        match tokio::task::spawn_blocking(move || {
            let remote = client()?;
            remote.info(body.appid.as_deref(), body.username.as_deref())
        })
        .await
        {
            Ok(Ok(info)) => success(
                "ilink info",
                json!({
                    "appid": info.appid,
                    "username": info.username,
                    "nickName": info.nick_name,
                    "signature": info.signature,
                    "avatar": info.avatar,
                    "version": info.version,
                    "clientVersion": info.client_version,
                    "updateTime": info.update_time,
                }),
            ),
            Ok(Err(err)) => remote_failure("ilink info", &err),
            Err(err) => internal_error("ilink info", &err.to_string()),
        }
    }

    pub(super) async fn download(
        State(state): State<AppState>,
        Json(body): Json<YybDownloadBody>,
    ) -> Response {
        if body.appid.is_none() && body.username.is_none() {
            return failure(
                "ilink download",
                "invalid_argument",
                "pass either appid or username",
                None,
                StatusCode::BAD_REQUEST,
            );
        }
        // One folder per program: <base>/original_wxapkg/<appid>/wxapkg.
        let base = state.resolve_output(body.out.as_deref(), "download");
        let folder = body
            .appid
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_owned())
            .or_else(|| {
                body.username
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| value.replace(['/', '\\'], "_"))
            })
            .unwrap_or_else(|| "program".to_owned());
        let out_dir = base
            .join("original_wxapkg")
            .join(wxapkg_core::paths::sanitize_segment(&folder))
            .join("wxapkg");
        let options = DownloadOptions {
            progress: None,
            appid: body.appid.clone().unwrap_or_default(),
            username: body.username.clone().unwrap_or_default(),
            out_dir: out_dir.clone(),
            all_packages: body.all_packages.unwrap_or(false),
            concurrency: body.concurrency.unwrap_or(3).clamp(1, 10),
            version_type: body.version_type.unwrap_or(0),
            version: body.version.unwrap_or(0),
            cancel: None,
        };
        let unpack_after = body.unpack.unwrap_or(false);
        let beautify = body.beautify.unwrap_or(false);

        state
            .events
            .publish("info", format!("downloading into {}", out_dir.display()));

        let events = state.events.clone();
        let progress = std::sync::Arc::new(move |done: usize, total: usize, module: &str| {
            events.publish(
                "info",
                format!("downloading package {done}/{total}: {module}"),
            );
        });
        let options_with_progress = DownloadOptions {
            progress: Some(progress),
            ..options
        };
        let result = tokio::task::spawn_blocking(move || {
            let remote = client()?;
            remote.download(&options_with_progress)
        })
        .await;

        let report: DownloadReport = match result {
            Ok(Ok(report)) => report,
            Ok(Err(err)) => return remote_failure("ilink download", &err),
            Err(err) => return internal_error("ilink download", &err.to_string()),
        };

        let failures = report
            .packages
            .iter()
            .filter(|package| !package.error.is_empty() && !package.skipped)
            .count();
        state.events.publish(
            if failures > 0 { "warn" } else { "info" },
            format!(
                "downloaded {}/{} package(s)",
                report.downloaded(),
                report.packages.len()
            ),
        );

        let unpacked = if unpack_after {
            match tokio::task::spawn_blocking({
                let report = report.clone();
                move || unpack_downloaded(&report, beautify)
            })
            .await
            {
                Ok(Ok(dir)) => Some(dir),
                Ok(Err(err)) => {
                    state
                        .events
                        .publish("error", format!("unpack failed: {err}"));
                    None
                }
                Err(_) => None,
            }
        } else {
            None
        };

        if let Some(dir) = &unpacked {
            state
                .events
                .publish("info", format!("project reconstructed at {dir}"));
        }

        success(
            "ilink download",
            json!({
                "appid": report.appid,
                "username": report.username,
                "outDir": report.out_dir,
                "packages": report.packages,
                "unpackedDir": unpacked,
            }),
        )
    }

    /// Reconstructs a project from freshly downloaded packages using the local engine.
    fn unpack_downloaded(report: &DownloadReport, beautify: bool) -> anyhow::Result<String> {
        let paths: Vec<PathBuf> = report
            .packages
            .iter()
            .filter(|package| package.error.is_empty() && !package.saved_path.is_empty())
            .map(|package| PathBuf::from(&package.saved_path))
            .collect();
        if paths.is_empty() {
            anyhow::bail!("no package downloaded successfully, so nothing could be unpacked");
        }
        let mut request = UnpackRequest::new(Source::Paths(paths), PathBuf::from(&report.out_dir));
        if !report.appid.is_empty() {
            request.project_folder = Some(format!("unpack_{}", report.appid));
        }
        request.clean = true;
        if beautify {
            request.options.format = Some(FormatOptions::default());
        }
        let build = unpack(&request)?;
        Ok(format!(
            "{} ({} files, {} warnings)",
            build.out_dir,
            build.total_files(),
            build.warnings.len()
        ))
    }

    fn base64_display(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    // ---- request bodies ----

    #[derive(Debug, Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct LoginQrBody {
        /// Accepted for compatibility; a login no longer names an account.
        #[allow(dead_code)]
        login_id: Option<String>,
        /// Accepted for compatibility; the native client reads proxy variables itself.
        #[allow(dead_code)]
        proxy: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct LoginCheckBody {
        // Accept the snake_case spelling too: it is what the CLI and our own docs used, and a
        // 422 for a field name is a poor way to learn about camelCase.
        #[serde(alias = "session_id")]
        session_id: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct QrSvgQuery {
        #[allow(dead_code)]
        session_id: Option<String>,
        content: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct RecentQuery {
        /// 1-based page number.
        page: Option<u32>,
        /// Entries per page.
        size: Option<u32>,
        /// Accepted for compatibility; one page carries `size` entries.
        #[allow(dead_code)]
        limit: Option<u32>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct YybTargetBody {
        appid: Option<String>,
        username: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct YybDownloadBody {
        appid: Option<String>,
        username: Option<String>,
        out: Option<String>,
        version_type: Option<u32>,
        version: Option<u32>,
        #[allow(dead_code)] // accepted for compatibility; the account in session is the dev
        dev_uin: Option<u32>,
        #[allow(dead_code)] // accepted for compatibility; callers unpack via `unpack`
        zip: Option<bool>,
        unpack: Option<bool>,
        beautify: Option<bool>,
        /// Also download resource widgets, which can be hundreds of megabytes.
        all_packages: Option<bool>,
        /// Parallel package downloads (1-10); default 3.
        concurrency: Option<usize>,
    }
}

#[cfg(feature = "remote")]
use remote_impl::{
    account_delete as ilink_account_delete, download as ilink_download, icon as ilink_icon,
    info as ilink_info, login_check as ilink_login_check, login_qr as ilink_login_qr,
    qr_svg as ilink_qr_svg, recent as ilink_recent, status as ilink_status,
};

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    fn app(workspace: &std::path::Path) -> Router {
        let state = AppState::new(None, Some(workspace.to_path_buf())).unwrap();
        router(state)
    }

    fn app_with_key(workspace: &std::path::Path, key: &str) -> Router {
        let state = AppState::new(Some(key.to_owned()), Some(workspace.to_path_buf())).unwrap();
        router(state)
    }

    /// Drives the router in-process, which is both faster and more faithful than binding.
    async fn call(app: Router, request: Request<Body>) -> (StatusCode, Value, HeaderMap) {
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value, headers)
    }

    /// Status only, for responses whose body never ends (the SSE stream).
    async fn status_only(app: Router, request: Request<Body>) -> StatusCode {
        app.oneshot(request).await.unwrap().status()
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn post_json(uri: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn delete(uri: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn version_negotiates_the_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let (status, value, headers) = call(app(dir.path()), get("/api/version")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["protocol"], json!(1));
        assert_eq!(value["data"]["protocol"], json!(1));
        assert_eq!(
            headers.get("x-api-protocol").and_then(|v| v.to_str().ok()),
            Some("1")
        );
    }

    #[tokio::test]
    async fn every_response_carries_the_protocol_header() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _, headers) = call(app(dir.path()), get("/api/health")).await;
        assert_eq!(
            headers.get("x-api-protocol").and_then(|v| v.to_str().ok()),
            Some("1")
        );
    }

    #[tokio::test]
    async fn path_traversal_is_refused() {
        let dir = tempfile::tempdir().unwrap();

        let (status, value, _) = call(
            app(dir.path()),
            get("/api/project/download?path=../../../etc/passwd"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(value["error"]["code"], json!("forbidden"));

        let (status, _, _) = call(
            app(dir.path()),
            post_json("/api/reveal", json!({ "path": "/etc/passwd" })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, _, _) = call(
            app(dir.path()),
            get("/api/project/file?path=../../../../etc/passwd"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, _, _) = call(
            app(dir.path()),
            post_json("/api/project/zip", json!({ "dir": "../../../etc" })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn appid_segments_cannot_traverse() {
        let dir = tempfile::tempdir().unwrap();
        let (status, value, _) = call(app(dir.path()), get("/api/programs/../icon")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["error"]["code"], json!("invalid_argument"));
    }

    #[tokio::test]
    async fn verify_answers_for_a_project_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            r#"{"pages":["pages/index/index"]}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("app.js"), "App({})").unwrap();
        std::fs::write(dir.path().join("project.config.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.path().join("pages/index")).unwrap();
        std::fs::write(dir.path().join("pages/index/index.js"), "Page({})").unwrap();
        std::fs::write(dir.path().join("pages/index/index.wxml"), "<view />").unwrap();

        let (status, value, _) = call(
            app(dir.path()),
            post_json(
                "/api/verify",
                json!({ "dir": dir.path().display().to_string(), "node": false }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["data"]["ok"], json!(true), "{value}");

        // A relative directory is anchored at the workspace, like /api/project/tree.
        let (status, value, _) = call(
            app(dir.path()),
            post_json("/api/verify", json!({ "dir": ".", "node": false })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["data"]["ok"], json!(true), "{value}");

        // The same endpoint rejects a path that is not a directory.
        let (status, value, _) = call(
            app(dir.path()),
            post_json("/api/verify", json!({ "dir": "/definitely/not/here" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(value["error"]["code"], json!("not_found"));
    }

    // Repacking was dropped from every front-end; the endpoint must not linger outside the
    // published contract.
    #[tokio::test]
    async fn the_pack_route_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (status, _, _) = call(app(dir.path()), post_json("/api/pack", json!({}))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn project_file_reads_text_and_refuses_binary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.js"), "App({});").unwrap();
        std::fs::write(dir.path().join("blob.bin"), b"a\0b").unwrap();

        let (status, value, _) = call(app(dir.path()), get("/api/project/file?path=app.js")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["data"]["content"], json!("App({});"));

        let (status, value, _) =
            call(app(dir.path()), get("/api/project/file?path=blob.bin")).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(value["error"]["code"], json!("binary_file"));
    }

    #[tokio::test]
    async fn project_tree_lists_entries_within_the_depth() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/pages/index")).unwrap();
        std::fs::write(dir.path().join("project/app.json"), "{}").unwrap();

        let (status, value, _) = call(
            app(dir.path()),
            get("/api/project/tree?dir=project&depth=2"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let entries = value["data"]["entries"].as_array().unwrap();
        let paths: Vec<&str> = entries
            .iter()
            .filter_map(|entry| entry["path"].as_str())
            .collect();
        assert!(paths.contains(&"app.json"));
        assert!(paths.contains(&"pages"));
        assert!(paths.contains(&"pages/index"));
        // `pages/index` is at depth 2, so its children must not appear.
        assert_eq!(
            entries
                .iter()
                .filter(|e| e["kind"] == json!("file"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn project_zip_round_trips_into_download() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project")).unwrap();
        std::fs::write(dir.path().join("project/app.js"), "App({});").unwrap();
        std::fs::create_dir_all(dir.path().join("project/_raw/main")).unwrap();
        std::fs::write(dir.path().join("project/_raw/main/app.js"), "raw").unwrap();

        let (status, value, _) = call(
            app(dir.path()),
            post_json("/api/project/zip", json!({ "dir": "project" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["data"]["files"], json!(1));
        assert_eq!(value["data"]["includeRaw"], json!(false));

        let (status, _, headers) = call(
            app(dir.path()),
            get("/api/project/download?path=project.zip"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let disposition = headers
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert!(disposition.starts_with("attachment"));
        assert_eq!(
            headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("application/zip")
        );
    }

    #[tokio::test]
    async fn cache_scopes_are_validated_before_anything_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir
            .path()
            .join("wx0000000000000001")
            .join("1539")
            .join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("app.json"), "{}").unwrap();

        let (status, value, _) = call(app(dir.path()), delete("/api/cache")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["error"]["code"], json!("invalid_argument"));

        let (status, value, _) = call(app(dir.path()), delete("/api/cache?scope=all")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["error"]["code"], json!("confirmation_required"));

        let (status, value, _) = call(app(dir.path()), delete("/api/cache?scope=projects")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["data"]["removedCount"], json!(1));
        assert!(!dir.path().join("wx0000000000000001").exists());
        assert!(dir.path().is_dir());
    }

    #[tokio::test]
    async fn programs_listing_is_scoped_to_the_requested_roots() {
        let dir = tempfile::tempdir().unwrap();
        let (status, value, _) = call(
            app(dir.path()),
            get("/api/programs?roots=/nonexistent-wxapkg-root&limit=5"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["data"]["count"], json!(0));
    }

    #[tokio::test]
    async fn programs_are_cached_but_refresh_bypasses_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        // One router, so both calls share the same state — and therefore the same cache.
        let router = app(dir.path());
        let uri = "/api/programs?roots=/nonexistent-wxapkg-root";

        let (_, first, _) = call(router.clone(), get(uri)).await;
        assert_eq!(first["data"]["cached"], json!(false));

        let (_, second, _) = call(router.clone(), get(uri)).await;
        assert_eq!(second["data"]["cached"], json!(true));
        assert!(second["data"]["cacheAgeSeconds"].is_i64());

        let (_, refreshed, _) = call(router, get(&format!("{uri}&refresh=true"))).await;
        assert_eq!(refreshed["data"]["cached"], json!(false));
    }

    #[tokio::test]
    async fn environment_reports_roots_and_omits_disk_space() {
        let dir = tempfile::tempdir().unwrap();
        let (status, value, _) = call(app(dir.path()), get("/api/environment")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(value["data"]["searchRoots"].is_array());
        assert!(value["data"]["diskSpace"].is_null());
        assert_eq!(
            value["data"]["workspace"].as_str(),
            Some(dir.path().display().to_string().as_str())
        );
    }

    #[tokio::test]
    async fn export_accepts_a_plain_boolean_for_beautify() {
        let workspace = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("app.js"), "App({})").unwrap();
        std::fs::write(source.path().join("app-config.json"), r#"{"pages":[]}"#).unwrap();
        let container = source.path().join("main.wxapkg");
        wxapkg_core::pipeline::repack(source.path(), &container).unwrap();

        // The console sends a toggle; an API client may send the full options object.
        for beautify in [json!(true), json!({"indentWidth": 4})] {
            let (status, value, _) = call(
                app(workspace.path()),
                post_json(
                    "/api/programs/export",
                    json!({ "paths": [container.display().to_string()], "beautify": beautify }),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{value}");
            assert_eq!(value["ok"], json!(true), "{value}");
        }
    }

    #[tokio::test]
    async fn export_reconstructs_a_project_from_a_local_container() {
        let workspace = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("pages/index")).unwrap();
        std::fs::write(source.path().join("app.js"), "App({})").unwrap();
        std::fs::write(source.path().join("pages/index/index.js"), "Page({})").unwrap();
        std::fs::write(
            source.path().join("app-config.json"),
            r#"{"pages":["pages/index/index"]}"#,
        )
        .unwrap();
        let container = source.path().join("main.wxapkg");
        wxapkg_core::pipeline::repack(source.path(), &container).unwrap();

        let (status, value, _) = call(
            app(workspace.path()),
            post_json(
                "/api/programs/export",
                json!({ "paths": [container.display().to_string()], "out": "exported", "verify": true }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["ok"], json!(true));
        // The pipeline names the folder after the container when no appid is available.
        assert_eq!(
            value["data"]["projectDir"],
            json!(workspace
                .path()
                .join("exported")
                .join("main")
                .display()
                .to_string())
        );
        assert!(!value["data"]["report"]["files"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(value["data"]["verification"]["ok"], json!(true));
        assert!(workspace.path().join("exported/main/app.json").is_file());
        assert!(workspace.path().join("exported/main/app.js").is_file());
    }

    #[tokio::test]
    async fn export_requires_a_target() {
        let dir = tempfile::tempdir().unwrap();
        let (status, value, _) = call(
            app(dir.path()),
            post_json("/api/programs/export", json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["error"]["code"], json!("invalid_argument"));
    }

    #[tokio::test]
    async fn the_api_requires_the_key_while_the_index_stays_public() {
        let dir = tempfile::tempdir().unwrap();
        let app = || app_with_key(dir.path(), "secret123");

        // The service index and the documentation are not locked out: a browser cannot
        // attach a header to a top-level navigation, and neither carries secrets.
        let (status, _, _) = call(app(), get("/")).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the service index must not be locked out"
        );
        let (status, _, _) = call(app(), get("/docs/")).await;
        assert_eq!(status, StatusCode::OK);

        let (status, value, _) = call(app(), get("/api/health")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(value["error"]["code"], json!("unauthorized"));
        assert_eq!(value["protocol"], json!(1));

        let (status, _, _) = call(
            app(),
            Request::builder()
                .uri("/api/health")
                .header("x-admin-key", "secret123")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // The query form exists for EventSource and <img>, which cannot set headers.
        let (status, _, _) = call(app(), get("/api/health?key=secret123")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            status_only(app(), get("/api/events?key=secret123")).await,
            StatusCode::OK
        );
        assert_eq!(
            status_only(app(), get("/api/events")).await,
            StatusCode::UNAUTHORIZED
        );
        if cfg!(feature = "remote") {
            let (status, _, _) =
                call(app(), get("/api/ilink/qr.svg?key=secret123&content=hello")).await;
            assert_eq!(status, StatusCode::OK);
        }

        let (status, value, _) = call(app(), get("/api/health?key=wrong")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(value["error"]["code"], json!("unauthorized"));
    }

    #[tokio::test]
    async fn without_a_configured_key_nothing_is_guarded() {
        let dir = tempfile::tempdir().unwrap();
        let (status, _, _) = call(app(dir.path()), get("/api/health")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_root_lists_the_api_entry_points() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(None, Some(dir.path().to_path_buf())).unwrap();
        let (status, value, _) = call(router(state), get("/")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["data"]["docs"], json!("/docs"));

        let state = AppState::new(None, Some(dir.path().to_path_buf())).unwrap();
        let (status, _, _) = call(router(state), get("/docs/")).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the API documentation stays available"
        );
    }
}
