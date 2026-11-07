// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Assembly of one or more containers into a Mini Program project that the WeChat
//! DevTools can open directly.
//!
//! The rules are derived from WeChat's own `app-config.json` and the naming
//! conventions of its local package cache:
//!
//! * **Main package** — carries `app-config.json` and the highest page count. The copy
//!   inside a sub-package is a duplicate of the same file.
//! * **Sub-package** — declared under `subPackages[].root`, or every entry of the
//!   container living under one declared root. PC/macOS cache names sub-packages
//!   `_root_with_underscores_.wxapkg`.
//! * **Plugin** — `plugins_wx<appid>.wxapkg`; its root is recorded as
//!   `plugins/wx<appid>`.
//! * **Foreign** — a container whose `entryPagePath` matches neither its assigned root
//!   nor any declared root, and which is larger than the host. Export tools routinely
//!   scoop up other Mini Programs from `pkg/general/`; those are quarantined instead of
//!   being merged silently.
//!
//! The generated tree:
//!
//! ```text
//! <out>/app.json app.js app.wxss project.config.json sitemap.json
//! <out>/pages/... component/... <subpackage root>/...
//! <out>/_raw/       original compiled bundles (excluded from packaging)
//! <out>/_foreign/   quarantined containers
//! <out>/_unpack_report.json
//! ```

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use walkdir::WalkDir;

use crate::beautify::{self, FormatOptions};
use crate::container::Wxapkg;
use crate::decompile::{bundle, wxml, wxss};
use crate::error::{IoContext, Result};
use crate::paths;
use crate::report::{BuildReport, Counts, PartKind, PartReport};

/// Bundle file names that always indicate compiled output.
const JS_BUNDLE_HINTS: &[&str] = &["app-service.js", "appservice.app.js", "appservice.js"];

/// Markers that identify a JavaScript file as a `wcc` compilation product.
const COMPILED_MARKERS: &[&str] = &[
    "$gwx",
    "__wxAppCode__",
    "setCssToHead",
    "gz$gwx",
    "__wxCodeSpace__",
];

/// How plugin declarations in `app.json` are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginsMode {
    /// Keep the declarations verbatim (most faithful).
    Keep,
    /// Keep the key but clear the map.
    Placeholder,
    /// Remove the declarations entirely (easiest to compile).
    Strip,
}

/// One container together with its resolved role.
#[derive(Debug)]
pub struct Part {
    /// Parsed container.
    pub pkg: Wxapkg,
    /// Destination root inside the project (`""` for the main package).
    pub root: String,
    /// Resolved role.
    pub kind: PartKind,
    /// Parsed `app-config.json`, when the container carries one.
    pub app_config: Option<Value>,
    /// Original file name.
    pub source: String,
}

impl Part {
    /// `true` when the container is a plugin.
    pub fn is_plugin(&self) -> bool {
        self.kind == PartKind::Plugin
    }
}

/// Result of classifying a set of containers.
#[derive(Debug)]
pub struct ResolvedParts {
    /// Classified containers, main package first.
    pub parts: Vec<Part>,
    /// Non-fatal classification problems.
    pub warnings: Vec<String>,
}

/// Builder configuration.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Copy original compiled bundles into `_raw/`.
    pub keep_raw: bool,
    /// Reconstruct WXML templates.
    pub decompile_wxml: bool,
    /// Normalise `require()` paths so the project resolves inside DevTools.
    pub rewrite_requires: bool,
    /// Drop pages / sub-packages without sources and repair the references to them.
    pub prune: bool,
    /// Synthesise `<view />` shells for pages whose WXML was never shipped.
    pub synthesize: bool,
    /// Remove plugin declarations and `plugin://` component references without sources.
    pub plugin_clean: bool,
    /// Plugin declaration handling.
    pub plugins_mode: PluginsMode,
    /// Rewrite `plugin://` references to a local `plugins/` directory.
    pub inline_plugins: bool,
    /// Suffix appended to every style selector.
    pub wxss_suffix: String,
    /// Beautify every reconstructed file.
    pub format: Option<FormatOptions>,
    /// Cap on warnings recorded in the report.
    pub max_warnings: usize,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            keep_raw: true,
            decompile_wxml: true,
            rewrite_requires: true,
            prune: true,
            synthesize: true,
            plugin_clean: true,
            plugins_mode: PluginsMode::Keep,
            inline_plugins: false,
            wxss_suffix: String::new(),
            format: None,
            max_warnings: 2000,
        }
    }
}

static RE_MAIN_NAME: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^main(-v?\d+)?$").expect("valid pattern"));

static RE_PLUGIN_NAME: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^plugins_wx([0-9a-fA-F]+)$").expect("valid pattern"));

static RE_ROOT_PLUGIN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^plugins/wx([0-9a-fA-F]+)$").expect("valid pattern"));

static RE_MAIN_STEM: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^main(?:-v(\d+))?$").expect("valid pattern"));

static RE_PAGES_PREFIX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^pages_(.+)$").expect("valid pattern"));

static RE_UNDERSCORE_ROOT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^_(.+)_$").expect("valid pattern"));

static RE_WXID: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"wx[0-9a-fA-F]{16}").expect("valid pattern"));

static RE_HTML_SUFFIX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\.html$").expect("valid pattern"));

/// Whether any page config selects the Skyline renderer.
fn uses_skyline(cfg: &Value) -> bool {
    cfg.get("page")
        .and_then(Value::as_object)
        .is_some_and(|pages| {
            pages.values().any(|page| {
                page.get("renderer").and_then(Value::as_str) == Some("skyline")
                    || page
                        .get("window")
                        .and_then(|w| w.get("renderer"))
                        .and_then(Value::as_str)
                        == Some("skyline")
            })
        })
}

static RE_JS_OBJECT_KEY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"([{,]\s*)([A-Za-z_$][\w$]*)\s*:").expect("valid pattern"));

static RE_JSON_TRAILING_COMMA: Lazy<Regex> =
    Lazy::new(|| Regex::new(r",\s*([}\]])").expect("valid pattern"));

/// Matches a redundant comma run left behind by the trailing-comma rewrite.
static RE_JSON_EMPTY_COMMA: Lazy<Regex> =
    Lazy::new(|| Regex::new(r",\s*,").expect("valid pattern"));

static RE_WARN_DEDUP: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(\S+?\.\w+): (the operand array is empty|template references operand indices)")
        .expect("valid pattern")
});

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Reads and parses the `app-config.json` inside `pkg`, when present.
pub fn read_app_config(pkg: &mut Wxapkg) -> Option<Value> {
    let entry = pkg
        .entries
        .iter()
        .find(|e| e.name.ends_with("app-config.json"))
        .cloned()?;
    let blob = pkg.read_entry(&entry, false, false).ok()?;
    serde_json::from_slice(&blob).ok()
}

fn filename_root_guess(stem: &str) -> Option<String> {
    if RE_MAIN_NAME.is_match(stem) {
        return Some(String::new());
    }
    if let Some(caps) = RE_PLUGIN_NAME.captures(stem) {
        return Some(format!("plugins/wx{}", &caps[1]));
    }
    None
}

fn assign(part: &mut Part, root: &str) {
    part.root = root.to_owned();
    part.kind = if root.starts_with("plugins/") {
        PartKind::Plugin
    } else if root.is_empty() {
        PartKind::Main
    } else {
        PartKind::Subpackage
    };
}

