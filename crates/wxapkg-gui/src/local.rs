// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Local pipeline: scan, inspect, unpack, extract, beautify, verify and repack.
//!
//! Every command here is a thin adapter over `wxapkg-core`; the GUI adds no behaviour of its
//! own, so its results are the CLI's results.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use wxapkg_core::beautify::FormatOptions;
use wxapkg_core::discover;
use wxapkg_core::pipeline::{self, Source, UnpackRequest};
use wxapkg_core::project::BuildOptions;
use wxapkg_core::verify::{VerifyOptions, VerifyReport};

use crate::{host, image_data_url, run_job};

/// One installed Mini Program, as the local view needs it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgramRow {
    appid: String,
    version: String,
    location: String,
    packages: usize,
    total_size: u64,
    encrypted: bool,
    /// Newest bundle on disk, as a unix timestamp.
    updated_at: Option<i64>,
    /// Container files, so the detail and raw views need no second lookup.
    package_paths: Vec<String>,
    /// Whether an export of this version already exists in the workspace.
    extracted: bool,
    project_dir: Option<String>,
}

/// The local listing plus the number of paths the walk could not read.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgramsView {
    programs: Vec<ProgramRow>,
    skipped_paths: usize,
    /// Directories the operating system refused; a non-zero count means the process needs
    /// file-access permission rather than that the listing is merely incomplete.
    denied_paths: usize,
}

/// Snapshot of the last finished scan, so the window can paint before walking again.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanCache {
    /// Unix timestamp of the scan that produced the rows.
    pub saved_at: i64,
    /// The rows, exactly as the scan returned them.
    pub rows: Vec<ProgramRow>,
}

/// Path of the scan cache.
fn scan_cache_path() -> Option<PathBuf> {
    let dir = crate::platform::cache_dir()?;
    crate::platform::ensure_dir(&dir).ok()?;
    Some(dir.join("last-scan.json"))
}

/// Returns the previous scan, without touching the WeChat data directories.
#[tauri::command]
pub async fn cached_scan() -> Result<Option<ScanCache>, String> {
    let result = tauri::async_runtime::spawn_blocking(|| {
        let path = scan_cache_path()?;
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str::<ScanCache>(&text).ok()
    })
    .await
    .map_err(|err| format!("the cache job could not be scheduled: {err}"))?;
    Ok(result)
}

/// Walks the WeChat data directories and lists installed Mini Programs.
#[tauri::command]
pub async fn scan_programs(app: AppHandle) -> Result<ProgramsView, String> {
    crate::job(&app, "scan", "扫描本机小程序", "start");
    let workspace = host::workspace();
    let handle = app.clone();
    let scan = run_job(&app, "scan", move || {
        let found =
            discover::scan_programs_matching(None, None, None).map_err(|err| err.to_string())?;
        let rows = found
            .programs
            .iter()
            .map(|program| {
                let dir = project_dir(&workspace, &program.appid);
                ProgramRow {
                    appid: program.appid.clone(),
                    version: program.version.clone(),
                    location: program.location.display().to_string(),
                    packages: program.packages.len(),
                    total_size: program.total_size(),
                    encrypted: program.is_encrypted(),
                    updated_at: newest_bundle_seconds(program),
                    package_paths: program
                        .packages
                        .iter()
                        .map(|package| package.path.display().to_string())
                        .collect(),
                    extracted: dir.is_dir(),
                    project_dir: Some(dir.display().to_string()),
                }
            })
            .collect();
        let view = ProgramsView {
            programs: rows,
            skipped_paths: found.skipped_paths,
            denied_paths: found.denied_paths,
        };

        // Remember the result for the next start; a failed write must not fail the scan.
        if let Some(path) = scan_cache_path() {
            let cache = ScanCache {
                saved_at: crate::platform::now_unix(),
                rows: view.programs.clone(),
            };
            if let Ok(text) = serde_json::to_string(&cache) {
                let _ = std::fs::write(path, text);
            }
        }

        crate::job_with(
            &handle,
            "scan",
            "扫描本机小程序",
            "progress",
            Some(view.programs.len()),
            Some(view.programs.len()),
            Some(format!("找到 {} 个小程序", view.programs.len())),
        );
        Ok(view)
    })
    .await;

    match &scan {
        Ok(view) => {
            if view.skipped_paths > 0 {
                crate::log(
                    &app,
                    "warn",
                    format!(
                        "{} path(s) could not be read; the listing may be incomplete",
                        view.skipped_paths
                    ),
                );
            }
            crate::job_with(
                &app,
                "scan",
                "扫描本机小程序",
                "done",
                Some(view.programs.len()),
                Some(view.programs.len()),
                Some(format!("{} 个小程序", view.programs.len())),
            );
        }
        Err(err) => crate::job_with(
            &app,
            "scan",
            "扫描本机小程序",
            "failed",
            None,
            None,
            Some(err.clone()),
        ),
    }
    scan
}

