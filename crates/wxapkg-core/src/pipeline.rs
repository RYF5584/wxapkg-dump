// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! High-level entry points shared by every front-end (CLI, HTTP server, GUI, MCP).
//!
//! Each function is a thin orchestration layer over [`crate::container`],
//! [`crate::project`] and [`crate::beautify`], and each returns a serialisable report so
//! that no front-end has to reimplement formatting or error classification.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

use crate::beautify::{self, FormatOptions};
use crate::container::Wxapkg;
use crate::discover;
use crate::error::{Error, IoContext, Result};
use crate::project::{resolve_parts, BuildOptions, Part, ProjectBuilder};
use crate::report::{BeautifyReport, BuildReport, ProgramSummary, RawExtractReport};

/// Input selection for [`unpack`].
#[derive(Debug, Clone)]
pub enum Source {
    /// Explicit files, archives or directories.
    Paths(Vec<PathBuf>),
    /// An appid to locate in the local WeChat data directories.
    AppId {
        /// Appid (`wx` + 16 hex digits).
        appid: String,
        /// Restrict to a specific version directory.
        version: Option<String>,
        /// Override the scanned roots.
        roots: Option<Vec<PathBuf>>,
    },
    /// A directory that is already a collection of containers.
    Directory(PathBuf),
}

/// Everything needed to turn containers into a project.
#[derive(Clone)]
pub struct UnpackRequest {
    /// Where the containers come from.
    pub source: Source,
    /// Output directory.
    pub out_dir: PathBuf,
    /// Explicit decryption key for `V1MMWX` containers.
    pub wxid: Option<String>,
    /// Manifest produced by an export tool, when available.
    pub manifest: Option<Value>,
    /// Explicit main package file name.
    pub main: Option<String>,
    /// Wipe the output directory before writing.
    pub clean: bool,
    /// Builder behaviour.
    pub options: BuildOptions,
    /// Called as each container is reconstructed: `(index, total, container name)`.
    #[allow(clippy::type_complexity)]
    pub progress: Option<std::sync::Arc<dyn Fn(usize, usize, &str) + Send + Sync>>,
    /// Folder name to use instead of the appid.
    pub project_folder: Option<String>,
    /// Prefix each restored folder with `unpack_` (user-selected inputs and downloads).
    pub unpack_prefix: bool,
}

impl UnpackRequest {
    /// Creates a request with default build options.
    pub fn new(source: Source, out_dir: impl Into<PathBuf>) -> Self {
        Self {
            source,
            out_dir: out_dir.into(),
            wxid: None,
            manifest: None,
            main: None,
            clean: false,
            options: BuildOptions::default(),
            progress: None,
            project_folder: None,
            unpack_prefix: false,
        }
    }
}

/// Scans for locally installed Mini Programs.
pub fn scan(
    roots: Option<&[PathBuf]>,
    appid: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<ProgramSummary>> {
    Ok(scan_matching(roots, appid, limit)?.0)
}

/// Like [`scan`], but also reports how many paths the walk could not read.
///
/// The count is not decoration: a subtree that cannot be read is skipped, so the listing comes
/// back shorter than reality while looking perfectly normal. Callers that show the result to a
/// person must surface it.
pub fn scan_matching(
    roots: Option<&[PathBuf]>,
    appid: Option<&str>,
    limit: Option<usize>,
) -> Result<(Vec<ProgramSummary>, usize)> {
    let outcome = discover::scan_programs_matching(roots, appid, limit)?;
    let programs = outcome.programs;
    // Belt and braces: the walk already prunes by appid, but an imperfect prune must cost time
    // rather than correctness, so the filter stays.
    let filter = appid.map(|a| a.trim().to_ascii_lowercase());
    let summaries = programs
        .into_iter()
        .filter(|p| filter.as_ref().is_none_or(|f| p.appid == *f))
        .map(|p| {
            let total_size = p.total_size();
            let packages = p.packages.len();
            let encrypted = p.is_encrypted();
            let location = p.location.to_string_lossy().into_owned();
            let updated_at = p
                .packages
                .iter()
                .filter_map(|package| {
                    let modified = package.path.metadata().ok()?.modified().ok()?;
                    let seconds = modified
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_secs();
                    i64::try_from(seconds).ok()
                })
                .max();
            ProgramSummary {
                appid: p.appid,
                version: p.version,
                location,
                packages,
                total_size,
                wxid: p.wxid,
                encrypted,
                updated_at,
            }
        })
        .collect();
    Ok((summaries, outcome.skipped_paths))
}

/// Collects every container reachable from `paths`.
///
/// Accepts individual `.wxapkg` files, ZIP archives containing them, and directories
/// that are walked recursively. Unparsable candidates are skipped rather than failing
/// the whole run so that a mixed directory still yields everything it can.
pub fn collect_packages(paths: &[PathBuf], wxid: Option<&str>) -> Result<Vec<Wxapkg>> {
    let mut out: Vec<Wxapkg> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut tempdirs: Vec<tempfile::TempDir> = Vec::new();

    for path in paths {
        if path.is_dir() {
            for candidate in walk_containers(path) {
                add_container(&candidate, wxid, &mut out, &mut seen);
            }
        } else if is_zip(path) {
            let dir = extract_zip(path, &mut tempdirs)?;
            for candidate in walk_containers(&dir) {
                add_container(&candidate, wxid, &mut out, &mut seen);
            }
        } else {
            add_container(path, wxid, &mut out, &mut seen);
        }
    }

    // Order the main package first so `resolve_parts` sees the most authoritative
    // `app-config.json` before the sub-package duplicates.
    out.sort_by(|a, b| {
        let a_main = a
            .path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("main"));
        let b_main = b
            .path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("main"));
        b_main.cmp(&a_main).then_with(|| a.path.cmp(&b.path))
    });
    Ok(out)
}

