// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! `wxapkg-dump ilink` — authorized remote download over the iLink channel.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use comfy_table::{presets::UTF8_FULL, Cell, ContentArrangement, Table};
use console::style;
use wxapkg_remote::{
    DownloadOptions, DownloadReport, Error as RemoteError, LoginStatus, MiniInfo, RecentItem,
    Remote,
};

use crate::exit::ExitCode;
use crate::output::{ellipsize, emit_failure, emit_success, human_bytes};
use crate::Cli;
use wxapkg_core::i18n::Key;

/// Remote command group. Exactly one account is ever remembered; a new login replaces it.
#[derive(Debug, Args)]
pub struct IlinkArgs {
    /// Subcommand.
    #[command(subcommand)]
    pub command: IlinkCommand,
}

impl IlinkArgs {
    /// Builds args from the promoted top-level commands.
    pub fn new(command: IlinkCommand) -> Self {
        Self { command }
    }
}

/// Remote-download subcommands.
#[derive(Debug, Subcommand)]
pub enum IlinkCommand {
    /// Show the signed-in account's status.
    Status,

    /// Scan a QR code; a confirmed login replaces the current account.
    ///
    /// Without a subcommand this is the interactive flow: render the QR, poll until the
    /// phone confirms, exchange and store the account.
    Login {
        #[command(subcommand)]
        mode: Option<LoginMode>,

        #[command(flatten)]
        args: LoginArgs,
    },

    /// List recently used Mini Programs.
    Recent(RecentArgs),

    /// Fetch a Mini Program's basic attributes.
    Info(TargetArgs),

    /// Download a Mini Program's packages.
    Download(DownloadArgs),

    /// Delete the signed-in account and every cache tied to it.
    Delete(DeleteArgs),
}

/// Split login operations: fetch the QR first, then check it from anywhere.
#[derive(Debug, Clone, Subcommand)]
pub enum LoginMode {
    /// Issue a QR code and print its session key plus links, then exit.
    GetQr {
        /// Also write the login QR image to this path.
        #[arg(long, value_name = "FILE")]
        qr_out: Option<PathBuf>,
    },

    /// Check a pending QR once and exit; a confirmed scan is exchanged on the spot.
    CheckQr {
        /// Session key printed by `login get-qr`.
        #[arg(long)]
        session: String,

        /// Seconds to long-poll before returning "still waiting".
        #[arg(long, default_value_t = 35)]
        wait: u64,
    },
}

/// `ilink login` arguments.
#[derive(Debug, Clone, Args)]
pub struct LoginArgs {
    /// Seconds to keep polling after the QR code is shown.
    #[arg(long, default_value_t = 180)]
    pub wait: u64,

    /// Also write the login QR image to this path.
    #[arg(long, value_name = "FILE")]
    pub qr_out: Option<PathBuf>,

    /// Do not print the QR code to the terminal.
    #[arg(long)]
    pub no_terminal: bool,
}

/// `recent` arguments.
#[derive(Debug, Clone, Args)]
pub struct RecentArgs {
    /// 1-based page number.
    #[arg(long, default_value_t = 1)]
    pub page: u32,

    /// Entries per page (1-100).
    #[arg(long, default_value_t = 50)]
    pub size: u32,

    /// Only report starred entries.
    #[arg(long)]
    pub starred: bool,
}

/// Selection of a Mini Program by appid or original id.
#[derive(Debug, Clone, Args)]
pub struct TargetArgs {
    /// Target: `wx…` appid or `gh_…` original id.
    #[arg(value_name = "ID", conflicts_with_all = ["appid", "username"])]
    pub id: Option<String>,

    /// Appid (`wx` + 16 hex digits).
    #[arg(long)]
    pub appid: Option<String>,

    /// Original id (`gh_…`).
    #[arg(long)]
    pub username: Option<String>,
}