/// Unix timestamp of the newest bundle of a program, in seconds.
fn newest_bundle_seconds(program: &discover::Program) -> Option<i64> {
    program
        .packages
        .iter()
        .filter_map(|package| {
            let modified = package.path.metadata().ok()?.modified().ok()?;
            i64::try_from(
                modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs(),
            )
            .ok()
        })
        .max()
}

/// The directory an export lands in when the caller does not name one.
/// Where a restored project lives: the workspace, then the appid.
fn project_dir(workspace: &std::path::Path, appid: &str) -> PathBuf {
    workspace.join(appid)
}

/// Reads the launcher icon WeChat cached for a program.
///
/// Deliberately does not open the container: with dozens of rows, resolving names or icons
/// from the bundles made the list feel stuck. WeChat's own cache is a single small file read.
#[tauri::command]
pub async fn program_icon(appid: String) -> Result<Option<String>, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        discover::cached_icon(&appid).and_then(|path| image_data_url(&path))
    })
    .await
    .map_err(|err| format!("the icon job could not be scheduled: {err}"))?;
    Ok(result)
}

/// Container structure of the selected files.
#[tauri::command]
pub async fn inspect(
    app: AppHandle,
    paths: Vec<String>,
    wxid: Option<String>,
) -> Result<Vec<pipeline::ContainerInfo>, String> {
    run_job(&app, "inspect", move || {
        let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
        if paths.is_empty() {
            return Err("no container was selected".to_owned());
        }
        pipeline::inspect(&paths, wxid.as_deref()).map_err(|err| err.to_string())
    })
    .await
}

/// What a raw extraction produced.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RawResult {
    out_dir: String,
    files: usize,
    notes: Vec<String>,
}

/// Extracts every file verbatim, without decompiling.
#[tauri::command]
pub async fn raw_export(
    app: AppHandle,
    paths: Vec<String>,
    out: Option<String>,
) -> Result<RawResult, String> {
    let out_dir = host::resolve_output(out.as_deref(), "original_wxapkg");
    run_job(&app, "raw export", move || {
        let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
        if paths.is_empty() {
            return Err("no container was selected".to_owned());
        }
        let report =
            pipeline::raw_extract(&paths, &out_dir, None, true).map_err(|err| err.to_string())?;
        Ok(RawResult {
            out_dir: report.out_dir,
            files: report.files,
            notes: report.notes,
        })
    })
    .await
}

/// A boolean toggle or explicit formatting options.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum BeautifyFlag {
    /// `true` applies the default formatting.
    Toggle(bool),
    /// Full options object.
    Options(FormatOptions),
}

impl BeautifyFlag {
    fn into_options(self) -> Option<FormatOptions> {
        match self {
            Self::Toggle(true) => Some(FormatOptions::default()),
            Self::Toggle(false) => None,
            Self::Options(options) => Some(options),
        }
    }
}

/// Reconstruction knobs, mirroring the CLI's `--no-*` switches with the same defaults.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct BuildFlags {
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

impl BuildFlags {
    /// Maps the request onto the engine's builder options.
    fn into_options(self) -> BuildOptions {
        let auto_fix = self.auto_fix.unwrap_or(true);
        BuildOptions {
            keep_raw: self.keep_raw.unwrap_or(true),
            decompile_wxml: self.decompile_wxml.unwrap_or(true),
            rewrite_requires: self.rewrite_requires.unwrap_or(true),
            prune: auto_fix && self.prune.unwrap_or(true),
            synthesize: auto_fix && self.synthesize.unwrap_or(true),
            plugin_clean: auto_fix && self.plugin_clean.unwrap_or(true),
            plugins_mode: match self.plugins.as_deref() {
                Some("placeholder") => wxapkg_core::project::PluginsMode::Placeholder,
                Some("strip") => wxapkg_core::project::PluginsMode::Strip,
                _ => wxapkg_core::project::PluginsMode::Keep,
            },
            wxss_suffix: self.wxss_suffix.unwrap_or_default(),
            ..BuildOptions::default()
        }
    }
}

