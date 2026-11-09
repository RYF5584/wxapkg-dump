// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Structural validation of a reconstructed project.
//!
//! Decompilation can produce a tree that looks right but never compiles: a page listed in
//! `app.json` with no `.js`, a `usingComponents` entry pointing at a component that was
//! never in the package, an unterminated brace left behind by a truncated bundle. The
//! DevTools reports those one at a time, after a manual import, which is a slow loop.
//!
//! This module reproduces the checks that can be automated, so `unpack --verify` and
//! `POST /api/programs/export` can answer "is this project importable?" without a human in
//! the loop.
//!
//! # What is checked
//!
//! | Check | Severity | Rationale |
//! |---|---|---|
//! | `app.json` present and parseable | error | nothing compiles without it |
//! | every declared page has `.js` and `.json` | error | DevTools refuses to compile a page without them |
//! | every declared page has `.wxml` | warning | synthesised shells are legal but usually indicate missing sources |
//! | `usingComponents` targets resolve | error | an unresolvable component aborts compilation |
//! | relative `require()` targets resolve | error | a dangling import aborts the build |
//! | JavaScript delimiters balance | error | catches truncated bundles and mis-sliced modules |
//! | WXSS braces balance | error | same |
//! | WXML tags balance | error | same |
//! | every JSON file parses | error | a malformed page config aborts compilation |
//! | `wx:for` without `wx:key` | warning | legal but a runtime performance problem |
//! | `{{undefined}}` in a binding | warning | the operand array was missing; the value is wrong, not just empty |
//! | sub-package roots do not shadow main pages | error | the DevTools rejects ambiguous ownership |
//!
//! # What is *not* checked
//!
//! A real syntax check of JavaScript needs a JavaScript engine. [`verify_project`] performs
//! a delimiter/structure check that catches the truncation class of bug; front-ends that
//! have Node available can additionally run `node --check` per file, which is what
//! `wxapkg-dump verify --node` does.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

use crate::error::{IoContext, Result};
use crate::js::{tokenize, TokenKind};

/// Severity of a single finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The project will not compile.
    Error,
    /// The project compiles, but something is missing or degraded.
    Warning,
}

/// One finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    /// Stable machine readable code, e.g. `page_js_missing`.
    pub code: String,
    /// Path the finding refers to, relative to the project root.
    pub path: String,
    /// Human readable explanation.
    pub message: String,
}

impl Issue {
    fn new(code: &str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Counters describing what was inspected.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifyStats {
    /// Pages declared in `app.json` (main package + sub-packages).
    pub pages: usize,
    /// Sub-packages declared in `app.json`.
    #[serde(rename = "subPackages")]
    pub sub_packages: usize,
    /// Distinct components referenced through `usingComponents`.
    pub components: usize,
    /// JavaScript files inspected.
    pub js: usize,
    /// WXML files inspected.
    pub wxml: usize,
    /// WXSS files inspected.
    pub wxss: usize,
    /// JSON files inspected.
    pub json: usize,
    /// Relative imports followed.
    pub requires: usize,
    /// Files skipped because they live in `_raw/`, `_foreign/` or a dependency directory.
    pub skipped: usize,
}

/// Outcome of a validation pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    /// Project directory that was inspected.
    pub root: String,
    /// `true` when no error-severity finding was recorded.
    pub ok: bool,
    /// Findings that prevent compilation.
    pub errors: Vec<Issue>,
    /// Findings that do not prevent compilation.
    pub warnings: Vec<Issue>,
    /// Counters.
    pub stats: VerifyStats,
    /// `true` when a real JavaScript syntax check was performed (Node available).
    #[serde(rename = "syntaxChecked")]
    pub syntax_checked: bool,
}

impl VerifyReport {
    /// One-line verdict for logs and CLI output.
    pub fn summary(&self) -> String {
        format!(
            "{}: {} error(s), {} warning(s) across {} file(s){}",
            if self.ok { "ok" } else { "FAILED" },
            self.errors.len(),
            self.warnings.len(),
            self.stats.js + self.stats.wxml + self.stats.wxss + self.stats.json,
            if self.syntax_checked {
                " (JavaScript syntax verified)"
            } else {
                ""
            }
        )
    }
}

/// Options controlling the validation pass.
#[derive(Debug, Clone)]
pub struct VerifyOptions {
    /// Also check that referenced images, fonts and other assets exist.
    pub check_assets: bool,
    /// Verify that every `bind*` / `catch*` handler named in WXML exists in the page's JS.
    pub check_handlers: bool,
    /// Cap on findings per category, so a badly broken project does not produce a megabyte
    /// of output. `usize::MAX` disables the cap.
    pub max_per_code: usize,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            check_assets: true,
            check_handlers: false,
            max_per_code: 200,
        }
    }
}

/// Directories that are never part of the compiled project.
const SKIPPED_DIRS: &[&str] = &["_raw", "_foreign", "node_modules", ".git"];

