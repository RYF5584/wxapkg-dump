// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Local discovery of Mini Program bundles installed by the WeChat desktop client.
//!
//! Supported layouts (all community documented; a recursive `*.wxapkg` sweep acts as
//! a catch-all so new WeChat revisions keep working):
//!
//! | Platform | Client | Location |
//! |---|---|---|
//! | Windows | WeChat 3.x | `%USERPROFILE%\Documents\WeChat Files\Applet\wx<appid>\<version>\*.wxapkg` |
//! | Windows | WeChat 4.x | `%USERPROFILE%\Documents\xwechat_files\<id>_<hash>\...\applet\...` |
//! | macOS | WeChat 3.x | `~/Library/Containers/com.tencent.xinWeChat/Data/.../Applet/wx<appid>/...` |
//! | macOS | WeChat 4.x | `~/Library/Containers/com.tencent.xinWeChat/Data/Documents/app_data/radium/users/<user>/applet/packages/wx<appid>/<version>/*.wxapkg` |
//!
//! Because the encryption key for PC/macOS bundles is the appid itself and the appid
//! is always part of the path, discovery can resolve decryption automatically.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use walkdir::WalkDir;

use crate::crypto;
use crate::error::{Error, Result};

/// Maximum directory depth explored below a search root.
const MAX_DEPTH: usize = 14;

/// Smallest plausible bundle size; anything below is a placeholder file.
const MIN_BUNDLE_SIZE: u64 = 20;

/// A single `.wxapkg` file found on disk.
#[derive(Debug, Clone, Serialize)]
pub struct PackageFile {
    /// Absolute path of the bundle.
    pub path: PathBuf,
    /// File size in bytes.
    pub size: u64,
    /// Appid inferred from the path, when possible.
    pub appid: Option<String>,
    /// `Some(true)` when the file starts with `V1MMWX`, `None` when unread.
    pub encrypted: Option<bool>,
}

/// One installed Mini Program: an appid plus a version directory.
#[derive(Debug, Clone, Serialize)]
pub struct Program {
    /// Appid (`wx` + 16 hex digits) or `unknown`.
    pub appid: String,
    /// Version directory name; empty when it could not be determined.
    pub version: String,
    /// Directory that holds the bundles.
    pub location: PathBuf,
    /// Key used for `V1MMWX` decryption; equals the appid on PC/macOS.
    pub wxid: String,
    /// Bundles belonging to this installation.
    pub packages: Vec<PackageFile>,
}

impl Program {
    /// Sum of all bundle sizes.
    pub fn total_size(&self) -> u64 {
        self.packages.iter().map(|p| p.size).sum()
    }

    /// `true` when at least one bundle is `V1MMWX` encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.packages.iter().any(|p| p.encrypted == Some(true))
    }

    /// The bundle that is most likely the main package (largest file).
    pub fn main_package(&self) -> Option<&PackageFile> {
        self.packages.iter().max_by_key(|p| p.size)
    }
}

/// Returns the platform default search roots, newest layout first.
pub fn default_roots() -> Vec<PathBuf> {
    let home = directories::UserDirs::new().map(|d| d.home_dir().to_path_buf());
    let Some(home) = home else {
        return Vec::new();
    };

    // Components are joined one at a time: a `"a/b"` literal keeps the forward slash on
    // Windows, so `doctor` would print `C:\Users\…\WeChat Files/Applet`.
    if cfg!(target_os = "macos") {
        let base = home
            .join("Library")
            .join("Containers")
            .join("com.tencent.xinWeChat")
            .join("Data");
        return vec![
            base.join("Documents")
                .join("app_data")
                .join("radium")
                .join("users"),
            base.join("Documents").join("xwechat_files"),
            base.join("Documents").join("WeChat Files").join("Applet"),
            base.join("Library")
                .join("Application Support")
                .join("com.tencent.xinWeChat"),
        ];
    }

    if cfg!(target_os = "windows") {
        let docs = home.join("Documents");
        let appdata = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Roaming"));
        return vec![
            docs.join("WeChat Files").join("Applet"),
            docs.join("xwechat_files"),
            appdata.join("Tencent").join("xwechat"),
            appdata.join("Tencent").join("WeChat").join("Applet"),
        ];
    }

    // Linux and everything else: wine prefixes plus the conventional documents path.
    vec![
        home.join(".wine").join("drive_c").join("users"),
        home.join("Documents").join("WeChat Files").join("Applet"),
        home.join("Documents").join("xwechat_files"),
    ]
}