/// What an unpack request carries.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnpackArgs {
    /// Installed appid to locate; mutually exclusive with `paths`.
    appid: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    out: Option<String>,
    #[serde(default)]
    beautify: Option<BeautifyFlag>,
    #[serde(default)]
    verify: bool,
    #[serde(default)]
    options: BuildFlags,
}

/// What an unpack produced.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    project_dir: String,
    report: wxapkg_core::BuildReport,
    verification: Option<VerifyReport>,
    /// One entry per Mini Program when the selection spanned several appids.
    #[serde(default)]
    projects: Vec<ExportProject>,
}

/// One restored project inside a multi-program selection.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportProject {
    appid: String,
    project_dir: String,
    files: usize,
    warnings: usize,
}

/// Reconstructs a project from an installed program or from explicit containers.
#[tauri::command]
pub async fn unpack(app: AppHandle, args: UnpackArgs) -> Result<ExportResult, String> {
    let workspace = host::workspace();
    crate::job(&app, "unpack", "还原源码", "start");
    let progress_app = app.clone();
    let progress: ContainerProgress = std::sync::Arc::new(move |done, total, name| {
        crate::job_with(
            &progress_app,
            "unpack",
            "还原源码",
            "progress",
            Some(done),
            Some(total),
            Some(format!("正在还原 {name}")),
        );
    });
    let outcome = run_job(&app, "unpack", move || {
        run_export(
            &workspace,
            ExportSpec {
                appid: args.appid.clone(),
                paths: args.paths.clone(),
                out: args.out.clone(),
                folder: None,
                unpack_prefix: false,
                options: args.options.into_options(),
                beautify: args.beautify.and_then(BeautifyFlag::into_options),
                verify: args.verify,
                progress: Some(progress),
            },
        )
    })
    .await;

    match &outcome {
        Ok(result) => crate::job_with(
            &app,
            "unpack",
            "还原源码",
            "done",
            None,
            None,
            Some(result.project_dir.clone()),
        ),
        Err(err) => crate::job_with(
            &app,
            "unpack",
            "还原源码",
            "failed",
            None,
            None,
            Some(err.clone()),
        ),
    }
    outcome
}

/// One program's outcome inside a batch export.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchItem {
    appid: String,
    ok: bool,
    project_dir: Option<String>,
    files: Option<usize>,
    error: Option<String>,
}

/// What a batch export produced.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchResult {
    results: Vec<BatchItem>,
    succeeded: usize,
    failed: usize,
}

/// Arguments of a batch export.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchArgs {
    appids: Vec<String>,
    out: Option<String>,
    #[serde(default)]
    beautify: bool,
    #[serde(default)]
    verify: bool,
    /// Destination directory per program is `<out>/<appid>`; this only sets the root.
    #[serde(default)]
    options: BuildFlags,
}

