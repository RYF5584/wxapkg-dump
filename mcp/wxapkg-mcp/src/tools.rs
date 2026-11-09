// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Tool descriptors and their implementations.
//!
//! Each tool is a thin adapter over [`wxapkg_core::pipeline`] or [`wxapkg_remote`]; the
//! payloads are the same ones the CLI and the HTTP API produce, serialised as pretty JSON
//! so an assistant can read them directly.

use serde_json::{json, Value};

use wxapkg_core::beautify::FormatOptions;
use wxapkg_core::pipeline::{
    beautify_tree, inspect, raw_extract, scan, unpack, Source, UnpackRequest,
};
use wxapkg_core::project::{BuildOptions, PluginsMode};

/// Returns the MCP tool descriptors.
pub fn descriptors() -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "wxapkg_scan_local_programs",
            "description": "List the WeChat Mini Programs cached by the WeChat client on this \
                            machine. Reports appid, version directory, bundle count, total size \
                            and whether the bundles are V1MMWX encrypted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appid": { "type": "string", "description": "Only report this appid (wx + 16 hex digits)." },
                    "limit": { "type": "integer", "description": "Stop after this many results." },
                    "roots": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Extra directories to search; the platform defaults are always included."
                    }
                }
            }
        }),
        json!({
            "name": "wxapkg_inspect_package",
            "description": "Print the structure of one or more .wxapkg containers without \
                            decompiling: format version, index/body sizes, file count and the \
                            pages, sub-packages and plugins declared in app-config.json. Use this \
                            when only structural information is needed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Container files, ZIP archives or directories."
                    },
                    "wxid": { "type": "string", "description": "Decryption key for V1MMWX containers; discovered from the path when omitted." }
                },
                "required": ["paths"]
            }
        }),
        json!({
            "name": "wxapkg_unpack_package",
            "description": "Reconstruct a WeChat DevTools-openable project from one or more \
                            containers. Recovers JavaScript modules, WXML templates, WXSS style \
                            sheets and page configuration, rebuilds app.json, and repairs \
                            references to pages, sub-packages and plugins that have no source in \
                            the package.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Containers, archives or directories. Omit when using appid."
                    },
                    "appid": { "type": "string", "description": "Locate the package in the local WeChat cache instead of passing paths." },
                    "version": { "type": "string", "description": "Version directory to use together with appid." },
                    "out": { "type": "string", "description": "Output directory." },
                    "wxid": { "type": "string", "description": "Decryption key for V1MMWX containers." },
                    "beautify": { "type": "boolean", "description": "Format every reconstructed source file." },
                    "keep_raw": { "type": "boolean", "description": "Keep the original compiled bundles under _raw/. Defaults to true." },
                    "auto_fix": { "type": "boolean", "description": "Apply the automatic repair passes. Defaults to true." }
                },
                "required": ["out"]
            }
        }),
        json!({
            "name": "wxapkg_raw_dump",
            "description": "Extract every file inside a container verbatim, without decompiling. \
                            Useful for pulling out assets, or for inspecting a package whose \
                            format is not yet supported.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "out": { "type": "string" },
                    "wxid": { "type": "string" },
                    "autoInflate": { "type": "boolean", "description": "Try to zlib-inflate entries that look compressed." }
                },
                "required": ["paths", "out"]
            }
        }),
        json!({
            "name": "wxapkg_beautify_project",
            "description": "Reformat the sources of an already reconstructed project in place: \
                            JavaScript, WXML, WXSS and JSON. The token stream is preserved, so \
                            nothing is added or dropped.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dir": { "type": "string", "description": "Project directory." },
                    "indentWidth": { "type": "integer", "description": "Spaces per level. Defaults to 2." },
                    "useTabs": { "type": "boolean" },
                    "maxWidth": { "type": "integer", "description": "Preferred maximum line width." }
                },
                "required": ["dir"]
            }
        }),
    ];

    #[cfg(feature = "remote")]
    {
        tools.push(json!({
            "name": "ilink_account_status",
            "description": "Report the signed-in remote account (nickname, avatar, usability). \
                            Read-only and offline; exactly one account is ever remembered.",
            "inputSchema": { "type": "object", "properties": {} }
        }));
        tools.push(json!({
            "name": "ilink_recent_mini_programs",
            "description": "List the Mini Programs recently used by the signed-in account, newest \
                            first, with nickname, icon and last-used time. Requires an existing \
                            session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "page": { "type": "integer", "description": "1-based page number.", "default": 1 },
                    "size": { "type": "integer", "description": "Entries per page (1-100).", "default": 50 }
                }
            }
        }));
        tools.push(json!({
            "name": "ilink_basic_info",
            "description": "Fetch a Mini Program's basic attributes (nickname, signature, avatar, \
                            version) from the App Store with the authorised session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appid": { "type": "string" },
                    "username": { "type": "string", "description": "Original id, including the @app suffix when present." }
                }
            }
        }));
        tools.push(json!({
            "name": "ilink_download_mini_program",
            "description": "Download a Mini Program's main package and all sub-packages from the \
                            WeChat CDN using the authorized iLink session, optionally \
                            reconstructing the project immediately. Requires a session created \
                            beforehand with `wxapkg-dump login`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appid": { "type": "string" },
                    "username": { "type": "string", "description": "Original id; keep the @app suffix." },
                    "out": { "type": "string", "description": "Destination directory." },
                    "concurrency": { "type": "integer", "description": "Parallel package downloads (1-10), default 3." },
                    "versionType": { "type": "integer", "description": "0 release, 1 trial, 2 development, 999 preview." },
                    "unpack": { "type": "boolean", "description": "Reconstruct the project after downloading." },
                    "beautify": { "type": "boolean", "description": "Beautify the reconstructed project." },
                    "allPackages": { "type": "boolean", "description": "Also download resource widgets; can be very large." },
                    "version": { "type": "integer", "description": "Specific version number; 0 means the latest." }
                },
                "required": ["out"]
            }
        }));
    }

    tools
}

