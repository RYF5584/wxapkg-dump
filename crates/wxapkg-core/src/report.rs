// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Structured results returned by the unpacking pipeline.
//!
//! Every entry point in [`crate::project`] and [`crate::pipeline`] produces one of
//! these reports, and every surface (CLI, HTTP, GUI, MCP) renders the same shape. That
//! keeps machine-readable output stable regardless of how the toolkit was invoked.

use serde::{Deserialize, Serialize};

/// Role a package plays inside a Mini Program project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PartKind {
    /// Main package: carries `app-config.json` and the entry page.
    Main,
    /// A sub-package declared under `subPackages[].root`.
    Subpackage,
    /// A plugin package (`plugins_wx<appid>` / `plugins/wx<appid>`).
    Plugin,
    /// A package that actually belongs to a different Mini Program.
    Foreign,
    /// Role could not be determined.
    Unknown,
}

impl PartKind {
    /// Stable lowercase identifier used in machine-readable output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Subpackage => "subpackage",
            Self::Plugin => "plugin",
            Self::Foreign => "foreign",
            Self::Unknown => "unknown",
        }
    }
}

/// How one container was classified and where its files landed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartReport {
    /// File name of the container.
    pub file: String,
    /// Detected role.
    pub kind: PartKind,
    /// Destination root inside the output tree (`/` for the main package).
    pub root: String,
    /// Container format version.
    pub version: u32,
    /// Number of files in the container.
    pub files: usize,
}

/// Per-category output counters.
///
/// Every field counts **files actually written to disk**, derived from the same set that
/// [`BuildReport::files`] lists, so their sum equals the number of entries in the output tree
/// (excluding the report itself). `beautified` is the exception: it counts files whose content
/// was reformatted, which a path cannot tell you.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Counts {
    /// Binary assets and any file copied verbatim (images, fonts, `.wxs`, ...).
    pub assets: usize,
    /// Reconstructed JavaScript modules.
    pub js: usize,
    /// Reconstructed WXML templates.
    pub wxml: usize,
    /// Reconstructed WXSS style sheets.
    pub wxss: usize,
    /// Reconstructed page / component JSON configuration files.
    pub json: usize,
    /// Original compiled bundles copied to `_raw/`.
    pub raw: usize,
    /// HTML files found inside the package (H5 pages of `web-view`).
    pub h5: usize,
    /// Files from packages belonging to another Mini Program.
    pub foreign: usize,
    /// Files beautified after reconstruction.
    pub beautified: usize,
}

/// Full report of a project build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildReport {
    /// Absolute output directory.
    #[serde(rename = "outDir")]
    pub out_dir: String,
    /// One entry per processed container.
    pub parts: Vec<PartReport>,
    /// Output counters.
    pub counts: Counts,
    /// Non-fatal problems that a human should review.
    pub warnings: Vec<String>,
    /// Informational notes about automatic repairs the builder applied.
    pub notes: Vec<String>,
    /// Relative paths of everything written, sorted.
    pub files: Vec<String>,
}

impl BuildReport {
    /// Creates an empty report for `out_dir`.
    pub fn new(out_dir: impl Into<String>) -> Self {
        Self {
            out_dir: out_dir.into(),
            parts: Vec::new(),
            counts: Counts::default(),
            warnings: Vec::new(),
            notes: Vec::new(),
            files: Vec::new(),
        }
    }

    /// Total number of files written.
    pub fn total_files(&self) -> usize {
        self.counts.assets
            + self.counts.js
            + self.counts.wxml
            + self.counts.wxss
            + self.counts.json
            + self.counts.raw
            + self.counts.h5
            + self.counts.foreign
    }
}

/// Summary of one installed Mini Program on this machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramSummary {
    /// Appid or `unknown`.
    pub appid: String,
    /// Version directory name.
    pub version: String,
    /// Directory holding the bundles.
    pub location: String,
    /// Number of bundles.
    pub packages: usize,
    /// Total size in bytes.
    #[serde(rename = "totalSize")]
    pub total_size: u64,
    /// Decryption key (equal to the appid on PC/macOS).
    pub wxid: String,
    /// Whether any bundle is `V1MMWX` encrypted.
    pub encrypted: bool,
    /// Unix timestamp of the newest bundle on disk.
    ///
    /// This is the closest thing to "when was this program last downloaded": WeChat writes
    /// the containers when it fetches an update, so the newest file is the newest fetch.
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<i64>,
}

/// Result of extracting files verbatim from a container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawExtractReport {
    /// Output directory.
    #[serde(rename = "outDir")]
    pub out_dir: String,
    /// Number of files written.
    pub files: usize,
    /// Container-level notes (decryption, length mismatches, ...).
    pub notes: Vec<String>,
}

/// Result of a beautification pass over an existing project tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeautifyReport {
    /// Root directory that was formatted.
    pub root: String,
    /// Files rewritten.
    pub formatted: Vec<String>,
    /// Files skipped (unsupported extension or already formatted).
    pub skipped: usize,
    /// Files that failed to parse and were left untouched.
    pub failed: Vec<String>,
}