impl TargetArgs {
    /// Resolves the positional id into the field the remote crate expects.
    ///
    /// `gh_…` is an original id, everything else is treated as an appid; the remote layer
    /// normalises a bare `gh_…` to `gh_…@app` on its own.
    pub fn resolve(&self) -> (Option<String>, Option<String>) {
        let Some(id) = self.id.as_deref() else {
            return (self.appid.clone(), self.username.clone());
        };
        if id.starts_with("gh_") {
            (None, Some(id.to_owned()))
        } else {
            (Some(id.to_owned()), None)
        }
    }
}

/// `download` arguments.
#[derive(Debug, Clone, Args)]
#[command(disable_version_flag = true)]
pub struct DownloadArgs {
    /// Target selection.
    #[command(flatten)]
    pub target: TargetArgs,

    /// Destination directory.
    #[arg(short, long, value_name = "DIR")]
    pub out: PathBuf,

    /// Channel: 0 release, 1 trial, 2 development, 999 preview.
    #[arg(long, default_value_t = 0)]
    pub version_type: u32,

    /// Specific version number; 0 means the latest.
    #[arg(long, default_value_t = 0)]
    pub version: u32,

    /// Also download resource widgets (fonts, maps, animations) — can be very large.
    #[arg(long)]
    pub all_packages: bool,

    /// Parallel package downloads (1-10).
    #[arg(long, default_value_t = 3)]
    pub concurrency: usize,

    /// Decompile the downloaded packages into a project tree.
    #[arg(long)]
    pub unpack: bool,

    /// Beautify the sources produced by `--unpack`.
    #[arg(long, requires = "unpack")]
    pub beautify: bool,
}

