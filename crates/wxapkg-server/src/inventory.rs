// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Local program inventory: list installed Mini Programs and export them over HTTP.
//!
//! These handlers exist so that a caller can go from "an appid I saw in the GUI" to
//! "an extracted project on disk" — or to a single file out of the container — without
//! knowing anything about the WeChat on-disk layout.

use std::path::{Path, PathBuf};

use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use wxapkg_core::beautify::FormatOptions;
use wxapkg_core::container::Wxapkg;
use wxapkg_core::discover::{self, Program};
use wxapkg_core::pipeline::{Source, UnpackRequest};
use wxapkg_core::project::BuildOptions;
use wxapkg_core::BuildReport;

use crate::api::{engine_failure, failure, internal_error, publish_report, success};
use crate::state::AppState;
use crate::util;

/// Routes owned by this module.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/programs", get(programs))
        .route("/programs/export", post(export_one))
        .route("/programs/export-batch", post(export_batch))
        .route("/programs/{appid}/icon", get(icon))
        .route("/programs/{appid}/raw/{*name}", get(raw_entry))
}

// ---------------------------------------------------------------------------
// inventory
// ---------------------------------------------------------------------------

/// How long a listing is reused before the corpus is walked again.
///
/// A full walk costs tens of seconds on a populated machine and the GUI opens the scan
/// view repeatedly, so a short TTL turns the repeats into an instant answer.
const PROGRAMS_CACHE_TTL_SECONDS: i64 = 30;

/// The inputs that determine a listing, and therefore the cache key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramsQuery {
    /// Roots that will actually be walked.
    pub roots: Vec<PathBuf>,
    /// Appid filter, already lower-cased.
    pub appid: Option<String>,
    /// Cap on the number of packages the walk examines.
    pub limit: Option<usize>,
}

/// One short-lived listing.
#[derive(Default)]
pub struct ProgramsCache {
    entry: Option<CachedListing>,
}

struct CachedListing {
    query: ProgramsQuery,
    stored_at: i64,
    items: Vec<Value>,
    /// Carried alongside the listing: a cache hit must not hide that the walk was incomplete.
    skipped: usize,
}

impl ProgramsCache {
    /// The cached listing when it matches `query` and is still fresh, with its age.
    pub fn get(&self, query: &ProgramsQuery, now: i64) -> Option<(Vec<Value>, usize, i64)> {
        let entry = self.entry.as_ref()?;
        let age = now.saturating_sub(entry.stored_at);
        if age >= PROGRAMS_CACHE_TTL_SECONDS || entry.query != *query {
            return None;
        }
        Some((entry.items.clone(), entry.skipped, age))
    }

    /// Replaces the cached listing.
    pub fn put(&mut self, query: ProgramsQuery, items: Vec<Value>, skipped: usize, now: i64) {
        self.entry = Some(CachedListing {
            query,
            stored_at: now,
            items,
            skipped,
        });
    }
}