/// Reconstructs several installed programs in one run.
#[tauri::command]
pub async fn export_batch(app: AppHandle, args: BatchArgs) -> Result<BatchResult, String> {
    if args.appids.is_empty() {
        return Err("select at least one program".to_owned());
    }
    let total = args.appids.len();
    let workspace = host::workspace();
    let beautify = args.beautify.then(FormatOptions::default);
    let handle = app.clone();

    crate::job_with(
        &app,
        "batch",
        "批量导出",
        "start",
        Some(0),
        Some(total),
        None,
    );

    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let target = args.out.as_deref().map(|root| {
            let root = root.trim();
            let base = PathBuf::from(root);
            if base.is_absolute() {
                base.display().to_string()
            } else {
                workspace.join(base).display().to_string()
            }
        });
        let workers = total
            .min(std::thread::available_parallelism().map_or(4, |n| n.get()))
            .clamp(1, 8);
        let appids = args.appids;
        let options = args.options;
        let verify = args.verify;
        let next = std::sync::atomic::AtomicUsize::new(0);
        let completed = std::sync::atomic::AtomicUsize::new(0);
        let results: std::sync::Mutex<Vec<Option<BatchItem>>> =
            std::sync::Mutex::new((0..total).map(|_| None).collect());

        std::thread::scope(|scope| {
            for _ in 0..workers {
                let handle = handle.clone();
                let workspace = &workspace;
                let target = target.clone();
                let options = options.clone();
                let beautify = beautify.clone();
                let appids = &appids;
                let results = &results;
                let next = &next;
                let completed = &completed;
                scope.spawn(move || loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(appid) = appids.get(index) else {
                        break;
                    };
                    crate::job_with(
                        &handle,
                        "batch",
                        "批量导出",
                        "progress",
                        Some(completed.load(std::sync::atomic::Ordering::Relaxed)),
                        Some(total),
                        Some(format!("正在导出 {appid}")),
                    );
                    let item = match run_export(
                        workspace,
                        ExportSpec {
                            appid: Some(appid.clone()),
                            paths: Vec::new(),
                            out: target.clone(),
                            folder: None,
                            unpack_prefix: false,
                            options: options.clone().into_options(),
                            beautify: beautify.clone(),
                            verify,
                            progress: None,
                        },
                    ) {
                        Ok(result) => BatchItem {
                            appid: appid.clone(),
                            ok: true,
                            project_dir: Some(result.project_dir),
                            files: Some(result.report.total_files()),
                            error: None,
                        },
                        Err(err) => BatchItem {
                            appid: appid.clone(),
                            ok: false,
                            project_dir: None,
                            files: None,
                            error: Some(err),
                        },
                    };
                    if let Ok(mut slots) = results.lock() {
                        slots[index] = Some(item);
                    }
                    let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    crate::job_with(
                        &handle,
                        "batch",
                        "批量导出",
                        "progress",
                        Some(done),
                        Some(total),
                        Some(format!("已完成 {appid}（{done}/{total}）")),
                    );
                });
            }
        });

        let results: Vec<BatchItem> = results
            .into_inner()
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect();
        let failed = results.iter().filter(|item| !item.ok).count();
        BatchResult {
            succeeded: results.len() - failed,
            results,
            failed,
        }
    })
    .await
    .map_err(|err| format!("the batch job could not be scheduled: {err}"))?;

    crate::job_with(
        &app,
        "batch",
        "批量导出",
        "done",
        Some(total),
        Some(total),
        Some(format!(
            "成功 {}，失败 {}",
            outcome.succeeded, outcome.failed
        )),
    );
    crate::log(
        &app,
        "info",
        format!(
            "批量导出完成：成功 {}，失败 {}",
            outcome.succeeded, outcome.failed
        ),
    );
    Ok(outcome)
}

/// Callback the pipeline invokes once per reconstructed container.
type ContainerProgress = std::sync::Arc<dyn Fn(usize, usize, &str) + Send + Sync>;

/// Everything one export needs besides the workspace root.
struct ExportSpec {
    appid: Option<String>,
    paths: Vec<String>,
    out: Option<String>,
    /// Folder name under `out`; `None` uses the appid.
    folder: Option<String>,
    /// Prefix restored folders with `unpack_` (user-selected containers).
    unpack_prefix: bool,
    options: BuildOptions,
    beautify: Option<FormatOptions>,
    verify: bool,
    progress: Option<ContainerProgress>,
}