/// Scans `roots` (or [`default_roots`]) and groups bundles by appid and version.
///
/// `limit` truncates the returned list after grouping; see `walk_bundles` for why it
/// cannot reduce the traversal itself.
pub fn scan_programs(roots: Option<&[PathBuf]>, limit: Option<usize>) -> Result<Vec<Program>> {
    Ok(scan_programs_matching(roots, None, limit)?.programs)
}

/// Like [`scan_programs`], but prunes the walk to one appid and reports what could not be read.
///
/// Pruning happens while descending, so this is the cheap way to answer "where is wx…"
/// without paying for the whole corpus: the walker never enters another appid's package
/// directory. The result is still filtered by appid afterwards, so an imperfect prune can
/// only cost time, never correctness.
///
/// When an appid was requested and nothing was found, the walk is repeated once: an empty
/// answer for an appid the caller chose because it exists is far more likely to be a
/// transient filesystem failure than a real absence. The retry is deliberately confined to
/// that case so the common path never pays for it.
pub fn scan_programs_matching(
    roots: Option<&[PathBuf]>,
    appid: Option<&str>,
    limit: Option<usize>,
) -> Result<ScanOutcome> {
    let wanted = appid
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    let owned;
    let roots = match roots {
        Some(r) => r,
        None => {
            owned = default_roots();
            &owned
        }
    };

    let mut found: Vec<PackageFile> = Vec::new();
    let mut skipped = 0usize;
    let mut denied = 0usize;
    for pass in 0..2 {
        found.clear();
        skipped = 0;
        denied = 0;
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for root in roots {
            if !root.is_dir() {
                continue;
            }
            let outcome = walk_bundles(root, wanted.as_deref())?;
            skipped += outcome.skipped;
            denied += outcome.denied;
            for pf in outcome.bundles {
                let key = std::fs::canonicalize(&pf.path).unwrap_or_else(|_| pf.path.clone());
                if seen.insert(key) {
                    found.push(pf);
                }
            }
            // A targeted lookup is satisfied by the first root that has the requested appid.
            // `found` can also contain unrelated bundles when a root does not follow the
            // fast layout, so testing emptiness alone would stop too early.
            if let Some(wanted) = wanted.as_deref() {
                if found.iter().any(|pf| {
                    pf.appid
                        .as_deref()
                        .is_some_and(|appid| appid.eq_ignore_ascii_case(wanted))
                }) {
                    break;
                }
            }
        }
        let worth_retrying = wanted.is_some() && found.is_empty();
        if !worth_retrying {
            break;
        }
        tracing::warn!(
            target: "wxapkg-dump::discover",
            appid = wanted.as_deref().unwrap_or_default(),
            skipped_paths = skipped,
            pass,
            "no bundle found for the requested appid; re-walking once before reporting empty"
        );
    }

    let mut groups: BTreeMap<(String, String, PathBuf), Program> = BTreeMap::new();
    for pf in found {
        let appid = pf.appid.clone().unwrap_or_else(|| "unknown".to_owned());
        if wanted.as_ref().is_some_and(|want| *want != appid) {
            continue;
        }
        let parent = pf
            .path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let version = version_segment(&pf.path, &appid);
        let entry = groups
            .entry((appid.clone(), version.clone(), parent.clone()))
            .or_insert_with(|| Program {
                appid: appid.clone(),
                version,
                location: parent,
                wxid: appid.clone(),
                packages: Vec::new(),
            });
        entry.packages.push(pf);
    }

    let mut programs: Vec<Program> = groups.into_values().collect();
    for program in &mut programs {
        for pf in &mut program.packages {
            pf.encrypted = head_is_encrypted(&pf.path);
        }
        program.packages.sort_by_key(|p| std::cmp::Reverse(p.size));
    }
    programs.sort_by(|a, b| (&a.appid, &a.version).cmp(&(&b.appid, &b.version)));
    if let Some(limit) = limit {
        programs.truncate(limit);
    }
    Ok(ScanOutcome {
        programs,
        skipped_paths: skipped,
        denied_paths: denied,
    })
}

/// Picks the installation matching `appid`, optionally restricted to `version`.
///
/// When several installations exist the most recently modified one wins.
pub fn pick_program(
    appid: &str,
    roots: Option<&[PathBuf]>,
    version: Option<&str>,
) -> Result<Option<Program>> {
    let wanted = appid.trim().to_ascii_lowercase();
    // Pruning during the walk is what makes `unpack --appid` cheap; the function filters by
    // appid itself, so no second pass is needed here.
    let mut candidates: Vec<Program> = scan_programs_matching(roots, Some(&wanted), None)?.programs;
    if let Some(v) = version {
        candidates.retain(|p| p.version == v);
    }
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(candidates.pop()),
        _ => {
            candidates.sort_by(|a, b| latest_mtime(b).partial_cmp(&latest_mtime(a)).unwrap());
            Ok(candidates.into_iter().next())
        }
    }
}