/// Classifies containers using a manifest when available, naming conventions next, and
/// content-based fallbacks last.
pub fn resolve_parts(pkgs: Vec<Wxapkg>, manifest: Option<&Value>) -> ResolvedParts {
    let mut warnings = Vec::new();
    let mut parts: Vec<Part> = Vec::with_capacity(pkgs.len());
    for mut pkg in pkgs {
        let app_config = read_app_config(&mut pkg);
        let source = pkg
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        parts.push(Part {
            pkg,
            root: String::new(),
            kind: PartKind::Unknown,
            app_config,
            source,
        });
    }

    if let Some(manifest) = manifest {
        match_manifest(&mut parts, manifest);
    }

    // Naming conventions for anything the manifest did not cover.
    for part in &mut parts {
        if part.kind != PartKind::Unknown {
            continue;
        }
        let stem = Path::new(&part.source)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(guess) = filename_root_guess(&stem) {
            assign(part, &guess);
        }
    }

    // Main package fallback: the one with the most declared pages.
    if !parts.iter().any(|p| p.kind == PartKind::Main) {
        let mut best = -1i64;
        let mut index: Option<usize> = None;
        for (i, part) in parts.iter().enumerate() {
            let Some(cfg) = &part.app_config else {
                continue;
            };
            let pages = cfg
                .get("pages")
                .and_then(Value::as_array)
                .map_or(0, Vec::len) as i64;
            if pages > best {
                best = pages;
                index = Some(i);
            }
        }
        let index = index.unwrap_or(0);
        if parts[index].app_config.is_none() {
            warnings.push(format!(
                "no container carries an app-config.json; {} is treated as the main package",
                parts[index].source
            ));
        }
        let source = parts[index].source.clone();
        let mut part = parts.remove(index);
        assign(&mut part, "");
        part.source = source;
        parts.insert(0, part);
    }

    let main_index = parts
        .iter()
        .position(|p| p.kind == PartKind::Main)
        .unwrap_or(0);

    let sub_roots: Vec<String> = parts[main_index]
        .app_config
        .as_ref()
        .and_then(|cfg| {
            cfg.get("subPackages")
                .or_else(|| cfg.get("subpackages"))
                .and_then(Value::as_array)
                .cloned()
        })
        .unwrap_or_default()
        .iter()
        .filter_map(|sp| sp.get("root").and_then(Value::as_str))
        .map(|r| r.trim().trim_end_matches('/').to_owned() + "/")
        .filter(|r| r != "/")
        .collect();

    // Remaining containers: match by entry prefix, then by naming convention.
    for (i, part) in parts.iter_mut().enumerate() {
        if i == main_index || matches!(part.kind, PartKind::Subpackage | PartKind::Plugin) {
            continue;
        }
        let names: Vec<String> = part
            .pkg
            .entries
            .iter()
            .map(|e| e.name.trim_start_matches('/').to_owned())
            .collect();
        let mut matched: Option<String> = None;

        for root in &sub_roots {
            let rr = root.trim_end_matches('/');
            if names.is_empty() {
                continue;
            }
            let scoped = &names[..names.len().min(400)];
            let hits = scoped
                .iter()
                .filter(|n| n.as_str() == rr || n.starts_with(&format!("{rr}/")))
                .count();
            // A small share of main-package assets may leak into a sub-package container
            // (skeleton screen artefacts), so a 90% prefix hit rate is the criterion.
            if hits > 0 && hits * 10 >= scoped.len() * 9 {
                matched = Some(root.clone());
                break;
            }
        }

        let stem = Path::new(&part.source)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if matched.is_none() {
            matched = sub_roots
                .iter()
                .find(|r| stem == r.trim_end_matches('/'))
                .cloned();
        }
        if matched.is_none() {
            if let Some(caps) = RE_PAGES_PREFIX.captures(&stem) {
                let candidate = format!("pages/{}/", caps[1].replace('_', "/"));
                matched = sub_roots
                    .iter()
                    .find(|r| r.trim_end_matches('/') == candidate.trim_end_matches('/'))
                    .cloned();
            }
        }
        if matched.is_none() {
            if let Some(caps) = RE_UNDERSCORE_ROOT.captures(&stem) {
                let candidate = format!("{}/", caps[1].replace('_', "/"));
                matched = sub_roots
                    .iter()
                    .find(|r| r.trim_end_matches('/') == candidate.trim_end_matches('/'))
                    .cloned();
            }
        }

        match matched {
            Some(root) => assign(part, &root),
            None => {
                part.root = String::new();
                part.kind = PartKind::Unknown;
                warnings.push(format!(
                    "{}: could not determine ownership; files are written at the container root",
                    part.source
                ));
            }
        }
    }

    // Foreign-container detection.
    let known_roots: Vec<String> = sub_roots
        .iter()
        .cloned()
        .chain(
            parts
                .iter()
                .filter(|p| p.is_plugin() && !p.root.is_empty())
                .map(|p| p.root.clone()),
        )
        .collect();
    let main_pages = parts[main_index]
        .app_config
        .as_ref()
        .and_then(|c| c.get("pages"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    for (i, part) in parts.iter_mut().enumerate() {
        if i == main_index {
            continue;
        }
        if looks_foreign(part, main_pages, &known_roots) {
            part.kind = PartKind::Foreign;
            warnings.push(format!(
                "{}: its app-config entryPagePath does not match the assigned root; the container \
                 looks like a different Mini Program and was quarantined under _foreign/",
                part.source
            ));
        }
    }

    let mut main = parts.remove(main_index);
    main.kind = PartKind::Main;
    main.root = String::new();
    let main_source = main.source.clone();
    parts.sort_by_key(|p| (p.kind != PartKind::Main, p.source.clone()));
    parts.insert(0, main);
    debug_assert_eq!(parts[0].source, main_source);

    ResolvedParts { parts, warnings }
}

fn looks_foreign(part: &Part, main_pages: usize, known_roots: &[String]) -> bool {
    let Some(cfg) = &part.app_config else {
        return false;
    };
    if part.kind == PartKind::Main {
        return false;
    }
    let pages = cfg
        .get("pages")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);

    if part.root.starts_with("plugins/") {
        // A plugin may legitimately carry an app-config; only "larger than the host"
        // is suspicious.
        return pages > main_pages;
    }

    let entry = RE_HTML_SUFFIX
        .replace(
            cfg.get("entryPagePath")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "",
        )
        .into_owned();
    if entry.is_empty() {
        return false;
    }
    if !part.root.is_empty() && entry.starts_with(&format!("{}/", part.root.trim_end_matches('/')))
    {
        return false;
    }
    if known_roots
        .iter()
        .filter(|r| !r.is_empty())
        .any(|r| entry.starts_with(&format!("{}/", r.trim_end_matches('/'))))
    {
        return false;
    }
    pages > main_pages
}

fn match_manifest(parts: &mut [Part], manifest: &Value) {
    let Some(items) = manifest.get("parts").and_then(Value::as_array) else {
        return;
    };
    let items: Vec<&Value> = items.iter().collect();
    if items.is_empty() {
        return;
    }
    let mut taken: HashSet<String> = HashSet::new();

    // 1) Plugins: `plugins_wx<id>` <-> `plugins/wx<id>`.
    for item in &items {
        let root = item.get("root").and_then(Value::as_str).unwrap_or_default();
        let Some(caps) = RE_ROOT_PLUGIN.captures(root) else {
            continue;
        };
        let want = format!("plugins_wx{}.wxapkg", &caps[1]);
        for part in parts.iter_mut() {
            if part.source == want && !taken.contains(&part.source) {
                assign(part, root);
                taken.insert(part.source.clone());
                break;
            }
        }
    }

    // 2) File name equal to the last root segment.
    for item in &items {
        let root = item.get("root").and_then(Value::as_str).unwrap_or_default();
        if root.is_empty() || root.starts_with("plugins/") {
            continue;
        }
        let leaf = root
            .trim_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        for part in parts.iter_mut() {
            if taken.contains(&part.source) {
                continue;
            }
            let stem = Path::new(&part.source)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if stem == leaf {
                assign(part, root);
                taken.insert(part.source.clone());
                break;
            }
        }
    }

    // 3) Main package: `main-v<version>` <-> root "".
    let main_versions: Vec<i64> = items
        .iter()
        .filter(|i| {
            i.get("root")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .is_empty()
        })
        .map(|i| i.get("version").and_then(Value::as_i64).unwrap_or(-1))
        .collect();
    if main_versions.is_empty() {
        return;
    }
    for part in parts.iter_mut() {
        if taken.contains(&part.source) {
            continue;
        }
        let stem = Path::new(&part.source)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(caps) = RE_MAIN_STEM.captures(&stem) else {
            continue;
        };
        let version = caps.get(1).and_then(|m| m.as_str().parse::<i64>().ok());
        let fits = match version {
            None => true,
            Some(v) => main_versions.contains(&v) || main_versions.contains(&-1),
        };
        if fits {
            assign(part, "");
            taken.insert(part.source.clone());
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builds a DevTools-openable project from classified containers.
pub struct ProjectBuilder {
    parts: Vec<Part>,
    out: PathBuf,
    options: BuildOptions,
    report: BuildReport,
    counts: Counts,
    written: BTreeSet<String>,
    known_modules: HashSet<String>,
    all_js: BTreeMap<String, String>,
    json_units: BTreeMap<String, String>,
    wxml_units: BTreeMap<String, (String, i32)>,
    wxss_units: BTreeMap<String, String>,
    ops_pool: wxml::OperandPool,
    page_windows: BTreeMap<String, Value>,
    app: Option<Value>,
    warn_seen: HashSet<String>,
    /// Called as each container is reconstructed: `(index, total, container name)`.
    #[allow(clippy::type_complexity)]
    pub progress: Option<std::sync::Arc<dyn Fn(usize, usize, &str) + Send + Sync>>,
}

impl ProjectBuilder {
    /// Creates a builder that writes into `out_dir`.
    pub fn new(parts: Vec<Part>, out_dir: impl Into<PathBuf>) -> Self {
        Self::with_options(parts, out_dir, BuildOptions::default())
    }

    /// Creates a builder with explicit options.
    pub fn with_options(
        parts: Vec<Part>,
        out_dir: impl Into<PathBuf>,
        options: BuildOptions,
    ) -> Self {
        let out_dir = out_dir.into();
        let out_str = out_dir.to_string_lossy().into_owned();
        Self {
            parts,
            out: out_dir,
            options,
            report: BuildReport::new(out_str),
            counts: Counts::default(),
            written: BTreeSet::new(),
            known_modules: HashSet::new(),
            all_js: BTreeMap::new(),
            json_units: BTreeMap::new(),
            wxml_units: BTreeMap::new(),
            wxss_units: BTreeMap::new(),
            ops_pool: wxml::OperandPool::new(),
            page_windows: BTreeMap::new(),
            app: None,
            warn_seen: HashSet::new(),
            progress: None,
        }
    }

    /// Adds a preprocessing warning collected during classification.
    pub fn push_warning(&mut self, warning: impl Into<String>) {
        self.report.warnings.push(warning.into());
    }

    /// Recomputes the counters from the set of files actually written.
    ///
    /// The incremental counters are incremented where a file is *reconstructed*, which is not
    /// the same as where it is *written*: a template replaced by a better-quality variant is
    /// counted twice, and a file written without going through the reconstruction path is not
    /// counted at all. That made `counts`, `files` and the directory on disk three different
    /// numbers. Deriving the totals from `written` makes them agree by construction.
    /// Whether the container is a Mini Game rather than a Mini Program.
    ///
    /// A Mini Game ships `game.js` (or `game.json`, or a `cocos/` engine directory) instead of
    /// `app.js`, and it has no pages at all. Treating one as a Mini Program is worse than a
    /// missing feature: it invents an `app.js` and a single-page `app.json`, so a correct
    /// extraction looks like a broken one. The entry itself stays a normal main package — only
    /// the finishing touches differ.
    fn is_mini_game(&self) -> bool {
        looks_like_mini_game(self.written.iter().map(String::as_str))
    }

    fn recount(&mut self) {
        let mut counts = Counts {
            // Reformatting is the one thing not derivable from a path.
            beautified: self.counts.beautified,
            ..Counts::default()
        };
        for path in &self.written {
            if path.starts_with("_raw/") {
                counts.raw += 1;
            } else if path.starts_with("_foreign/") {
                counts.foreign += 1;
            } else if path.ends_with(".js") || path.ends_with(".wxs") {
                counts.js += 1;
            } else if path.ends_with(".wxml") {
                counts.wxml += 1;
            } else if path.ends_with(".wxss") {
                counts.wxss += 1;
            } else if path.ends_with(".json") {
                counts.json += 1;
            } else if path.ends_with(".html") {
                counts.h5 += 1;
            } else {
                counts.assets += 1;
            }
        }
        self.counts = counts;
    }

    /// Runs the full build and returns the report.
    pub fn build(mut self) -> Result<BuildReport> {
        std::fs::create_dir_all(&self.out).with_path(&self.out)?;

        // Page-level window overrides and plugin metadata come from the main package.
        let main_cfg = self
            .parts
            .first()
            .and_then(|p| p.app_config.clone())
            .unwrap_or(Value::Null);
        if let Some(page_map) = main_cfg.get("page").and_then(Value::as_object) {
            for (key, value) in page_map {
                let page = RE_HTML_SUFFIX.replace(key, "").into_owned();
                if value.is_object() {
                    self.page_windows.insert(page, value.clone());
                }
            }
        }

        let total = self.parts.len();
        for index in 0..self.parts.len() {
            let source = self.parts[index].source.clone();
            if let Some(progress) = &self.progress {
                progress(index + 1, total, &source);
            }
            if self.parts[index].kind == PartKind::Foreign {
                self.dump_foreign(index)?;
                self.report.parts.push(PartReport {
                    file: source,
                    kind: PartKind::Foreign,
                    root: "(quarantined)".to_owned(),
                    version: self.parts[index].pkg.version,
                    files: self.parts[index].pkg.entries.len(),
                });
                continue;
            }
            self.report.parts.push(PartReport {
                file: source,
                kind: self.parts[index].kind,
                root: if self.parts[index].root.is_empty() {
                    "/".to_owned()
                } else {
                    self.parts[index].root.clone()
                },
                version: self.parts[index].pkg.version,
                files: self.parts[index].pkg.entries.len(),
            });
            self.process_part(index)?;
        }

        self.emit_app_json()?;
        self.emit_project_config()?;
        self.emit_sitemap()?;
        self.ensure_min_files()?;
        if self.options.synthesize {
            self.synthesize_missing_wxml()?;
        }
        if self.options.prune || self.options.plugin_clean {
            self.prune_app_json()?;
        }
        // Count from what was written, not from what was reconstructed, so `counts`,
        // `files` and the directory on disk cannot disagree.
        self.recount();

        self.emit_report()?;

        self.report.counts = self.counts.clone();
        self.report.files = self.written.iter().cloned().collect();
        Ok(self.report)
    }

    // ---- I/O ----

    fn path(&self, rel: &str) -> PathBuf {
        paths::native(&self.out, rel)
    }

    /// `true` when the file is on disk with content.
    ///
    /// An empty WXML is not a usable template: the DevTools opens the page and shows
    /// nothing, and the compiler rejects some constructs around it. Empty files therefore
    /// count as missing so the synthesiser can replace them.
    fn has_content(&self, rel: &str) -> bool {
        if self.written.contains(rel) {
            return true;
        }
        let path = self.path(rel);
        match std::fs::read_to_string(&path) {
            Ok(text) => !text.trim().is_empty(),
            Err(_) => path.is_file(),
        }
    }

    fn write_bytes(&mut self, rel: &str, data: &[u8], force: bool) -> Result<bool> {
        let rel = paths::sanitize_relative(rel);
        if rel.is_empty() {
            return Ok(false);
        }
        if !force && self.written.contains(&rel) {
            return Ok(false);
        }
        let path = self.path(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_path(parent)?;
        }
        std::fs::write(&path, data).with_path(&path)?;
        self.written.insert(rel);
        Ok(true)
    }

    fn write_text(&mut self, rel: &str, text: &str, force: bool) -> Result<bool> {
        let mut text = self.format_if_enabled(rel, text);
        // A template that decompiled to nothing is not a project file the DevTools can
        // open; an empty `<view />` keeps the page valid and says "nothing was recovered".
        if rel.ends_with(".wxml") && text.trim().is_empty() {
            text = "<view />".to_owned();
        }
        // Every text file ends with exactly one newline — the DevTools formatter, git and
        // diff tools all treat a missing final newline as a defect.
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        self.write_bytes(rel, text.as_bytes(), force)
    }

    fn write_json(&mut self, rel: &str, value: &Value, force: bool) -> Result<bool> {
        let mut text = serde_json::to_string_pretty(value)?;
        text.push('\n');
        self.write_bytes(rel, text.as_bytes(), force)
    }

    /// Applies beautification when the extension is supported and formatting is on.
    fn format_if_enabled(&mut self, rel: &str, text: &str) -> String {
        let Some(options) = self.options.format.clone() else {
            return text.to_owned();
        };
        if !beautify::supports(rel) {
            return text.to_owned();
        }
        let formatted = beautify::beautify_by_extension(rel, text, &options);
        if formatted != text {
            self.counts.beautified += 1;
        }
        formatted
    }

    /// Writes a file without routing it through the reconstruction pipeline.
    ///
    /// Used for `_raw/` and `_foreign/`, which are verbatim copies rather than reconstructed
    /// sources. It still records the path, because otherwise those files exist on disk while
    /// being absent from both the counters and the file list — which is how the reported totals
    /// and the directory came to disagree.
    fn raw_write(&mut self, rel: &str, data: &[u8]) -> Result<()> {
        let rel = paths::sanitize_relative(rel);
        let path = self.path(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_path(parent)?;
        }
        std::fs::write(&path, data).with_path(&path)?;
        self.written.insert(rel);
        Ok(())
    }

    // ---- per container ----

    fn process_part(&mut self, index: usize) -> Result<()> {
        let root = self.parts[index].root.clone();
        let source = self.parts[index].source.clone();
        let entries = self.parts[index].pkg.entries.clone();

        let mut bundles: Vec<(String, String)> = Vec::new();
        let mut blobs: Vec<Option<Vec<u8>>> = Vec::with_capacity(entries.len());

        for entry in &entries {
            let name = entry.name.trim_start_matches('/').to_owned();
            if name.is_empty() || name.ends_with('/') {
                blobs.push(None);
                continue;
            }
            let blob = self.parts[index]
                .pkg
                .read_entry(entry, false, false)
                .unwrap_or_default();
            let rel = join_root(&root, &name);
            let base = name.rsplit('/').next().unwrap_or(&name).to_owned();
            let lower = name.to_ascii_lowercase();

            if lower.ends_with(".html") {
                if self.options.keep_raw {
                    self.raw_write(&format!("_raw/{source}/{name}"), &blob)?;
                    self.counts.h5 += 1;
                }
                blobs.push(None);
                continue;
            }
            if lower.ends_with("app-config.json") {
                if self.options.keep_raw {
                    self.raw_write(&format!("_raw/{source}/app-config.json"), &blob)?;
                }
                blobs.push(None);
                continue;
            }
            if lower.ends_with(".js") {
                if let Ok(text) = String::from_utf8(blob.clone()) {
                    if COMPILED_MARKERS.iter().any(|m| text.contains(m))
                        || JS_BUNDLE_HINTS.contains(&base.as_str())
                    {
                        bundles.push((rel, text));
                        blobs.push(None);
                        continue;
                    }
                }
            }
            blobs.push(Some(blob));
        }

        // Operand groups may be defined in a different bundle than the render functions
        // that reference them, so the pool is filled before any bundle is decompiled.
        for (_, text) in &bundles {
            wxml::collect_ops(text, &mut self.ops_pool);
        }

        for (rel, text) in &bundles {
            self.decompile_bundle(index, rel, text)?;
        }

        for (entry, blob) in entries.iter().zip(blobs) {
            let Some(blob) = blob else { continue };
            let name = entry.name.trim_start_matches('/');
            let rel = join_root(&root, name);
            if self.write_bytes(&rel, &blob, false)? {
                self.counts.assets += 1;
            }
        }
        Ok(())
    }

    fn report_warnings(&mut self, warnings: Vec<String>) {
        for warning in warnings {
            let key = RE_WARN_DEDUP
                .captures(&warning)
                .map(|c| format!("{}:{}", &c[1], &c[2]))
                .unwrap_or_else(|| warning.clone());
            if self.warn_seen.insert(key) && self.report.warnings.len() < self.options.max_warnings
            {
                self.report.warnings.push(warning);
            }
        }
    }

    fn decompile_bundle(&mut self, index: usize, rel_in_pkg: &str, src: &str) -> Result<()> {
        let root = self.parts[index].root.clone();
        let source = self.parts[index].source.clone();

        // JavaScript modules.
        let (bodies, _spans) = bundle::strip_define_bodies(src);
        for (module_path, body) in bodies {
            // The module registry also contains virtual schemes such as
            // `plugin-private://wx…/…`; writing them verbatim would create directories whose
            // names are illegal on Windows and unusable everywhere else. Nothing references
            // them through `require()`, so they are dropped with a note instead.
            if module_path.contains("://") {
                self.report.warnings.push(format!(
                    "{rel_in_pkg}: skipped the virtual module `{module_path}` (not a real file path)"
                ));
                continue;
            }
            let full = join_root(&root, &module_path);
            if !is_safe_output_path(&full) {
                self.report.warnings.push(format!(
                    "{rel_in_pkg}: skipped `{module_path}` because it does not map to a safe path"
                ));
                continue;
            }
            self.known_modules.insert(full.clone());
            if let Some(dir) = posix_dirname(&module_path) {
                if !dir.is_empty() {
                    self.known_modules.insert(join_root(&root, &dir));
                }
            }
            self.all_js.insert(full, body);
        }

        // Page / component configuration.
        for (key, raw) in bundle::find_appcode_json(src) {
            if key.contains("://") || !is_safe_output_path(&normalize_rel(&key)) {
                continue;
            }
            let norm = normalize_rel(&key);
            let full = join_root(&root, &norm);
            let parsed = serde_json::from_str::<Value>(&raw)
                .ok()
                .or_else(|| js_object_to_json(&raw));
            let Some(value) = parsed else {
                self.report.warnings.push(format!(
                    "{rel_in_pkg}: failed to parse __wxAppCode__ entry {key}"
                ));
                continue;
            };
            let mut text = serde_json::to_string_pretty(&value)?;
            text.push('\n');
            self.json_units.insert(full, text);
            self.counts.json += 1;
        }

        // WXML.
        if self.options.decompile_wxml {
            let (units, warnings) = wxml::decompile_app_service(src, true, Some(&self.ops_pool));
            self.report_warnings(warnings);
            for (key, text) in units {
                let norm = normalize_rel(&key);
                let full = join_root(&root, &norm);
                // Several bundles can describe the same template. Keep the rendition
                // with the fewest unresolvable bindings, then the longest one.
                let score = quality_score(&text);
                match self.wxml_units.get(&full) {
                    Some((existing, existing_score)) if *existing_score >= score => {}
                    _ => {
                        self.wxml_units.insert(full, (text, score));
                        self.counts.wxml += 1;
                    }
                }
            }
        }

        // WXSS.
        let (sheets, warnings) = wxss::extract_wxss(src);
        self.report_warnings(warnings);
        for (key, css) in sheets {
            if css.trim().is_empty() {
                continue;
            }
            let full = join_root(&root, &key);
            let css = if self.options.wxss_suffix.is_empty() {
                css
            } else {
                css.replace('.', &format!(".{}", self.options.wxss_suffix))
            };
            self.wxss_units.insert(full, css);
            self.counts.wxss += 1;
        }

        if self.options.keep_raw {
            self.raw_write(&format!("_raw/{source}/{rel_in_pkg}"), src.as_bytes())?;
            self.counts.raw += 1;
        }
        Ok(())
    }

    fn dump_foreign(&mut self, index: usize) -> Result<()> {
        let stem = Path::new(&self.parts[index].source)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let entries = self.parts[index].pkg.entries.clone();
        for entry in &entries {
            let name = entry.name.trim_start_matches('/').to_owned();
            if name.is_empty() || name.ends_with('/') {
                continue;
            }
            let blob = self.parts[index]
                .pkg
                .read_entry(entry, false, false)
                .unwrap_or_default();
            self.raw_write(&format!("_foreign/{stem}/{name}"), &blob)?;
            self.counts.foreign += 1;
        }
        Ok(())
    }

    // ---- project files ----

    fn emit_app_json(&mut self) -> Result<()> {
        let cfg = self
            .parts
            .first()
            .and_then(|p| p.app_config.clone())
            .unwrap_or(Value::Null);

        let sub_roots: Vec<String> = cfg
            .get("subPackages")
            .or_else(|| cfg.get("subpackages"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|sp| sp.get("root").and_then(Value::as_str))
            .map(|r| r.trim().trim_end_matches('/').to_owned() + "/")
            .filter(|r| r != "/")
            .collect();

        let pages_all: Vec<String> = cfg
            .get("pages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|p| {
                RE_HTML_SUFFIX
                    .replace(p.as_str().unwrap_or_default(), "")
                    .into_owned()
            })
            .collect();

        let mut main_pages: Vec<String> = Vec::new();
        let mut sub_pages: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for root in &sub_roots {
            sub_pages.insert(root.clone(), Vec::new());
        }
        for page in &pages_all {
            let hit = sub_roots
                .iter()
                .find(|r| page.starts_with(&format!("{}/", r.trim_end_matches('/'))));
            match hit {
                Some(root) => sub_pages
                    .entry(root.clone())
                    .or_default()
                    .push(page.clone()),
                None => main_pages.push(page.clone()),
            }
        }

        if main_pages.is_empty() && sub_pages.values().all(Vec::is_empty) && !self.is_mini_game() {
            let mut inferred: Vec<String> = self
                .wxml_units
                .keys()
                .filter(|p| p.ends_with(".wxml") && !p.ends_with("app.wxml"))
                .map(|p| p[..p.len() - ".wxml".len()].to_owned())
                .collect();
            inferred.sort();
            if !inferred.is_empty() {
                main_pages = inferred;
                self.report.notes.push(
                    "app-config.json declares no pages; the page list was inferred from the \
                     reconstructed WXML files"
                        .to_owned(),
                );
            }
        }

        let mut app = Map::new();
        app.insert(
            "pages".to_owned(),
            Value::Array(
                main_pages
                    .iter()
                    .map(|p| Value::String(p.clone()))
                    .collect(),
            ),
        );
        if !sub_roots.is_empty() {
            let mut list = Vec::new();
            for root in &sub_roots {
                let mut item = Map::new();
                item.insert("root".to_owned(), Value::String(root.clone()));
                let pages = sub_pages.get(root).cloned().unwrap_or_default();
                if !pages.is_empty() {
                    let prefix = format!("{}/", root.trim_end_matches('/'));
                    item.insert(
                        "pages".to_owned(),
                        Value::Array(
                            pages
                                .iter()
                                .map(|p| {
                                    Value::String(p.strip_prefix(&prefix).unwrap_or(p).to_owned())
                                })
                                .collect(),
                        ),
                    );
                }
                list.push(Value::Object(item));
            }
            app.insert("subPackages".to_owned(), Value::Array(list));
        }

        if let Some(window) = cfg
            .get("global")
            .and_then(|g| g.get("window"))
            .filter(|w| w.is_object())
        {
            app.insert("window".to_owned(), window.clone());
        }
        if let Some(tabbar) = cfg.get("tabBar").filter(|t| t.is_object()) {
            let emitted = self.emit_tabbar(tabbar.clone())?;
            app.insert("tabBar".to_owned(), emitted);
        }

        const PASSTHROUGH: &[&str] = &[
            "plugins",
            "preloadRule",
            "permission",
            "requiredPrivateInfos",
            "networkTimeout",
            "functionalPages",
            "workers",
            "navigateToMiniProgramAppIdList",
            "requiredBackgroundModes",
            "__usePrivacyCheck__",
            "supportedMaterials",
            "entryPagePath",
            "lazyCodeLoading",
            "rendererOptions",
            "componentFramework",
        ];
        for key in PASSTHROUGH {
            let Some(value) = cfg.get(*key) else { continue };
            if *key == "plugins" && self.options.plugins_mode != PluginsMode::Keep {
                let count = value.as_object().map_or(0, Map::len);
                self.report.notes.push(format!(
                    "plugin declarations were handled with mode {:?} ({count} entries); components \
                     referencing plugin:// must be replaced with local implementations",
                    self.options.plugins_mode
                ));
                if self.options.plugins_mode == PluginsMode::Placeholder {
                    app.insert("plugins".to_owned(), Value::Object(Map::new()));
                }
                continue;
            }
            if *key == "entryPagePath" {
                app.insert(
                    "entryPagePath".to_owned(),
                    Value::String(
                        RE_HTML_SUFFIX
                            .replace(value.as_str().unwrap_or_default(), "")
                            .into_owned(),
                    ),
                );
                continue;
            }
            // The compiled config stores the framework as a selection object; app.json
            // accepts only the chosen name.
            if *key == "componentFramework" {
                let chosen = uses_skyline(&cfg)
                    .then_some("glass-easel")
                    .or_else(|| value.as_str())
                    .or_else(|| {
                        value
                            .get("default")
                            .and_then(Value::as_str)
                            .filter(|name| matches!(*name, "exparser" | "glass-easel"))
                    });
                if let Some(name) = chosen {
                    app.insert(
                        "componentFramework".to_owned(),
                        Value::String(name.to_owned()),
                    );
                }
                continue;
            }
            app.insert((*key).to_owned(), value.clone());
        }
        // Skyline pages require the matching lazy-loading mode; the compiled page configs keep
        // `renderer: "skyline"` while the app-level flag is sometimes absent. Without it the
        // DevTools refuses to start the simulator.
        if !app.contains_key("lazyCodeLoading") && uses_skyline(&cfg) {
            app.insert(
                "lazyCodeLoading".to_owned(),
                Value::String("requiredComponents".to_owned()),
            );
            self.report.notes.push(
                "a page uses the Skyline renderer; app.json now sets lazyCodeLoading to \
                 requiredComponents as the DevTools requires"
                    .to_owned(),
            );
        }
        if let Some(appid) = cfg.get("appId") {
            app.insert("_appId".to_owned(), appid.clone());
        }

        let plugin_count = cfg
            .get("plugins")
            .and_then(Value::as_object)
            .map_or(0, Map::len);
        if self.options.plugins_mode == PluginsMode::Keep && plugin_count > 0 {
            let plugins = cfg
                .get("plugins")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            self.report.notes.push(format!(
                "app.json declares {plugin_count} plugin(s) ({}...). DevTools requires adding each \
                 plugin under Settings → Plugins before those pages compile; pass --plugins strip \
                 to drop the declarations instead",
                plugins.keys().take(4).cloned().collect::<Vec<_>>().join(", ")
            ));
            let missing: Vec<String> = plugins
                .iter()
                .filter(|(_, v)| {
                    let provider = v
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    !self.parts.iter().any(|p| {
                        matches!(p.kind, PartKind::Plugin | PartKind::Foreign)
                            && p.root.ends_with(provider)
                    })
                })
                .map(|(k, _)| k.clone())
                .collect();
            if !missing.is_empty() {
                self.report.warnings.push(format!(
                    "these plugins have no source in the imported containers and cannot be \
                     resolved inside DevTools: {}",
                    missing.join(", ")
                ));
            }
        }

        let value = Value::Object(app);
        self.report.notes.push(format!(
            "app.json: {} main pages, {} sub-packages, {plugin_count} plugins",
            main_pages.len(),
            sub_roots.len()
        ));
        self.app = Some(value.clone());
        self.write_json("app.json", &value, false)?;
        Ok(())
    }

    fn emit_tabbar(&mut self, tabbar: Value) -> Result<Value> {
        let Some(map) = tabbar.as_object() else {
            return Ok(tabbar);
        };
        let mut out = Map::new();
        for (key, value) in map {
            if key != "list" {
                out.insert(key.clone(), value.clone());
            }
        }
        let mut items = Vec::new();
        for (idx, item) in map
            .get("list")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            let Some(entry) = item.as_object() else {
                continue;
            };
            let mut entry = entry.clone();
            if let Some(page) = entry.get("pagePath").and_then(Value::as_str) {
                entry.insert(
                    "pagePath".to_owned(),
                    Value::String(RE_HTML_SUFFIX.replace(page, "").into_owned()),
                );
            }
            for (src_key, dst_key) in [
                ("iconData", "iconPath"),
                ("selectedIconData", "selectedIconPath"),
            ] {
                let Some(data) = entry.remove(src_key) else {
                    continue;
                };
                let Some(text) = data.as_str().filter(|s| !s.is_empty()) else {
                    continue;
                };
                let rel = format!("images/tabbar/tab{idx}_{dst_key}.png");
                if let Some(bytes) = decode_base64(text) {
                    self.write_bytes(&rel, &bytes, false)?;
                    entry.insert(dst_key.to_owned(), Value::String(rel));
                }
            }
            items.push(Value::Object(entry));
        }
        if !items.is_empty() {
            out.insert("list".to_owned(), Value::Array(items));
        }
        Ok(Value::Object(out))
    }

    fn emit_project_config(&mut self) -> Result<()> {
        // The real appid lets the DevTools open the project under its own identity; without
        // one the standard placeholder keeps the field well-formed.
        let appid = self
            .parts
            .iter()
            .find_map(|part| crate::discover::appid_in_path(&part.pkg.path));
        let name = appid.clone().unwrap_or_else(|| {
            self.out
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "wxapkg-dump".to_owned())
        });
        let value = json!({
            "description": "Reconstructed by wxapkg-dump; for study and security research only",
            "miniprogramRoot": "./",
            "packOptions": {
                "ignore": [
                    { "type": "folder", "value": "_raw" },
                    { "type": "file", "value": "_unpack_report.json" }
                ]
            },
            "setting": {
                "urlCheck": false,
                "es6": true,
                "enhance": true,
                "postcss": false,
                "minified": false,
                "newFeature": true,
                "bigPackageSizeSupport": true
            },
            "compileType": "miniprogram",
            "libVersion": "3.5.0",
            "appid": appid.unwrap_or_else(|| "touristappid".to_owned()),
            "projectname": name,
            "condition": {}
        });
        self.write_json("project.config.json", &value, false)?;
        Ok(())
    }

    fn emit_sitemap(&mut self) -> Result<()> {
        let value = json!({
            "desc": "See the official documentation for the semantics of this file",
            "rules": [ { "action": "allow", "page": "*" } ]
        });
        self.write_json("sitemap.json", &value, false)?;
        Ok(())
    }

    fn ensure_min_files(&mut self) -> Result<()> {
        let mut rewrites = 0usize;
        let index = bundle::ModuleIndex::new(self.known_modules.clone());
        let js: Vec<(String, String)> = self
            .all_js
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (full, body) in js {
            let code = if self.options.rewrite_requires {
                let (code, n) = bundle::rewrite_requires(&body, &full, &index);
                rewrites += n;
                code
            } else {
                body
            };
            let code = if code.ends_with('\n') {
                code
            } else {
                format!("{code}\n")
            };
            self.write_text(&full, &code, false)?;
            self.counts.js += 1;
        }
        if rewrites > 0 {
            self.report.notes.push(format!(
                "rewrote {rewrites} require() paths so the project resolves inside DevTools"
            ));
        }

        let json_units: Vec<(String, String)> = self
            .json_units
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (full, text) in json_units {
            if self.written.contains(&full) {
                continue;
            }
            self.write_text(&full, &text, false)?;
        }

        let wxml_units: Vec<(String, String)> = self
            .wxml_units
            .iter()
            .map(|(k, (v, _))| (k.clone(), v.clone()))
            .collect();
        for (full, text) in wxml_units {
            self.write_text(&full, &text, false)?;
        }

        let wxss_units: Vec<(String, String)> = self
            .wxss_units
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (full, css) in wxss_units {
            if full == "app.wxss" || full.ends_with("/app.wxss") {
                self.write_text("app.wxss", &css, false)?;
            } else {
                self.write_text(&full, &css, false)?;
            }
        }

        // Page-level window overrides.
        let windows: Vec<(String, Value)> = self
            .page_windows
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (page, patch) in windows {
            let rel = format!("{page}.json");
            let mut merged = self
                .read_json_rel(&rel)
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            if let Some(patch) = patch.as_object() {
                for (k, v) in patch {
                    // The compiled page map sometimes keeps the window block wrapped;
                    // page JSON takes those fields at the top level, and a literal
                    // The DevTools rejects a literal `"window"` key as an invalid page.json.
                    if k == "window" {
                        if let Some(window) = v.as_object() {
                            for (wk, wv) in window {
                                merged.entry(wk.clone()).or_insert_with(|| wv.clone());
                            }
                        }
                    } else {
                        merged.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
            // Page renderers accept a fixed vocabulary; component-level values such as
            // `rect-palette` appear in compiled configs and make the page unloadable.
            if let Some(renderer) = merged.get("renderer").and_then(Value::as_str) {
                if !matches!(renderer, "cover-view" | "skyline" | "webview" | "xr-frame") {
                    merged.insert("renderer".to_owned(), Value::String("webview".to_owned()));
                }
            }
            // DevTools: a Skyline page must also name the component framework and use
            // the custom navigation style.
            if merged.get("renderer").and_then(Value::as_str) == Some("skyline") {
                if !merged.contains_key("componentFramework") {
                    merged.insert(
                        "componentFramework".to_owned(),
                        Value::String("glass-easel".to_owned()),
                    );
                }
                if !merged.contains_key("navigationStyle") {
                    merged.insert(
                        "navigationStyle".to_owned(),
                        Value::String("custom".to_owned()),
                    );
                }
            }
            let mut text = serde_json::to_string_pretty(&Value::Object(merged))?;
            text.push('\n');
            self.write_text(&rel, &text, true)?;
        }

        if !self.written.contains("app.js") {
            if self.is_mini_game() {
                // Say what it is instead of manufacturing a Mini Program around it.
                self.report.notes.push(
                    "this is a Mini Game, not a Mini Program: it ships game.js instead of \
                     app.js and has no pages, so app.js/app.json/app.wxss are intentionally \
                     absent and the files below are the game's own sources"
                        .to_owned(),
                );
            } else {
                self.write_text("app.js", "App({\n  onLaunch() {},\n});\n", false)?;
                self.report.notes.push(
                    "the container carried no app.js; a minimal implementation was generated"
                        .to_owned(),
                );
            }
        }
        if !self.written.contains("app.wxss") && !self.is_mini_game() {
            self.write_text("app.wxss", "", false)?;
        }
        if !self.written.contains("app.json") && !self.is_mini_game() {
            self.write_json(
                "app.json",
                &json!({ "pages": ["pages/index/index"] }),
                false,
            )?;
        }
        Ok(())
    }

    fn read_json_rel(&self, rel: &str) -> Option<Value> {
        let path = self.path(rel);
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Gives every page / component whose WXML was never shipped a `<view />` shell.
    ///
    /// Skyline / glass-easel builds (Taro, for instance) drive rendering from the
    /// runtime and do not package page WXML at all, yet DevTools refuses to compile a
    /// page without one.
    fn synthesize_missing_wxml(&mut self) -> Result<()> {
        let mut pages = 0usize;
        let mut components = 0usize;

        let mut declared: Vec<String> = self
            .app
            .as_ref()
            .and_then(|a| a.get("pages"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|p| p.as_str().unwrap_or_default().to_owned())
            .collect();
        if let Some(subs) = self
            .app
            .as_ref()
            .and_then(|a| a.get("subPackages"))
            .and_then(Value::as_array)
        {
            for sub in subs {
                let root = sub
                    .get("root")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim_end_matches('/')
                    .to_owned();
                for page in sub
                    .get("pages")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                {
                    let p = page.as_str().unwrap_or_default();
                    declared.push(if p.is_empty() {
                        root.clone()
                    } else {
                        format!("{root}/{p}")
                    });
                }
            }
        }

        for page in declared {
            let rel = format!("{page}.wxml");
            if self.has_content(&rel) {
                continue;
            }
            if !self.path(&format!("{page}.js")).exists() {
                continue;
            }
            self.write_text(&rel, "<view />\n", true)?;
            pages += 1;
        }

        // Components: a `.json` + `.js` pair without a matching `.wxml`.
        let candidates = self.walk_output()?;
        for rel in candidates {
            if !rel.ends_with(".json") {
                continue;
            }
            let base = Path::new(&rel)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if matches!(
                base.as_str(),
                "app.json" | "project.config.json" | "sitemap.json"
            ) {
                continue;
            }
            let stem = rel.trim_end_matches(".json").to_owned();
            let wxml = format!("{stem}.wxml");
            if self.has_content(&wxml) {
                continue;
            }
            if !self.path(&format!("{stem}.js")).exists() {
                continue;
            }
            self.write_text(&wxml, "<view />\n", true)?;
            components += 1;
        }

        if pages > 0 || components > 0 {
            self.counts.wxml += pages + components;
            self.report.notes.push(format!(
                "synthesised <view /> shells for {pages} page(s) and {components} component(s) whose \
                 WXML is not present in the container (typical of Skyline / Taro builds)"
            ));
        }
        Ok(())
    }

    /// Lists every file under the output directory, relative and POSIX-normalised.
    fn walk_output(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let root = self.out.clone();
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                !matches!(
                    e.file_name().to_string_lossy().as_ref(),
                    "_raw" | "_foreign" | "node_modules" | ".git"
                )
            })
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
        Ok(out)
    }

    /// Trims `app.json` down to what actually exists on disk and repairs references.
    fn prune_app_json(&mut self) -> Result<()> {
        let Some(app) = self.app.clone() else {
            return Ok(());
        };
        let mut app = app.as_object().cloned().unwrap_or_default();

        // Kept as a diagnostic copy only, never in the project root: an extra `*.json` there
        // makes the tree look non-standard and invites the DevTools to treat it as a page
        // config. `_raw/` is already excluded from packaging.
        if self.options.keep_raw {
            self.write_json(
                "_raw/app.json.full.json",
                &Value::Object(app.clone()),
                false,
            )?;
        }

        let mut dropped_pages: Vec<String> = Vec::new();
        let mut dropped_roots: Vec<String> = Vec::new();
        let mut kept_subs: Vec<Value> = app
            .get("subPackages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        // Snapshot the reconstructed WXML paths so the predicate below borrows only
        // local state, leaving `self` free to accumulate notes.
        let existing_wxml: HashSet<String> = self
            .walk_output()?
            .into_iter()
            .filter(|p| p.ends_with(".wxml"))
            .collect();
        let kept_page = |rel: &str| existing_wxml.contains(&format!("{rel}.wxml"));

        if self.options.prune {
            let pages: Vec<String> = app
                .get("pages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|p| p.as_str().unwrap_or_default().to_owned())
                .collect();
            let kept: Vec<Value> = pages
                .iter()
                .filter(|p| kept_page(p))
                .map(|p| Value::String(p.clone()))
                .collect();
            dropped_pages.extend(pages.iter().filter(|p| !kept_page(p)).cloned());
            app.insert("pages".to_owned(), Value::Array(kept));

            let mut new_subs = Vec::new();
            for sub in &kept_subs {
                let root = sub
                    .get("root")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim_end_matches('/')
                    .to_owned();
                let pages: Vec<String> = sub
                    .get("pages")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|p| p.as_str().unwrap_or_default().to_owned())
                    .collect();
                let mut kept = Vec::new();
                for page in &pages {
                    let full = if page.is_empty() {
                        root.clone()
                    } else {
                        format!("{root}/{page}")
                    };
                    if kept_page(&full) {
                        kept.push(Value::String(page.clone()));
                    } else {
                        dropped_pages.push(full);
                    }
                }
                if kept.is_empty() {
                    dropped_roots.push(root);
                } else {
                    let mut rebuilt = sub.as_object().cloned().unwrap_or_default();
                    rebuilt.insert("pages".to_owned(), Value::Array(kept));
                    new_subs.push(Value::Object(rebuilt));
                }
            }
            kept_subs = new_subs;
            if kept_subs.is_empty() {
                app.remove("subPackages");
                app.remove("subpackages");
            } else {
                app.insert("subPackages".to_owned(), Value::Array(kept_subs.clone()));
            }

            // entryPagePath
            if let Some(entry) = app.get("entryPagePath").and_then(Value::as_str) {
                let entry = entry.to_owned();
                if !kept_page(&entry) {
                    let fallback = app
                        .get("pages")
                        .and_then(Value::as_array)
                        .and_then(|p| p.first())
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    match fallback {
                        Some(page) => {
                            self.report.notes.push(format!(
                                "entryPagePath pointed at {entry} (no source) and now points at {page}"
                            ));
                            app.insert("entryPagePath".to_owned(), Value::String(page));
                        }
                        None => {
                            app.remove("entryPagePath");
                        }
                    }
                }
            }

            // tabBar
            if let Some(tabbar) = app.get("tabBar").and_then(Value::as_object).cloned() {
                if let Some(list) = tabbar.get("list").and_then(Value::as_array) {
                    let kept: Vec<Value> = list
                        .iter()
                        .filter(|it| {
                            kept_page(
                                it.get("pagePath")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default(),
                            )
                        })
                        .cloned()
                        .collect();
                    if kept.is_empty() {
                        app.remove("tabBar");
                    } else {
                        let mut rebuilt = tabbar.clone();
                        rebuilt.insert("list".to_owned(), Value::Array(kept));
                        app.insert("tabBar".to_owned(), Value::Object(rebuilt));
                    }
                }
            }

            // preloadRule
            if let Some(rule) = app.get("preloadRule").and_then(Value::as_object).cloned() {
                let valid_roots: HashSet<String> = kept_subs
                    .iter()
                    .filter_map(|s| s.get("root").and_then(Value::as_str))
                    .map(|r| r.trim_end_matches('/').to_owned())
                    .collect();
                let kept_pages: HashSet<String> = app
                    .get("pages")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|p| p.as_str().unwrap_or_default().to_owned())
                    .collect();
                let mut rebuilt = Map::new();
                let mut dropped_pkgs = 0usize;
                for (key, value) in &rule {
                    let kk = key.trim_end_matches('/').to_owned();
                    if !valid_roots.contains(&kk) && !kept_pages.contains(&kk) {
                        continue;
                    }
                    if let Some(packages) = value.get("packages").and_then(Value::as_array) {
                        let kept: Vec<Value> = packages
                            .iter()
                            .filter(|p| {
                                valid_roots
                                    .contains(p.as_str().unwrap_or_default().trim_end_matches('/'))
                            })
                            .cloned()
                            .collect();
                        dropped_pkgs += packages.len() - kept.len();
                        if kept.is_empty() {
                            continue;
                        }
                        if kept.len() != packages.len() {
                            let mut patched = value.as_object().cloned().unwrap_or_default();
                            patched.insert("packages".to_owned(), Value::Array(kept));
                            rebuilt.insert(key.clone(), Value::Object(patched));
                            continue;
                        }
                    }
                    rebuilt.insert(key.clone(), value.clone());
                }
                if rebuilt.is_empty() {
                    app.remove("preloadRule");
                } else {
                    app.insert("preloadRule".to_owned(), Value::Object(rebuilt));
                }
                if dropped_pkgs > 0 {
                    self.report.notes.push(format!(
                        "preloadRule: dropped {dropped_pkgs} reference(s) to sub-packages without sources"
                    ));
                }
            }

            // workers
            if let Some(workers) = app.get("workers").and_then(Value::as_str) {
                if !self.path(workers).exists() {
                    app.remove("workers");
                    self.report.notes.push(
                        "the workers directory has no sources; the declaration was removed"
                            .to_owned(),
                    );
                }
            }
        }

        if self.options.plugin_clean {
            if let Some(plugins) = app.get("plugins").and_then(Value::as_object).cloned() {
                let available: HashSet<String> = self
                    .parts
                    .iter()
                    .filter(|p| matches!(p.kind, PartKind::Plugin | PartKind::Foreign))
                    .filter_map(|p| RE_WXID.find(&p.root).map(|m| m.as_str().to_owned()))
                    .collect();
                let missing: Vec<String> = plugins
                    .iter()
                    .filter(|(_, v)| {
                        let provider = v
                            .get("provider")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        !available.contains(provider)
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                if !missing.is_empty() {
                    for key in &missing {
                        app.remove(key);
                    }
                    let mut remaining = plugins.clone();
                    for key in &missing {
                        remaining.remove(key);
                    }
                    if remaining.is_empty() {
                        app.remove("plugins");
                    } else {
                        app.insert("plugins".to_owned(), Value::Object(remaining));
                    }
                    let removed_refs = self.strip_plugin_component_refs()?;
                    self.report.notes.push(format!(
                        "removed {} plugin declaration(s) without sources{}",
                        missing.len(),
                        if removed_refs > 0 {
                            format!(" and {removed_refs} plugin:// component reference(s)")
                        } else {
                            String::new()
                        }
                    ));
                }
            }
        }

        self.write_json("app.json", &Value::Object(app.clone()), true)?;

        let mut summary = format!(
            "auto-repair: {} main page(s), {} sub-package(s)",
            app.get("pages")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
            kept_subs.len()
        );
        if !dropped_pages.is_empty() {
            summary.push_str(&format!(
                "; dropped {} page(s) without sources",
                dropped_pages.len()
            ));
        }
        if !dropped_roots.is_empty() {
            summary.push_str(&format!(" and {} sub-package(s)", dropped_roots.len()));
        }
        summary.push_str(" (the full declaration is kept in _raw/app.json.full.json)");
        self.report.notes.push(summary);

        if self.options.prune {
            let removed = self.strip_missing_component_refs()?;
            if removed > 0 {
                self.report.notes.push(format!(
                    "removed {removed} usingComponents reference(s) pointing at components without sources"
                ));
            }
        }
        self.app = Some(Value::Object(app));
        Ok(())
    }

    fn strip_missing_component_refs(&self) -> Result<usize> {
        let mut removed = 0usize;
        for rel in self.walk_output()? {
            if !rel.ends_with(".json")
                || matches!(
                    rel.as_str(),
                    "app.json" | "project.config.json" | "sitemap.json"
                )
            {
                continue;
            }
            let path = self.path(&rel);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(map) = value.as_object_mut() else {
                continue;
            };
            let Some(uc) = map
                .get_mut("usingComponents")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            let dir = posix_dirname(&rel).unwrap_or_default();
            let bad: Vec<String> = uc
                .iter()
                .filter(|(_, reference)| {
                    let Some(reference) = reference.as_str() else {
                        return false;
                    };
                    if reference.is_empty()
                        || reference.contains("://")
                        || reference.starts_with("//")
                    {
                        return false;
                    }
                    let target = if let Some(stripped) = reference.strip_prefix('/') {
                        stripped.to_owned()
                    } else {
                        posix_join(&dir, reference)
                    };
                    !self.path(&format!("{target}.json")).exists()
                })
                .map(|(name, _)| name.clone())
                .collect();
            if bad.is_empty() {
                continue;
            }
            for name in &bad {
                uc.remove(name);
            }
            removed += bad.len();
            let mut text = serde_json::to_string_pretty(&value)?;
            text.push('\n');
            std::fs::write(&path, text).with_path(&path)?;
        }
        Ok(removed)
    }

    fn strip_plugin_component_refs(&self) -> Result<usize> {
        let mut removed = 0usize;
        for rel in self.walk_output()? {
            if !rel.ends_with(".json") || matches!(rel.as_str(), "app.json" | "project.config.json")
            {
                continue;
            }
            let path = self.path(&rel);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(map) = value.as_object_mut() else {
                continue;
            };
            let Some(uc) = map
                .get_mut("usingComponents")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            let bad: Vec<String> = uc
                .iter()
                .filter(|(_, v)| {
                    v.as_str().is_some_and(|s| {
                        s.starts_with("plugin://") || s.starts_with("plugin-private://")
                    })
                })
                .map(|(k, _)| k.clone())
                .collect();
            if bad.is_empty() {
                continue;
            }
            for name in &bad {
                uc.remove(name);
            }
            removed += bad.len();
            let mut text = serde_json::to_string_pretty(&value)?;
            text.push('\n');
            std::fs::write(&path, text).with_path(&path)?;
        }
        Ok(removed)
    }

    fn emit_report(&mut self) -> Result<()> {
        let data = json!({
            "generatedBy": concat!("wxapkg-dump ", env!("CARGO_PKG_VERSION")),
            "outDir": self.report.out_dir,
            "parts": self.report.parts,
            "counts": self.counts,
            // The list is the authoritative record of what landed on disk; a report without it
            // cannot be reconciled against the tree, which is exactly the check a reviewer
            // wants to run.
            "files": self.written.iter().cloned().collect::<Vec<_>>(),
            "notes": self.report.notes,
            "warnings": self.report.warnings,
        });
        let mut text = serde_json::to_string_pretty(&data)?;
        text.push('\n');
        let path = self.out.join("_unpack_report.json");
        std::fs::write(&path, text).with_path(&path)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rejects a path that would escape the output directory or is invalid on some platform.
///
/// Windows forbids `<>:"|?*` and trailing dots or spaces; a colon is the character that
/// actually appears in WeChat's virtual module names, but the check is deliberately broad so
/// a malformed registry entry can never produce an unwritable tree.
pub fn is_safe_output_path(relative: &str) -> bool {
    if relative.is_empty() {
        return false;
    }
    if relative.starts_with('/') || relative.contains("..") {
        return false;
    }
    relative.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && !segment.chars().any(|c| {
                matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\') || (c as u32) < 0x20
            })
            && !segment.ends_with(' ')
            && !segment.ends_with('.')
    })
}

/// Higher is better: fewer unresolved bindings and more content.
pub fn quality_score(wxml: &str) -> i32 {
    let undefined = wxml.matches("undefined").count() as i32;
    let blocks = wxml.matches("<block").count() as i32;
    let length = (wxml.len() / 64) as i32;
    (-undefined * 8) - blocks + length
}

/// Joins a container root with an in-container relative path.
///
/// Entry names come in two flavours: relative to the sub-package root (most packages)
/// and already project-absolute (newer Taro-style builds). Both are handled so that
/// `root/root/` can never be produced.
pub fn join_root(root: &str, rel: &str) -> String {
    let mut rel = rel.replace('\\', "/");
    while let Some(rest) = rel.strip_prefix("./") {
        rel = rest.to_owned();
    }
    let rel = rel.trim_start_matches('/').to_owned();
    if root.is_empty() {
        return rel;
    }
    let rr = format!("{}/", root.trim_end_matches('/'));
    if rel.starts_with(&rr) {
        return rel;
    }
    format!("{}{}", rr, rel)
}

/// Strips leading `./` and `/`.
pub fn normalize_rel(path: &str) -> String {
    path.trim_start_matches("./")
        .trim_start_matches('/')
        .to_owned()
}

fn posix_dirname(path: &str) -> Option<String> {
    path.rsplit_once('/').map(|(dir, _)| dir.to_owned())
}

fn posix_join(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        return normalize_rel(rel);
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for segment in rel.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, byte) in TABLE.iter().enumerate() {
        lookup[*byte as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let value = lookup[byte as usize];
        if value == 255 {
            continue;
        }
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// Converts a JavaScript object literal into JSON.
///
/// Handles the two deviations WeChat emits: trailing commas and unquoted keys.
pub fn js_object_to_json(raw: &str) -> Option<Value> {
    // Trailing commas can nest (`{a:{b:1,}, ,}`), so the rewrite runs to a fixed point.
    let mut current = raw.trim().to_owned();
    for _ in 0..8 {
        let next = RE_JSON_TRAILING_COMMA
            .replace_all(&current, "$1")
            .into_owned();
        let next = RE_JSON_EMPTY_COMMA.replace_all(&next, ",").into_owned();
        if next == current {
            break;
        }
        current = next;
    }
    let quoted = RE_JS_OBJECT_KEY
        .replace_all(&current, |caps: &regex::Captures<'_>| {
            let (prefix, key) = (&caps[1], &caps[2]);
            format!("{prefix}\"{key}\":")
        })
        .into_owned();
    let normalized = quoted.replace(char::from(39), "\"");
    serde_json::from_str(&normalized).ok()
}

/// Whether a set of output paths belongs to a Mini Game rather than a Mini Program.
///
/// Separated from the builder so the rule can be tested without constructing a container:
/// the signal is the presence of the game entry point or of an engine directory.
fn looks_like_mini_game<'a>(paths: impl IntoIterator<Item = &'a str>) -> bool {
    paths
        .into_iter()
        .any(|path| path == "game.js" || path == "game.json" || path.starts_with("cocos/"))
}

#[cfg(test)]
mod tests {
    use super::looks_like_mini_game;

    #[test]
    fn mini_games_are_recognised_from_their_entry_point_or_engine() {
        assert!(looks_like_mini_game(["game.js", "project.config.json"]));
        assert!(looks_like_mini_game(["game.json"]));
        assert!(looks_like_mini_game(["cocos/cc.js", "assets/a.png"]));
        // A Mini Program must never be misclassified: it has app.js and no game entry.
        assert!(!looks_like_mini_game([
            "app.js",
            "app.json",
            "pages/index/index.js"
        ]));
        // `game` appearing as an ordinary path segment is not a signal.
        assert!(!looks_like_mini_game(["pages/game/game.js", "app.js"]));
        assert!(!looks_like_mini_game(Vec::<&str>::new()));
    }

    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn join_root_avoids_double_prefixes() {
        assert_eq!(join_root("", "pages/a.js"), "pages/a.js");
        assert_eq!(join_root("sub/rec", "index.js"), "sub/rec/index.js");
        assert_eq!(join_root("sub/rec", "sub/rec/index.js"), "sub/rec/index.js");
        assert_eq!(join_root("sub/rec", "./index.js"), "sub/rec/index.js");
    }

    #[test]
    fn js_object_literals_become_json() {
        let value = js_object_to_json("{usingComponents:{'a-b':'/components/a'},}").unwrap();
        assert_eq!(value["usingComponents"]["a-b"], "/components/a");
    }

    #[test]
    fn base64_roundtrip_for_tabbar_icons() {
        // "hello" in base64.
        let data = decode_base64("aGVsbG8=").unwrap();
        assert_eq!(data, b"hello");
    }

    #[test]
    fn posix_join_resolves_parent_segments() {
        assert_eq!(posix_join("pages/a", "../../common/x"), "common/x");
        assert_eq!(posix_join("", "./x"), "x");
    }

    #[test]
    fn unsafe_output_paths_are_rejected() {
        assert!(is_safe_output_path("pages/index/index.js"));
        assert!(is_safe_output_path("components/a-b/index.js"));
        assert!(!is_safe_output_path("plugin-private://wx123/app.js"));
        assert!(!is_safe_output_path("../../../etc/passwd"));
        assert!(!is_safe_output_path("/absolute/path.js"));
        assert!(!is_safe_output_path("dir/"));
        assert!(!is_safe_output_path("name."));
        assert!(!is_safe_output_path("a\\b.js"));
    }

    #[test]
    fn quality_score_prefers_fewer_undefined_bindings() {
        assert!(
            quality_score("<view a=\"{{x}}\" />") > quality_score("<view a=\"{{undefined}}\" />")
        );
    }
}