/// `GET /api/programs`
///
/// `appid` prunes the walk itself, so asking about one program is far cheaper than listing
/// everything; `limit` is applied after grouping, so exactly that many programs come back.
async fn programs(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let pairs = util::parse_query(raw.as_deref().unwrap_or_default());
    let appid = util::query_value(&pairs, "appid")
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    let roots: Vec<PathBuf> = {
        let requested: Vec<PathBuf> = util::query_all(&pairs, "roots")
            .into_iter()
            .map(PathBuf::from)
            .collect();
        if requested.is_empty() {
            discover::default_roots()
        } else {
            requested
        }
    };
    let limit = util::query_number::<usize>(&pairs, "limit");
    let refresh = util::query_flag(&pairs, "refresh", false);

    let query = ProgramsQuery {
        roots,
        appid,
        limit,
    };

    if !refresh {
        if let Ok(cache) = state.programs_cache.lock() {
            if let Some((items, skipped, age)) = cache.get(&query, util::now()) {
                state.events.publish(
                    "info",
                    format!("served {} program(s) from cache ({age}s old)", items.len()),
                );
                return success(
                    "programs",
                    json!({
                        "programs": items,
                        "count": items.len(),
                        "cached": true,
                        "cacheAgeSeconds": age,
                        "skippedPaths": skipped,
                    }),
                );
            }
        }
    }

    state.events.publish(
        "info",
        "walking the WeChat data directories for installed Mini Programs",
    );

    let walk = tokio::task::spawn_blocking({
        let query = query.clone();
        let workspace = state.workspace.clone();
        move || -> Result<(Vec<Value>, u128, usize), wxapkg_core::Error> {
            let started = std::time::Instant::now();
            let found = discover::scan_programs_matching(
                Some(&query.roots),
                query.appid.as_deref(),
                query.limit,
            )?;
            let items = found
                .programs
                .iter()
                .map(|program| program_json(program, &workspace))
                .collect::<Vec<_>>();
            Ok((items, started.elapsed().as_millis(), found.skipped_paths))
        }
    })
    .await;

    match walk {
        Ok(Ok((items, elapsed_ms, skipped))) => {
            state.events.publish(
                "info",
                format!(
                    "corpus walk finished: {} program(s) in {elapsed_ms}ms (cached for {PROGRAMS_CACHE_TTL_SECONDS}s)",
                    items.len()
                ),
            );
            if skipped > 0 {
                // A directory we could not read makes the listing shorter without making it
                // wrong-looking, so it has to be said out loud rather than debug-logged.
                state.events.publish(
                    "warn",
                    format!(
                        "{skipped} path(s) under the search roots could not be read; the list may be incomplete"
                    ),
                );
            }
            if let Ok(mut cache) = state.programs_cache.lock() {
                cache.put(query, items.clone(), skipped, util::now());
            }
            success(
                "programs",
                json!({
                    "programs": items,
                    "count": items.len(),
                    "cached": false,
                    "skippedPaths": skipped,
                }),
            )
        }
        Ok(Err(err)) => engine_failure("programs", &err),
        Err(err) => internal_error("programs", &err.to_string()),
    }
}

/// Renders one installed program, including whether it has already been exported.
fn program_json(program: &Program, workspace: &Path) -> Value {
    let project_dir = expected_project_dir(workspace, &program.appid);
    let extracted = project_dir.is_dir();
    let main = program
        .main_package()
        .map(|package| package.path.display().to_string());
    let modified = program
        .packages
        .iter()
        .filter_map(|package| util::modified_at(&package.path))
        .max();

    json!({
        "appid": program.appid,
        "version": program.version,
        "wxid": program.wxid,
        "location": program.location.display().to_string(),
        "user": layout_user(&program.location),
        "encrypted": program.is_encrypted(),
        "totalSize": program.total_size(),
        "mainBundle": main,
        "modifiedAt": modified,
        "packages": program
            .packages
            .iter()
            .map(|package| json!({
                "name": package
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "path": package.path.display().to_string(),
                "size": package.size,
                "encrypted": package.encrypted.unwrap_or(false),
            }))
            .collect::<Vec<_>>(),
        "extracted": extracted,
        "projectDir": if extracted { Some(project_dir.display().to_string()) } else { None },
        // When the newest bundle landed on disk: the closest thing to "last downloaded".
        "updatedAt": modified,
    })
}

/// The directory an export lands in when the caller does not name one: the workspace
/// followed by the appid, matching what the pipeline writes.
fn expected_project_dir(workspace: &Path, appid: &str) -> PathBuf {
    workspace.join(appid)
}