/// Performs one export. Blocking; only ever called from a worker.
fn run_export(workspace: &std::path::Path, spec: ExportSpec) -> Result<ExportResult, String> {
    let appid = spec
        .appid
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    if appid.is_none() && spec.paths.is_empty() {
        return Err("select an installed program, or point at containers".to_owned());
    }

    let paths: Vec<PathBuf> = spec.paths.iter().map(PathBuf::from).collect();
    // Only an appid *selection* must exist locally. Restoring downloaded containers passes
    // the appid as their decryption key; those files live on disk, not in WeChat's cache.
    if paths.is_empty() {
        if let Some(value) = &appid {
            discover::require_program(value, None, None)
                .map_err(|err| format!("cannot locate {value}: {err}"))?;
        }
    }

    // The pipeline appends the appid to whatever base it is given, so the workspace is the
    // base here and the final directory is reported back in `report.out_dir`.
    let base = match spec
        .out
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(explicit) => {
            let path = PathBuf::from(explicit);
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        }
        // The pipeline appends the appid, so the workspace is the base to pass.
        None => workspace.to_path_buf(),
    };

    let mut request = UnpackRequest::new(
        match &appid {
            Some(value) if paths.is_empty() => Source::AppId {
                appid: value.clone(),
                version: None,
                roots: None,
            },
            _ => Source::Paths(paths.clone()),
        },
        base,
    );
    request.wxid = appid.clone();
    request.clean = true;
    request.project_folder = spec.folder.clone();
    request.unpack_prefix = spec.unpack_prefix;
    request.options = spec.options;
    request.options.format = spec.beautify;
    if !paths.is_empty() {
        request.manifest = pipeline::find_manifest(&paths);
    }
    request.progress = spec.progress;

    let mut reports = pipeline::unpack_grouped(&request).map_err(|err| err.to_string())?;
    if reports.is_empty() {
        return Err("nothing was restored".to_owned());
    }
    let multiple = reports.len() > 1;
    let projects: Vec<ExportProject> = reports
        .iter()
        .map(|report| ExportProject {
            appid: Path::new(&report.out_dir)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            project_dir: report.out_dir.clone(),
            files: report.total_files(),
            warnings: report.warnings.len(),
        })
        .collect();
    // A selection that produced several projects opens at the base folder; one project
    // opens at its own root.
    let project = if multiple {
        PathBuf::from(&projects[0].project_dir)
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(&projects[0].project_dir))
    } else {
        PathBuf::from(&projects[0].project_dir)
    };
    let report = reports.remove(0);
    let verification = (!multiple && spec.verify)
        .then(|| wxapkg_core::verify::verify_project(&project, &VerifyOptions::default()).ok())
        .flatten();
    Ok(ExportResult {
        project_dir: project.display().to_string(),
        report,
        verification,
        projects,
    })
}

/// Formats an existing project tree in place.
#[tauri::command]
pub async fn beautify(
    app: AppHandle,
    path: String,
    options: Option<FormatOptions>,
) -> Result<wxapkg_core::BeautifyReport, String> {
    run_job(&app, "beautify", move || {
        let root = PathBuf::from(&path);
        if !root.is_dir() {
            return Err(format!("{path} is not a directory"));
        }
        pipeline::beautify_tree(&root, &options.unwrap_or_default()).map_err(|err| err.to_string())
    })
    .await
}

/// Validates a reconstructed project, optionally with a real JavaScript syntax check.
#[tauri::command]
pub async fn verify(app: AppHandle, path: String, node: bool) -> Result<VerifyReport, String> {
    run_job(&app, "verify", move || {
        let root = PathBuf::from(&path);
        if !root.is_dir() {
            return Err(format!("{path} is not a directory"));
        }
        let mut report = wxapkg_core::verify::verify_project(&root, &VerifyOptions::default())
            .map_err(|err| err.to_string())?;
        if node {
            match wxapkg_core::verify::node_syntax_check(&root) {
                Ok(failures) => pipeline::merge_syntax_failures(&mut report, &failures),
                Err(err) => {
                    return Err(format!(
                        "node is unavailable, so the syntax check was skipped: {err}"
                    ))
                }
            }
        }
        Ok(report)
    })
    .await
}

/// What a repack produced.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackResult {
    out: String,
    files: usize,
}

/// Packs a project directory back into a container.
#[tauri::command]
pub async fn pack(app: AppHandle, path: String, out: Option<String>) -> Result<PackResult, String> {
    run_job(&app, "pack", move || {
        let root = PathBuf::from(&path);
        if !root.is_dir() {
            return Err(format!("{path} is not a directory"));
        }
        let target = match out
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(explicit) => PathBuf::from(explicit),
            None => {
                let name = root
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "project".to_owned());
                root.with_file_name(format!("{name}.wxapkg"))
            }
        };
        let files = pipeline::repack(&root, &target).map_err(|err| err.to_string())?;
        Ok(PackResult {
            out: target.display().to_string(),
            files,
        })
    })
    .await
}

// ---------------------------------------------------------------------------
// explicit targets: one container, or a whole folder of them
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// explicit targets: one container, or a whole folder of them
// ---------------------------------------------------------------------------

