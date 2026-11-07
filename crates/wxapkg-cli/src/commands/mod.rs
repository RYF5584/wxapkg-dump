// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Subcommand implementations.

#[cfg(feature = "remote")]
pub mod ilink;

use comfy_table::{presets::UTF8_FULL, Cell, ContentArrangement, Table};
use console::style;
use wxapkg_core::beautify::FormatOptions;
use wxapkg_core::pipeline::{
    beautify_tree, inspect, raw_extract, scan_matching as scan_programs, Source, UnpackRequest,
};
use wxapkg_core::project::{BuildOptions, PluginsMode};
use wxapkg_core::{BuildReport, ContainerInfo, ProgramSummary, RawExtractReport};

use crate::exit::ExitCode;
use crate::output::{ellipsize, emit_failure, emit_success, human_bytes};
use crate::{BeautifyArgs, Cli, InfoArgs, PluginsModeArg, ScanArgs, UnpackArgs, VerifyArgs};

/// `scan`
pub fn cmd_scan(args: &ScanArgs, cli: &Cli) -> ExitCode {
    // `scan DIR...` and `scan --path DIR` are the same thing; positional roots are the
    // conventional form and what `unpack`/`info` accept.
    let mut roots: Vec<std::path::PathBuf> = args.inputs.clone();
    roots.extend(args.paths.iter().cloned());
    let roots = (!roots.is_empty()).then_some(roots);
    // Uses the matching variant so the skipped-path count is available: a directory we could
    // not read makes the listing shorter without making it look wrong.
    match scan_programs(roots.as_deref(), args.appid.as_deref(), args.limit) {
        Ok((programs, skipped)) => {
            if skipped > 0 {
                // stderr on purpose: stdout carries the machine-readable result and nothing else.
                eprintln!(
                    "{}",
                    wxapkg_core::i18n::Key::WarnSkippedPaths
                        .format(cli.lang(), &[("count", &skipped.to_string())])
                );
            }
            if programs.is_empty() {
                // An empty machine is a normal answer, not a failure: scripts parse the
                // envelope and expect `ok: true` with an empty list.
                let hint = wxapkg_core::i18n::Key::HintPassPathRoot.get(cli.lang());
                return emit_success("scan", cli.json, programs, move |_| {
                    eprintln!(
                        "{}",
                        wxapkg_core::i18n::Key::ScanNothingFound.get(cli.lang())
                    );
                    eprintln!("{hint}");
                });
            }
            let show_packages = args.packages;
            emit_success("scan", cli.json, programs, move |programs| {
                print_scan_table(programs, show_packages, cli.lang());
            })
        }
        Err(err) => fail("scan", cli, &err),
    }
}

fn print_scan_table(
    programs: &[ProgramSummary],
    show_packages: bool,
    lang: wxapkg_core::i18n::Lang,
) {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            "appid",
            "version",
            "bundles",
            "size",
            "encrypted",
            "location",
        ]);
    for program in programs {
        table.add_row(vec![
            Cell::new(&program.appid),
            Cell::new(if program.version.is_empty() {
                "-".to_owned()
            } else {
                program.version.clone()
            }),
            Cell::new(program.packages),
            Cell::new(human_bytes(program.total_size)),
            Cell::new(if program.encrypted { "yes" } else { "no" }),
            Cell::new(ellipsize(&program.location, 60)),
        ]);
    }
    println!("{table}");
    let bundles: usize = programs.iter().map(|p| p.packages).sum();
    println!(
        "{}",
        style(wxapkg_core::i18n::Key::ScanSummary.format(
            lang,
            &[
                ("programs", &programs.len().to_string()),
                ("bundles", &bundles.to_string()),
            ]
        ))
        .dim()
    );
    if show_packages {
        println!();
    }
}

