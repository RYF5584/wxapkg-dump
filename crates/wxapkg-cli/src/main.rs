// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! `wxapkg-dump` — command line interface for wxapkg-dump.
//!
//! ```text
//! wxapkg-dump scan                       # list Mini Programs installed by the WeChat client
//! wxapkg-dump info  ./main.wxapkg        # structural summary of a container
//! wxapkg-dump unpack ./main.wxapkg --raw out  # dump container contents verbatim
//! wxapkg-dump unpack --appid wx... -o out     # reconstruct a DevTools-openable project
//! wxapkg-dump beautify ./out                  # reformat a reconstructed tree
//! wxapkg-dump login                           # authorized remote session over iLink
//! ```
//!
//! Every subcommand supports `--json` and emits the same envelope shape, which makes
//! the tool safe to drive from scripts and from AI agents.

mod commands;
mod exit;
mod output;

use std::process::ExitCode as ProcExit;

use clap::{Args, Parser, Subcommand, ValueEnum};

use exit::ExitCode;

/// WeChat Mini Program package toolkit: local extraction, decompilation, beautification
/// and authorized remote download.
#[derive(Debug, Parser)]
#[command(
    name = "wxapkg-dump",
    version,
    about = "WeChat Mini Program package toolkit",
    long_about = "Extract, decompile and beautify WeChat Mini Program packages, either from \
                  the local WeChat client cache or by downloading them from the App Store \
                  with an authorized session.",
    propagate_version = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Emit machine readable JSON instead of human oriented output.
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress informational output.
    #[arg(long, short, global = true)]
    pub quiet: bool,

    /// Increase log verbosity (`-v`, `-vv`).
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Disable coloured output.
    #[arg(long, global = true, env = "NO_COLOR")]
    pub no_color: bool,

    /// Run the Model Context Protocol server over stdin/stdout.
    #[arg(long)]
    pub mcp: bool,

    /// Run the HTTP API and its interactive documentation instead of a subcommand.
    #[arg(long)]
    pub server: bool,

    /// Start the desktop shell (wxapkg-dump-gui), optionally with the public HTTP API.
    #[arg(long)]
    pub gui: bool,

    /// Language for messages: `zh-Hans` or `en`. Overrides WXAPKG_LANG and the locale.
    ///
    /// With nothing set, a Chinese locale is served in Chinese and every other locale in
    /// English; a machine where the locale cannot be determined gets Chinese.
    #[arg(long, global = true, value_name = "TAG")]
    pub lang: Option<String>,

    /// Subcommand to run. Omitted when `--server` or `--gui` is used.
    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// Language to render messages in, resolved once per invocation.
    pub fn lang(&self) -> wxapkg_core::i18n::Lang {
        wxapkg_core::i18n::Lang::resolve_with_locale(
            self.lang.as_deref(),
            sys_locale::get_locale().as_deref(),
        )
    }
}