/// `account-delete` arguments.
#[derive(Debug, Clone, Args)]
pub struct DeleteArgs {
    /// Skip the confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

/// Runs a remote subcommand.
pub fn run(args: &IlinkArgs, cli: &Cli) -> ExitCode {
    let remote = match Remote::open() {
        Ok(remote) => remote,
        Err(err) => return fail("ilink", cli, &err),
    };

    match &args.command {
        IlinkCommand::Status => run_status(&remote, cli),
        IlinkCommand::Login {
            mode,
            args: login_args,
        } => match mode {
            Some(LoginMode::GetQr { qr_out }) => run_login_get_qr(&remote, qr_out.as_deref(), cli),
            Some(LoginMode::CheckQr { session, wait }) => {
                run_login_check_qr(&remote, session, *wait, cli)
            }
            None => run_login(&remote, login_args, cli),
        },
        IlinkCommand::Recent(recent_args) => run_recent(&remote, recent_args, cli),
        IlinkCommand::Info(target_args) => run_info(&remote, target_args, cli),
        IlinkCommand::Download(download_args) => run_download(&remote, download_args, cli),
        IlinkCommand::Delete(delete_args) => run_delete(&remote, delete_args, cli),
    }
}

fn fail(command: &str, cli: &Cli, err: &RemoteError) -> ExitCode {
    let hint = match err {
        RemoteError::SessionExpired { .. } | RemoteError::NotLoggedIn(_) => {
            Some("运行 `wxapkg-dump login` 重新扫码登录")
        }
        _ => None,
    };
    let code = match err {
        RemoteError::SessionExpired { .. } => "session_expired",
        RemoteError::NotLoggedIn(_) => "not_logged_in",
        RemoteError::InvalidArgument(_) => "invalid_argument",
        RemoteError::Upstream(_) => "upstream_rejected",
        _ => "remote_failed",
    };
    emit_failure(command, cli.json, code, &err.to_string(), hint)
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn run_status(remote: &Remote, cli: &Cli) -> ExitCode {
    let account = match remote.account() {
        Ok(account) => account,
        Err(err) => return fail("ilink status", cli, &err),
    };
    let stale = account
        .as_ref()
        .is_some_and(|account| account.state == wxapkg_remote::AccountState::Stale);
    let warnings: Vec<String> = stale
        .then(|| "登录已过期，请重新扫码登录".to_owned())
        .into_iter()
        .collect();
    // Session keys are credentials: the store keeps them, every CLI/API payload does not.
    let public_account = account.clone().map(|mut account| {
        account.session = None;
        account
    });
    let payload = serde_json::json!({
        "implementation": "native",
        "storage": remote.store().root().display().to_string(),
        "account": public_account,
        "warnings": warnings,
    });
    emit_success("ilink status", cli.json, payload, |_| {
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(vec!["field", "value"]);
        let lang = cli.lang();
        table.add_row(vec![
            Cell::new(Key::StatusImplementation.format(lang, &[])),
            Cell::new("native (Rust)"),
        ]);
        table.add_row(vec![
            Cell::new(Key::StatusStorage.format(lang, &[])),
            Cell::new(ellipsize(&remote.store().root().display().to_string(), 60)),
        ]);
        match account.as_ref() {
            Some(inner) => {
                table.add_row(vec![
                    Cell::new(Key::StatusAccount.format(lang, &[])),
                    Cell::new(&inner.nick_name),
                ]);
                table.add_row(vec![
                    Cell::new(Key::StatusState.format(lang, &[])),
                    Cell::new(inner.state.label(false)),
                ]);
                if let Some(openid) = &inner.openid {
                    table.add_row(vec![Cell::new("openid"), Cell::new(openid)]);
                }
            }
            None => {
                table.add_row(vec![
                    Cell::new(Key::StatusAccount.format(lang, &[])),
                    Cell::new(Key::StatusNoAccount.format(lang, &[])),
                ]);
            }
        }
        println!("{table}");
        for warning in &warnings {
            eprintln!("{}", style(warning).yellow());
        }
    })
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

fn run_login(remote: &Remote, args: &LoginArgs, cli: &Cli) -> ExitCode {
    let session = match remote.begin_login() {
        Ok(session) => session,
        Err(err) => return fail("ilink login", cli, &err),
    };

    // The login code is served as an image; a caller without a window wants the file.
    if let Some(path) = &args.qr_out {
        match remote.qr_image(&session) {
            Ok(bytes) => {
                if let Err(err) = std::fs::write(path, &bytes) {
                    return fail(
                        "ilink login",
                        cli,
                        &RemoteError::io(format!("writing {}", path.display()), err),
                    );
                }
            }
            Err(err) => return fail("ilink login", cli, &err),
        }
    }

    if !cli.json && !args.no_terminal {
        print_login_qr(&session.qr_url, session.remaining());
    }

    let deadline = Instant::now() + Duration::from_secs(args.wait.max(1));
    let mut last = "waiting";
    while Instant::now() < deadline {
        match remote.poll(&session) {
            Ok(status) => {
                let name = status.name();
                if name != last {
                    last = name;
                    if !cli.json {
                        report_login_state(&status);
                    }
                }
                match status {
                    LoginStatus::Authorized { .. } => return finish_login(remote, cli),
                    LoginStatus::Expired | LoginStatus::Cancelled => {
                        return emit_failure(
                            "ilink login",
                            cli.json,
                            "session_expired",
                            "the QR code expired or was cancelled before the login finished",
                            Some("run the command again to get a fresh QR code"),
                        );
                    }
                    LoginStatus::Waiting | LoginStatus::Scanned => {}
                }
            }
            Err(err) => return fail("ilink login", cli, &err),
        }
        std::thread::sleep(Duration::from_millis(1200));
    }

    emit_failure(
        "ilink login",
        cli.json,
        "session_expired",
        "timed out waiting for the QR code to be scanned",
        Some("pass --wait with a larger value, or --print-only to fetch the QR link alone"),
    )
}

/// Renders the login link as a terminal QR.
///
/// The upstream login code itself is an image, so a terminal can only offer the link: scan
/// it with any camera to open the page, or pass `--qr-out` for the image file.
fn print_login_qr(link: &str, remaining: u32) {
    match qrcode::QrCode::with_error_correction_level(link, qrcode::EcLevel::M) {
        Ok(code) => {
            let mut out = String::new();
            for width in 0..code.width() {
                for height in 0..code.width() {
                    out.push_str(if code[(width, height)] == qrcode::Color::Dark {
                        "██"
                    } else {
                        "  "
                    });
                }
                out.push('\n');
            }
            println!("{out}");
        }
        Err(err) => eprintln!(
            "{}",
            style(format!("cannot render the QR code: {err}")).yellow()
        ),
    }
    println!(
        "{}",
        style("open this link and scan the code it shows").bold()
    );
    println!("{}", style(link).dim());
    if remaining > 0 {
        println!("{}", style(format!("valid for {remaining}s")).dim());
    }
}

/// `ilink login get-qr` — RFC 8628 step one: issue the QR and hand back a handle.
///
/// The handle (`session`) is all `check-qr` needs, so another process — or another
/// machine, or an AI agent — can own the polling while whoever scans does so at leisure.
fn run_login_get_qr(remote: &Remote, qr_out: Option<&std::path::Path>, cli: &Cli) -> ExitCode {
    let session = match remote.begin_login() {
        Ok(session) => session,
        Err(err) => return fail("ilink login get-qr", cli, &err),
    };
    let mut image_path = None;
    if let Some(path) = qr_out {
        match remote.qr_image(&session) {
            Ok(bytes) => {
                if let Err(err) = std::fs::write(path, &bytes) {
                    return fail(
                        "ilink login get-qr",
                        cli,
                        &RemoteError::io(format!("writing {}", path.display()), err),
                    );
                }
                image_path = Some(path.display().to_string());
            }
            Err(err) => return fail("ilink login get-qr", cli, &err),
        }
    }
    let payload = serde_json::json!({
        "session": session.session_id,
        "qr_url": session.qr_url,
        "qr_image": image_path,
        "expires_in": session.remaining(),
        "note": "run `wxapkg-dump ilink login check-qr --session <session>` to poll",
    });
    emit_success("ilink login get-qr", cli.json, payload, |payload| {
        println!("{payload}");
    })
}

/// `ilink login check-qr` — RFC 8628 step two: poll once with the handle.
///
/// Long-polls up to `--wait` seconds (the upstream holds the request open anyway), then
/// reports one of: waiting / scanned / confirmed / expired / cancelled. A confirmed scan
/// is exchanged into the stored account inside this call.
fn run_login_check_qr(remote: &Remote, session_id: &str, wait: u64, cli: &Cli) -> ExitCode {
    // The handle is a pending login the store remembers; resolve it without assuming
    // this process created it.
    let session = match remote.store().pending() {
        Ok(pending) => pending
            .into_iter()
            .find(|session| session.session_id == session_id),
        Err(err) => return fail("ilink login check-qr", cli, &err),
    };
    let Some(session) = session else {
        return emit_failure(
            "ilink login check-qr",
            cli.json,
            "invalid_argument",
            &format!("unknown login session {session_id}"),
            Some("run `wxapkg-dump ilink login get-qr` first"),
        );
    };

    let deadline = Instant::now() + Duration::from_secs(wait.max(1));
    loop {
        match remote.poll(&session) {
            Ok(status) => {
                let state = status.name().to_owned();
                match status {
                    LoginStatus::Authorized { .. } => {
                        let account = remote.account().ok().flatten().unwrap_or_default();
                        let payload = serde_json::json!({
                            "status": "confirmed",
                            "nick_name": account.nick_name,
                            "openid": account.openid,
                        });
                        return emit_success(
                            "ilink login check-qr",
                            cli.json,
                            payload,
                            |payload| {
                                println!("{payload}");
                            },
                        );
                    }
                    LoginStatus::Expired | LoginStatus::Cancelled => {
                        return emit_success(
                            "ilink login check-qr",
                            cli.json,
                            serde_json::json!({ "status": state }),
                            |payload| {
                                println!("{payload}");
                            },
                        );
                    }
                    LoginStatus::Waiting | LoginStatus::Scanned => {}
                }
            }
            Err(err) => return fail("ilink login check-qr", cli, &err),
        }
        if Instant::now() >= deadline {
            return emit_success(
                "ilink login check-qr",
                cli.json,
                serde_json::json!({ "status": "waiting" }),
                |payload| {
                    println!("{payload}");
                },
            );
        }
        std::thread::sleep(Duration::from_millis(1200));
    }
}

fn report_login_state(status: &LoginStatus) {
    let message = match status {
        LoginStatus::Waiting => "waiting for the QR code to be scanned…".to_owned(),
        LoginStatus::Scanned => "scanned; confirm on your phone…".to_owned(),
        LoginStatus::Authorized { .. } => {
            "confirmed; exchanging the scan for a session…".to_owned()
        }
        LoginStatus::Expired => "expired".to_owned(),
        LoginStatus::Cancelled => "cancelled".to_owned(),
    };
    eprintln!("{}", style(message).cyan());
}

fn finish_login(remote: &Remote, cli: &Cli) -> ExitCode {
    let account = remote.account().ok().flatten().unwrap_or_default();
    let payload = serde_json::json!({
        "nick_name": account.nick_name,
        "avatar": account.avatar,
        "openid": account.openid,
    });
    emit_success("ilink login", cli.json, payload, |_| {
        println!(
            "{} {}",
            style(Key::LoginAs.format(cli.lang(), &[])).green(),
            if account.nick_name.is_empty() {
                "unknown"
            } else {
                &account.nick_name
            }
        );
    })
}

// ---------------------------------------------------------------------------
// recent / info / download / delete
// ---------------------------------------------------------------------------

fn run_recent(remote: &Remote, args: &RecentArgs, cli: &Cli) -> ExitCode {
    let list = match remote.recent(args.page, args.size) {
        Ok(list) => list,
        Err(err) => return fail("ilink recent", cli, &err),
    };

    let items = apply_recent_filters(&list, args, remote);
    let payload = serde_json::json!({ "total": list.iter_all().count(), "items": items });
    emit_success("ilink recent", cli.json, payload, |_| {
        if items.is_empty() {
            println!("{}", Key::RecentEmpty.format(cli.lang(), &[]));
            return;
        }
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(vec!["name", "appid", "username", "source", "last used"]);
        for item in &items {
            table.add_row(vec![
                Cell::new(ellipsize(&item.nick_name, 24)),
                Cell::new(&item.appid),
                Cell::new(&item.username),
                Cell::new(&item.source),
                Cell::new(format_timestamp(item.last_used_at)),
            ]);
        }
        println!("{table}");
    })
}

/// One recent entry flattened with its cached profile.
#[derive(Debug, Clone, serde::Serialize)]
struct RecentRow {
    username: String,
    appid: String,
    nick_name: String,
    icon_url: String,
    last_used_at: i64,
    source: String,
}

fn apply_recent_filters(
    list: &wxapkg_remote::RecentList,
    args: &RecentArgs,
    remote: &Remote,
) -> Vec<RecentRow> {
    let base: Vec<&RecentItem> = if args.starred {
        list.starred.iter().collect()
    } else {
        list.iter_all().collect()
    };
    base.into_iter()
        .map(|item| {
            let meta = remote.program_meta(&item.username);
            RecentRow {
                username: item.username.clone(),
                appid: meta.as_ref().map(|m| m.appid.clone()).unwrap_or_default(),
                nick_name: meta
                    .as_ref()
                    .map(|m| m.nick_name.clone())
                    .unwrap_or_default(),
                icon_url: meta.as_ref().map(|m| m.avatar.clone()).unwrap_or_default(),
                last_used_at: item.update_time,
                source: item.source.to_owned(),
            }
        })
        .collect()
}

fn run_info(remote: &Remote, args: &TargetArgs, cli: &Cli) -> ExitCode {
    let (appid, username) = args.resolve();
    if appid.is_none() && username.is_none() {
        return emit_failure(
            "ilink info",
            cli.json,
            "invalid_argument",
            "pass an id, --appid or --username to select a Mini Program",
            None,
        );
    }
    let info: MiniInfo = match remote.info(appid.as_deref(), username.as_deref()) {
        Ok(info) => info,
        Err(err) => return fail("ilink info", cli, &err),
    };
    emit_success(
        "ilink info",
        cli.json,
        info.clone(),
        move |info: &MiniInfo| {
            println!("{}", style(&info.nick_name).bold());
            println!("  appid       : {}", info.appid);
            println!("  username    : {}", info.username);
            if !info.signature.is_empty() {
                println!("  signature   : {}", info.signature);
            }
            if info.version > 0 {
                if info.client_version > 0 {
                    println!("  version     : {}({})", info.version, info.client_version);
                } else {
                    println!("  version     : {}", info.version);
                }
            }
            if info.update_time > 0 {
                println!("  updated     : {}", format_timestamp(info.update_time));
            }
            if !info.avatar.is_empty() {
                println!("  avatar      : {}", info.avatar);
            }
        },
    )
}

fn run_download(remote: &Remote, args: &DownloadArgs, cli: &Cli) -> ExitCode {
    let (appid, username) = args.target.resolve();
    if appid.is_none() && username.is_none() {
        return emit_failure(
            "ilink download",
            cli.json,
            "invalid_argument",
            "pass an id, --appid or --username to select a Mini Program",
            None,
        );
    }

    let show_bar = !cli.json;
    let bar = show_bar.then(indicatif::ProgressBar::new_spinner);
    let progress: Option<wxapkg_remote::wxa::DownloadProgress> = bar.as_ref().map(|bar| {
        let bar = bar.clone();
        let tick = move |done: usize, total: usize, module: &str| {
            let percent = (done.saturating_mul(100)) / total.max(1);
            bar.set_message(module.to_string());
            bar.set_prefix(format!("{done}/{total} ({percent}%)"));
        };
        std::sync::Arc::new(tick) as wxapkg_remote::wxa::DownloadProgress
    });
    if let Some(bar) = &bar {
        bar.set_style(
            indicatif::ProgressStyle::default_spinner()
                .template("{spinner:.green} {prefix} {msg}")
                .unwrap(),
        );
    }
    // One folder per program: <out>/original_wxapkg/<appid>/wxapkg/*.wxapkg.
    let resolved_appid = appid
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let username = username.clone().unwrap_or_default();
            (!username.trim().is_empty())
                .then(|| remote.info(None, Some(username.trim())).ok())
                .flatten()
                .map(|info| info.appid)
                .filter(|value| !value.is_empty())
        });
    let folder = resolved_appid.clone().unwrap_or_else(|| {
        username
            .clone()
            .unwrap_or_default()
            .replace(['/', '\\'], "_")
    });
    let out_dir = args
        .out
        .join("original_wxapkg")
        .join(wxapkg_core::paths::sanitize_segment(&folder))
        .join("wxapkg");
    let options = DownloadOptions {
        appid: resolved_appid.clone().unwrap_or_default(),
        username: username.unwrap_or_default(),
        out_dir: out_dir.clone(),
        all_packages: args.all_packages,
        concurrency: args.concurrency.clamp(1, 10),
        progress,
        version_type: args.version_type,
        version: args.version,
        cancel: None,
    };
    let report: DownloadReport = match remote.download(&options) {
        Ok(report) => report,
        Err(err) => return fail("ilink download", cli, &err),
    };

    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    // `--unpack` chains the toolkit's own decompiler over the freshly downloaded packages,
    // so one command yields a DevTools-openable project.
    let unpacked = args
        .unpack
        .then(|| unpack_downloaded(&report, cli, args.beautify))
        .flatten();

    let failed: Vec<String> = report
        .packages
        .iter()
        .filter(|package| !package.error.is_empty() && !package.skipped)
        .map(|package| format!("{}: {}", package.module, package.error))
        .collect();
    let code = emit_success(
        "ilink download",
        cli.json,
        report.clone(),
        move |report: &DownloadReport| {
            let mut table = Table::new();
            table
                .load_preset(UTF8_FULL)
                .set_content_arrangement(ContentArrangement::Dynamic)
                .set_header(vec!["module", "type", "version", "size", "status"]);
            for package in &report.packages {
                let status = if package.skipped {
                    "skipped (resource)".to_owned()
                } else if package.error.is_empty() {
                    "downloaded".to_owned()
                } else {
                    package.error.clone()
                };
                table.add_row(vec![
                    Cell::new(if package.module.is_empty() {
                        "(main)"
                    } else {
                        &package.module
                    }),
                    Cell::new(package.package_type),
                    Cell::new(package.version),
                    Cell::new(human_bytes(package.size)),
                    Cell::new(status),
                ]);
            }
            println!("{table}");
            println!("packages written to {}", report.out_dir);
            if let Some(dir) = &unpacked {
                println!("reconstructed project: {dir}");
            }
        },
    );

    if failed.is_empty() {
        code
    } else {
        eprintln!(
            "{}",
            style(format!("{} package(s) failed", failed.len())).yellow()
        );
        ExitCode::Upstream
    }
}

fn run_delete(remote: &Remote, args: &DeleteArgs, cli: &Cli) -> ExitCode {
    if !args.yes && !cli.json {
        eprintln!("{}", Key::DeleteNeedsYes.format(cli.lang(), &[]));
        return ExitCode::Usage;
    }
    match remote.delete_account() {
        Ok(()) => emit_success("ilink delete", cli.json, (), |_| {
            println!("{}", Key::DeleteDone.format(cli.lang(), &[]));
        }),
        Err(err) => fail("ilink delete", cli, &err),
    }
}

/// Runs the local decompiler over freshly downloaded packages.
///
/// Returns the project directory, or `None` when nothing could be reconstructed.
fn unpack_downloaded(report: &DownloadReport, cli: &Cli, beautify: bool) -> Option<String> {
    use wxapkg_core::pipeline::{unpack as run_unpack, Source, UnpackRequest};

    let usable: Vec<std::path::PathBuf> = report
        .packages
        .iter()
        .filter(|package| package.error.is_empty() && !package.saved_path.is_empty())
        .map(|package| std::path::PathBuf::from(&package.saved_path))
        .collect();
    if usable.is_empty() {
        eprintln!(
            "{}",
            style("nothing to unpack: no package downloaded successfully").yellow()
        );
        return None;
    }

    // Containers live in `<out>/original_wxapkg`; the project goes next to them inside the
    // program's own folder: `<out>/<appid>/decompile_source`.
    // The project sits next to the containers: <appid>/wxapkg/unpack_<appid>/.
    let base = std::path::PathBuf::from(&report.out_dir);
    let mut request = UnpackRequest::new(Source::Paths(usable), base);
    request.wxid = (!report.appid.is_empty()).then(|| report.appid.clone());
    if !report.appid.is_empty() {
        request.project_folder = Some(format!("unpack_{}", report.appid));
    }
    request.clean = true;
    if beautify {
        request.options.format = Some(wxapkg_core::beautify::FormatOptions::default());
    }

    match run_unpack(&request) {
        Ok(build) => {
            if !cli.json {
                println!(
                    "reconstructed {} files ({} warnings)",
                    build.total_files(),
                    build.warnings.len()
                );
            }
            Some(build.out_dir)
        }
        Err(err) => {
            eprintln!(
                "{}",
                style(format!("unpack after download failed: {err}")).red()
            );
            None
        }
    }
}

fn format_timestamp(seconds: i64) -> String {
    if seconds <= 0 {
        return "-".to_owned();
    }
    // Timestamps arrive as Unix seconds; the table shows them in the operator's own zone,
    // which is what every other tool in their terminal does.
    match chrono::DateTime::from_timestamp(seconds, 0) {
        Some(utc) => utc
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => "-".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_render_as_local_dates() {
        assert_eq!(format_timestamp(0), "-");
        // The rendering is zone-dependent, so only the shape is asserted here.
        let rendered = format_timestamp(1_700_000_000);
        assert_eq!(rendered.len(), 19, "{rendered}");
        assert!(rendered.starts_with("2023-11-1"), "{rendered}");
    }
}