/// `info`
pub fn cmd_info(args: &InfoArgs, cli: &Cli) -> ExitCode {
    if args.input.input.is_empty() {
        return emit_failure(
            "info",
            cli.json,
            "invalid_argument",
            "at least one input path is required",
            None,
        );
    }
    match inspect(&args.input.input, args.input.wxid.as_deref()) {
        Ok(infos) if infos.is_empty() => {
            let input = args
                .input
                .input
                .first()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            let reason = match cli.lang() {
                wxapkg_core::i18n::Lang::ZhHans => "输入中没有可读的 wxapkg 容器",
                wxapkg_core::i18n::Lang::En => {
                    "no readable wxapkg container was found in the given input"
                }
            };
            let err = wxapkg_core::Error::invalid_container(input, reason);
            fail("info", cli, &err)
        }
        Ok(infos) => {
            let verbose = args.entries;
            emit_success("info", cli.json, infos, move |infos| {
                for info in infos {
                    print_container(info);
                }
                let _ = verbose;
            })
        }
        Err(err) => fail("info", cli, &err),
    }
}

fn print_container(info: &ContainerInfo) {
    println!("{}", style(&info.path).bold());
    println!("  role       : container v{}", info.version);
    println!(
        "  layout     : index {} / body {} / {} files",
        human_bytes(u64::from(info.index_len)),
        human_bytes(u64::from(info.body_len)),
        info.files
    );
    if info.pages > 0 || info.sub_packages > 0 || info.plugins > 0 {
        println!(
            "  app-config : {} pages, {} sub-packages, {} plugins",
            info.pages, info.sub_packages, info.plugins
        );
    }
    for note in &info.notes {
        println!("  note       : {note}");
    }
    println!();
}

/// `unpack`
pub fn cmd_unpack(args: &UnpackArgs, cli: &Cli) -> ExitCode {
    let out = args.out.clone().unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    });

    // --raw: verbatim content export, no decompilation (the former `raw` command).
    if let Some(raw_dir) = &args.raw {
        let mut inputs = expand_inputs(&args.input);
        // `--appid` works here too: it is the easy way to export what `scan` just found.
        if inputs.is_empty() {
            if let Some(appid) = args
                .appid
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                let outcome =
                    match wxapkg_core::discover::scan_programs_matching(None, Some(appid), None) {
                        Ok(outcome) => outcome,
                        Err(err) => return fail("unpack --raw", cli, &err),
                    };
                let program = outcome.programs.into_iter().find(|program| {
                    args.version
                        .as_deref()
                        .is_none_or(|version| program.version == version)
                });
                match program {
                    Some(program) => {
                        inputs = program.packages.iter().map(|p| p.path.clone()).collect();
                    }
                    None => {
                        return fail(
                            "unpack --raw",
                            cli,
                            &wxapkg_core::Error::ProgramNotFound {
                                appid: appid.to_owned(),
                                version: args.version.clone(),
                            },
                        );
                    }
                }
            }
        }
        if inputs.is_empty() {
            return emit_failure(
                "unpack",
                cli.json,
                "invalid_argument",
                "at least one input path is required",
                None,
            );
        }
        return match raw_extract(&inputs, raw_dir, args.wxid.as_deref(), args.auto_inflate) {
            Ok(report) => {
                let dir = raw_dir.clone();
                emit_success(
                    "unpack --raw",
                    cli.json,
                    report,
                    move |report: &RawExtractReport| {
                        for note in &report.notes {
                            println!("note: {note}");
                        }
                        println!("exported {} file(s) to {}", report.files, dir.display());
                    },
                )
            }
            Err(err) => fail("unpack --raw", cli, &err),
        };
    }

    if let Some(appid) = &args.appid {
        if appid.trim().is_empty() {
            return emit_failure(
                "unpack",
                cli.json,
                "invalid_argument",
                "--appid is empty",
                None,
            );
        }
        let mut request = UnpackRequest::new(
            Source::AppId {
                appid: appid.trim().to_owned(),
                version: args.version.clone(),
                roots: None,
            },
            out.clone(),
        );
        request.wxid = args.wxid.clone();
        request.main = args.main.clone();
        request.clean = args.clean;
        request.options = build_options(args);
        return finish_unpack(request, cli, args.strict);
    }

    if args.input.is_empty() {
        return emit_failure(
            "unpack",
            cli.json,
            "invalid_argument",
            "pass one or more container paths, or use --appid to locate them automatically",
            None,
        );
    }

    // Explicit inputs default next to what the user selected: a file restores into its
    // parent folder, a folder restores inside itself, always as `unpack_<appid>/`.
    let out = args.out.clone().unwrap_or_else(|| {
        let first = args.input.first();
        match first {
            Some(path) if path.is_dir() => path.clone(),
            Some(path) => path
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| std::path::PathBuf::from(".")),
            None => std::path::PathBuf::from("."),
        }
    });
    let mut request = UnpackRequest::new(Source::Paths(args.input.clone()), out);
    request.wxid = args.wxid.clone();
    request.main = args.main.clone();
    request.clean = args.clean;
    request.unpack_prefix = true;
    request.options = build_options(args);
    request.manifest = wxapkg_core::pipeline::find_manifest(&args.input);
    finish_unpack(request, cli, args.strict)
}