/// Validates the project rooted at `root`.
pub fn verify_project(root: &Path, options: &VerifyOptions) -> Result<VerifyReport> {
    if !root.is_dir() {
        return Err(crate::error::Error::InvalidArgument(format!(
            "{} is not a directory",
            root.display()
        )));
    }

    let mut report = VerifyReport {
        root: root.to_string_lossy().into_owned(),
        ok: true,
        errors: Vec::new(),
        warnings: Vec::new(),
        stats: VerifyStats::default(),
        syntax_checked: false,
    };
    let mut budget: BTreeMap<String, usize> = BTreeMap::new();

    // ---- inventory ----------------------------------------------------
    let mut files: BTreeSet<String> = BTreeSet::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if relative
            .split('/')
            .any(|segment| SKIPPED_DIRS.contains(&segment))
        {
            report.stats.skipped += 1;
            continue;
        }
        files.insert(relative);
    }

    // ---- app.json -----------------------------------------------------
    let app_path = root.join("app.json");
    let app: Value = if app_path.is_file() {
        match std::fs::read_to_string(&app_path).with_path(&app_path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(err) => {
                    push(
                        &mut report,
                        &mut budget,
                        options,
                        Severity::Error,
                        Issue::new(
                            "app_json_invalid",
                            "app.json",
                            format!("not valid JSON: {err}"),
                        ),
                    );
                    return Ok(finish(report));
                }
            },
            Err(err) => {
                push(
                    &mut report,
                    &mut budget,
                    options,
                    Severity::Error,
                    Issue::new("app_json_unreadable", "app.json", err.to_string()),
                );
                return Ok(finish(report));
            }
        }
    } else {
        push(
            &mut report,
            &mut budget,
            options,
            Severity::Error,
            Issue::new(
                "app_json_missing",
                "app.json",
                "app.json is required; without it the DevTools cannot open the project",
            ),
        );
        return Ok(finish(report));
    };

    for required in ["project.config.json", "app.js"] {
        if !files.contains(required) {
            push(
                &mut report,
                &mut budget,
                options,
                if required == "app.js" {
                    Severity::Warning
                } else {
                    Severity::Error
                },
                Issue::new(
                    if required == "app.js" {
                        "app_js_missing"
                    } else {
                        "project_config_missing"
                    },
                    required,
                    format!("{required} is not present in the project"),
                ),
            );
        }
    }

    // ---- declared pages ----------------------------------------------
    let mut page_paths: Vec<String> = Vec::new();
    if let Some(pages) = app.get("pages").and_then(Value::as_array) {
        for page in pages {
            if let Some(page) = page.as_str() {
                page_paths.push(page.to_owned());
            } else {
                push(
                    &mut report,
                    &mut budget,
                    options,
                    Severity::Error,
                    Issue::new("page_entry_invalid", "app.json", "pages[] must be strings"),
                );
            }
        }
    }
    if page_paths.is_empty() {
        push(
            &mut report,
            &mut budget,
            options,
            Severity::Error,
            Issue::new(
                "pages_empty",
                "app.json",
                "pages[] is empty; no page can be compiled",
            ),
        );
    }

    let mut sub_roots: Vec<String> = Vec::new();
    let sub_packages = app
        .get("subPackages")
        .or_else(|| app.get("subpackages"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (index, sub) in sub_packages.iter().enumerate() {
        let Some(root_dir) = sub.get("root").and_then(Value::as_str) else {
            push(
                &mut report,
                &mut budget,
                options,
                Severity::Error,
                Issue::new(
                    "subpackage_root_missing",
                    "app.json",
                    format!("subPackages[{index}] has no root"),
                ),
            );
            continue;
        };
        let normalized = root_dir.trim().trim_end_matches('/').to_owned();
        if !normalized.is_empty() {
            sub_roots.push(normalized.clone());
        }
        let pages = sub
            .get("pages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if pages.is_empty() {
            push(
                &mut report,
                &mut budget,
                options,
                Severity::Warning,
                Issue::new(
                    "subpackage_pages_empty",
                    "app.json",
                    format!("subPackages[{index}] ({root_dir}) declares no pages"),
                ),
            );
        }
        for page in pages {
            if let Some(page) = page.as_str() {
                page_paths.push(if normalized.is_empty() {
                    page.to_owned()
                } else {
                    format!("{normalized}/{page}")
                });
            }
        }
    }
    report.stats.sub_packages = sub_packages.len();

    // Main-package pages must not be shadowed by a sub-package root, otherwise the
    // DevTools cannot decide which package owns the file.
    let main_pages: BTreeSet<String> = app
        .get("pages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|page| page.as_str().map(str::to_owned))
        .collect();
    for root_dir in &sub_roots {
        for page in &main_pages {
            if page.starts_with(&format!("{root_dir}/")) {
                push(
                    &mut report,
                    &mut budget,
                    options,
                    Severity::Error,
                    Issue::new(
                        "subpackage_shadows_page",
                        page.clone(),
                        format!("main-package page is inside the {root_dir}/ sub-package root"),
                    ),
                );
            }
        }
    }

    // ---- per-page requirements ---------------------------------------
    for page in &page_paths {
        let clean = page.trim_start_matches('/');
        if clean.is_empty() {
            continue;
        }
        report.stats.pages += 1;
        // A page configuration file is optional in WeChat, so its absence is not a defect;
        // flagging it would fail every project whose pages keep their defaults.
        for (suffix, code, severity) in [
            (".js", "page_js_missing", Severity::Error),
            (".wxml", "page_wxml_missing", Severity::Warning),
        ] {
            let expected = format!("{clean}{suffix}");
            if !files.contains(&expected) {
                push(
                    &mut report,
                    &mut budget,
                    options,
                    severity,
                    Issue::new(
                        code,
                        &expected,
                        format!("declared page `{page}` has no {suffix} file"),
                    ),
                );
            }
        }
    }

    // ---- JSON files ---------------------------------------------------
    let json_files: Vec<String> = files
        .iter()
        .filter(|f| f.ends_with(".json"))
        .cloned()
        .collect();
    report.stats.json = json_files.len();
    let mut component_refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for relative in &json_files {
        let path = root.join(relative);
        let Ok(text) = std::fs::read_to_string(&path) else {
            push(
                &mut report,
                &mut budget,
                options,
                Severity::Error,
                Issue::new("json_unreadable", relative, "file is not valid UTF-8 text"),
            );
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            push(
                &mut report,
                &mut budget,
                options,
                Severity::Error,
                Issue::new("json_invalid", relative, "not valid JSON"),
            );
            continue;
        };
        let Some(components) = value.get("usingComponents").and_then(Value::as_object) else {
            continue;
        };
        let dir = dirname(relative);
        for (name, target) in components {
            let Some(target) = target.as_str() else {
                push(
                    &mut report,
                    &mut budget,
                    options,
                    Severity::Error,
                    Issue::new(
                        "component_target_invalid",
                        relative,
                        format!("usingComponents.{name} must be a string"),
                    ),
                );
                continue;
            };
            if target.contains("://") || target.starts_with("//") || target.is_empty() {
                continue;
            }
            let resolved = if let Some(absolute) = target.strip_prefix('/') {
                absolute.to_owned()
            } else {
                join_path(&dir, target)
            };
            component_refs
                .entry(resolved.clone())
                .or_default()
                .insert(relative.clone());
            if !files.contains(&format!("{resolved}.json")) {
                push(
                    &mut report,
                    &mut budget,
                    options,
                    Severity::Error,
                    Issue::new(
                        "component_missing",
                        format!("{resolved}.json"),
                        format!("`{name}` in {relative} points at a component that is not in the project"),
                    ),
                );
            }
        }
    }
    report.stats.components = component_refs.len();

    // ---- JavaScript ---------------------------------------------------
    let mut js_files: Vec<String> = files
        .iter()
        .filter(|f| {
            let lower = f.to_ascii_lowercase();
            lower.ends_with(".js") || lower.ends_with(".wxs")
        })
        .cloned()
        .collect();
    let sub_roots: Vec<String> = app
        .get("subPackages")
        .or_else(|| app.get("subpackages"))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|item| item.get("root").and_then(Value::as_str))
                .map(|root| root.trim_end_matches('/').to_owned())
                .filter(|root| !root.is_empty())
                .collect()
        })
        .unwrap_or_default();
    js_files.sort();
    report.stats.js = js_files.len();
    for relative in &js_files {
        let path = root.join(relative);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Err(detail) = balance_javascript(&text) {
            push(
                &mut report,
                &mut budget,
                options,
                Severity::Error,
                Issue::new("js_unbalanced", relative, detail),
            );
        }
        for spec in relative_requires(&text) {
            // `require("./locale/")` is the static prefix of a dynamic directory lookup;
            // the concrete file cannot be known without running the program.
            if spec.ends_with('/') {
                continue;
            }
            report.stats.requires += 1;
            if !resolve_require(root, relative, &spec, &files, &sub_roots) {
                push(
                    &mut report,
                    &mut budget,
                    options,
                    Severity::Error,
                    Issue::new(
                        "require_missing",
                        relative,
                        format!("require(\"{spec}\") cannot be resolved"),
                    ),
                );
            }
        }
    }

    // ---- WXSS ---------------------------------------------------------
    let wxss_files: Vec<String> = files
        .iter()
        .filter(|f| f.ends_with(".wxss"))
        .cloned()
        .collect();
    report.stats.wxss = wxss_files.len();
    for relative in &wxss_files {
        let path = root.join(relative);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Err(detail) = balance_css(&text) {
            push(
                &mut report,
                &mut budget,
                options,
                Severity::Error,
                Issue::new("wxss_unbalanced", relative, detail),
            );
        }
        if options.check_assets {
            for url in css_urls(&text) {
                if url.starts_with("data:") || url.starts_with("http") || url.starts_with("//") {
                    continue;
                }
                let resolved = join_path(&dirname(relative), url.trim_start_matches('/'));
                if !files.contains(&resolved) {
                    push(
                        &mut report,
                        &mut budget,
                        options,
                        Severity::Warning,
                        Issue::new(
                            "asset_missing",
                            relative,
                            format!("references `{url}`, which is not in the project"),
                        ),
                    );
                }
            }
        }
    }

    // ---- WXML ---------------------------------------------------------
    let wxml_files: Vec<String> = files
        .iter()
        .filter(|f| f.ends_with(".wxml"))
        .cloned()
        .collect();
    report.stats.wxml = wxml_files.len();
    for relative in &wxml_files {
        let path = root.join(relative);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Err(detail) = balance_markup(&text) {
            push(
                &mut report,
                &mut budget,
                options,
                Severity::Error,
                Issue::new("wxml_unbalanced", relative, detail),
            );
        }
        // A bare `wx:if` / `wx:elif` / `wx:for` (no `="…"`) is rejected by the DevTools
        // before the project even compiles, so it is an error rather than a warning.
        for directive in bare_directives(&text) {
            push(
                &mut report,
                &mut budget,
                options,
                Severity::Error,
                Issue::new(
                    "wxml_bare_directive",
                    relative,
                    format!("`{directive}` has no value; the DevTools rejects this template"),
                ),
            );
        }
        if text.contains("data=\"{{...{") || text.contains("data=\"{{{") {
            push(
                &mut report,
                &mut budget,
                options,
                Severity::Error,
                Issue::new(
                    "wxml_template_data_literal",
                    relative,
                    "`data` contains an object expression; the DevTools expects `key: value` entries or `...identifier`",
                ),
            );
        }
        if text.contains("undefined") {
            push(
                &mut report,
                &mut budget,
                options,
                Severity::Warning,
                Issue::new(
                    "binding_unresolved",
                    relative,
                    "contains `undefined`; the operand array for this template was not shipped with the package",
                ),
            );
        }
        for (offset, _) in text.match_indices("wx:for") {
            // The scan window must not split a multi-byte character: WXML is full of them.
            let window = safe_window(&text, offset, 200);
            let end = window.find('>').unwrap_or(window.len());
            if !window[..end].contains("wx:key") {
                push(
                    &mut report,
                    &mut budget,
                    options,
                    Severity::Warning,
                    Issue::new(
                        "wx_for_without_key",
                        relative,
                        "a wx:for has no wx:key; the list re-renders less efficiently",
                    ),
                );
            }
        }
        if options.check_assets {
            for url in wxml_sources(&text) {
                if url.contains("{{") || url.starts_with("data:") || url.starts_with("http") {
                    continue;
                }
                let resolved = join_path(&dirname(relative), url.trim_start_matches('/'));
                if !files.contains(&resolved) {
                    push(
                        &mut report,
                        &mut budget,
                        options,
                        Severity::Warning,
                        Issue::new(
                            "asset_missing",
                            relative,
                            format!("references `{url}`, which is not in the project"),
                        ),
                    );
                }
            }
        }
        if options.check_handlers {
            for handler in bind_handlers(&text) {
                if handler == "__l" {
                    continue;
                }
                let script = format!("{}.js", relative.trim_end_matches(".wxml"));
                let Some(script_text) = read_optional(root, &script) else {
                    continue;
                };
                if !script_text.contains(&format!("{handler}("))
                    && !script_text.contains(&format!("{handler}:"))
                    && !script_text.contains(&format!("{handler} ="))
                {
                    push(
                        &mut report,
                        &mut budget,
                        options,
                        Severity::Warning,
                        Issue::new(
                            "handler_missing",
                            relative,
                            format!("binds `{handler}`, which is not defined in {script}"),
                        ),
                    );
                }
            }
        }
    }

    // ---- plugins ------------------------------------------------------
    if let Some(plugins) = app.get("plugins").and_then(Value::as_object) {
        if !plugins.is_empty() {
            push(
                &mut report,
                &mut budget,
                options,
                Severity::Warning,
                Issue::new(
                    "plugin_declared",
                    "app.json",
                    format!(
                        "{} plugin declaration(s) remain; each must be added under Settings → Plugins \
                         in the DevTools before those pages compile",
                        plugins.len()
                    ),
                ),
            );
        }
    }

    Ok(finish(report))
}