fn add_container(
    path: &Path,
    wxid: Option<&str>,
    out: &mut Vec<Wxapkg>,
    seen: &mut HashSet<PathBuf>,
) {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !seen.insert(key) {
        return;
    }
    match Wxapkg::open(path, wxid) {
        Ok(pkg) => out.push(pkg),
        Err(err) => {
            tracing::debug!(target: "wxapkg-dump::pipeline", path = %path.display(), error = %err, "skipping container");
        }
    }
}

fn walk_containers(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".wxapkg") {
            out.push(entry.path().to_path_buf());
        }
    }
    out.sort();
    out
}

fn is_zip(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

fn extract_zip(path: &Path, keep: &mut Vec<tempfile::TempDir>) -> Result<PathBuf> {
    let dir = tempfile::Builder::new()
        .prefix("wxapkg-dump-")
        .tempdir()
        .with_path(path)?;
    let file = std::fs::File::open(path).with_path(path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|err| Error::Archive {
        path: path.to_string_lossy().into_owned(),
        reason: err.to_string(),
    })?;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|err| Error::Archive {
            path: path.to_string_lossy().into_owned(),
            reason: err.to_string(),
        })?;
        let Some(enclosed) = entry.enclosed_name() else {
            continue;
        };
        let enclosed = enclosed.to_path_buf();
        let lower = enclosed.to_string_lossy().to_ascii_lowercase();
        if !(lower.ends_with(".wxapkg") || lower.ends_with(".json")) {
            continue;
        }
        let dest = dir.path().join(&enclosed);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).with_path(parent)?;
        }
        let mut sink = std::fs::File::create(&dest).with_path(&dest)?;
        std::io::copy(&mut entry, &mut sink).with_path(&dest)?;
    }
    let path = dir.path().to_path_buf();
    keep.push(dir);
    Ok(path)
}