/// Recovers the WeChat 4.x account hash from a `radium/users/<hash>` location.
fn layout_user(location: &Path) -> Option<String> {
    let components: Vec<String> = location
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    let index = components.iter().position(|part| part == "users")?;
    components
        .get(index + 1)
        .filter(|part| !part.is_empty())
        .cloned()
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ExportOptionsBody {
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
struct ExportBody {
    appid: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    out: Option<String>,
    #[serde(default)]
    options: ExportOptionsBody,
    beautify: Option<BeautifyRequest>,
    #[serde(default)]
    verify: bool,
}

/// Accepts `true` for the default format or an object for explicit control, so a console
/// toggle and an API client configuring indentation share one field.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum BeautifyRequest {
    Toggle(bool),
    Options(FormatOptions),
}

impl BeautifyRequest {
    fn into_options(self) -> Option<FormatOptions> {
        match self {
            Self::Toggle(true) => Some(FormatOptions::default()),
            Self::Toggle(false) => None,
            Self::Options(options) => Some(options),
        }
    }
}

/// One export, described independently of the HTTP layer so batch can reuse it.
struct ExportSpec {
    appid: Option<String>,
    paths: Vec<String>,
    out: Option<String>,
    options: BuildOptions,
    beautify: Option<FormatOptions>,
}

/// The result of one export.
struct Exported {
    program: Value,
    report: BuildReport,
    project_dir: PathBuf,
    /// One entry per Mini Program when the selection spanned several appids.
    projects: Vec<Value>,
}

/// `POST /api/programs/export`
async fn export_one(State(state): State<AppState>, Json(body): Json<ExportBody>) -> Response {
    let spec = ExportSpec {
        appid: body.appid.clone(),
        paths: body.paths.clone(),
        out: body.out.clone(),
        options: export_build_options(&body.options),
        beautify: body
            .beautify
            .clone()
            .and_then(BeautifyRequest::into_options),
    };
    if spec.appid.is_none() && spec.paths.is_empty() {
        return failure(
            "programs export",
            "invalid_argument",
            "provide `appid`, or `paths` to read packages from",
            None,
            StatusCode::BAD_REQUEST,
        );
    }

    state
        .events
        .publish("info", "exporting a Mini Program into the workspace");

    let workspace = state.workspace.clone();
    let result = tokio::task::spawn_blocking(move || run_export(&workspace, spec)).await;
    match result {
        Ok(Ok(exported)) => {
            publish_report(&state, &exported.report);
            state.events.publish(
                "info",
                format!("project written to {}", exported.project_dir.display()),
            );
            let verification = body.verify.then(|| verify_project(&exported.project_dir));
            success(
                "programs export",
                json!({
                    "program": exported.program,
                    "report": exported.report,
                    "projectDir": exported.project_dir.display().to_string(),
                    "projects": exported.projects,
                    "artifacts": artifacts_of(&exported.project_dir),
                    "verification": verification,
                }),
            )
        }
        Ok(Err(err)) => engine_failure("programs export", &err),
        Err(err) => internal_error("programs export", &err.to_string()),
    }
}

/// `POST /api/programs/export-batch`
async fn export_batch(State(state): State<AppState>, Json(body): Json<BatchBody>) -> Response {
    if body.appids.is_empty() {
        return failure(
            "programs export-batch",
            "invalid_argument",
            "appids must contain at least one entry",
            None,
            StatusCode::BAD_REQUEST,
        );
    }
    let concurrency = body.concurrency.unwrap_or(2).clamp(1, 4);
    let beautify = body.beautify.then(FormatOptions::default);
    let verify = body.verify;
    let out = body.out.clone();
    let paths: Vec<String> = body.paths.clone();

    state.events.publish(
        "info",
        format!(
            "exporting {} program(s), {} at a time",
            body.appids.len(),
            concurrency
        ),
    );

    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut handles = Vec::with_capacity(body.appids.len());

    for appid in body.appids.clone() {
        // Acquiring before spawning is what bounds the number of in-flight exports.
        let Ok(permit) = permits.clone().acquire_owned().await else {
            return internal_error(
                "programs export-batch",
                "the concurrency limiter was closed",
            );
        };
        let state = state.clone();
        let paths = paths.clone();
        let out = out.clone();
        let beautify = beautify.clone();
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let workspace = state.workspace.clone();
            let spec = ExportSpec {
                appid: Some(appid.clone()),
                paths,
                out,
                options: BuildOptions::default(),
                beautify,
            };
            let outcome = tokio::task::spawn_blocking(move || {
                let exported = run_export(&workspace, spec)?;
                Ok::<_, wxapkg_core::Error>(exported)
            })
            .await;

            match outcome {
                Ok(Ok(exported)) => {
                    state.events.publish(
                        "info",
                        format!(
                            "{appid}: exported {} file(s) to {}",
                            exported.report.total_files(),
                            exported.project_dir.display()
                        ),
                    );
                    let verification = verify.then(|| verify_project(&exported.project_dir));
                    let error = verification
                        .as_ref()
                        .filter(|value| !value["ok"].as_bool().unwrap_or(false))
                        .map(|_| "verification reported problems".to_owned());
                    json!({
                        "appid": appid,
                        "ok": error.is_none(),
                        "projectDir": exported.project_dir.display().to_string(),
                        "totalFiles": exported.report.total_files(),
                        "warnings": exported.report.warnings.len(),
                        "verification": verification,
                        "error": error,
                    })
                }
                Ok(Err(err)) => {
                    state
                        .events
                        .publish("error", format!("{appid}: export failed: {err}"));
                    json!({
                        "appid": appid,
                        "ok": false,
                        "projectDir": Value::Null,
                        "totalFiles": 0,
                        "error": err.to_string(),
                    })
                }
                Err(err) => {
                    state
                        .events
                        .publish("error", format!("{appid}: export task failed: {err}"));
                    json!({
                        "appid": appid,
                        "ok": false,
                        "projectDir": Value::Null,
                        "totalFiles": 0,
                        "error": err.to_string(),
                    })
                }
            }
        }));
    }

    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(value) => results.push(value),
            Err(err) => results.push(json!({
                "appid": Value::Null,
                "ok": false,
                "projectDir": Value::Null,
                "totalFiles": 0,
                "error": err.to_string(),
            })),
        }
    }

    let succeeded = results
        .iter()
        .filter(|item| item["ok"].as_bool().unwrap_or(false))
        .count();
    state.events.publish(
        "info",
        format!(
            "batch export finished: {succeeded}/{} succeeded",
            results.len()
        ),
    );
    success(
        "programs export-batch",
        json!({
            "results": results,
            "succeeded": succeeded,
            "failed": results.len() - succeeded,
        }),
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchBody {
    #[serde(default)]
    appids: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    out: Option<String>,
    #[serde(default)]
    beautify: bool,
    #[serde(default)]
    verify: bool,
    concurrency: Option<usize>,
}

/// Performs one export. Blocking; only ever called from `spawn_blocking`.
fn run_export(workspace: &Path, spec: ExportSpec) -> Result<Exported, wxapkg_core::Error> {
    let appid = spec
        .appid
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);

    let located = match &appid {
        Some(value) => Some(discover::require_program(value, None, None)?),
        None => None,
    };

    // The pipeline appends the appid, so this is only the base directory.
    let base = match spec.out.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        Some(explicit) => {
            let path = PathBuf::from(explicit);
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        }
        None => workspace.to_path_buf(),
    };

    let source = match &appid {
        Some(value) if spec.paths.is_empty() => Source::AppId {
            appid: value.clone(),
            version: None,
            roots: None,
        },
        _ => Source::Paths(spec.paths.iter().map(PathBuf::from).collect()),
    };

    let mut request = UnpackRequest::new(source, base);
    request.wxid = appid.clone();
    request.clean = true;
    request.options = spec.options;
    request.options.format = spec.beautify;
    if let Some(paths) =
        (!spec.paths.is_empty()).then(|| spec.paths.iter().map(PathBuf::from).collect::<Vec<_>>())
    {
        request.manifest = wxapkg_core::pipeline::find_manifest(&paths);
    }

    let mut reports = wxapkg_core::pipeline::unpack_grouped(&request)?;
    if reports.is_empty() {
        return Err(wxapkg_core::Error::InvalidArgument(
            "nothing was restored".to_owned(),
        ));
    }
    let multiple = reports.len() > 1;
    let projects: Vec<Value> = reports
        .iter()
        .map(|report| {
            json!({
                "outDir": report.out_dir,
                "files": report.total_files(),
                "warnings": report.warnings.len(),
            })
        })
        .collect();
    let report = reports.remove(0);
    let project_dir = if multiple {
        PathBuf::from(&report.out_dir)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(&report.out_dir))
    } else {
        PathBuf::from(&report.out_dir)
    };
    let program = match &located {
        Some(program) => program_json(program, workspace),
        None => json!({
            "appid": appid,
            "version": Value::Null,
            "location": Value::Null,
            "packages": spec.paths,
            "extracted": project_dir.is_dir(),
            "projectDir": project_dir.display().to_string(),
        }),
    };

    Ok(Exported {
        program,
        report,
        project_dir,
        projects,
    })
}