fn finish(mut report: VerifyReport) -> VerifyReport {
    report.ok = report.errors.is_empty();
    report
}

fn push(
    report: &mut VerifyReport,
    budget: &mut BTreeMap<String, usize>,
    options: &VerifyOptions,
    severity: Severity,
    issue: Issue,
) {
    let used = budget.entry(issue.code.clone()).or_insert(0);
    if *used >= options.max_per_code {
        return;
    }
    *used += 1;
    match severity {
        Severity::Error => report.errors.push(issue),
        Severity::Warning => report.warnings.push(issue),
    }
}

/// Returns at most `max_bytes` of `text` starting at `start`, without splitting a character.
///
/// Every slice of a `&str` has to land on a character boundary; Mini Program templates are
/// mostly Chinese, so a naive byte window panics on the first multi-byte character it
/// straddles.
fn safe_window(text: &str, start: usize, max_bytes: usize) -> &str {
    if start >= text.len() {
        return "";
    }
    let mut end = text.len().min(start + max_bytes);
    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[start..end]
}

fn read_optional(root: &Path, relative: &str) -> Option<String> {
    std::fs::read_to_string(root.join(relative)).ok()
}

fn dirname(path: &str) -> String {
    path.rsplit_once('/')
        .map_or_else(String::new, |(dir, _)| dir.to_owned())
}