/// Resolves a single installed program, returning a rich error when absent.
pub fn require_program(
    appid: &str,
    roots: Option<&[PathBuf]>,
    version: Option<&str>,
) -> Result<Program> {
    pick_program(appid, roots, version)?.ok_or_else(|| Error::ProgramNotFound {
        appid: appid.to_owned(),
        version: version.map(str::to_owned),
    })
}

/// Directory names that cannot hold a Mini Program bundle.
///
/// This is deliberately a denylist: a wrong entry can only make the walk longer, never lose
/// a package, whereas an allowlist of "meaningful" names would silently drop a layout nobody
/// anticipated. Entries were added for the directories that dominate a WeChat data tree —
/// the walk is I/O bound (33 s wall at 3 % CPU before pruning), so every skipped directory
/// is time returned to the user.
const PRUNED_DIRECTORIES: &[&str] = &[
    "Image",
    "FileStorage",
    "FileStorage2",
    "Cache",
    "cache",
    "GPUCache",
    "Code Cache",
    "Temp",
    "temp",
    "Video",
    "Sns",
    "Favorites",
    "Attachment",
    "Emoji",
    "msg",
    "log",
    "logs",
    "crashpad",
    "Crashpad",
    "blob_storage",
    "IndexedDB",
    "Local Storage",
    "Session Storage",
    "Service Worker",
    "WMPF",
    "databases",
];

/// `wx` (or `WX`) followed by exactly 16 hex digits.
fn is_appid_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.len() == 18
        && lower.starts_with("wx")
        && lower[2..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Whether the string is exactly an appid (`wx` + 16 hex digits).
pub fn is_appid(value: &str) -> bool {
    is_appid_name(value)
}

/// Returns the appid carried by a container path, if any component looks like one.
///
/// Local caches put the appid in the path (`…/Applet/wx…/<version>/__APP__.wxapkg`), which
/// is how a restored project knows where to go when the caller only passed a file.
pub fn appid_in_path(path: &Path) -> Option<String> {
    path.components().rev().find_map(|component| {
        let name = component.as_os_str().to_string_lossy();
        // Downloaded packages are named `<appid>_<module>_t<type>.wxapkg`, so the appid can
        // lead the file name even when no directory carries it.
        let lower = name.to_ascii_lowercase();
        if is_appid_name(&lower) {
            return Some(lower);
        }
        let prefix: String = lower.chars().take(18).collect();
        if is_appid_name(&prefix) && lower.len() > 18 {
            let next = lower.as_bytes()[18];
            if next == b'_' || next == b'.' || next == b'-' {
                return Some(prefix);
            }
        }
        None
    })
}

/// Whether the walk can skip this directory outright.
///
/// `wanted` is the appid the caller asked for, when there is one: a sibling directory under
/// `packages/` named after a different appid cannot contain a bundle whose inferred appid
/// matches, because [`crypto::find_wxid`] reads the appid from the path itself.
fn is_prunable_directory(entry: &walkdir::DirEntry, wanted: Option<&str>) -> bool {
    let name = entry.file_name().to_string_lossy();
    if PRUNED_DIRECTORIES.contains(&name.as_ref()) {
        return true;
    }
    // The WeChat base library is not a Mini Program; a post-filter drops it too, but not
    // descending into it at all is the point of pruning.
    if name.eq_ignore_ascii_case("publicLib") {
        return true;
    }
    if let Some(wanted) = wanted {
        let in_packages = entry
            .path()
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|parent| parent == "packages");
        if in_packages && is_appid_name(&name) && !name.eq_ignore_ascii_case(wanted) {
            return true;
        }
    }
    false
}

/// Result of a scan, including what the walk could not read.
///
/// `skipped_paths` exists because an unreadable directory used to be a silent omission: the
/// caller received a shorter list with no way to tell it apart from a complete one. WeChat
/// rewrites its data tree while it runs, so this is a normal condition, and a quietly
/// incomplete answer is worse than an error.
#[derive(Debug, Clone, Default)]
pub struct ScanOutcome {
    /// Programs found, ordered by appid and version.
    pub programs: Vec<Program>,
    /// Directories the walk could not read.
    ///
    /// Non-zero means the listing may be shorter than reality: the walk skips a subtree it
    /// cannot read rather than failing. Callers must surface this to the user — a silently
    /// incomplete list is worse than an error.
    ///
    /// An appid-filtered walk that finds nothing is re-walked once before reporting empty, but
    /// the retry does not make an incomplete listing safe: it only covers the case where the
    /// whole result came back bare.
    pub skipped_paths: usize,
    /// Directories the operating system refused to open.
    ///
    /// A non-zero count usually means the process lacks permission for the WeChat container
    /// (macOS Full Disk Access, or a sandbox). It is separate from `skipped_paths` because
    /// the fix is different: one is transient, the other is a setting the user can change.
    pub denied_paths: usize,
}