/// Expands directory inputs into the container files they hold (for multi-program splits).
fn expand_inputs(inputs: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for path in inputs {
        if path.is_dir() {
            for entry in walk_inputs(path) {
                out.push(entry);
            }
        } else {
            out.push(path.clone());
        }
    }
    out
}

fn walk_inputs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_inputs(&path));
        } else if path
            .extension()
            .is_some_and(|ext| ext == "wxapkg" || ext == "wxapkg2")
        {
            out.push(path);
        }
    }
    out
}

/// Shared tail of every restore: one project per Mini Program, then the report.
fn finish_unpack(
    request: wxapkg_core::pipeline::UnpackRequest,
    cli: &Cli,
    strict: bool,
) -> ExitCode {
    match wxapkg_core::pipeline::unpack_grouped(&request) {
        // The common case stays byte-for-byte the old output.
        Ok(mut reports) if reports.len() == 1 => {
            let report = reports.remove(0);
            // The pipeline decides the final directory (`<out>/<appid>/`), so the message
            // must report what it actually wrote rather than the base the user passed.
            let project = report.out_dir.clone();
            let warnings = report.warnings.len();
            let code = emit_success("unpack", cli.json, report, move |report: &BuildReport| {
                println!("project written to {project}");
                if report.warnings.is_empty() {
                    println!("open it with the WeChat DevTools via Import Project");
                    return;
                }
                for warning in report.warnings.iter().take(20) {
                    println!("warning: {warning}");
                }
                let more = report.warnings.len().saturating_sub(20);
                if more > 0 {
                    println!("… and {more} more warning(s)");
                }
            });
            if strict && warnings > 0 {
                ExitCode::Warnings
            } else {
                code
            }
        }
        // A folder (or selection) that spans several programs: one project per appid.
        Ok(reports) => {
            let projects: Vec<serde_json::Value> = reports
                .iter()
                .map(|report| {
                    serde_json::json!({
                        "outDir": report.out_dir,
                        "files": report.total_files(),
                        "warnings": report.warnings.len(),
                    })
                })
                .collect();
            let count = projects.len();
            let warnings: usize = reports.iter().map(|report| report.warnings.len()).sum();
            let code = emit_success(
                "unpack",
                cli.json,
                serde_json::json!({ "projects": projects }),
                move |value: &serde_json::Value| {
                    println!("restored {count} Mini Program(s):");
                    for project in value["projects"].as_array().into_iter().flatten() {
                        println!(
                            "  {} ({} files)",
                            project["outDir"].as_str().unwrap_or_default(),
                            project["files"].as_u64().unwrap_or(0)
                        );
                    }
                },
            );
            if strict && warnings > 0 {
                ExitCode::Warnings
            } else {
                code
            }
        }
        Err(err) => fail("unpack", cli, &err),
    }
}

fn build_options(args: &UnpackArgs) -> BuildOptions {
    let mut options = BuildOptions {
        keep_raw: !args.no_raw,
        decompile_wxml: !args.no_wxml,
        rewrite_requires: !args.no_require_rewrite,
        prune: !(args.no_prune || args.no_auto_fix),
        synthesize: !(args.no_synthesize || args.no_auto_fix),
        plugin_clean: !(args.no_plugin_clean || args.no_auto_fix),
        plugins_mode: match args.plugins {
            PluginsModeArg::Keep => PluginsMode::Keep,
            PluginsModeArg::Placeholder => PluginsMode::Placeholder,
            PluginsModeArg::Strip => PluginsMode::Strip,
        },
        wxss_suffix: args.wxss_suffix.clone(),
        format: args.beautify.then(|| FormatOptions {
            indent_width: args.indent,
            use_tabs: args.use_tabs,
            ..FormatOptions::default()
        }),
        ..BuildOptions::default()
    };
    if args.max_warnings == 0 {
        options.max_warnings = usize::MAX;
    }
    options
}