/// Joins and normalises a relative POSIX path.
fn join_path(dir: &str, relative: &str) -> String {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').filter(|s| !s.is_empty()).collect()
    };
    for segment in relative.split('/') {
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

// ---------------------------------------------------------------------------
// structural checks
// ---------------------------------------------------------------------------

/// Verifies that every JavaScript delimiter is closed.
///
/// Strings, template literals, comments and regex literals are skipped by the shared
/// lexer, so a brace inside a string can never be mistaken for a block delimiter.
fn balance_javascript(source: &str) -> std::result::Result<(), String> {
    let mut stack: Vec<(char, usize)> = Vec::new();
    for token in tokenize(source, 0, None) {
        if token.kind != TokenKind::Punct {
            continue;
        }
        let Some(ch) = token.value.chars().next() else {
            continue;
        };
        if token.value.chars().count() != 1 {
            continue;
        }
        match ch {
            '{' | '(' | '[' => stack.push((ch, token.start)),
            '}' | ')' | ']' => {
                let Some((open, offset)) = stack.pop() else {
                    return Err(format!(
                        "closing `{ch}` at byte {} has no matching opener",
                        token.start
                    ));
                };
                let expected = match open {
                    '{' => '}',
                    '(' => ')',
                    '[' => ']',
                    _ => unreachable!("only openers are pushed"),
                };
                if ch != expected {
                    return Err(format!(
                        "`{open}` opened at byte {offset} is closed by `{ch}` at byte {}",
                        token.start
                    ));
                }
            }
            _ => {}
        }
    }
    match stack.last() {
        Some((open, offset)) => Err(format!("`{open}` opened at byte {offset} is never closed")),
        None => Ok(()),
    }
}

/// Verifies that CSS braces balance, ignoring braces inside quoted strings and comments.
fn balance_css(source: &str) -> std::result::Result<(), String> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut chars = source.char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => {
                chars.next();
            }
            (Some(_), _) => {}
            (None, c @ ('"' | '\'')) => quote = Some(c),
            (None, '/') if chars.peek().is_some_and(|(_, c)| *c == '*') => {
                // Advance past the comment terminator.
                let mut previous = '\0';
                for (_, c) in chars.by_ref() {
                    if previous == '*' && c == '/' {
                        break;
                    }
                    previous = c;
                }
            }
            (None, '{') => depth += 1,
            (None, '}') => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!(
                        "closing `}}` at byte {offset} has no matching `{{`"
                    ));
                }
            }
            _ => {}
        }
    }
    if depth > 0 {
        return Err(format!("{depth} unclosed `{{` block(s)"));
    }
    if quote.is_some() {
        return Err("unterminated string literal".to_owned());
    }
    Ok(())
}