/// What one root contributed, and what the walk could not read under it.
#[derive(Debug, Default)]
struct WalkOutcome {
    bundles: Vec<PackageFile>,
    skipped: usize,
    /// Directories a permission check refused.
    denied: usize,
    /// Directories whose read was interrupted, to be retried once.
    interrupted: Vec<PathBuf>,
}

/// Walks `root` for every bundle it can reach.
///
/// There is deliberately no limit parameter. A package-counted cap cannot honour a
/// program-counted one — several packages belong to a single program — and measuring showed
/// it did not pay for itself anyway, so the caller's limit is applied after grouping instead.
/// The walk is I/O bound, so pruning directories ([`is_prunable_directory`]) is what actually
/// makes it cheap.
fn walk_bundles(root: &Path, wanted: Option<&str>) -> Result<WalkOutcome> {
    // A single-appid lookup is the GUI's hot path: try the known layouts directly instead
    // of enumerating every user and every program. The recursive walk remains the fallback.
    if let Some(appid) = wanted {
        if let Some(outcome) = direct_lookup(root, appid) {
            return Ok(outcome);
        }
    }
    // The layout the client actually uses is only three levels deep. Walking it directly
    // avoids a recursive walk over every file, which is where almost all of the scan time
    // used to go. Anything unexpected falls back to the general walk, so this is an
    // optimisation and never a correctness risk.
    if let Some(outcome) = fast_walk(root, wanted) {
        return Ok(outcome);
    }

    let mut outcome = walk_once(root, wanted)?;

    // WeChat rewrites its data tree while it runs, so `readdir` on a directory it is
    // re-creating returns EINTR. That is a transient condition, not an unreadable subtree:
    // walking the directory once more almost always succeeds, and reporting it as a
    // skipped path would make an ordinary scan look broken. Only a second failure counts.
    let interrupted = std::mem::take(&mut outcome.interrupted);
    for directory in interrupted {
        match walk_once(&directory, wanted) {
            Ok(retry) => {
                outcome.bundles.extend(retry.bundles);
                outcome.skipped += retry.skipped;
                outcome.denied += retry.denied;
                outcome.interrupted = retry.interrupted;
            }
            Err(err) => {
                tracing::debug!(
                    target: "wxapkg-dump::discover",
                    path = %directory.display(),
                    error = %err,
                    "a directory could not be re-walked after an interrupted read"
                );
                outcome.skipped += 1;
            }
        }
    }
    Ok(outcome)
}

/// Probes the known appid layouts without walking unrelated programs.
///
/// macOS 4.x: `root/<user>/applet/packages/<appid>/<version>/*.wxapkg`.
/// Windows 3.x: `root/<appid>/<version>/*.wxapkg`.
/// Also accepts one extra nesting level (`root/<a>/<b>/applet/packages/<appid>`), which
/// covers the `xwechat_files/<id>_<hash>/...` layout.
fn direct_lookup(root: &Path, appid: &str) -> Option<WalkOutcome> {
    let mut bundles = Vec::new();
    let mut probe = |dir: &Path| collect_appid_bundles(dir, appid, &mut bundles);

    probe(&root.join(appid));
    let first = std::fs::read_dir(root).ok()?;
    for entry in first.flatten() {
        if !entry.file_type().ok()?.is_dir() {
            continue;
        }
        let base = entry.path();
        probe(&base.join(appid));
        probe(&base.join("Applet").join(appid));
        probe(&base.join("applet").join("packages").join(appid));
        probe(&base.join("packages").join(appid));
        for nested in std::fs::read_dir(&base).into_iter().flatten().flatten() {
            if !nested.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let deeper = nested.path();
            probe(&deeper.join("applet").join("packages").join(appid));
            probe(&deeper.join("Applet").join(appid));
            probe(&deeper.join("packages").join(appid));
            probe(&deeper.join(appid));
        }
    }
    if bundles.is_empty() {
        None
    } else {
        Some(WalkOutcome {
            bundles,
            ..WalkOutcome::default()
        })
    }
}