/// Decrypts every `V1MMWX` container in a folder, trying the caller's keys in turn.
///
/// Plaintext containers are copied through so the output folder is a complete, usable set.
/// `keys` may hold several appids — the whole folder is tried against each until one opens
/// a given container, exactly like the reference tools do for mixed downloads.
#[tauri::command]
pub async fn decrypt_folder(
    app: AppHandle,
    folder: String,
    keys: Vec<String>,
    out: Option<String>,
) -> Result<wxapkg_core::pipeline::DecryptFolderReport, String> {
    let handle = app.clone();
    run_job(&app, "decrypt folder", move || {
        let out_dir = host::resolve_output(out.as_deref(), "decrypted");
        let progress_app = handle.clone();
        let progress = move |done: usize, total: usize, name: &str| {
            crate::job_with(
                &progress_app,
                "decrypt",
                "文件夹解密",
                "progress",
                Some(done),
                Some(total),
                Some(name.to_owned()),
            );
        };
        let report = wxapkg_core::pipeline::decrypt_folder(
            std::path::Path::new(&folder),
            &keys,
            &out_dir,
            Some(&progress),
        )
        .map_err(|err| err.to_string())?;
        crate::job_with(
            &handle,
            "decrypt",
            "文件夹解密",
            "done",
            Some(report.outcomes.len()),
            Some(report.outcomes.len()),
            Some(format!(
                "解密 {}，明文 {}，失败 {}",
                report.decrypted, report.plain, report.failed
            )),
        );
        Ok(report)
    })
    .await
}

/// Unpacks explicitly chosen container files, with an optional decryption key.
#[tauri::command]
pub async fn unpack_files(
    app: AppHandle,
    paths: Vec<String>,
    key: Option<String>,
    out: Option<String>,
    folder: Option<String>,
    beautify: bool,
) -> Result<ExportResult, String> {
    let workspace = host::workspace();
    crate::job(&app, "unpack", "还原源码", "start");
    let key = key
        .map(|k| k.trim().to_lowercase())
        .filter(|k| !k.is_empty());
    let outcome = run_job(&app, "unpack files", move || {
        let paths: Vec<String> = paths.into_iter().filter(|p| !p.trim().is_empty()).collect();
        if paths.is_empty() {
            return Err("no container was chosen".to_owned());
        }
        run_export(
            &workspace,
            ExportSpec {
                appid: key,
                paths,
                out,
                folder,
                unpack_prefix: true,
                options: BuildOptions::default(),
                beautify: beautify.then(FormatOptions::default),
                verify: false,
                progress: None,
            },
        )
    })
    .await;
    match &outcome {
        Ok(result) => crate::job_with(
            &app,
            "unpack",
            "还原源码",
            "done",
            None,
            None,
            Some(result.project_dir.clone()),
        ),
        Err(err) => crate::job_with(
            &app,
            "unpack",
            "还原源码",
            "failed",
            None,
            None,
            Some(err.clone()),
        ),
    }
    outcome
}

#[cfg(test)]
mod export_tests {
    use super::*;

    fn write_container(dir: &Path) -> PathBuf {
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("pages/index")).unwrap();
        std::fs::write(src.join("app.js"), "App({});").unwrap();
        std::fs::write(
            src.join("app-config.json"),
            r#"{"pages":["pages/index/index"]}"#,
        )
        .unwrap();
        std::fs::write(src.join("pages/index/index.js"), "Page({});").unwrap();
        let out = dir.join("wx0000000000000000___APP___t4.wxapkg");
        wxapkg_core::pipeline::repack(&src, &out).unwrap();
        out
    }

    // Regression: a downloaded record passes its appid as the decryption key. The program
    // is not installed in WeChat's cache, so the "locate the program" guard must not run.
    #[test]
    fn downloaded_containers_restore_without_a_local_install() {
        let tmp = tempfile::tempdir().unwrap();
        let container = write_container(tmp.path());
        let out = tmp.path().join("original_wxapkg/wx0000000000000000/wxapkg");
        let spec = ExportSpec {
            appid: Some("wx0000000000000000".to_owned()),
            paths: vec![container.display().to_string()],
            out: Some(out.display().to_string()),
            folder: Some("unpack_wx0000000000000000".to_owned()),
            unpack_prefix: true,
            options: BuildOptions::default(),
            beautify: None,
            verify: false,
            progress: None,
        };
        let result = run_export(tmp.path(), spec).expect("restore downloaded containers");
        assert!(Path::new(&result.project_dir).join("app.json").is_file());
    }
}