/// Available subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// List Mini Programs installed by the WeChat client on this machine.
    #[command(alias = "list")]
    Scan(ScanArgs),

    /// Print the structure of one or more containers.
    Info(InfoArgs),

    /// Reconstruct a project the WeChat DevTools can open.
    Unpack(UnpackArgs),

    /// Reformat the sources of a reconstructed project.
    Beautify(BeautifyArgs),

    /// Validate a reconstructed project and report whether it will compile.
    Verify(VerifyArgs),

    /// Print the toolkit's build and environment information.
    Doctor,

    /// Show the signed-in remote account.
    #[cfg(feature = "remote")]
    AccountStatus,

    /// Log in with a WeChat scan; a confirmed login replaces the current account.
    #[cfg(feature = "remote")]
    Login {
        #[command(subcommand)]
        mode: Option<commands::ilink::LoginMode>,

        #[command(flatten)]
        args: commands::ilink::LoginArgs,
    },

    /// List recently used Mini Programs.
    #[cfg(feature = "remote")]
    Recent(commands::ilink::RecentArgs),

    /// Fetch a Mini Program's basic attributes from the remote service.
    #[cfg(feature = "remote")]
    RemoteInfo(commands::ilink::TargetArgs),

    /// Download a Mini Program's packages from the remote service.
    #[cfg(feature = "remote")]
    Download(commands::ilink::DownloadArgs),

    /// Delete the remote account and every cache tied to it.
    #[cfg(feature = "remote")]
    AccountDelete(commands::ilink::DeleteArgs),

    /// Run the HTTP API server (same as the global --server flag).
    Serve {
        /// Address to bind; supports -H/--host and -p/--port as alternatives.
        #[arg(long, value_name = "ADDR")]
        bind: Option<String>,

        /// Host to bind (alternative to --bind).
        #[arg(short = 'H', long, value_name = "HOST")]
        host: Option<String>,

        /// Port to bind (alternative to --bind).
        #[arg(short = 'p', long, value_name = "PORT")]
        port: Option<u16>,

        /// Required x-admin-key when binding to a non-loopback address.
        #[arg(long, value_name = "KEY")]
        admin_key: Option<String>,

        /// Explicitly allow binding to a non-loopback address.
        #[arg(long)]
        allow_remote: bool,

        /// Directory restored projects default to.
        #[arg(long, value_name = "DIR")]
        workspace: Option<std::path::PathBuf>,
    },

    /// Generate a shell completion script for this binary.
    Completions {
        /// Shell to generate the script for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

/// Arguments shared by every container-consuming subcommand.
#[derive(Debug, Args, Clone)]
pub struct InputArgs {
    /// `.wxapkg` files, ZIP archives or directories to read; may be repeated.
    #[arg(value_name = "INPUT")]
    pub input: Vec<std::path::PathBuf>,

    /// Decryption key for `V1MMWX` containers (`wx` + 16 hex digits).
    ///
    /// Normally discovered from the file path, which contains the appid.
    #[arg(long)]
    pub wxid: Option<String>,
}

/// `scan` arguments.
#[derive(Debug, Args)]
#[command(disable_version_flag = true)]
pub struct ScanArgs {
    /// Directories to scan; the platform defaults are added unless one is given.
    #[arg(value_name = "DIR")]
    pub inputs: Vec<std::path::PathBuf>,

    /// Additional search roots; the platform defaults are always included.
    #[arg(long = "path", value_name = "DIR")]
    pub paths: Vec<std::path::PathBuf>,

    /// Only report this appid.
    #[arg(long)]
    pub appid: Option<String>,

    /// Stop after this many programs.
    #[arg(long)]
    pub limit: Option<usize>,

    /// List the bundles of each program.
    #[arg(long)]
    pub packages: bool,
}

/// `info` arguments.
#[derive(Debug, Args)]
pub struct InfoArgs {
    /// Container inputs.
    #[command(flatten)]
    pub input: InputArgs,

    /// List every entry inside each container.
    #[arg(long)]
    pub entries: bool,
}

/// `unpack` arguments.
#[derive(Debug, Args)]
#[command(disable_version_flag = true)]
pub struct UnpackArgs {
    /// Container inputs; not required when `--appid` is given.
    #[arg(value_name = "INPUT")]
    pub input: Vec<std::path::PathBuf>,

    /// Output directory; defaults to the current directory.
    ///
    /// A folder input holding several programs restores each into `<out>/<appid>/`.
    #[arg(short, long, value_name = "DIR")]
    pub out: Option<std::path::PathBuf>,

    /// Export container contents verbatim into this directory instead of restoring.
    /// Works with explicit inputs or with `--appid`.
    #[arg(long, value_name = "DIR", conflicts_with = "main")]
    pub raw: Option<std::path::PathBuf>,

    /// With --raw: zlib-inflate entries that look compressed.
    #[arg(long, requires = "raw")]
    pub auto_inflate: bool,

    /// Locate the package of this appid in the local WeChat data directories.
    #[arg(long)]
    pub appid: Option<String>,

    /// Restrict `--appid` to a specific version directory.
    #[arg(long)]
    pub version: Option<String>,

    /// Decryption key for `V1MMWX` containers.
    #[arg(long)]
    pub wxid: Option<String>,

    /// Explicit main package file name.
    #[arg(long)]
    pub main: Option<String>,

    /// Empty the output directory before writing.
    #[arg(long)]
    pub clean: bool,

    /// Do not keep the original compiled bundles under `_raw/`.
    #[arg(long)]
    pub no_raw: bool,

    /// Skip WXML reconstruction.
    #[arg(long)]
    pub no_wxml: bool,

    /// Leave `require()` specifiers untouched.
    #[arg(long)]
    pub no_require_rewrite: bool,

    /// Do not drop pages or sub-packages without sources.
    #[arg(long)]
    pub no_prune: bool,

    /// Do not synthesise `<view />` shells for missing WXML.
    #[arg(long)]
    pub no_synthesize: bool,

    /// Do not clean up plugin declarations and `plugin://` references.
    #[arg(long)]
    pub no_plugin_clean: bool,

    /// Disable every automatic repair pass (faithful mode).
    #[arg(long)]
    pub no_auto_fix: bool,

    /// How to treat plugin declarations in `app.json`.
    #[arg(long, value_enum, default_value_t = PluginsModeArg::Keep)]
    pub plugins: PluginsModeArg,

    /// Suffix appended to every reconstructed style selector.
    #[arg(long, default_value = "")]
    pub wxss_suffix: String,

    /// Beautify every reconstructed source file.
    #[arg(long)]
    pub beautify: bool,

    /// Use tabs instead of spaces when beautifying.
    #[arg(long, requires = "beautify")]
    pub use_tabs: bool,

    /// Indent width used when beautifying.
    #[arg(long, default_value_t = 2, requires = "beautify")]
    pub indent: usize,

    /// Treat warnings as a failure (exit code 1).
    #[arg(long)]
    pub strict: bool,

    /// Maximum number of warnings printed before truncating.
    #[arg(long, default_value_t = 20)]
    pub max_warnings: usize,
}

/// Plugin declaration handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PluginsModeArg {
    /// Keep the declarations.
    Keep,
    /// Keep the key but clear the map.
    Placeholder,
    /// Remove the declarations.
    Strip,
}