/// Collects `.wxapkg` files from an appid directory and its immediate version children.
fn collect_appid_bundles(dir: &Path, appid: &str, out: &mut Vec<PackageFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut add = |path: PathBuf, meta: std::fs::Metadata| {
        if meta.is_file() && meta.len() >= MIN_BUNDLE_SIZE {
            out.push(PackageFile {
                path,
                size: meta.len(),
                appid: Some(appid.to_owned()),
                encrypted: None,
            });
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.ends_with(".wxapkg") || name.ends_with(".wxapkg.invalid") {
                add(path, meta);
            }
            continue;
        }
        if meta.is_dir() {
            if let Ok(children) = std::fs::read_dir(&path) {
                for child in children.flatten() {
                    let child_path = child.path();
                    let Ok(child_meta) = child.metadata() else {
                        continue;
                    };
                    let name = child.file_name().to_string_lossy().to_ascii_lowercase();
                    if child_meta.is_file()
                        && (name.ends_with(".wxapkg") || name.ends_with(".wxapkg.invalid"))
                    {
                        add(child_path, child_meta);
                    }
                }
            }
        }
    }
}

/// Enumerates `root/<appid>/<version>/*.wxapkg` directly.
///
/// Returns `None` when the directory does not look like that layout, so the caller can fall
/// back to the recursive walk.
fn fast_walk(root: &Path, wanted: Option<&str>) -> Option<WalkOutcome> {
    let mut outcome = WalkOutcome::default();
    let roots = std::fs::read_dir(root).ok()?;
    let mut saw_program = false;

    for entry in roots.flatten() {
        let type_ = entry.file_type().ok()?;
        if !type_.is_dir() {
            // A stray file at the top level means this is not the layout we know.
            return None;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let appid = name.to_ascii_lowercase();
        if !is_appid_like(&appid) {
            // A few known siblings live beside the program directories; anything else means
            // the layout is not what this fast path assumes, so hand over to the walk.
            if matches!(
                appid.as_str(),
                "publiclib" | "temp" | "codecache" | "wxapkg"
            ) {
                continue;
            }
            return None;
        }
        saw_program = true;
        if let Some(wanted) = wanted {
            if appid != wanted {
                continue;
            }
        }

        let dir = entry.path();
        for version_entry in std::fs::read_dir(&dir).ok()?.flatten() {
            if !version_entry.file_type().ok()?.is_dir() {
                continue;
            }
            let version_dir = version_entry.path();
            for file in std::fs::read_dir(&version_dir).ok()?.flatten() {
                let path = file.path();
                let file_name = file.file_name().to_string_lossy().to_ascii_lowercase();
                if !file_name.ends_with(".wxapkg") && !file_name.ends_with(".wxapkg.invalid") {
                    continue;
                }
                let Ok(metadata) = file.metadata() else {
                    continue;
                };
                if !metadata.is_file() || metadata.len() < MIN_BUNDLE_SIZE {
                    continue;
                }
                outcome.bundles.push(PackageFile {
                    path,
                    size: metadata.len(),
                    appid: Some(appid.clone()),
                    encrypted: None,
                });
            }
        }
    }

    // An empty tree is indistinguishable from a layout we did not recognise; let the general
    // walk answer in that case rather than claiming "nothing here".
    if saw_program {
        Some(outcome)
    } else {
        None
    }
}

/// `wx` followed by exactly 16 hex digits.
fn is_appid_like(name: &str) -> bool {
    name.len() == 18 && name.starts_with("wx") && name[2..].chars().all(|c| c.is_ascii_hexdigit())
}

/// One pass of the walk; interrupted directories are collected for a retry.
fn walk_once(root: &Path, wanted: Option<&str>) -> Result<WalkOutcome> {
    let mut outcome = WalkOutcome::default();
    let root_depth = root.components().count();
    let walker = WalkDir::new(root)
        .max_depth(MAX_DEPTH)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            if name.starts_with('.') && name.len() > 1 {
                return false;
            }
            !(e.file_type().is_dir() && is_prunable_directory(e, wanted))
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                // Three distinct conditions, three distinct treatments: an interrupted read
                // is retried by the caller, a denied directory is a permission problem the
                // user can fix, and anything else is a genuinely unreadable subtree.
                let kind = err.io_error().map(std::io::Error::kind);
                if kind == Some(std::io::ErrorKind::Interrupted) {
                    if let Some(path) = err.path() {
                        outcome.interrupted.push(path.to_path_buf());
                    }
                    continue;
                }
                if kind == Some(std::io::ErrorKind::PermissionDenied) {
                    outcome.denied += 1;
                    tracing::debug!(
                        target: "wxapkg-dump::discover",
                        error = %err,
                        "a directory was refused by the system"
                    );
                    continue;
                }
                outcome.skipped += 1;
                tracing::warn!(
                    target: "wxapkg-dump::discover",
                    error = %err,
                    "skipping unreadable path; the scan result may be incomplete"
                );
                continue;
            }
        };
        if entry.depth() > root_depth + MAX_DEPTH {
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !name.ends_with(".wxapkg") && !name.ends_with(".wxapkg.invalid") {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.len() < MIN_BUNDLE_SIZE {
            continue;
        }
        let path = entry.path().to_path_buf();
        let normalized = path.to_string_lossy().replace('\\', "/");
        // `publicLib` holds the WeChat base library, not a Mini Program: it carries a
        // wxapkg-shaped payload but has no appid and cannot be opened as a project.
        if normalized.contains("/publicLib/") || normalized.contains("/publicLib\\") {
            continue;
        }
        let appid = crypto::find_wxid(&normalized).map(|s| s.to_ascii_lowercase());
        outcome.bundles.push(PackageFile {
            path,
            size: metadata.len(),
            appid,
            encrypted: None,
        });
    }
    Ok(outcome)
}