/// `beautify`
pub fn cmd_beautify(args: &BeautifyArgs, cli: &Cli) -> ExitCode {
    let options = FormatOptions {
        indent_width: args.indent,
        use_tabs: args.use_tabs,
        max_width: args.max_width,
        preserve_line_breaks: !args.collapse_lines,
        ..FormatOptions::default()
    };
    match beautify_tree(&args.dir, &options) {
        Ok(report) => {
            let count = report.formatted.len();
            let dir = args.dir.clone();
            emit_success("beautify", cli.json, report, move |_| {
                println!("formatted {count} file(s) under {}", dir.display());
            })
        }
        Err(err) => fail("beautify", cli, &err),
    }
}

/// Reports an engine failure with its text in the selected language.
///
/// The error *code* is never translated — it is the machine contract — while the message and
/// the hint are. Every command funnels engine errors through here so no path can accidentally
/// print the untranslated `Display` form.
fn fail(command: &str, cli: &Cli, err: &wxapkg_core::Error) -> ExitCode {
    let lang = cli.lang();
    emit_failure(
        command,
        cli.json,
        err.code(),
        &err.message(lang),
        err.hint(lang).as_deref(),
    )
}

/// `verify`
pub fn cmd_verify(args: &VerifyArgs, cli: &Cli) -> ExitCode {
    use wxapkg_core::pipeline::merge_syntax_failures;
    use wxapkg_core::verify::{verify_project, VerifyOptions};

    let options = VerifyOptions {
        check_assets: !args.no_assets,
        check_handlers: args.handlers,
        max_per_code: if args.all { usize::MAX } else { 20 },
    };

    let mut report = match verify_project(&args.dir, &options) {
        Ok(report) => report,
        Err(err) => return fail("verify", cli, &err),
    };

    // A real syntax check needs a JavaScript engine; the structural pass only proves that
    // delimiters balance. When Node is present we can do better, so we do.
    if args.node {
        match wxapkg_core::verify::node_syntax_check(&args.dir) {
            Ok(failures) => {
                if !failures.is_empty() && !cli.json {
                    eprintln!(
                        "{}",
                        style(format!(
                            "node --check reported {} failing file(s)",
                            failures.len()
                        ))
                        .red()
                    );
                }
                merge_syntax_failures(&mut report, &failures);
            }
            Err(err) => {
                eprintln!(
                    "{}",
                    style(format!(
                        "node --check unavailable, skipping real syntax check: {err}"
                    ))
                    .yellow()
                );
            }
        }
    }

    let failed = !report.ok;
    let code = emit_success("verify", cli.json, report.clone(), move |report| {
        print_verify_report(report);
    });

    if failed {
        // The project itself is invalid input; this is a check result, not a runtime fault.
        ExitCode::Usage
    } else if args.strict && !report.warnings.is_empty() {
        ExitCode::Warnings
    } else {
        code
    }
}

fn print_verify_report(report: &wxapkg_core::verify::VerifyReport) {
    let verdict = if report.ok {
        style("PASS").green().bold()
    } else {
        style("FAIL").red().bold()
    };
    println!("{verdict}  {}", report.root);
    println!(
        "  {} page(s), {} sub-package(s), {} component ref(s)",
        report.stats.pages, report.stats.sub_packages, report.stats.components
    );
    println!(
        "  inspected: {} js, {} wxml, {} wxss, {} json ({} import(s), {} skipped)",
        report.stats.js,
        report.stats.wxml,
        report.stats.wxss,
        report.stats.json,
        report.stats.requires,
        report.stats.skipped
    );
    if report.syntax_checked {
        println!("  javascript syntax verified with node --check");
    }

    if !report.errors.is_empty() {
        println!(
            "\n{}",
            style(format!("errors ({})", report.errors.len()))
                .red()
                .bold()
        );
        for issue in &report.errors {
            println!("  [{}] {}: {}", issue.code, issue.path, issue.message);
        }
    }
    if !report.warnings.is_empty() {
        println!(
            "\n{}",
            style(format!("warnings ({})", report.warnings.len()))
                .yellow()
                .bold()
        );
        for issue in &report.warnings {
            println!("  [{}] {}: {}", issue.code, issue.path, issue.message);
        }
    }
    println!("\n{}", report.summary());
}