/// Executes a tool by name, returning a JSON string or a human readable failure.
pub fn call(name: &str, arguments: &Value) -> Result<String, String> {
    match name {
        "wxapkg_scan_local_programs" => scan_local(arguments),
        "wxapkg_inspect_package" => inspect_packages(arguments),
        "wxapkg_unpack_package" => unpack_package(arguments),
        "wxapkg_raw_dump" => raw_dump(arguments),
        "wxapkg_beautify_project" => beautify_project(arguments),
        #[cfg(feature = "remote")]
        "ilink_account_status" => ilink_account_status(),
        #[cfg(feature = "remote")]
        "ilink_recent_mini_programs" => ilink_recent(arguments),
        #[cfg(feature = "remote")]
        "ilink_basic_info" => ilink_basic_info(arguments),
        #[cfg(feature = "remote")]
        "ilink_download_mini_program" => ilink_download(arguments),
        other => Err(format!("unknown tool `{other}`")),
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn json_out<T: serde::Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|err| format!("cannot serialise the result: {err}"))
}

fn string_list(arguments: &Value, key: &str) -> Vec<std::path::PathBuf> {
    arguments
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(std::path::PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

fn required_str(arguments: &Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("`{key}` is required"))
}

fn optional_str(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
}

fn flag(arguments: &Value, key: &str, default: bool) -> bool {
    arguments
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// local tools
// ---------------------------------------------------------------------------

fn scan_local(arguments: &Value) -> Result<String, String> {
    let roots = string_list(arguments, "roots");
    let roots = (!roots.is_empty()).then_some(roots);
    let appid = optional_str(arguments, "appid");
    let limit = arguments
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as usize);

    let programs =
        scan(roots.as_deref(), appid.as_deref(), limit).map_err(|err| err.to_string())?;
    if programs.is_empty() {
        return Ok(
            "No Mini Program was found. If the WeChat client keeps its data elsewhere, pass \
             `roots` explicitly."
                .to_owned(),
        );
    }
    json_out(&json!({ "count": programs.len(), "programs": programs }))
}

fn inspect_packages(arguments: &Value) -> Result<String, String> {
    let paths = string_list(arguments, "paths");
    if paths.is_empty() {
        return Err("`paths` must contain at least one entry".to_owned());
    }
    let wxid = optional_str(arguments, "wxid");
    let containers = inspect(&paths, wxid.as_deref()).map_err(|err| err.to_string())?;
    json_out(&json!({ "count": containers.len(), "containers": containers }))
}

fn unpack_package(arguments: &Value) -> Result<String, String> {
    let out = required_str(arguments, "out")?;
    let paths = string_list(arguments, "paths");
    let appid = optional_str(arguments, "appid");

    if paths.is_empty() && appid.is_none() {
        return Err("provide `paths`, or `appid` to locate the package automatically".to_owned());
    }

    let source = match &appid {
        Some(appid) => Source::AppId {
            appid: appid.clone(),
            version: optional_str(arguments, "version"),
            roots: None,
        },
        None => Source::Paths(paths.clone()),
    };

    let auto_fix = flag(arguments, "auto_fix", true);
    let mut request = UnpackRequest::new(source, std::path::PathBuf::from(&out));
    request.wxid = optional_str(arguments, "wxid");
    request.clean = true;
    request.options = BuildOptions {
        keep_raw: flag(arguments, "keep_raw", true),
        prune: auto_fix,
        synthesize: auto_fix,
        plugin_clean: auto_fix,
        plugins_mode: PluginsMode::Keep,
        format: flag(arguments, "beautify", false).then(FormatOptions::default),
        ..BuildOptions::default()
    };
    if !paths.is_empty() {
        request.manifest = wxapkg_core::pipeline::find_manifest(&paths);
    }

    let mut reports =
        wxapkg_core::pipeline::unpack_grouped(&request).map_err(|err| err.to_string())?;
    if reports.len() > 1 {
        let projects: Vec<Value> = reports
            .iter()
            .map(|report| {
                json!({
                    "outDir": report.out_dir,
                    "totalFiles": report.total_files(),
                    "warnings": report.warnings,
                })
            })
            .collect();
        let count = projects.len();
        return json_out(&json!({
            "projects": projects,
            "summary": format!("Reconstructed {count} Mini Program(s), one folder per appid."),
        }));
    }
    let report = reports.remove(0);
    json_out(&json!({
        "outDir": report.out_dir,
        "totalFiles": report.total_files(),
        "counts": report.counts,
        "parts": report.parts,
        "notes": report.notes,
        "warnings": report.warnings,
        "summary": format!(
            "Reconstructed {} file(s) into {}. {} note(s), {} warning(s).",
            report.total_files(),
            report.out_dir,
            report.notes.len(),
            report.warnings.len()
        ),
    }))
}

fn raw_dump(arguments: &Value) -> Result<String, String> {
    let paths = string_list(arguments, "paths");
    if paths.is_empty() {
        return Err("`paths` must contain at least one entry".to_owned());
    }
    let out = required_str(arguments, "out")?;
    let wxid = optional_str(arguments, "wxid");
    let report = raw_extract(
        &paths,
        std::path::Path::new(&out),
        wxid.as_deref(),
        flag(arguments, "autoInflate", false),
    )
    .map_err(|err| err.to_string())?;
    json_out(&report)
}

fn beautify_project(arguments: &Value) -> Result<String, String> {
    let dir = required_str(arguments, "dir")?;
    let options = FormatOptions {
        indent_width: arguments
            .get("indentWidth")
            .and_then(Value::as_u64)
            .map_or(2, |v| v as usize),
        use_tabs: flag(arguments, "useTabs", false),
        max_width: arguments
            .get("maxWidth")
            .and_then(Value::as_u64)
            .map_or(110, |v| v as usize),
        ..FormatOptions::default()
    };
    let report =
        beautify_tree(std::path::Path::new(&dir), &options).map_err(|err| err.to_string())?;
    json_out(&report)
}

// ---------------------------------------------------------------------------
// App Store tools
// ---------------------------------------------------------------------------

#[cfg(feature = "remote")]
fn remote_client() -> Result<wxapkg_remote::Remote, String> {
    wxapkg_remote::Remote::open().map_err(|err| err.to_string())
}

#[cfg(feature = "remote")]
fn ilink_account_status() -> Result<String, String> {
    let client = remote_client()?;
    let Some(account) = client.account().map_err(|err| err.to_string())? else {
        return Ok(
            "No account is signed in. The user must scan a QR code first; run \
             `wxapkg-dump login` in a terminal, since a QR code cannot be shown here."
                .to_owned(),
        );
    };
    json_out(&json!({
        "nickName": account.nick_name,
        "avatar": account.avatar,
        "openid": account.openid,
        "online": account.state == wxapkg_remote::AccountState::Online,
        "state": account.state.label(false),
    }))
}

#[cfg(feature = "remote")]
fn ilink_recent(arguments: &Value) -> Result<String, String> {
    let client = remote_client()?;
    let page = arguments
        .get("page")
        .and_then(Value::as_u64)
        .map_or(1u32, |v| v as u32);
    let size = arguments
        .get("size")
        .and_then(Value::as_u64)
        .map_or(50u32, |v| v as u32);
    let list = client.recent(page, size).map_err(|err| err.to_string())?;
    let items: Vec<Value> = list
        .iter_all()
        .map(|item| {
            let meta = client.program_meta(&item.username);
            json!({
                "username": item.username,
                "appid": meta.as_ref().map(|m| m.appid.clone()).unwrap_or_default(),
                "nickName": meta.as_ref().map(|m| m.nick_name.clone()).unwrap_or_default(),
                "lastUsedAt": item.update_time,
            })
        })
        .collect();
    json_out(&json!({ "total": list.iter_all().count(), "items": items }))
}

#[cfg(feature = "remote")]
fn ilink_basic_info(arguments: &Value) -> Result<String, String> {
    let appid = optional_str(arguments, "appid");
    let username = optional_str(arguments, "username");
    if appid.is_none() && username.is_none() {
        return Err("provide `appid` or `username`".to_owned());
    }
    let client = remote_client()?;
    let info = client
        .info(appid.as_deref(), username.as_deref())
        .map_err(|err| err.to_string())?;
    json_out(&json!({
        "appid": info.appid,
        "username": info.username,
        "nickName": info.nick_name,
        "signature": info.signature,
        "avatar": info.avatar,
        "version": info.version,
    }))
}

#[cfg(feature = "remote")]
fn ilink_download(arguments: &Value) -> Result<String, String> {
    let out = required_str(arguments, "out")?;
    let appid = optional_str(arguments, "appid");
    let username = optional_str(arguments, "username");
    if appid.is_none() && username.is_none() {
        return Err("provide `appid` or `username`".to_owned());
    }

    let client = remote_client()?;
    let requested_appid = appid
        .clone()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_default();
    let requested_username = username
        .clone()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_default();
    // Resolve the folder name before writing anything, so a username-only call still lands
    // under original_wxapkg/<appid>/wxapkg like the CLI.
    let resolved_appid = if requested_appid.is_empty() && !requested_username.is_empty() {
        client
            .info(None, Some(requested_username.as_str()))
            .ok()
            .map(|info| info.appid)
            .filter(|value| !value.is_empty())
            .unwrap_or_default()
    } else {
        requested_appid.clone()
    };
    let folder = if resolved_appid.is_empty() {
        requested_username.replace(['/', '\\'], "_")
    } else {
        resolved_appid.clone()
    };
    let archive = std::path::PathBuf::from(&out)
        .join("original_wxapkg")
        .join(wxapkg_core::paths::sanitize_segment(&folder))
        .join("wxapkg");
    let params = wxapkg_remote::DownloadOptions {
        all_packages: flag(arguments, "allPackages", false),
        concurrency: arguments
            .get("concurrency")
            .and_then(serde_json::Value::as_u64)
            .map_or(3, |v| (v as usize).clamp(1, 10)),
        progress: None,
        appid: resolved_appid.clone(),
        username: requested_username,
        out_dir: archive.clone(),
        version_type: arguments
            .get("versionType")
            .and_then(Value::as_u64)
            .map_or(0, |v| v as u32),
        version: arguments
            .get("version")
            .and_then(Value::as_u64)
            .map_or(0, |v| v as u32),
        cancel: None,
    };
    let report = client.download(&params).map_err(|err| err.to_string())?;

    // Chaining the local decompiler here keeps the tool's contract simple: one call
    // yields a project when the caller asks for one.
    let mut unpacked: Option<Value> = None;
    if flag(arguments, "unpack", false) {
        let paths: Vec<std::path::PathBuf> = report
            .packages
            .iter()
            .filter(|package| package.error.is_empty() && !package.saved_path.is_empty())
            .map(|package| std::path::PathBuf::from(&package.saved_path))
            .collect();
        if !paths.is_empty() {
            let project_dir = archive.clone();
            let mut request = UnpackRequest::new(Source::Paths(paths), project_dir);
            request.clean = true;
            request.wxid = (!report.appid.is_empty()).then(|| report.appid.clone());
            request.project_folder =
                (!report.appid.is_empty()).then(|| format!("unpack_{}", report.appid));
            request.manifest = wxapkg_core::pipeline::find_manifest(
                &report
                    .packages
                    .iter()
                    .filter(|package| !package.saved_path.is_empty())
                    .map(|package| std::path::PathBuf::from(&package.saved_path))
                    .collect::<Vec<_>>(),
            );
            if flag(arguments, "beautify", false) {
                request.options.format = Some(FormatOptions::default());
            }
            match unpack(&request) {
                Ok(build) => {
                    unpacked = Some(json!({
                        "outDir": build.out_dir.clone(),
                        "totalFiles": build.total_files(),
                        "warnings": build.warnings,
                    }));
                }
                Err(err) => {
                    unpacked = Some(json!({ "error": err.to_string() }));
                }
            }
        }
    }

    json_out(&json!({
        "appid": report.appid,
        "username": report.username,
        "outDir": report.out_dir,
        "downloaded": report.downloaded(),
        "total": report.packages.len(),
        "packages": report.packages,
        "project": unpacked,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_are_unique_and_well_formed() {
        let tools = descriptors();
        assert!(tools.len() >= 5);
        let mut seen = std::collections::HashSet::new();
        for tool in &tools {
            let name = tool["name"].as_str().unwrap();
            assert!(seen.insert(name.to_owned()), "duplicate tool {name}");
            assert!(tool["description"].as_str().unwrap().len() > 30);
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn unknown_tools_are_reported() {
        assert!(call("nope", &json!({})).is_err());
    }

    #[test]
    fn missing_required_arguments_are_reported() {
        let err = call("wxapkg_unpack_package", &json!({})).unwrap_err();
        assert!(err.contains("out"), "{err}");
    }

    #[test]
    fn inspect_requires_paths() {
        let err = call("wxapkg_inspect_package", &json!({})).unwrap_err();
        assert!(err.contains("paths"), "{err}");
    }
}