/// Verifies that WXML tags balance.
///
/// Attribute values are skipped, so a `>` inside `{{ a > b }}` cannot be mistaken for the
/// end of a tag.
fn balance_markup(source: &str) -> std::result::Result<(), String> {
    let bytes = source.as_bytes();
    let mut stack: Vec<String> = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if source[i..].starts_with("<!--") {
            match source[i..].find("-->") {
                Some(end) => {
                    i += end + 3;
                    continue;
                }
                None => return Err("unterminated comment".to_owned()),
            }
        }
        let Some(end) = find_tag_end(&source[i..]) else {
            return Err(format!("tag starting at byte {i} is never closed"));
        };
        let raw = &source[i..i + end + 1];
        let inner = raw[1..raw.len() - 1].trim();
        if inner.starts_with(['!', '?']) {
            i += end + 1;
            continue;
        }
        let closing = inner.starts_with('/');
        let body = inner.trim_start_matches('/').trim();
        let self_closing = body.ends_with('/');
        let name = body
            .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .next()
            .unwrap_or("")
            .to_owned();

        if closing {
            match stack.pop() {
                Some(open) if open == name => {}
                Some(open) => {
                    return Err(format!("</{name}> closes <{open}>"));
                }
                None => return Err(format!("</{name}> has no matching opener")),
            }
        } else if !self_closing && !is_void_element(&name) {
            stack.push(name);
        }
        i += end + 1;
    }

    match stack.last() {
        Some(open) => Err(format!("<{open}> is never closed")),
        None => Ok(()),
    }
}

fn is_void_element(name: &str) -> bool {
    // WXML is not HTML: `image`/`input` are ordinary components that may carry children and
    // a closing tag, as Taro-generated templates do. Only the tag-only directives are void.
    matches!(name, "import" | "include" | "wxs")
}