/// Maps the request-level option object onto the engine's builder options.
fn export_build_options(body: &ExportOptionsBody) -> BuildOptions {
    let auto_fix = body.auto_fix.unwrap_or(true);
    BuildOptions {
        keep_raw: body.keep_raw.unwrap_or(true),
        decompile_wxml: body.decompile_wxml.unwrap_or(true),
        rewrite_requires: body.rewrite_requires.unwrap_or(true),
        prune: auto_fix && body.prune.unwrap_or(true),
        synthesize: auto_fix && body.synthesize.unwrap_or(true),
        plugin_clean: auto_fix && body.plugin_clean.unwrap_or(true),
        plugins_mode: match body.plugins.as_deref() {
            Some("placeholder") => wxapkg_core::project::PluginsMode::Placeholder,
            Some("strip") => wxapkg_core::project::PluginsMode::Strip,
            _ => wxapkg_core::project::PluginsMode::Keep,
        },
        wxss_suffix: body.wxss_suffix.clone().unwrap_or_default(),
        ..BuildOptions::default()
    }
}

/// What an export produced, so a caller can hand the paths to the project endpoints.
fn artifacts_of(project_dir: &Path) -> Vec<Value> {
    let mut artifacts = vec![json!({
        "kind": "project",
        "path": project_dir.display().to_string(),
    })];
    for (kind, name) in [
        ("report", "_unpack_report.json"),
        ("raw", "_raw"),
        ("foreign", "_foreign"),
    ] {
        let path = project_dir.join(name);
        if path.exists() {
            artifacts.push(json!({ "kind": kind, "path": path.display().to_string() }));
        }
    }
    artifacts
}