/// Locates a `manifest.json` / `_manifest.json` describing package ownership.
pub fn find_manifest(paths: &[PathBuf]) -> Option<Value> {
    for path in paths {
        if path.is_dir() {
            for name in ["manifest.json", "_manifest.json"] {
                let direct = path.join(name);
                if direct.is_file() {
                    if let Ok(value) = read_json(&direct) {
                        return Some(value);
                    }
                }
            }
            for entry in WalkDir::new(path).follow_links(false).into_iter().flatten() {
                let name = entry.file_name().to_string_lossy();
                if entry.file_type().is_file()
                    && (name == "manifest.json" || name == "_manifest.json")
                {
                    if let Ok(value) = read_json(entry.path()) {
                        return Some(value);
                    }
                }
            }
        } else if is_zip(path) {
            if let Ok(file) = std::fs::File::open(path) {
                if let Ok(mut archive) = zip::ZipArchive::new(file) {
                    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
                    for name in names {
                        let base = name.rsplit('/').next().unwrap_or(&name);
                        if base != "manifest.json" && base != "_manifest.json" {
                            continue;
                        }
                        use std::io::Read;
                        let mut text = String::new();
                        if let Ok(mut entry) = archive.by_name(&name) {
                            if entry.read_to_string(&mut text).is_ok() {
                                if let Ok(value) = serde_json::from_str(&text) {
                                    return Some(value);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn read_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_path(path)?;
    Ok(serde_json::from_str(&text)?)
}

/// Builds a project from the request.
///
/// The project always lands in `<out>/<appid>/`, never directly in `<out>/`: one workspace
/// holds many programs, and a flat layout makes the second restore collide with the first.
/// When the appid cannot be read from the input, the container or directory name is used so
/// the shape stays the same.
pub fn unpack(request: &UnpackRequest) -> Result<BuildReport> {
    let (paths, named): (Vec<PathBuf>, Option<String>) = match &request.source {
        Source::Paths(paths) => (paths.clone(), None),
        Source::Directory(dir) => (vec![dir.clone()], None),
        Source::AppId {
            appid,
            version,
            roots,
        } => {
            let program = discover::require_program(appid, roots.as_deref(), version.as_deref())?;
            (vec![program.location], Some(program.appid))
        }
    };
    if paths.is_empty() {
        return Err(Error::InvalidArgument(
            "no input was provided: pass a path, an archive or an appid".to_owned(),
        ));
    }

    let mut packages = collect_packages(&paths, request.wxid.as_deref())?;
    let manifest = request.manifest.clone().or_else(|| find_manifest(&paths));

    if let Some(main) = &request.main {
        let hit = packages.iter().position(|p| {
            let name = p.path.file_name().map(|n| n.to_string_lossy().into_owned());
            name.as_deref() == Some(main.as_str())
                || p.path
                    .file_stem()
                    .is_some_and(|s| s.to_string_lossy() == main.as_str())
        });
        match hit {
            Some(index) => {
                let chosen = packages.remove(index);
                packages.insert(0, chosen);
            }
            None => {
                return Err(Error::InvalidArgument(format!(
                    "no container matched the requested main package {main:?}"
                )))
            }
        }
    }

    if packages.is_empty() {
        return Err(Error::InvalidArgument(
            "no readable wxapkg-dump container was found at the given input".to_owned(),
        ));
    }

    // The folder is named after the appid so repeated restores into one workspace never
    // collide; explicit inputs fall back to the appid in their path, then to their name.
    let folder = request
        .project_folder
        .as_deref()
        .map(crate::paths::sanitize_relative)
        .filter(|folder| !folder.is_empty())
        .or(named)
        .or_else(|| {
            let found: std::collections::BTreeSet<String> = packages
                .iter()
                .filter_map(|pkg| discover::appid_in_path(&pkg.path))
                .collect();
            (found.len() == 1)
                .then(|| found.into_iter().next())
                .flatten()
        })
        .or_else(|| {
            paths.first().and_then(|path| {
                let name = if path.is_dir() {
                    path.file_name()
                } else {
                    path.file_stem()
                };
                name.map(|name| crate::paths::sanitize_segment(&name.to_string_lossy()))
            })
        })
        .filter(|folder| !folder.is_empty());
    let out_dir = match &folder {
        Some(folder) => request.out_dir.join(folder),
        None => request.out_dir.clone(),
    };
    if request.clean && out_dir.is_dir() {
        std::fs::remove_dir_all(&out_dir).with_path(&out_dir)?;
    }

    let resolved = resolve_parts(packages, manifest.as_ref());
    let mut builder =
        ProjectBuilder::with_options(resolved.parts, out_dir, request.options.clone());
    builder.progress = request.progress.clone();
    for warning in resolved.warnings {
        builder.push_warning(warning);
    }
    builder.build()
}

/// Containers that belong to one Mini Program, grouped from explicit inputs.
#[derive(Debug, Clone)]
pub struct InputGroup {
    /// Folder the project lands in (`<out>/<key>/`): the appid, or a directory/file name.
    pub key: String,
    /// Container files (or archives) of this program, in input order.
    pub paths: Vec<PathBuf>,
    /// Appid when one was recognised; also the decryption key.
    pub appid: Option<String>,
}

/// Groups explicit inputs by the Mini Program they belong to.
///
/// The appid is taken from the containing directory first and the file name second, which
/// keeps a plugin container (`plugins_wx…`) with its host. Inputs without any appid are
/// grouped by their nearest directory (or file) name, so a folder of loose containers still
/// becomes one project instead of being mixed with unrelated programs.
pub fn group_inputs(paths: &[PathBuf]) -> Vec<InputGroup> {
    let mut groups: Vec<InputGroup> = Vec::new();
    let mut used: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for path in paths {
        let from_dir = path.is_dir();
        let files: Vec<PathBuf> = if from_dir {
            walk_containers(path)
        } else {
            vec![path.clone()]
        };
        for file in files {
            let by_dir = file
                .parent()
                .and_then(discover::appid_in_path)
                .map(|value| value.to_ascii_lowercase());
            let appid = by_dir
                .clone()
                .or_else(|| discover::appid_in_path(&file).map(|value| value.to_ascii_lowercase()));
            let base = appid.clone().unwrap_or_else(|| {
                let name = if from_dir {
                    file.parent().and_then(|parent| parent.file_name())
                } else {
                    file.file_stem()
                }
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_owned());
                crate::paths::sanitize_segment(&name)
            });
            // Two unrelated folders may share a name; keep the projects apart.
            let mut key = base.clone();
            let mut suffix = 2;
            while used.contains(&key)
                && !groups
                    .iter()
                    .any(|group| group.key == key && group.appid == appid)
            {
                key = format!("{base}-{suffix}");
                suffix += 1;
            }

            match groups.iter_mut().find(|group| group.key == key) {
                Some(group) => group.paths.push(file),
                None => {
                    used.insert(key.clone());
                    groups.push(InputGroup {
                        key,
                        paths: vec![file],
                        appid,
                    });
                }
            }
        }
    }
    groups
}

/// Restores explicit inputs, one project per Mini Program.
///
/// `unpack` merges everything it is given into a single project — correct for one
/// program's main + sub-packages, wrong for a folder that happens to hold several programs.
/// This wrapper groups first and keeps the `<out>/<appid>/` shape for each group; a single
/// group behaves exactly like [`unpack`].
pub fn unpack_grouped(request: &UnpackRequest) -> Result<Vec<BuildReport>> {
    let paths = match &request.source {
        Source::Paths(paths) => paths.clone(),
        Source::Directory(dir) => vec![dir.clone()],
        Source::AppId { .. } => return Ok(vec![unpack(request)?]),
    };
    let mut groups = group_inputs(&paths);
    // A key the caller supplied is the program's identity when the file name does not
    // carry one; without this an encrypted `foo.wxapkg` would restore as `unpack_foo`
    // even though its appid is known.
    if groups.len() == 1 && groups[0].appid.is_none() {
        if let Some(appid) = request
            .wxid
            .as_deref()
            .filter(|value| discover::is_appid(value))
        {
            let appid = appid.to_ascii_lowercase();
            groups[0].key = appid.clone();
            groups[0].appid = Some(appid);
        }
    }
    let single_file = matches!(&request.source, Source::Paths(items)
        if items.len() == 1 && !items[0].is_dir());
    if groups.is_empty() {
        return Ok(vec![unpack(request)?]);
    }
    let folder_for = |key: &str| {
        if request.unpack_prefix {
            format!("unpack_{key}")
        } else {
            key.to_owned()
        }
    };
    // One program selected as a file keeps the file-stem folder. A folder (or several files)
    // uses the group key, which is the appid when known and the folder name otherwise.
    if groups.len() == 1 && single_file {
        if !request.unpack_prefix {
            return Ok(vec![unpack(request)?]);
        }
        let key = folder_for(&groups[0].key);
        let mut sub = request.clone();
        sub.project_folder = Some(key);
        sub.manifest = None;
        return Ok(vec![unpack(&sub)?]);
    }

    let mut reports = Vec::with_capacity(groups.len());
    for group in groups {
        let mut sub = request.clone();
        sub.source = Source::Paths(group.paths);
        sub.project_folder = Some(folder_for(&group.key));
        // A manifest written for the whole selection describes other programs too.
        sub.manifest = None;
        if sub.wxid.is_none() {
            sub.wxid = group
                .appid
                .clone()
                .or_else(|| discover::is_appid(&group.key).then(|| group.key.clone()));
        }
        reports.push(unpack(&sub)?);
    }
    Ok(reports)
}

/// Extracts every file of every container verbatim, without decompiling.
pub fn raw_extract(
    paths: &[PathBuf],
    out_dir: &Path,
    wxid: Option<&str>,
    auto_inflate: bool,
) -> Result<RawExtractReport> {
    let mut packages = collect_packages(paths, wxid)?;
    if packages.is_empty() {
        return Err(Error::InvalidArgument(
            "no readable wxapkg-dump container was found at the given input".to_owned(),
        ));
    }

    // Entries may point at overlapping offset ranges, which makes decompilation
    // impossible, but raw extraction only needs a stable iteration order.
    packages.sort_by(|a, b| a.path.cmp(&b.path));

    std::fs::create_dir_all(out_dir).with_path(out_dir)?;
    let mut files = 0usize;
    let mut notes = Vec::new();

    for pkg in &mut packages {
        let stem = pkg
            .path
            .file_stem()
            .map(|s| crate::paths::sanitize_segment(&s.to_string_lossy()))
            .unwrap_or_else(|| "container".to_owned());
        // Group by appid when the path carries one, so a raw export has the same
        // `<out>/<appid>/…` shape as a restored project.
        let prefix = match discover::appid_in_path(&pkg.path) {
            Some(appid) => format!("{}/{stem}", crate::paths::sanitize_segment(&appid)),
            None => stem.clone(),
        };
        let entries = pkg.entries.clone();
        for entry in &entries {
            let name = entry.name.trim_start_matches('/').replace('\\', "/");
            if name.is_empty() || name.ends_with('/') {
                continue;
            }
            let Ok(blob) = pkg.read_entry(entry, auto_inflate, false) else {
                continue;
            };
            let dest = crate::paths::native(out_dir, &format!("{prefix}/{name}"));
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).with_path(parent)?;
            }
            std::fs::write(&dest, &blob).with_path(&dest)?;
            files += 1;
        }
        notes.extend(pkg.notes.iter().cloned());
    }

    Ok(RawExtractReport {
        out_dir: out_dir.to_string_lossy().into_owned(),
        files,
        notes,
    })
}

/// Structural summary of every container reachable from `paths`.
pub fn inspect(paths: &[PathBuf], wxid: Option<&str>) -> Result<Vec<ContainerInfo>> {
    let mut packages = collect_packages(paths, wxid)?;
    let entries: Vec<(String, u32, u32, u32, usize, Vec<String>)> = packages
        .iter()
        .map(|p| {
            (
                p.path.to_string_lossy().into_owned(),
                p.version,
                p.index_len,
                p.body_len,
                p.entries.len(),
                p.notes.clone(),
            )
        })
        .collect();

    let mut out = Vec::new();
    for (index, (path, version, index_len, body_len, files, notes)) in
        entries.into_iter().enumerate()
    {
        let app_config = packages[index].app_config();
        out.push(ContainerInfo {
            path,
            version,
            index_len,
            body_len,
            files,
            notes,
            pages: app_config
                .as_ref()
                .and_then(|c| c.get("pages"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
            sub_packages: app_config
                .as_ref()
                .and_then(|c| c.get("subPackages").or_else(|| c.get("subpackages")))
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
            plugins: app_config
                .as_ref()
                .and_then(|c| c.get("plugins"))
                .and_then(Value::as_object)
                .map_or(0, serde_json::Map::len),
        });
    }
    Ok(out)
}

/// Structural summary of one container.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContainerInfo {
    /// Container path.
    pub path: String,
    /// Format version.
    pub version: u32,
    /// Declared index length.
    #[serde(rename = "indexLen")]
    pub index_len: u32,
    /// Declared body length.
    #[serde(rename = "bodyLen")]
    pub body_len: u32,
    /// File count.
    pub files: usize,
    /// Parser notes.
    pub notes: Vec<String>,
    /// Declared page count.
    pub pages: usize,
    /// Declared sub-package count.
    #[serde(rename = "subPackages")]
    pub sub_packages: usize,
    /// Declared plugin count.
    pub plugins: usize,
}

/// Formats every supported file under `root` in place.
pub fn beautify_tree(root: &Path, options: &FormatOptions) -> Result<BeautifyReport> {
    if !root.is_dir() {
        return Err(Error::InvalidArgument(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let mut report = BeautifyReport {
        root: root.to_string_lossy().into_owned(),
        formatted: Vec::new(),
        skipped: 0,
        failed: Vec::new(),
    };

    for entry in WalkDir::new(root).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with("_raw/") || rel.starts_with("_foreign/") {
            report.skipped += 1;
            continue;
        }
        if !beautify::supports(&rel) {
            report.skipped += 1;
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            report.failed.push(rel);
            continue;
        };
        let formatted = beautify::beautify_by_extension(&rel, &text, options);
        if formatted == text {
            report.skipped += 1;
            continue;
        }
        match std::fs::write(path, formatted) {
            Ok(()) => report.formatted.push(rel),
            Err(_) => report.failed.push(rel),
        }
    }
    Ok(report)
}

/// Bundles rewritten modules back into a container-shaped directory tree.
///
/// The WeChat DevTools cannot import a `.wxapkg`; this only exists so the toolkit can
/// round-trip its own output (unpack → edit → repack) for distribution to a device that
/// accepts a plain package layout.
pub fn repack(source_dir: &Path, out_file: &Path) -> Result<usize> {
    if !source_dir.is_dir() {
        return Err(Error::InvalidArgument(format!(
            "{} is not a directory",
            source_dir.display()
        )));
    }

    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    for entry in WalkDir::new(source_dir)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(source_dir)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with('_') && rel.starts_with("_raw") {
            continue;
        }
        if rel == "_unpack_report.json" {
            continue;
        }
        entries.push((rel, entry.path().to_path_buf()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut index = Vec::new();
    let mut body = Vec::new();
    let header_size = 14usize;
    let mut index_size = 4usize;
    for (name, _) in &entries {
        index_size += 4 + name.len() + 8;
    }
    index.extend_from_slice(&(entries.len() as u32).to_be_bytes());

    for (name, path) in &entries {
        let data = std::fs::read(path).with_path(path)?;
        let offset = header_size + index_size + body.len();
        index.extend_from_slice(&(name.len() as u32).to_be_bytes());
        index.extend_from_slice(name.as_bytes());
        index.extend_from_slice(&(offset as u32).to_be_bytes());
        index.extend_from_slice(&(data.len() as u32).to_be_bytes());
        body.extend_from_slice(&data);
    }

    let mut out = Vec::with_capacity(header_size + index.len() + body.len());
    out.push(crate::container::MAGIC);
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&(index.len() as u32).to_be_bytes());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.push(crate::container::END_MARK);
    out.extend_from_slice(&index);
    out.extend_from_slice(&body);

    if let Some(parent) = out_file.parent() {
        std::fs::create_dir_all(parent).with_path(parent)?;
    }
    std::fs::write(out_file, &out).with_path(out_file)?;
    Ok(entries.len())
}

/// Validates a reconstructed project.
///
/// Front-ends with Node available can additionally run `node --check` per JavaScript file
/// and fold the results in with [`merge_syntax_failures`].
pub fn verify(
    root: &Path,
    options: &crate::verify::VerifyOptions,
) -> Result<crate::verify::VerifyReport> {
    crate::verify::verify_project(root, options)
}

/// Records external JavaScript syntax failures on top of a structural report.
pub fn merge_syntax_failures(
    report: &mut crate::verify::VerifyReport,
    failures: &[(String, String)],
) {
    for (path, detail) in failures {
        report.errors.push(crate::verify::Issue {
            code: "js_syntax".to_owned(),
            path: path.clone(),
            message: detail.clone(),
        });
    }
    report.syntax_checked = true;
    report.ok = report.errors.is_empty();
}

/// Convenience alias used by front-ends that only need the parts of a request.
pub fn parts_of(parts: &[Part]) -> Vec<(String, String, String)> {
    parts
        .iter()
        .map(|p| {
            (
                p.source.clone(),
                p.kind.as_str().to_owned(),
                if p.root.is_empty() {
                    "/".to_owned()
                } else {
                    p.root.clone()
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn repack_roundtrips_through_the_container_parser() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pages/index")).unwrap();
        std::fs::write(dir.path().join("app.js"), "App({});").unwrap();
        std::fs::write(dir.path().join("pages/index/index.js"), "Page({});").unwrap();

        let out = dir.path().join("out.wxapkg");
        let count = repack(dir.path(), &out).unwrap();
        assert_eq!(count, 2);

        let mut pkg = Wxapkg::open(&out, None).unwrap();
        assert_eq!(pkg.version, 0);
        assert_eq!(pkg.entries.len(), 2);
        let entry = pkg.find_entry("app.js").unwrap().clone();
        assert_eq!(pkg.read_entry(&entry, false, true).unwrap(), b"App({});");
    }

    #[test]
    fn inspect_reports_container_shape() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("a.wxapkg");
        std::fs::write(dir.path().join("x.js"), "x").unwrap();
        repack(dir.path(), &out).unwrap();
        let info = inspect(&[out], None).unwrap();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].files, 1);
    }

    /// Builds a container from explicit entries. A test cannot stage these on disk,
    /// because `plugin-private://…` is not a legal file name on Windows.
    fn container_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let header = 14usize;
        let mut index_size = 4usize;
        for (name, _) in entries {
            index_size += 4 + name.len() + 8;
        }

        let mut index = Vec::new();
        let mut body = Vec::new();
        index.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (name, data) in entries {
            let offset = header + index_size + body.len();
            index.extend_from_slice(&(name.len() as u32).to_be_bytes());
            index.extend_from_slice(name.as_bytes());
            index.extend_from_slice(&(offset as u32).to_be_bytes());
            index.extend_from_slice(&(data.len() as u32).to_be_bytes());
            body.extend_from_slice(data);
        }

        let mut out = vec![crate::container::MAGIC];
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(index.len() as u32).to_be_bytes());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.push(crate::container::END_MARK);
        out.extend_from_slice(&index);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn a_folder_with_two_appids_restores_two_projects() {
        let dir = tempfile::tempdir().unwrap();
        for appid in ["wxaaaaaaaaaaaaaaaa", "wxbbbbbbbbbbbbbbbb"] {
            let version = dir.path().join(appid).join("1");
            std::fs::create_dir_all(&version).unwrap();
            std::fs::write(
                version.join("__APP__.wxapkg"),
                container_bytes(&[
                    ("app-config.json", br#"{"pages":["pages/index/index"]}"#),
                    ("pages/index/index.js", b"Page({});"),
                ]),
            )
            .unwrap();
        }
        let out = dir.path().join("out");
        let reports = unpack_grouped(&UnpackRequest::new(
            Source::Paths(vec![dir.path().to_path_buf()]),
            out.clone(),
        ))
        .unwrap();
        let mut paths: Vec<String> = reports
            .iter()
            .map(|report| report.out_dir.clone())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                out.join("wxaaaaaaaaaaaaaaaa").display().to_string(),
                out.join("wxbbbbbbbbbbbbbbbb").display().to_string(),
            ]
        );
    }

    #[test]
    fn a_folder_of_loose_containers_uses_the_folder_name() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("packages");
        std::fs::create_dir_all(&folder).unwrap();
        for name in ["a.wxapkg", "b.wxapkg"] {
            std::fs::write(
                folder.join(name),
                container_bytes(&[("app-config.json", br#"{"pages":["pages/a/a"]}"#)]),
            )
            .unwrap();
        }
        let out = dir.path().join("out");
        let reports = unpack_grouped(&UnpackRequest::new(
            Source::Paths(vec![dir.path().to_path_buf()]),
            out.clone(),
        ))
        .unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].out_dir,
            out.join("packages").display().to_string()
        );
    }

    #[test]
    fn user_selected_inputs_use_the_unpack_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("wxdddddddddddddddd_app_t4.wxapkg");
        std::fs::write(
            &file,
            container_bytes(&[("app-config.json", br#"{"pages":["pages/a/a"]}"#)]),
        )
        .unwrap();
        let out = dir.path().join("out");
        let mut request = UnpackRequest::new(Source::Paths(vec![file]), out.clone());
        request.unpack_prefix = true;
        let reports = unpack_grouped(&request).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].out_dir,
            out.join("unpack_wxdddddddddddddddd").display().to_string()
        );
    }

    #[test]
    fn a_folder_with_two_appids_uses_prefixed_folders() {
        let dir = tempfile::tempdir().unwrap();
        for appid in ["wxeeeeeeeeeeeeeeee", "wxffffffffffffffff"] {
            let version = dir.path().join("in").join(appid).join("1");
            std::fs::create_dir_all(&version).unwrap();
            std::fs::write(
                version.join("__APP__.wxapkg"),
                container_bytes(&[("app-config.json", br#"{"pages":["pages/a/a"]}"#)]),
            )
            .unwrap();
        }
        let out = dir.path().join("out");
        let mut request =
            UnpackRequest::new(Source::Paths(vec![dir.path().join("in")]), out.clone());
        request.unpack_prefix = true;
        let reports = unpack_grouped(&request).unwrap();
        let mut paths: Vec<String> = reports
            .iter()
            .map(|report| report.out_dir.clone())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                out.join("unpack_wxeeeeeeeeeeeeeeee").display().to_string(),
                out.join("unpack_wxffffffffffffffff").display().to_string(),
            ]
        );
    }

    #[test]
    fn a_supplied_key_names_an_unnamed_container() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plugin.wxapkg");
        std::fs::write(
            &file,
            container_bytes(&[("app-config.json", br#"{"pages":["pages/a/a"]}"#)]),
        )
        .unwrap();
        let out = dir.path().join("out");
        let mut request = UnpackRequest::new(Source::Paths(vec![file]), out.clone());
        request.wxid = Some("wx1212121212121212".to_owned());
        request.unpack_prefix = true;
        let reports = unpack_grouped(&request).unwrap();
        assert_eq!(
            reports[0].out_dir,
            out.join("unpack_wx1212121212121212").display().to_string()
        );
    }

    #[test]
    fn inputs_group_per_appid() {
        let dir = tempfile::tempdir().unwrap();
        for appid in ["wxaaaaaaaaaaaaaaaa", "wxbbbbbbbbbbbbbbbb"] {
            let version = dir.path().join(appid).join("1");
            std::fs::create_dir_all(&version).unwrap();
            std::fs::write(version.join("__APP__.wxapkg"), b"x").unwrap();
        }
        let groups = group_inputs(&[dir.path().to_path_buf()]);
        let keys: Vec<&str> = groups.iter().map(|group| group.key.as_str()).collect();
        assert_eq!(keys, ["wxaaaaaaaaaaaaaaaa", "wxbbbbbbbbbbbbbbbb"]);
        assert_eq!(groups[0].appid.as_deref(), Some("wxaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn loose_containers_group_by_folder_name() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("packages");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("a.wxapkg"), b"x").unwrap();
        std::fs::write(folder.join("b.wxapkg"), b"x").unwrap();
        let groups = group_inputs(&[folder]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].key, "packages");
        assert_eq!(groups[0].paths.len(), 2);
        assert!(groups[0].appid.is_none());
    }

    #[test]
    fn an_appid_in_the_file_name_names_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("wxcccccccccccccccc_app_t4.wxapkg");
        std::fs::write(&file, b"x").unwrap();
        let groups = group_inputs(&[file]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].key, "wxcccccccccccccccc");
    }

    // Regression: a container may carry virtual-scheme entries such as
    // `plugin-private://wx…/components/playcanvas/index.js`. Writing them verbatim fails
    // on Windows with ERROR_INVALID_NAME (the `:` is a reserved character) and leaves an
    // unusable directory everywhere else.
    #[test]
    fn virtual_scheme_entries_are_restored_under_a_legal_path() {
        let dir = tempfile::tempdir().unwrap();
        // Local caches keep the appid in the path, which is what names the project folder.
        let appid = "wx513725758120fb69";
        let version = dir.path().join(appid).join("1");
        std::fs::create_dir_all(&version).unwrap();
        let file = version.join("__APP__.wxapkg");
        std::fs::write(
            &file,
            container_bytes(&[
                ("app-config.json", br#"{"pages":["pages/index/index"]}"#),
                ("pages/index/index.js", b"Page({});"),
                (
                    "plugin-private://wx513725758120fb69/components/playcanvas/index.js",
                    b"Component({});",
                ),
            ]),
        )
        .unwrap();

        let out = dir.path().join("out");
        let report = unpack(&UnpackRequest::new(Source::Paths(vec![file]), out.clone())).unwrap();

        assert_eq!(report.out_dir, out.join(appid).display().to_string());
        let restored = out
            .join(appid)
            .join("plugin-private_/wx513725758120fb69/components/playcanvas/index.js");
        assert!(restored.is_file(), "missing {}", restored.display());
        assert!(!out.join(appid).join("plugin-private:").exists());
    }

    // A container copied somewhere without an appid still gets its own folder, named after
    // the file, so a workspace never mixes two restores in one directory.
    #[test]
    fn a_nameless_input_falls_back_to_its_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.wxapkg");
        std::fs::write(
            &file,
            container_bytes(&[("app-config.json", br#"{"pages":["pages/a/a"]}"#)]),
        )
        .unwrap();

        let out = dir.path().join("out");
        let report = unpack(&UnpackRequest::new(Source::Paths(vec![file]), out.clone())).unwrap();

        assert_eq!(report.out_dir, out.join("main").display().to_string());
        assert!(out.join("main/app.json").is_file());
    }
}

// ---------------------------------------------------------------------------
// folder decryption
// ---------------------------------------------------------------------------

/** callback reporting one file's turn */ /**/
pub(crate) type FolderProgress = dyn Fn(usize, usize, &str);

/// What happened to one file during a folder decryption.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DecryptOutcome {
    /// Source file.
    pub file: String,
    /// Whether a usable container landed in the output directory.
    pub ok: bool,
    /// "plain" when the file was never encrypted, the key that worked, or the failure.
    pub note: String,
}

/// What a folder decryption produced.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DecryptFolderReport {
    /// Where the usable set was written.
    pub out_dir: String,
    /// Per-file outcomes, in walk order.
    pub outcomes: Vec<DecryptOutcome>,
    /// Containers that needed a key and got one.
    pub decrypted: usize,
    /// Containers that were already plaintext and were copied through.
    pub plain: usize,
    /// Files no key could open.
    pub failed: usize,
}

/// Decrypts every `V1MMWX` container under `folder`, trying `keys` in turn.
///
/// Plaintext containers are copied through, so the output directory is a complete, usable
/// set. Candidate keys are the caller's list plus a guess derived from each path — a mixed
/// folder of several Mini Programs decrypts in one pass.
pub fn decrypt_folder(
    folder: &std::path::Path,
    keys: &[String],
    out_dir: &std::path::Path,
    progress: Option<&FolderProgress>,
) -> Result<DecryptFolderReport> {
    if !folder.is_dir() {
        return Err(Error::InvalidArgument(format!(
            "{} is not a directory",
            folder.display()
        )));
    }
    std::fs::create_dir_all(out_dir)
        .map_err(|err| Error::io(format!("creating {}", out_dir.display()), err))?;

    let keys: Vec<String> = keys
        .iter()
        .map(|key| key.trim().to_lowercase())
        .filter(|key| key.len() == 18 && key.starts_with("wx"))
        .collect();

    let mut files = Vec::new();
    collect_wxapkg_files(folder, &mut files);
    files.sort();
    let total = files.len();
    let mut report = DecryptFolderReport {
        out_dir: out_dir.display().to_string(),
        ..DecryptFolderReport::default()
    };

    for (index, file) in files.iter().enumerate() {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(report_progress) = progress {
            report_progress(index, total, &name);
        }
        let note = match std::fs::read(file) {
            Err(err) => {
                report.failed += 1;
                format!("read failed: {err}")
            }
            Ok(data) if !crate::crypto::is_encrypted(&data) => {
                let target = out_dir.join(&name);
                std::fs::write(&target, &data)
                    .map_err(|err| Error::io(format!("writing {}", target.display()), err))?;
                report.plain += 1;
                "plain".to_owned()
            }
            Ok(data) => {
                let mut attempts = keys.clone();
                if let Some(guess) = crate::crypto::find_wxid(&file.display().to_string()) {
                    if !attempts.contains(&guess) {
                        attempts.push(guess);
                    }
                }
                let mut opened = None;
                let mut last = String::new();
                for key in &attempts {
                    match crate::crypto::decrypt(key, &data, &file.display().to_string()) {
                        Ok(bytes) if bytes.first() == Some(&crate::container::MAGIC) => {
                            opened = Some((key.clone(), bytes));
                            break;
                        }
                        Ok(_) => last = format!("key {key} produced garbage"),
                        Err(err) => last = err.to_string(),
                    }
                }
                match opened {
                    Some((key, bytes)) => {
                        let target = out_dir.join(&name);
                        std::fs::write(&target, &bytes).map_err(|err| {
                            Error::io(format!("writing {}", target.display()), err)
                        })?;
                        report.decrypted += 1;
                        key
                    }
                    None => {
                        report.failed += 1;
                        if attempts.is_empty() {
                            "no key given and none could be derived from the path".to_owned()
                        } else {
                            last
                        }
                    }
                }
            }
        };
        report.outcomes.push(DecryptOutcome {
            file: file.display().to_string(),
            ok: !note.starts_with("read failed")
                && !note.starts_with("no key")
                && !note.starts_with("key ")
                && !note.contains("wrong"),
            note,
        });
    }
    if let Some(report_progress) = progress {
        report_progress(total, total, "done");
    }
    Ok(report)
}

#[cfg(test)]
mod decrypt_folder_tests {
    use super::*;

    /// Inverse of `crypto::decrypt`, enough to build a test fixture.
    fn seal_for_test(wxid: &str, plain: &[u8]) -> Vec<u8> {
        let key = crate::crypto::derive_key(wxid);
        let head_len = 1023;
        let mut head = vec![0u8; 1024];
        head[..head_len.min(plain.len())].copy_from_slice(&plain[..head_len.min(plain.len())]);
        head[1023] = 1; // PKCS#7 one-byte padding, as the official encryptor writes
        let mut sealed_head = vec![0u8; head.len()];
        let cipher = <aes::Aes256 as aes::cipher::KeyInit>::new_from_slice(&key).unwrap();
        let mut prev = *crate::crypto::AES_IV_FOR_TEST;
        for (index, chunk) in head.chunks_exact(16).enumerate() {
            let mut xored: [u8; 16] = [0u8; 16];
            for (i, b) in chunk.iter().enumerate() {
                xored[i] = b ^ prev[i];
            }
            let mut block = aes::cipher::generic_array::GenericArray::from(xored);
            use aes::cipher::BlockEncrypt as _;
            cipher.encrypt_block(&mut block);
            prev = block.into();
            sealed_head[index * 16..index * 16 + 16].copy_from_slice(&prev);
        }
        let xor_key = wxid.as_bytes()[wxid.len() - 2];
        let mut out = Vec::new();
        out.extend_from_slice(crate::crypto::V1MMWX_MAGIC);
        out.extend_from_slice(&sealed_head[..1024]);
        out.extend(
            plain[head_len.min(plain.len())..]
                .iter()
                .map(|b| b ^ xor_key),
        );
        out
    }

    #[test]
    fn a_folder_round_trips_through_decryption_with_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let wxid = "wx0011223344556677";
        let mut plain = vec![0xBEu8];
        plain.extend_from_slice(&(plain.len() as u32 + 4).to_be_bytes()); // a fake header
        plain.extend(std::iter::repeat_n(0x41, 4096));
        std::fs::write(dir.path().join("a.wxapkg"), seal_for_test(wxid, &plain)).unwrap();
        // A decoy key sits first in the list; the real one must still win by rotation.
        std::fs::write(dir.path().join("b.wxapkg"), &plain).unwrap();

        let out = dir.path().join("decrypted");
        let report = decrypt_folder(
            dir.path(),
            &["wx9999888877776666".to_owned(), wxid.to_owned()],
            &out,
            None,
        )
        .unwrap();
        assert_eq!(report.decrypted, 1);
        assert_eq!(report.plain, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(
            report
                .outcomes
                .iter()
                .find(|o| o.note == wxid)
                .unwrap()
                .note,
            wxid
        );
        assert!(report.outcomes.iter().any(|o| o.note == "plain"));
        let opened = std::fs::read(out.join("a.wxapkg")).unwrap();
        assert_eq!(opened, plain);
    }
}

/// Recursively gathers container files, skipping the toolkit's own output directories.
fn collect_wxapkg_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            if !name.starts_with('_') && name != "decrypted" {
                collect_wxapkg_files(&path, out);
            }
        } else if name.ends_with(".wxapkg") || name.ends_with(".wxapkg2") {
            out.push(path);
        }
    }
}