/// `beautify` arguments.
#[derive(Debug, Args)]
pub struct BeautifyArgs {
    /// Project directory to format in place.
    #[arg(value_name = "DIR")]
    pub dir: std::path::PathBuf,

    /// Use tabs instead of spaces.
    #[arg(long)]
    pub use_tabs: bool,

    /// Indent width.
    #[arg(long, default_value_t = 2)]
    pub indent: usize,

    /// Preferred maximum line width.
    #[arg(long, default_value_t = 110)]
    pub max_width: usize,

    /// Do not preserve the line breaks found in the source.
    #[arg(long)]
    pub collapse_lines: bool,
}

/// `verify` arguments.
#[derive(Debug, Args)]
#[command(disable_version_flag = true)]
pub struct VerifyArgs {
    /// Project directory produced by `unpack`.
    #[arg(value_name = "DIR")]
    pub dir: std::path::PathBuf,

    /// Also run `node --check` on every JavaScript file when Node is available.
    ///
    /// The built-in pass only proves delimiters balance; this turns it into a real syntax
    /// check.
    #[arg(long)]
    pub node: bool,

    /// Also verify that every `bind*` handler named in WXML exists in the page script.
    #[arg(long)]
    pub handlers: bool,

    /// Skip the check that referenced images and fonts exist.
    #[arg(long)]
    pub no_assets: bool,

    /// Print every finding rather than the first 20 per code.
    #[arg(long)]
    pub all: bool,