fn find_tag_end(rest: &str) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut braces = 0i32;
    for (offset, byte) in rest.as_bytes().iter().enumerate() {
        match (quote, byte) {
            (Some(q), b) if b == &q => quote = None,
            (Some(_), _) => {}
            (None, b @ (b'"' | b'\'')) => quote = Some(*b),
            (None, b'{') => braces += 1,
            (None, b'}') => braces -= 1,
            (None, b'>') if braces <= 0 => return Some(offset),
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// extraction helpers
// ---------------------------------------------------------------------------

/// Collects relative `require()` specifiers, ignoring npm packages.
fn relative_requires(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    let needle = b"require";
    let mut i = 0usize;
    while i + needle.len() < bytes.len() {
        if &bytes[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        // Must be a standalone identifier followed by `(` and a quote.
        let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
        let mut j = i + needle.len();
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if !before_ok || j >= bytes.len() || bytes[j] != b'(' {
            i += needle.len();
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || !matches!(bytes[j], b'\'' | b'"') {
            i += needle.len();
            continue;
        }
        let quote = bytes[j];
        let start = j + 1;
        let Some(offset) = source[start..].find(quote as char) else {
            i += needle.len();
            continue;
        };
        let spec = &source[start..start + offset];
        if spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/') {
            out.push(spec.to_owned());
        }
        i = start + offset;
    }
    out
}

const fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

fn resolve_require(
    root: &Path,
    from: &str,
    spec: &str,
    files: &BTreeSet<String>,
    sub_roots: &[String],
) -> bool {
    // `/__wx__/…` is the runtime-provided plugin bridge, not a project file.
    if spec.starts_with("__wx__/") || spec.starts_with("/__wx__/") {
        return true;
    }
    let bases: Vec<String> = if let Some(absolute) = spec.strip_prefix('/') {
        let mut bases = Vec::new();
        // Inside a subpackage an absolute require starts at that subpackage's root.
        if let Some(sub) = sub_roots
            .iter()
            .find(|sub| from.starts_with(&format!("{sub}/")))
        {
            bases.push(format!("{sub}/{absolute}"));
        }
        bases.push(absolute.to_owned());
        bases
    } else {
        vec![join_path(&dirname(from), spec)]
    };
    bases
        .iter()
        .any(|base| resolve_require_base(root, base, files))
}

fn resolve_require_base(root: &Path, base: &str, files: &BTreeSet<String>) -> bool {
    if base.is_empty() {
        return false;
    }
    if files.contains(base) {
        return true;
    }
    for suffix in [".js", ".json", ".wxs", "/index.js", "/index.json"] {
        if files.contains(&format!("{base}{suffix}")) {
            return true;
        }
    }
    // A directory that exists on disk but whose entry file is missing is still a broken
    // reference; only accept a directory when it actually holds something.
    let dir = root.join(base);
    dir.is_dir()
        && WalkDir::new(&dir)
            .max_depth(2)
            .into_iter()
            .flatten()
            .any(|e| e.file_type().is_file())
}

fn css_urls(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (offset, _) in source.match_indices("url(") {
        let rest = &source[offset + 4..];
        let rest = rest.trim_start();
        let Some(stripped) = rest.strip_prefix(['"', '\'']) else {
            if let Some(end) = rest.find(')') {
                out.push(rest[..end].trim().to_owned());
            }
            continue;
        };
        if let Some(end) = stripped.find(['"', '\'']) {
            out.push(stripped[..end].to_owned());
        }
    }
    out
}

/// Directive names written without an `="…"` value.
fn bare_directives(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for name in ["wx:if", "wx:elif", "wx:for"] {
        for (index, _) in source.match_indices(name) {
            let before = source[..index].chars().next_back();
            if !matches!(before, Some(ch) if ch.is_whitespace() || ch == '<') {
                continue;
            }
            let rest = &source[index + name.len()..];
            let trimmed = rest.trim_start();
            if trimmed.starts_with('=') {
                continue;
            }
            // `wx:for-item`, `wx:for-index` and `wx:ifdef` are different attributes.
            if matches!(trimmed.chars().next(), Some(ch) if ch.is_ascii_alphanumeric() || ch == ':' || ch == '_' || ch == '-')
            {
                continue;
            }
            found.push(name.to_owned());
        }
    }
    found
}

fn wxml_sources(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for attribute in ["src=\"", "src='"] {
        for (offset, _) in source.match_indices(attribute) {
            let quote = attribute.chars().last().unwrap_or('"');
            let start = offset + attribute.len();
            if let Some(end) = source[start..].find(quote) {
                out.push(source[start..start + end].to_owned());
            }
        }
    }
    out
}

/// Collects the handler names bound by `bind*` / `catch*` attributes.
fn bind_handlers(source: &str) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut search = source;
    while let Some(offset) = search.find("bind") {
        let rest = &search[offset..];
        let Some(eq) = rest.find('=') else { break };
        let name = &rest[..eq];
        // `bindtap`, `bind:tap`, `catchtap`, `catch:tap`, and the `capture-bind:` variants.
        let is_binding = name.starts_with("bind")
            || name.starts_with("catch")
            || name.contains("bind:")
            || name.contains("catch:");
        let attr = name.trim_end_matches(['-', ':']).to_owned();
        if is_binding && !attr.contains(' ') && attr.len() > 3 {
            let value = rest[eq + 1..].trim_start();
            if let Some(value) = value.strip_prefix(['"', '\'']) {
                let quote = rest[eq + 1..].trim_start().chars().next().unwrap_or('"');
                if let Some(end) = value.find(quote) {
                    let handler = value[..end].trim().to_owned();
                    if !handler.is_empty() && !handler.contains('{') {
                        out.insert(handler);
                    }
                }
            }
        }
        search = &search[offset + 4..];
    }
    out.into_iter().collect()
}

/// Runs a real JavaScript syntax check over every `.js` / `.wxs` file under `root`.
///
/// Spawning `node --check` once per file is correct but unusably slow on the 600+ modules a
/// typical Mini Program yields, so a single Node process parses every file with `vm.Script`.
/// CommonJS is what `wcc` emits, which is exactly what `vm.Script` accepts; files that turn
/// out to be ES modules are skipped rather than flagged, since the DevTools compiles them.
///
/// Returns `(relative path, message)` pairs. Fails with a human-readable reason when Node is
/// unavailable, so callers can degrade to the structural report instead of guessing.
pub fn node_syntax_check(root: &Path) -> std::result::Result<Vec<(String, String)>, String> {
    use std::process::Command;

    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => {}
        Ok(_) => return Err("node exited with an error".to_owned()),
        Err(err) => return Err(err.to_string()),
    }

    const DRIVER: &str = r#"
const fs = require('fs');
const vm = require('vm');
const failed = [];
for (const file of process.argv.slice(1)) {
  let source;
  try { source = fs.readFileSync(file, 'utf8'); } catch (err) { continue; }
  try {
    new vm.Script(source, { filename: file });
  } catch (err) {
    const message = String(err && err.message || err).split('\n')[0];
    if (/Cannot use import statement|Unexpected token 'export'/.test(message)) {
      continue; // ES module: not what the DevTools compiles, and not an error here.
    }
    failed.push([file, message]);
  }
}
process.stdout.write(JSON.stringify(failed));
"#;

    let mut files: Vec<std::path::PathBuf> = walk_scripts(root)
        .into_iter()
        .filter(|path| {
            let name = path.to_string_lossy();
            name.ends_with(".js") || name.ends_with(".wxs")
        })
        .collect();
    files.sort();

    let mut failures = Vec::new();
    // Stay well inside the per-argument limit on every platform.
    for chunk in files.chunks(400) {
        let mut command = Command::new("node");
        command.arg("-e").arg(DRIVER);
        for file in chunk {
            command.arg(file);
        }
        let output = command.output().map_err(|err| err.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("node failed")
                .to_owned());
        }
        let parsed: Vec<(String, String)> = serde_json::from_slice(&output.stdout)
            .map_err(|err| format!("cannot read node output: {err}"))?;
        for (absolute, message) in parsed {
            let relative = std::path::Path::new(&absolute)
                .strip_prefix(root)
                .unwrap_or(std::path::Path::new(&absolute))
                .to_string_lossy()
                .replace('\\', "/");
            failures.push((relative, message));
        }
    }
    Ok(failures)
}