/// Cheap structural check of a freshly written project.
///
/// It deliberately looks at the filesystem rather than the report: the point of `verify`
/// is to catch a build that reported success but produced nothing usable.
fn verify_project(project_dir: &Path) -> Value {
    let mut file_count = 0usize;
    for entry in walkdir::WalkDir::new(project_dir)
        .max_depth(14)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if entry.file_type().is_file() {
            file_count += 1;
        }
    }

    let checks = json!([
        {
            "name": "app.json present",
            "ok": project_dir.join("app.json").is_file(),
        },
        {
            "name": "project.config.json present",
            "ok": project_dir.join("project.config.json").is_file(),
        },
        {
            "name": "files written",
            "ok": file_count > 0,
            "detail": file_count,
        },
    ]);
    let ok = checks.as_array().is_some_and(|list| {
        list.iter()
            .all(|check| check["ok"].as_bool().unwrap_or(false))
    });
    json!({
        "dir": project_dir.display().to_string(),
        "ok": ok,
        "fileCount": file_count,
        "checks": checks,
    })
}

// ---------------------------------------------------------------------------
// icon and raw entries
// ---------------------------------------------------------------------------

/// `GET /api/programs/{appid}/icon`
async fn icon(UrlPath(appid): UrlPath<String>) -> Response {
    let Ok(appid) = crate::guard::ensure_segment(&appid) else {
        return invalid_id("programs icon", &appid);
    };
    let appid = appid.to_owned();

    let result = tokio::task::spawn_blocking(move || {
        let program = discover::require_program(&appid, None, None)?;
        let Some(main) = program.main_package() else {
            return Ok::<_, wxapkg_core::Error>(None);
        };
        let mut pkg = Wxapkg::open(&main.path, Some(&program.wxid))?;
        Ok(pkg.find_icon())
    })
    .await;

    match result {
        Ok(Ok(Some((name, bytes)))) => {
            let content_type = util::content_type_for(&name, &bytes);
            (
                StatusCode::OK,
                [
                    ("content-type", content_type),
                    // Icons are immutable for a given installed version.
                    ("cache-control", "public, max-age=86400"),
                ],
                bytes,
            )
                .into_response()
        }
        Ok(Ok(None)) => failure(
            "programs icon",
            "not_found",
            "no icon could be located inside the installed bundle",
            Some("the program may not be installed, or it ships no image asset"),
            StatusCode::NOT_FOUND,
        ),
        Ok(Err(err)) => engine_failure("programs icon", &err),
        Err(err) => internal_error("programs icon", &err.to_string()),
    }
}