    /// Treat warnings as a failure (exit code 1).
    #[arg(long)]
    pub strict: bool,
}

/// Mode selected by a leading flag, before the subcommand parser runs.
///
/// `--server` and `--gui` take their own flags (`--bind`, `--workspace`, …), which the
/// subcommand parser would reject, so they are dispatched on the raw argument vector. The
/// flags are stripped before the remaining arguments reach the server's own parser.
#[derive(Debug, Default, Clone, Copy)]
struct Mode {
    gui: bool,
    server: bool,
}

fn detect_mode(argv: &[String]) -> Option<(Mode, Vec<String>)> {
    let mut mode = Mode::default();
    let mut rest = Vec::with_capacity(argv.len());
    for argument in argv {
        match argument.as_str() {
            "--gui" => mode.gui = true,
            "--server" => mode.server = true,
            _ => rest.push(argument.clone()),
        }
    }
    (mode.gui || mode.server).then_some((mode, rest))
}

/// Locates the desktop shell that ships next to this binary.
fn desktop_shell() -> Option<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("WXAPKG_GUI") {
        let path = std::path::PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let names: &[&str] = if cfg!(windows) {
        &["wxapkg-dump-gui.exe", "wxapkg-gui.exe"]
    } else {
        &["wxapkg-dump-gui", "wxapkg-gui"]
    };
    for name in names {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // A macOS bundle keeps the executable three levels down. Accept both the current
    // product name and the pre-1.0 bundle name.
    for relative in [
        "wxapkg-dump-gui.app/Contents/MacOS/wxapkg-dump-gui",
        "wxapkg-dump.app/Contents/MacOS/wxapkg-dump",
    ] {
        let bundled = dir.join(relative);
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    None
}

/// Reads the value of a flag from an argument list that has already had the mode removed.
fn flag_value(argv: &[String], flag: &str) -> Option<String> {
    let index = argv.iter().position(|argument| argument == flag)?;
    argv.get(index + 1).cloned()
}

/// Starts the desktop shell.
///
/// `--gui --server` opens the shell *and* the public API: the address (and an optional key)
/// is handed over through the environment rather than through the window.
fn run_gui(mode: Mode, argv: Vec<String>) -> ProcExit {
    let Some(shell) = desktop_shell() else {
        eprintln!(
            "the desktop shell (wxapkg-dump-gui) was not found next to this binary; \
             install it alongside `wxapkg-dump`, or point WXAPKG_GUI at it"
        );
        return ProcExit::from(3u8);
    };
    let mut command = std::process::Command::new(&shell);
    if mode.server {
        let bind = flag_value(&argv, "--bind").unwrap_or_else(|| "127.0.0.1:8788".to_owned());
        command.env("WXAPKG_PUBLIC_API", bind);
        if let Some(key) = flag_value(&argv, "--admin-key") {
            command.env("WXAPKG_PUBLIC_KEY", key);
        }
        if let Some(workspace) = flag_value(&argv, "--workspace") {
            command.env("WXAPKG_WORKSPACE", workspace);
        }
    }
    match command.spawn() {
        Ok(_) => ProcExit::from(0u8),
        Err(err) => {
            eprintln!(
                "cannot start the desktop shell at {}: {err}",
                shell.display()
            );
            ProcExit::from(3u8)
        }
    }
}

/// Resolves the listen address from `--bind`, `--host` and `--port`.
///
/// A bare `--port` binds loopback. Requiring `--host` as well, as this used to, silently
/// dropped the port and served on the default one — the opposite of what the flag says.
fn serve_addr(bind: Option<&str>, host: Option<&str>, port: Option<u16>) -> Option<String> {
    if let Some(bind) = bind {
        return Some(bind.to_owned());
    }
    match (host, port) {
        (Some(host), Some(port)) => Some(format!("{host}:{port}")),
        (Some(host), None) => Some(format!("{host}:8788")),
        (None, Some(port)) => Some(format!("127.0.0.1:{port}")),
        (None, None) => None,
    }
}

fn run_mode(mode: Mode, argv: Vec<String>) -> ProcExit {
    // The GUI is a standalone desktop application: it never falls back to a browser, and
    // the API server is a separate mode that the shell can also host on request.
    if mode.gui {
        return run_gui(mode, argv);
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("cannot start the async runtime: {err}");
            return ProcExit::from(3u8);
        }
    };
    match runtime.block_on(wxapkg_server::command_line(argv)) {
        Ok(()) => ProcExit::from(0u8),
        Err(err) => {
            eprintln!("{err}");
            ProcExit::from(3u8)
        }
    }
}

fn main() -> ProcExit {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some((mode, rest)) = detect_mode(&argv) {
        return run_mode(mode, rest);
    }

    let cli = Cli::parse();

    if cli.mcp {
        return match wxapkg_mcp::run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{err}");
                ProcExit::from(3u8)
            }
        };
    }

    if let Some(Command::Completions { shell }) = &cli.command {
        let mut cmd = <Cli as clap::CommandFactory>::command();
        let bin = cmd.get_name().to_owned();
        clap_complete::generate(*shell, &mut cmd, bin, &mut std::io::stdout());
        return std::process::ExitCode::SUCCESS;
    }
    init_tracing(cli.verbose, cli.quiet);

    if cli.no_color {
        console::set_colors_enabled(false);
    } else {
        console::set_colors_enabled(console::user_attended());
    }

    let Some(command) = &cli.command else {
        // Nothing to do without a subcommand; point at the flags that are complete programs.
        eprintln!("no subcommand given; try `wxapkg-dump --help`, `wxapkg-dump --gui`, or `wxapkg-dump --server`");
        return ProcExit::from(2u8);
    };
    let code: ExitCode = match command {
        Command::Serve {
            ref bind,
            ref host,
            ref port,
            ref admin_key,
            ref allow_remote,
            ref workspace,
        } => {
            let mut argv = Vec::new();
            if let Some(addr) = serve_addr(bind.as_deref(), host.as_deref(), *port) {
                argv.push(format!("--bind={addr}"));
            }
            if let Some(key) = admin_key {
                argv.push(format!("--admin-key={key}"));
            }
            if *allow_remote {
                argv.push("--allow-remote".to_owned());
            }
            if let Some(dir) = workspace {
                argv.push(format!("--workspace={}", dir.display()));
            }
            return run_mode(
                Mode {
                    server: true,
                    gui: false,
                },
                argv,
            );
        }
        Command::Completions { .. } => unreachable!("handled before dispatch"),
        Command::Scan(args) => commands::cmd_scan(args, &cli),
        Command::Info(args) => commands::cmd_info(args, &cli),
        Command::Unpack(args) => commands::cmd_unpack(args, &cli),
        Command::Beautify(args) => commands::cmd_beautify(args, &cli),
        Command::Verify(args) => commands::cmd_verify(args, &cli),
        Command::Doctor => commands::cmd_doctor(&cli),
        #[cfg(feature = "remote")]
        Command::AccountStatus => commands::ilink::run(
            &commands::ilink::IlinkArgs::new(commands::ilink::IlinkCommand::Status),
            &cli,
        ),
        Command::Login { mode, args } => commands::ilink::run(
            &commands::ilink::IlinkArgs::new(commands::ilink::IlinkCommand::Login {
                mode: mode.clone(),
                args: args.clone(),
            }),
            &cli,
        ),
        Command::Recent(args) => commands::ilink::run(
            &commands::ilink::IlinkArgs::new(commands::ilink::IlinkCommand::Recent(args.clone())),
            &cli,
        ),
        Command::RemoteInfo(args) => commands::ilink::run(
            &commands::ilink::IlinkArgs::new(commands::ilink::IlinkCommand::Info(args.clone())),
            &cli,
        ),
        Command::Download(args) => commands::ilink::run(
            &commands::ilink::IlinkArgs::new(commands::ilink::IlinkCommand::Download(args.clone())),
            &cli,
        ),
        Command::AccountDelete(args) => commands::ilink::run(
            &commands::ilink::IlinkArgs::new(commands::ilink::IlinkCommand::Delete(args.clone())),
            &cli,
        ),
    };

    ProcExit::from(code.as_i32() as u8)
}