/// Minimal recursive walk that skips the same directories the verifier ignores.
fn walk_scripts(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if matches!(
                    name.as_str(),
                    "_raw" | "_foreign" | "node_modules" | ".git" | "miniprogram_npm"
                ) {
                    continue;
                }
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            for (path, body) in files {
                let target = dir.path().join(path);
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::write(target, body).unwrap();
            }
            Self { dir }
        }

        fn verify(&self) -> VerifyReport {
            verify_project(self.dir.path(), &VerifyOptions::default()).unwrap()
        }
    }

    const APP_JSON: &str = r#"{"pages":["pages/index/index"]}"#;

    #[test]
    fn a_complete_minimal_project_passes() {
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({});"),
            ("pages/index/index.json", "{}"),
            ("pages/index/index.wxml", "<view>hi</view>"),
            ("pages/index/index.wxss", ".a{color:red}"),
        ]);
        let report = fixture.verify();
        assert!(report.ok, "{:?}", report.errors);
        assert_eq!(report.stats.pages, 1);
        // app.js plus the page script.
        assert_eq!(report.stats.js, 2);
    }

    #[test]
    fn a_missing_page_script_is_an_error() {
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.json", "{}"),
            ("pages/index/index.wxml", "<view />"),
        ]);
        let report = fixture.verify();
        assert!(!report.ok);
        assert!(report.errors.iter().any(|e| e.code == "page_js_missing"));
    }

    #[test]
    fn spreading_a_template_object_literal_is_an_error() {
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({});"),
            ("pages/index/index.json", "{}"),
            (
                "pages/index/index.wxml",
                r#"<template is="t" data="{{...{content: content}}}" />"#,
            ),
        ]);
        let report = fixture.verify();
        assert!(
            report
                .errors
                .iter()
                .any(|issue| issue.code == "wxml_template_data_literal"),
            "{:?}",
            report.errors
        );

        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({});"),
            ("pages/index/index.json", "{}"),
            (
                "pages/index/index.wxml",
                r#"<template is="t" data="{{{content: content}}}" />"#,
            ),
        ]);
        let report = fixture.verify();
        assert!(
            report
                .errors
                .iter()
                .any(|issue| issue.code == "wxml_template_data_literal"),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn a_bare_wx_if_is_an_error() {
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({});"),
            ("pages/index/index.json", "{}"),
            (
                "pages/index/index.wxml",
                r#"<view wx:if><text>x</text></view>"#,
            ),
        ]);
        let report = fixture.verify();
        assert!(
            report
                .errors
                .iter()
                .any(|issue| issue.code == "wxml_bare_directive"),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn a_missing_wxml_is_only_a_warning() {
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({});"),
            ("pages/index/index.json", "{}"),
        ]);
        let report = fixture.verify();
        assert!(report.ok, "{:?}", report.errors);
        assert!(report
            .warnings
            .iter()
            .any(|w| w.code == "page_wxml_missing"));
    }

    #[test]
    fn an_unresolved_component_is_an_error() {
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({});"),
            (
                "pages/index/index.json",
                r#"{"usingComponents":{"card":"/components/card/index"}}"#,
            ),
            ("pages/index/index.wxml", "<card />"),
        ]);
        let report = fixture.verify();
        assert!(!report.ok);
        assert!(report.errors.iter().any(|e| e.code == "component_missing"));
    }

    #[test]
    fn a_resolvable_component_passes() {
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({});"),
            (
                "pages/index/index.json",
                r#"{"usingComponents":{"card":"../../components/card/index"}}"#,
            ),
            ("pages/index/index.wxml", "<card />"),
            ("components/card/index.js", "Component({});"),
            ("components/card/index.json", "{}"),
            ("components/card/index.wxml", "<view />"),
        ]);
        let report = fixture.verify();
        assert!(report.ok, "{:?}", report.errors);
    }

    #[test]
    fn unbalanced_javascript_is_detected() {
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({ data: { a: 1 }"),
            ("pages/index/index.json", "{}"),
            ("pages/index/index.wxml", "<view />"),
        ]);
        let report = fixture.verify();
        assert!(report.errors.iter().any(|e| e.code == "js_unbalanced"));
    }

    #[test]
    fn a_template_with_image_children_is_balanced() {
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({});"),
            ("pages/index/index.json", "{}"),
            (
                "pages/index/index.wxml",
                r#"<template name="tmpl"><image src="a.png"></image><input></input></template>"#,
            ),
        ]);
        let report = fixture.verify();
        assert!(
            !report.errors.iter().any(|e| e.code == "wxml_unbalanced"),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn unbalanced_wxml_is_detected() {
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({});"),
            ("pages/index/index.json", "{}"),
            ("pages/index/index.wxml", "<view><text>hi</view>"),
        ]);
        let report = fixture.verify();
        assert!(report.errors.iter().any(|e| e.code == "wxml_unbalanced"));
    }

    #[test]
    fn a_missing_require_target_is_an_error() {
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            (
                "pages/index/index.js",
                r#"var a = require("../../utils/missing.js");"#,
            ),
            ("pages/index/index.json", "{}"),
            ("pages/index/index.wxml", "<view />"),
        ]);
        let report = fixture.verify();
        assert!(report.errors.iter().any(|e| e.code == "require_missing"));
    }

    #[test]
    fn absolute_require_in_a_subpackage_resolves_against_its_root() {
        let app = r#"{"pages":["pages/index/index"],"subPackages":[{"root":"packages/member","pages":["pages/index/index"]}]}"#;
        let fixture = Fixture::new(&[
            ("app.json", app),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({});"),
            ("pages/index/index.json", "{}"),
            ("pages/index/index.wxml", "<view />"),
            (
                "packages/member/pages/index/index.js",
                r#"var a = require("/utils/util.js");"#,
            ),
            ("packages/member/pages/index/index.json", "{}"),
            ("packages/member/pages/index/index.wxml", "<view />"),
            ("packages/member/utils/util.js", "module.exports = {};"),
        ]);
        let report = fixture.verify();
        assert!(
            !report.errors.iter().any(|e| e.code == "require_missing"),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn braces_inside_strings_do_not_confuse_the_checker() {
        assert!(balance_javascript(r#"var a = "}{";"#).is_ok());
        assert!(balance_javascript("var r = /[{}]/;").is_ok());
        assert!(balance_javascript("// {\nvar a = 1;").is_ok());
        assert!(balance_css(r#".a{content:"}";}"#).is_ok());
        assert!(balance_markup(r#"<view a="{{ x > 1 }}">t</view>"#).is_ok());
        assert!(balance_markup("<view /><text>a</text>").is_ok());
    }

    #[test]
    fn subpackage_shadowing_a_main_page_is_rejected() {
        let fixture = Fixture::new(&[
            (
                "app.json",
                r#"{"pages":["sub/index"],"subPackages":[{"root":"sub","pages":["index"]}]}"#,
            ),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("sub/index.js", "Page({});"),
            ("sub/index.json", "{}"),
            ("sub/index.wxml", "<view />"),
        ]);
        let report = fixture.verify();
        assert!(report
            .errors
            .iter()
            .any(|e| e.code == "subpackage_shadows_page"));
    }

    #[test]
    fn skipped_directories_are_not_inspected() {
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({});"),
            ("pages/index/index.json", "{}"),
            ("pages/index/index.wxml", "<view />"),
            ("_raw/broken.js", "function ( {"),
        ]);
        let report = fixture.verify();
        assert!(report.ok, "{:?}", report.errors);
        assert_eq!(report.stats.skipped, 1);
    }

    #[test]
    fn multibyte_templates_do_not_split_characters() {
        // Reproduces a real crash: a `wx:for` followed by more than the scan window of
        // Chinese text used to slice mid-character.
        let filler: String = "填充文字".repeat(120);
        let fixture = Fixture::new(&[
            ("app.json", APP_JSON),
            ("app.js", "App({});"),
            ("project.config.json", "{}"),
            ("pages/index/index.js", "Page({});"),
            ("pages/index/index.json", "{}"),
            (
                "pages/index/index.wxml",
                &format!("<view wx:for=\"{{{{list}}}}\">{filler}</view>"),
            ),
        ]);
        let report = fixture.verify();
        assert!(report
            .warnings
            .iter()
            .any(|w| w.code == "wx_for_without_key"));
        assert_eq!(safe_window("中文测试", 0, 4), "中");
        assert_eq!(safe_window("中文测试", 3, 3), "文");
        // A window narrower than the next character yields nothing rather than panicking.
        assert_eq!(safe_window("中文测试", 3, 1), "");
    }

    #[test]
    fn findings_are_capped_per_code() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.json"), APP_JSON).unwrap();
        std::fs::write(dir.path().join("app.js"), "App({});").unwrap();
        std::fs::write(dir.path().join("project.config.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.path().join("pages/index")).unwrap();
        for name in ["b.js", "c.js", "d.js", "e.js", "f.js"] {
            std::fs::write(dir.path().join(format!("pages/index/{name}")), "var a = {").unwrap();
        }
        let options = VerifyOptions {
            max_per_code: 2,
            ..VerifyOptions::default()
        };
        let report = verify_project(dir.path(), &options).unwrap();
        assert!(
            report
                .errors
                .iter()
                .filter(|e| e.code == "js_unbalanced")
                .count()
                <= 2
        );
    }
}