/// `doctor`
pub fn cmd_doctor(cli: &Cli) -> ExitCode {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_header(vec!["component", "status", "detail"]);
    let mut facts = serde_json::Map::new();
    facts.insert(
        "version".to_owned(),
        serde_json::Value::String(wxapkg_core::VERSION.to_owned()),
    );
    facts.insert(
        "target".to_owned(),
        serde_json::Value::String(format!(
            "{}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )),
    );

    table.add_row(vec![
        Cell::new("wxapkg-dump"),
        Cell::new("ok"),
        Cell::new(wxapkg_core::VERSION),
    ]);
    table.add_row(vec![
        Cell::new("target"),
        Cell::new("ok"),
        Cell::new(format!(
            "{}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )),
    ]);

    #[cfg(feature = "remote")]
    {
        let base = directories::BaseDirs::new().map(|dirs| dirs.data_dir().join("wxapkg-dump"));
        let mut remote_facts = serde_json::Map::new();
        remote_facts.insert("enabled".to_owned(), serde_json::Value::Bool(true));
        table.add_row(vec![
            Cell::new("remote download"),
            Cell::new("ok"),
            Cell::new("native (Rust)".to_owned()),
        ]);
        if let Some(base) = &base {
            let mut paths = serde_json::Map::new();
            for (label, key, dir) in [
                ("config dir", "configDir", base.clone()),
                ("sessions", "sessionsDir", base.join("remote")),
                ("cache dir", "cacheDir", base.join("cache")),
                ("logs dir", "logsDir", base.join("logs")),
            ] {
                table.add_row(vec![
                    Cell::new(label),
                    Cell::new("path"),
                    Cell::new(ellipsize(&dir.display().to_string(), 70)),
                ]);
                paths.insert(
                    key.to_owned(),
                    serde_json::Value::String(dir.display().to_string()),
                );
            }
            remote_facts.insert("paths".to_owned(), serde_json::Value::Object(paths));
        }

        let stale = wxapkg_remote::Remote::open()
            .ok()
            .and_then(|remote| remote.account().ok().flatten())
            .map(|account| (account.state == wxapkg_remote::AccountState::Stale) as usize)
            .unwrap_or(0);
        let cached = wxapkg_remote::Remote::open()
            .ok()
            .and_then(|remote| remote.account().ok().flatten().map(|_| 1usize))
            .unwrap_or(0);
        table.add_row(vec![
            Cell::new("remote account"),
            Cell::new(if stale == 0 { "ok" } else { "attention" }),
            Cell::new(if cached == 0 {
                "not signed in".to_owned()
            } else if stale == 0 {
                "signed in".to_owned()
            } else {
                "signed in, session expired".to_owned()
            }),
        ]);
        remote_facts.insert("signedIn".to_owned(), serde_json::Value::Bool(cached > 0));
        remote_facts.insert(
            "sessionExpired".to_owned(),
            serde_json::Value::Bool(stale > 0),
        );
        facts.insert("remote".to_owned(), serde_json::Value::Object(remote_facts));
    }
    #[cfg(not(feature = "remote"))]
    {
        table.add_row(vec![
            Cell::new("remote download"),
            Cell::new("disabled"),
            Cell::new("built without the `remote` feature"),
        ]);
        facts.insert("remote".to_owned(), serde_json::json!({ "enabled": false }));
    }

    let roots = wxapkg_core::discover::default_roots();
    let mut root_facts = Vec::new();
    for root in roots.iter() {
        let exists = root.is_dir();
        if root_facts.len() < 4 {
            table.add_row(vec![
                Cell::new("search root"),
                Cell::new(if exists { "present" } else { "absent" }),
                Cell::new(ellipsize(&root.display().to_string(), 70)),
            ]);
        }
        root_facts.push(serde_json::json!({
            "path": root.display().to_string(),
            "present": exists,
        }));
    }
    facts.insert(
        "searchRoots".to_owned(),
        serde_json::Value::Array(root_facts),
    );

    emit_success(
        "doctor",
        cli.json,
        serde_json::Value::Object(facts),
        move |_| {
            println!("{table}");
        },
    )
}