/// Locates the launcher icon WeChat itself cached for `appid`.
///
/// The client writes a 140×140 PNG per installed Mini Program into `<applet>/icon/`, next to
/// the `packages` directory it stores bundles in. Reading that file is both faster and more
/// faithful than digging an icon out of the container, which many builds do not ship.
pub fn cached_icon(appid: &str) -> Option<PathBuf> {
    cached_icon_in(&default_roots(), appid)
}

/// [`cached_icon`] over an explicit set of package roots, for tests and unusual layouts.
pub fn cached_icon_in(roots: &[PathBuf], appid: &str) -> Option<PathBuf> {
    let prefix = format!("{appid}_");
    for root in roots {
        // A root is the `packages` directory; the icon cache is its sibling.
        let Some(parent) = root.parent() else {
            continue;
        };
        let icon_dir = parent.join("icon");
        let Ok(entries) = std::fs::read_dir(&icon_dir) else {
            continue;
        };
        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) || !name.to_ascii_lowercase().ends_with(".png") {
                continue;
            }
            let path = entry.path();
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(seen, _)| modified > *seen) {
                best = Some((modified, path));
            }
        }
        if let Some((_, path)) = best {
            return Some(path);
        }
    }
    None
}

/// Finds the numeric version segment that immediately follows the appid component.
fn version_segment(path: &Path, appid: &str) -> String {
    let parts: Vec<String> = path
        .to_string_lossy()
        .replace('\\', "/")
        .split('/')
        .map(str::to_owned)
        .collect();
    for (i, seg) in parts.iter().enumerate() {
        if seg.eq_ignore_ascii_case(appid) {
            if let Some(next) = parts.get(i + 1) {
                if next.chars().all(|c| c.is_ascii_digit()) && !next.is_empty() {
                    return next.clone();
                }
            }
            break;
        }
    }
    String::new()
}

fn head_is_encrypted(path: &Path) -> Option<bool> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 6];
    file.read_exact(&mut head).ok()?;
    Some(&head == crypto::V1MMWX_MAGIC)
}