/// `GET /api/programs/{appid}/raw/{name}`
async fn raw_entry(
    State(state): State<AppState>,
    UrlPath((appid, name)): UrlPath<(String, String)>,
) -> Response {
    if crate::guard::ensure_segment(&appid).is_err() {
        return invalid_id("programs raw", &appid);
    }
    let trimmed = name.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.contains("..") {
        return failure(
            "programs raw",
            "invalid_argument",
            "the entry name is not acceptable",
            None,
            StatusCode::BAD_REQUEST,
        );
    }
    let entry_name = trimmed.to_owned();
    let lookup = entry_name.clone();

    state
        .events
        .publish("info", format!("reading {entry_name} from {appid}"));

    let result = tokio::task::spawn_blocking(move || {
        let program = discover::require_program(&appid, None, None)?;
        for package in &program.packages {
            let mut pkg = Wxapkg::open(&package.path, Some(&program.wxid))?;
            let Some(entry) = pkg.find_entry(&lookup).cloned() else {
                continue;
            };
            let blob = pkg.read_entry(&entry, false, false)?;
            return Ok::<_, wxapkg_core::Error>(Some((entry.name, blob)));
        }
        Ok(None)
    })
    .await;

    match result {
        Ok(Ok(Some((name, bytes)))) => {
            let content_type = util::content_type_for(&name, &bytes);
            (
                StatusCode::OK,
                [
                    ("content-type", content_type),
                    ("cache-control", "no-store"),
                ],
                bytes,
            )
                .into_response()
        }
        Ok(Ok(None)) => failure(
            "programs raw",
            "entry_not_found",
            &format!("no entry named {entry_name:?} in the installed packages"),
            Some("list the container with POST /api/inspect first"),
            StatusCode::NOT_FOUND,
        ),
        Ok(Err(err)) => engine_failure("programs raw", &err),
        Err(err) => internal_error("programs raw", &err.to_string()),
    }
}

fn invalid_id(command: &str, value: &str) -> Response {
    failure(
        command,
        "invalid_argument",
        &format!("{value:?} is not a valid appid segment"),
        None,
        StatusCode::BAD_REQUEST,
    )
}

/// Locates an icon, preferring names referenced by configuration.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_user_reads_the_radium_hash() {
        let path =
            PathBuf::from("/x/radium/users/765a2332e32edc826b84707ff681e7bd/applet/packages/wx1/2");
        assert_eq!(
            layout_user(&path).as_deref(),
            Some("765a2332e32edc826b84707ff681e7bd")
        );
        assert_eq!(layout_user(Path::new("/tmp/other")), None);
    }

    #[test]
    fn expected_project_dir_is_the_appid_folder() {
        let root = Path::new("/work");
        assert_eq!(
            expected_project_dir(root, "wx1"),
            PathBuf::from("/work/wx1")
        );
    }

    #[test]
    fn verify_reports_missing_entries() {
        let dir = tempfile::tempdir().unwrap();
        let report = verify_project(dir.path());
        assert_eq!(report["ok"], json!(false));
        std::fs::write(dir.path().join("app.json"), "{}").unwrap();
        std::fs::write(dir.path().join("project.config.json"), "{}").unwrap();
        assert_eq!(verify_project(dir.path())["ok"], json!(true));
    }

    #[test]
    fn cache_hits_only_for_the_same_query_and_within_the_ttl() {
        let mut cache = ProgramsCache::default();
        let query = ProgramsQuery {
            roots: vec![PathBuf::from("/corpus")],
            appid: None,
            limit: None,
        };

        assert!(cache.get(&query, 1_000).is_none(), "nothing stored yet");

        cache.put(query.clone(), vec![json!({ "appid": "wx1" })], 7, 1_000);
        let (items, skipped, age) = cache.get(&query, 1_010).expect("fresh listing");
        assert_eq!(items.len(), 1);
        assert_eq!(age, 10);
        assert_eq!(skipped, 7, "the skipped count must survive the cache");
        assert!(
            cache
                .get(&query, 1_000 + PROGRAMS_CACHE_TTL_SECONDS)
                .is_none(),
            "the entry must expire at the TTL"
        );

        for other in [
            ProgramsQuery {
                appid: Some("wx2".to_owned()),
                ..query.clone()
            },
            ProgramsQuery {
                limit: Some(5),
                ..query.clone()
            },
            ProgramsQuery {
                roots: vec![PathBuf::from("/elsewhere")],
                ..query.clone()
            },
        ] {
            assert!(
                cache.get(&other, 1_010).is_none(),
                "a different query must not be served from this entry"
            );
        }
    }
}