impl Cli {
    /// Beautification options requested on the command line, when any.
    ///
    /// `ilink download --unpack` reuses the unpack defaults, so the toolkit's formatting is
    /// opted into with `--beautify` on the unpack subcommand only.
    pub fn unpack_options(&self) -> Option<wxapkg_core::beautify::FormatOptions> {
        match self.command.as_ref() {
            Some(Command::Unpack(args)) if args.beautify => {
                Some(wxapkg_core::beautify::FormatOptions {
                    indent_width: args.indent,
                    use_tabs: args.use_tabs,
                    ..wxapkg_core::beautify::FormatOptions::default()
                })
            }
            _ => None,
        }
    }
}

fn init_tracing(verbosity: u8, quiet: bool) {
    use tracing_subscriber::EnvFilter;
    let level = if quiet {
        "error"
    } else {
        match verbosity {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("wxapkg-dump={level},wxapkg_core={level}")));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod serve_addr_tests {
    use super::serve_addr;

    #[test]
    fn bind_wins_over_host_and_port() {
        assert_eq!(
            serve_addr(Some("0.0.0.0:9000"), Some("127.0.0.1"), Some(7000)).as_deref(),
            Some("0.0.0.0:9000")
        );
    }

    // Regression: `serve --port 8877` used to serve on 8788 because the port was only
    // read when `--host` was also given.
    #[test]
    fn a_port_alone_binds_loopback() {
        assert_eq!(
            serve_addr(None, None, Some(8877)).as_deref(),
            Some("127.0.0.1:8877")
        );
    }

    #[test]
    fn host_alone_keeps_the_default_port() {
        assert_eq!(
            serve_addr(None, Some("0.0.0.0"), None).as_deref(),
            Some("0.0.0.0:8788")
        );
    }

    #[test]
    fn host_and_port_combine() {
        assert_eq!(
            serve_addr(None, Some("127.0.0.1"), Some(9001)).as_deref(),
            Some("127.0.0.1:9001")
        );
        assert_eq!(serve_addr(None, None, None), None);
    }
}

#[cfg(test)]
mod mode_tests {
    use super::detect_mode;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn server_and_gui_are_detected_with_their_own_flags() {
        let (mode, rest) = detect_mode(&argv(&["--server", "--bind", "127.0.0.1:9000"])).unwrap();
        assert!(mode.server && !mode.gui);
        // The mode flag must be stripped, or the server's own parser sees an unknown argument.
        assert_eq!(rest, argv(&["--bind", "127.0.0.1:9000"]));

        let (mode, rest) = detect_mode(&argv(&["--gui", "--workspace", "/tmp/x"])).unwrap();
        assert!(mode.gui && !mode.server);
        assert_eq!(rest, argv(&["--workspace", "/tmp/x"]));
    }

    #[test]
    fn gui_and_server_combine() {
        let (mode, rest) =
            detect_mode(&argv(&["--gui", "--server", "--bind", "0.0.0.0:9000"])).unwrap();
        assert!(mode.gui && mode.server);
        assert_eq!(rest, argv(&["--bind", "0.0.0.0:9000"]));
    }

    #[test]
    fn subcommands_are_not_mistaken_for_modes() {
        assert!(detect_mode(&argv(&["scan"])).is_none());
        assert!(detect_mode(&argv(&["unpack", "--appid", "wx1", "-o", "/tmp/o"])).is_none());
        // A path that merely contains the word is not the flag.
        assert!(detect_mode(&argv(&["raw", "./--server"])).is_none());
    }
}