fn latest_mtime(program: &Program) -> f64 {
    program
        .packages
        .iter()
        .filter_map(|p| {
            std::fs::metadata(&p.path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
        })
        .fold(0.0_f64, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_segment_is_read_after_the_appid() {
        let p = PathBuf::from("/x/applet/packages/wx9fdab02115681f58/12/main.wxapkg");
        assert_eq!(version_segment(&p, "wx9fdab02115681f58"), "12");
        let p = PathBuf::from("/x/applet/packages/wx9fdab02115681f58/main.wxapkg");
        assert_eq!(version_segment(&p, "wx9fdab02115681f58"), "");
    }

    #[test]
    fn default_roots_are_never_empty_on_desktop_targets() {
        assert!(!default_roots().is_empty());
    }

    /// Writes a payload large enough to clear [`MIN_BUNDLE_SIZE`].
    fn write_bundle(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, vec![b'x'; MIN_BUNDLE_SIZE as usize + 8]).unwrap();
    }

    #[test]
    fn a_targeted_lookup_finds_the_windows_radium_layout() {
        let dir = tempfile::tempdir().unwrap();
        let appid = "wx513725758120fb69";
        let root = dir.path().join("xwechat");
        write_bundle(
            &root
                .join("radium")
                .join("Applet")
                .join("packages")
                .join(appid)
                .join("24")
                .join("__APP__.wxapkg"),
        );
        let found = scan_programs_matching(Some(&[root]), Some(appid), None).unwrap();
        assert_eq!(found.programs.len(), 1, "{:?}", found.programs);
        assert_eq!(found.programs[0].appid, appid);
    }

    #[test]
    fn pruned_directories_are_never_walked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A decoy inside a denylisted directory must stay invisible.
        write_bundle(&root.join("Image/decoy.wxapkg"));
        write_bundle(&root.join("msg/decoy.wxapkg"));
        // ...but an unknown, plausible directory must still be walked, or a layout we did
        // not anticipate would silently disappear.
        write_bundle(&root.join("applet/packages/wx9fdab02115681f58/12/main.wxapkg"));
        write_bundle(&root.join("odd-but-plausible/thing.wxapkg"));

        let found = scan_programs(Some(&[root.to_path_buf()]), None).unwrap();
        let appids: Vec<&str> = found.iter().map(|p| p.appid.as_str()).collect();
        assert_eq!(
            appids,
            vec!["unknown", "wx9fdab02115681f58"],
            "the denylist must prune Image/ and msg/ but nothing else"
        );
    }

    #[test]
    fn the_base_library_is_pruned_but_its_siblings_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_bundle(&root.join("applet/publicLib/1551/publicLib.wxapkg"));
        write_bundle(&root.join("applet/packages/wx9fdab02115681f58/12/main.wxapkg"));

        let found = scan_programs(Some(&[root.to_path_buf()]), None).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].appid, "wx9fdab02115681f58");
    }

    #[test]
    fn requesting_one_appid_prunes_the_sibling_package_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let wanted = "wx9fdab02115681f58";
        write_bundle(&root.join(format!("applet/packages/{wanted}/12/main.wxapkg")));
        write_bundle(&root.join("applet/packages/wx069ba97219f66d99/17/main.wxapkg"));

        let all = scan_programs(Some(&[root.to_path_buf()]), None).unwrap();
        assert_eq!(all.len(), 2, "an unfiltered scan still sees both programs");

        let only = scan_programs_matching(Some(&[root.to_path_buf()]), Some(wanted), None).unwrap();
        assert_eq!(only.programs.len(), 1);
        assert_eq!(only.programs[0].appid, wanted);
        // The uppercased form must prune identically: the walk compares case-insensitively.
        let upper = scan_programs_matching(
            Some(&[root.to_path_buf()]),
            Some(&wanted.to_uppercase()),
            None,
        )
        .unwrap();
        assert_eq!(upper.programs.len(), 1);
        assert_eq!(upper.programs[0].appid, wanted);
    }

    #[test]
    fn a_program_limit_is_counted_after_grouping() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // One program owning several packages, then two single-package programs: a
        // package-counted cap would stop inside the first program.
        for name in ["main.wxapkg", "_sub_.wxapkg", "_sub2_.wxapkg"] {
            write_bundle(&root.join(format!("applet/packages/wx9fdab02115681f58/12/{name}")));
        }
        write_bundle(&root.join("applet/packages/wx069ba97219f66d99/17/main.wxapkg"));
        write_bundle(&root.join("applet/packages/wx17ab5a15e61efc32/3/main.wxapkg"));

        let limited = scan_programs(Some(&[root.to_path_buf()]), Some(1)).unwrap();
        assert_eq!(limited.len(), 1, "limit counts programs, not packages");
        assert_eq!(limited[0].appid, "wx069ba97219f66d99");
        assert_eq!(limited[0].packages.len(), 1);

        let all = scan_programs(Some(&[root.to_path_buf()]), None).unwrap();
        assert_eq!(all.len(), 3);
        let first = all
            .iter()
            .find(|p| p.appid == "wx9fdab02115681f58")
            .unwrap();
        assert_eq!(first.packages.len(), 3, "grouping keeps every package");
    }

    #[test]
    fn the_direct_layout_is_walked_without_recursing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("packages");
        write_bundle(&root.join("wx9fdab02115681f58/12/main.wxapkg"));
        write_bundle(&root.join("wx9fdab02115681f58/12/_sub_.wxapkg"));
        write_bundle(&root.join("wx069ba97219f66d99/17/main.wxapkg"));

        let found = scan_programs_matching(Some(std::slice::from_ref(&root)), None, None).unwrap();
        assert_eq!(found.programs.len(), 2);
        let first = found
            .programs
            .iter()
            .find(|p| p.appid == "wx9fdab02115681f58")
            .unwrap();
        assert_eq!(first.packages.len(), 2, "both bundles of the program");
        assert_eq!(found.skipped_paths, 0);

        // A layout the fast path does not understand still yields results: the general walk
        // takes over instead of reporting nothing.
        let nested = dir.path().join("nested");
        write_bundle(&nested.join("applet/packages/wx1111111111111111/3/main.wxapkg"));
        let fallback = scan_programs_matching(Some(&[nested]), None, None).unwrap();
        assert_eq!(fallback.programs.len(), 1);
        assert_eq!(fallback.programs[0].appid, "wx1111111111111111");
    }

    #[test]
    fn the_wechat_icon_cache_is_found_next_to_the_packages() {
        let dir = tempfile::tempdir().unwrap();
        let packages = dir.path().join("applet/packages");
        let icons = dir.path().join("applet/icon");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::create_dir_all(&icons).unwrap();
        std::fs::write(icons.join("wx069ba97219f66d99_abc.png"), b"png").unwrap();
        std::fs::write(icons.join("wx069ba97219f66d99_def.png"), b"png").unwrap();
        std::fs::write(icons.join("wx1111111111111111_abc.png"), b"png").unwrap();

        let found = cached_icon_in(std::slice::from_ref(&packages), "wx069ba97219f66d99").unwrap();
        assert!(found.starts_with(&icons));
        assert!(found
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("wx069ba97219f66d99_"));

        // Another program's icon must not be borrowed.
        assert!(cached_icon_in(std::slice::from_ref(&packages), "wx2222222222222222").is_none());
        // No cache directory at all is not an error.
        let bare = tempfile::tempdir().unwrap();
        assert!(cached_icon_in(&[bare.path().join("packages")], "wx069ba97219f66d99").is_none());
    }

    #[test]
    fn appid_names_are_recognised_exactly() {
        assert!(is_appid_name("wx9fdab02115681f58"));
        assert!(is_appid_name("WX9FDAB02115681F58"));
        assert!(!is_appid_name("wx9fdab02115681f5"));
        assert!(!is_appid_name("_pagesFood_"));
        assert!(!is_appid_name("__APP__"));
        assert!(!is_appid_name("wx9fdab02115681f5z"));
    }

    #[test]
    fn a_clean_walk_reports_nothing_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(
            &dir.path()
                .join("applet/packages/wx9fdab02115681f58/12/main.wxapkg"),
        );
        let outcome =
            scan_programs_matching(Some(&[dir.path().to_path_buf()]), None, None).unwrap();
        assert_eq!(outcome.skipped_paths, 0);
        assert_eq!(outcome.programs.len(), 1);
    }

    /// An unreadable directory used to shrink the result silently; it must now be counted.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_is_counted_not_dropped_silently() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_bundle(&root.join("applet/packages/wx9fdab02115681f58/12/main.wxapkg"));
        let locked = root.join("locked");
        write_bundle(&locked.join("wx069ba97219f66d99/17/main.wxapkg"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        // A privileged test runner can still read it, in which case the mode proves nothing.
        let enforced = std::fs::read_dir(&locked).is_err();
        let outcome = scan_programs_matching(Some(&[root.to_path_buf()]), None, None).unwrap();

        // Restore before the temporary directory is dropped so cleanup cannot fail.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        if enforced {
            // A refused directory is a permission problem, not a transient one: it is
            // counted separately so the window can point at the system setting to change.
            assert_eq!(
                outcome.denied_paths, 1,
                "the refused directory must be reported, not silently omitted"
            );
            assert_eq!(outcome.skipped_paths, 0);
            assert_eq!(
                outcome.programs.len(),
                1,
                "the readable program still comes back"
            );
        } else {
            assert_eq!(outcome.denied_paths, 0);
            assert_eq!(outcome.skipped_paths, 0);
            assert_eq!(outcome.programs.len(), 2);
        }
    }

    /// An empty answer for an explicitly requested appid triggers exactly one re-walk.
    #[test]
    fn an_empty_filtered_result_is_retried_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_bundle(&root.join("applet/packages/wx9fdab02115681f58/12/main.wxapkg"));

        // Nothing matches: the retry runs but still reports empty, and that is fine.
        let missing = scan_programs_matching(
            Some(&[root.to_path_buf()]),
            Some("wx069ba97219f66d99"),
            None,
        )
        .unwrap();
        assert!(missing.programs.is_empty());
        assert_eq!(missing.skipped_paths, 0);

        // A matching appid must never be reported empty, retry or not.
        let present = scan_programs_matching(
            Some(&[root.to_path_buf()]),
            Some("wx9fdab02115681f58"),
            None,
        )
        .unwrap();
        assert_eq!(present.programs.len(), 1);
    }
}
