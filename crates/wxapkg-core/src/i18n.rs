// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Language selection and the message catalogue.
//!
//! Three surfaces produce text for a person: the command line, the HTTP API and the web
//! console. They share one catalogue rather than carrying three sets of strings, because the
//! alternative is that a message gets fixed in one place and stays wrong in the other two.
//!
//! What is deliberately *not* translated: error codes, JSON field names and log lines. The
//! first two are a machine contract and must be identical in every language; the third is read
//! by whoever triages an issue, and a single language is easier to search than two.
//!
//! Every entry is declared as a pair inside an exhaustive `match`, so a key cannot be added
//! without both translations — the compiler enforces what a test would only catch later.

use std::fmt::Write as _;

use crate::Error;

/// A language the catalogue has translations for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    /// Simplified Chinese. The fallback when detection fails.
    #[default]
    ZhHans,
    /// English.
    En,
}

impl Lang {
    /// Environment variable that selects the language.
    pub const ENV: &'static str = "WXAPKG_LANG";

    /// Environment variables consulted, in order of decreasing precedence.
    ///
    /// `LC_ALL` wins over `LC_MESSAGES`, which wins over `LANG`; this is the POSIX precedence
    /// order and what every other command line tool follows.
    const LOCALE_ENV: [&'static str; 3] = ["LC_ALL", "LC_MESSAGES", "LANG"];

    /// Tag reported in output and accepted as an override.
    pub fn code(self) -> &'static str {
        match self {
            Self::ZhHans => "zh-Hans",
            Self::En => "en",
        }
    }

    /// Parses a language tag, returning `None` when it names no language we support.
    ///
    /// Accepts what locale variables actually contain: `zh_CN.UTF-8`, `zh-Hans-CN`, `en_US`,
    /// `C`, `POSIX`, and an empty string. Anything starting with `zh` is Chinese — `zh_TW` and
    /// `zh_HK` included, since a Chinese reader is far better served by Simplified Chinese than
    /// by English.
    pub fn from_tag(tag: &str) -> Option<Self> {
        let tag = tag.trim();
        if tag.is_empty() {
            return None;
        }
        // `LANG=C` / `POSIX` mean "no locale chosen", not "English".
        if tag.eq_ignore_ascii_case("C") || tag.eq_ignore_ascii_case("POSIX") {
            return None;
        }
        let primary = tag
            .split(['_', '-', '.', '@'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match primary.as_str() {
            "zh" => Some(Self::ZhHans),
            "" => None,
            // Every other well-formed tag is served by English.
            _ => Some(Self::En),
        }
    }

    /// Resolves the language to use.
    ///
    /// Precedence: an explicit override, then [`Lang::ENV`], then the POSIX locale variables,
    /// then the fallback. **Detection failure falls back to Chinese**, not English: a user who
    /// has expressed no preference gets the language the project is written for.
    pub fn resolve(explicit: Option<&str>) -> Self {
        Self::resolve_impl(
            explicit,
            |key| std::env::var_os(key).and_then(|value| value.to_str().map(str::to_owned)),
            None,
        )
    }

    /// Resolves the language with the platform's display locale supplied by the caller.
    ///
    /// The CLI passes `sys_locale::get_locale()` here: on Windows there are no POSIX
    /// variables, and a terminal that happens to export `LANG=en_US` must not override a
    /// Chinese display language. An explicit `--lang` or `WXAPKG_LANG` still wins.
    pub fn resolve_with_locale(explicit: Option<&str>, os_locale: Option<&str>) -> Self {
        Self::resolve_impl(
            explicit,
            |key| std::env::var_os(key).and_then(|value| value.to_str().map(str::to_owned)),
            os_locale.map(str::to_owned),
        )
    }

    /// [`Lang::resolve`] with the environment injected, for deterministic tests.
    #[cfg(test)]
    fn resolve_with(explicit: Option<&str>, env: impl Fn(&str) -> Option<String>) -> Self {
        Self::resolve_impl(explicit, env, None)
    }

    /// [`Lang::resolve`] with both the environment and the OS locale injected.
    #[cfg(test)]
    fn resolve_with_os(
        explicit: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
        os: Option<String>,
    ) -> Self {
        Self::resolve_impl(explicit, env, os)
    }

    fn resolve_impl(
        explicit: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
        os: Option<String>,
    ) -> Self {
        if let Some(lang) = explicit.and_then(Self::from_tag) {
            return lang;
        }
        if let Some(lang) = env(Self::ENV).as_deref().and_then(Self::from_tag) {
            return lang;
        }
        if let Some(lang) = os.as_deref().and_then(Self::from_tag) {
            return lang;
        }
        for key in Self::LOCALE_ENV {
            if let Some(lang) = env(key).as_deref().and_then(Self::from_tag) {
                return lang;
            }
        }
        Self::default()
    }

    /// Whether a client's `Accept-Language` header asks for Chinese.
    ///
    /// Only the primary subtag of the first entry is considered. Full quality-value negotiation
    /// would be over-engineering for a two-language catalogue, but ignoring the header entirely
    /// would be worse: a browser that asks for English should get English.
    pub fn from_accept_language(header: &str) -> Option<Self> {
        header.split(',').find_map(|entry| {
            let tag = entry.split(';').next().unwrap_or_default();
            Self::from_tag(tag)
        })
    }
}

/// A message in the catalogue.
///
/// The variant name is the key; the translations live in the table below, one exhaustive
/// `match` arm per variant so neither language can be left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Key {
    /// `Io`
    IoError,
    /// `PlainIo`
    PlainIoError,
    /// `InvalidContainer`
    InvalidContainer,
    /// `MissingDecryptionKey`
    MissingDecryptionKey,
    /// `DecryptionFailed`
    DecryptionFailed,
    /// `Archive`
    ArchiveError,
    /// `Json`
    JsonError,
    /// `Regex`
    RegexError,
    /// `EntryNotFound`
    EntryNotFound,
    /// `ProgramNotFound`, no version named.
    ProgramNotFound,
    /// `ProgramNotFound`, a specific version was requested.
    ProgramNotFoundWithVersion,
    /// `InvalidArgument`
    InvalidArgument,
    /// `Unsupported`
    Unsupported,

    /// Hint: list the installations to find a valid appid.
    HintRunScan,
    /// Hint: supply the decryption key explicitly.
    HintPassWxid,
    /// Hint: pass a path instead of an appid.
    HintPassPath,
    /// Hint: the container may be truncated or not a container at all.
    HintCheckContainer,
    /// Hint: log in again.
    HintLoginAgain,
    /// Hint: retry later, the network failed.
    HintRetryLater,
    /// Scan summary: how many programs and bundles were found.
    ScanSummary,
    /// remote status: implementation row.
    StatusImplementation,
    /// remote status: storage row.
    StatusStorage,
    /// remote status: the signed-in account row (takes the name).
    StatusAccount,
    /// remote status: no account signed in.
    StatusNoAccount,
    /// remote status: session state row (takes the label).
    StatusState,
    /// remote recent: the list is empty.
    RecentEmpty,
    /// remote login: logged in as (takes the name).
    LoginAs,
    /// remote delete: the account is gone.
    DeleteDone,
    /// remote delete: refusing without --yes.
    DeleteNeedsYes,
    /// Account state: online.
    StateOnline,
    /// Account state: the session expired.
    StateStale,
    /// Account state: offline.
    StateOffline,
    /// Session expired headline.
    SessionExpired,
    /// Not signed in headline.
    NotSignedIn,
    /// Hint: the account may not have access to this Mini Program.
    HintCheckPermission,

    /// CLI: the walk could not read some paths.
    WarnSkippedPaths,
    /// CLI: nothing was found in the searched directories.
    ScanNothingFound,
    /// CLI: the search roots are configurable.
    HintPassPathRoot,
}

impl Key {
    /// Both translations, Chinese first.
    ///
    /// Exhaustive on purpose: adding a variant without filling this in does not compile.
    fn texts(self) -> (&'static str, &'static str) {
        match self {
            Self::IoError => (
                "读取 {path} 失败（详情：{source}）",
                "cannot read {path} (detail: {source})",
            ),
            Self::PlainIoError => (
                "输入输出错误（详情：{source}）",
                "I/O error (detail: {source})",
            ),
            Self::InvalidContainer => (
                "{path} 不是合法的 wxapkg 容器（详情：{reason}）",
                "{path} is not a valid wxapkg container (detail: {reason})",
            ),
            Self::MissingDecryptionKey => (
                "{path} 是 V1MMWX 加密容器，但没有可用的密钥（详情：{reason}）",
                "{path} is a V1MMWX container and no usable key was found (detail: {reason})",
            ),
            Self::DecryptionFailed => (
                "解密 {path} 后仍不是合法容器（详情：{reason}）",
                "decrypting {path} did not produce a usable container (detail: {reason})",
            ),
            Self::ArchiveError => (
                "读取归档 {path} 失败（详情：{reason}）",
                "cannot read the archive {path} (detail: {reason})",
            ),
            Self::JsonError => (
                "JSON 解析失败（详情：{source}）",
                "invalid JSON (detail: {source})",
            ),
            Self::RegexError => (
                "正则表达式 {pattern} 非法（详情：{reason}）",
                "invalid pattern {pattern} (detail: {reason})",
            ),
            Self::EntryNotFound => (
                "{path} 中没有条目 {name}",
                "no entry named {name} in {path}",
            ),
            Self::ProgramNotFound => (
                "本机找不到小程序 {appid}",
                "mini program {appid} was not found on this machine",
            ),
            Self::ProgramNotFoundWithVersion => (
                "本机找不到小程序 {appid} 的版本 {version}",
                "mini program {appid} has no installed version {version}",
            ),
            Self::InvalidArgument => ("参数不合法：{source}", "invalid argument: {source}"),
            Self::Unsupported => ("不支持：{source}", "unsupported: {source}"),

            Self::HintRunScan => (
                "先运行 `wxapkg-dump scan` 看本机装了哪些小程序",
                "run `wxapkg-dump scan` to list what is installed on this machine",
            ),
            Self::HintPassWxid => (
                "用 --wxid 指定该小程序的 appid 作为密钥",
                "pass --wxid with the appid of the Mini Program",
            ),
            Self::HintPassPath => ("改为直接指定容器路径", "pass the container path instead"),
            Self::HintCheckContainer => (
                "确认容器完整；被截断的文件无法解析",
                "check that the container is complete; a truncated file cannot be parsed",
            ),
            Self::HintLoginAgain => (
                "运行 `wxapkg-dump login` 重新扫码登录",
                "run `wxapkg-dump login` to scan the code again",
            ),
            Self::HintRetryLater => (
                "检查网络或代理设置后重试",
                "check the network and proxy settings, then retry",
            ),
            Self::ScanSummary => (
                "{programs} 个小程序，{bundles} 个包",
                "{programs} program(s), {bundles} bundle(s)",
            ),
            Self::StatusImplementation => ("实现", "implementation"),
            Self::StatusStorage => ("存储", "storage"),
            Self::StatusAccount => ("账号", "account"),
            Self::StatusNoAccount => ("（未登录）", "(not signed in)"),
            Self::StatusState => ("状态", "state"),
            Self::RecentEmpty => (
                "该账号没有最近使用记录",
                "no recently used Mini Program was reported by the account",
            ),
            Self::LoginAs => ("登录成功：", "logged in as"),
            Self::DeleteDone => ("账号已删除", "account deleted"),
            Self::DeleteNeedsYes => (
                "拒绝删除账号：缺少 --yes；该操作会丢弃会话与全部相关缓存",
                "refusing to delete the account without --yes; this drops the session and \
                 every cache tied to it",
            ),
            Self::StateOnline => ("在线", "online"),
            Self::StateStale => ("需重新登录", "sign-in expired"),
            Self::StateOffline => ("离线", "offline"),
            Self::SessionExpired => (
                "登录已过期，请重新扫码登录",
                "The session has expired; scan the QR code again",
            ),
            Self::NotSignedIn => (
                "尚未登录，请先扫码登录",
                "Not signed in; scan the QR code first",
            ),
            Self::HintCheckPermission => (
                "确认该账号有这个小程序的权限",
                "check that this account has access to the Mini Program",
            ),

            Self::WarnSkippedPaths => (
                "有 {count} 个路径读不到，列表可能不完整",
                "{count} path(s) could not be read; the listing may be incomplete",
            ),
            Self::ScanNothingFound => (
                "搜索目录里没有找到任何 wxapkg 容器",
                "no wxapkg container was found in the scanned directories",
            ),
            Self::HintPassPathRoot => (
                "用 --path 指定微信数据目录",
                "pass --path with the WeChat data directory to search elsewhere",
            ),
        }
    }

    /// The translated text.
    pub fn get(self, lang: Lang) -> &'static str {
        let (zh, en) = self.texts();
        match lang {
            Lang::ZhHans => zh,
            Lang::En => en,
        }
    }

    /// The translated text with `{name}` placeholders substituted.
    ///
    /// A placeholder with no value is left as-is rather than replaced with an empty string:
    /// seeing `{path}` in a message is an obvious bug report, whereas "cannot read : " looks
    /// like a real but confusing failure.
    pub fn format(self, lang: Lang, args: &[(&str, &str)]) -> String {
        let mut text = self.get(lang).to_owned();
        for (name, value) in args {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
    }
}

impl Error {
    /// The message to show a person, in `lang`.
    ///
    /// [`Display`](std::fmt::Display) stays English and is what logs and backtraces carry; this
    /// is the presentation layer, and it is the only place a translation appears.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::Io { path, source } => Key::IoError.format(
                lang,
                &[
                    ("path", &path.display().to_string()),
                    ("source", &source.to_string()),
                ],
            ),
            Self::PlainIo(source) => {
                Key::PlainIoError.format(lang, &[("source", &source.to_string())])
            }
            Self::InvalidContainer { path, reason } => {
                Key::InvalidContainer.format(lang, &[("path", path), ("reason", reason)])
            }
            Self::MissingDecryptionKey { path, reason } => {
                Key::MissingDecryptionKey.format(lang, &[("path", path), ("reason", reason)])
            }
            Self::DecryptionFailed { path, reason } => {
                Key::DecryptionFailed.format(lang, &[("path", path), ("reason", reason)])
            }
            Self::Archive { path, reason } => {
                Key::ArchiveError.format(lang, &[("path", path), ("reason", reason)])
            }
            Self::Json(source) => Key::JsonError.format(lang, &[("source", &source.to_string())]),
            Self::Regex { pattern, reason } => {
                Key::RegexError.format(lang, &[("pattern", pattern), ("reason", reason)])
            }
            Self::EntryNotFound { name, path } => {
                Key::EntryNotFound.format(lang, &[("name", name), ("path", path)])
            }
            Self::ProgramNotFound { appid, version } => match version {
                Some(version) => Key::ProgramNotFoundWithVersion
                    .format(lang, &[("appid", appid), ("version", version)]),
                None => Key::ProgramNotFound.format(lang, &[("appid", appid)]),
            },
            Self::InvalidArgument(text) => Key::InvalidArgument.format(lang, &[("source", text)]),
            Self::Unsupported(text) => Key::Unsupported.format(lang, &[("source", text)]),
        }
    }

    /// An actionable next step, when one exists.
    ///
    /// Deliberately `Option`: a hint that says "check your input" is noise. Only errors the user
    /// can act on get one.
    pub fn hint(&self, lang: Lang) -> Option<String> {
        let key = match self {
            Self::ProgramNotFound { .. } => Key::HintRunScan,
            Self::MissingDecryptionKey { .. } => Key::HintPassWxid,
            Self::DecryptionFailed { .. } | Self::InvalidContainer { .. } => {
                Key::HintCheckContainer
            }
            Self::EntryNotFound { .. } => Key::HintPassPath,
            _ => return None,
        };
        Some(key.get(lang).to_owned())
    }
}

/// Renders a `(message, hint)` pair the way the JSON envelope and the console both want it.
pub fn describe(lang: Lang, err: &Error) -> (String, Option<String>) {
    (err.message(lang), err.hint(lang))
}

/// Writes a translated line, for callers building larger messages.
pub fn push_line(out: &mut String, lang: Lang, key: Key, args: &[(&str, &str)]) {
    let _ = writeln!(out, "{}", key.format(lang, args));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_tags_resolve_to_chinese() {
        for tag in ["zh", "zh_CN.UTF-8", "zh-Hans-CN", "zh_TW", "zh-HK", "ZH_cn"] {
            assert_eq!(
                Lang::from_tag(tag),
                Some(Lang::ZhHans),
                "{tag} should be served by Chinese"
            );
        }
    }

    #[test]
    fn other_tags_resolve_to_english() {
        for tag in ["en", "en_US.UTF-8", "de_DE", "ja_JP.UTF-8", "fr"] {
            assert_eq!(
                Lang::from_tag(tag),
                Some(Lang::En),
                "{tag} should be English"
            );
        }
    }

    #[test]
    fn the_windows_display_language_beats_a_stray_lang_variable() {
        let env = |key: &str| (key == "LANG").then(|| "en_US.UTF-8".to_owned());
        assert_eq!(
            Lang::resolve_with_os(None, env, Some("zh-CN".to_owned())),
            Lang::ZhHans
        );
        // An explicit choice still wins over the OS.
        assert_eq!(
            Lang::resolve_with_os(Some("en"), env, Some("zh-CN".to_owned())),
            Lang::En
        );
    }

    #[test]
    fn an_unchosen_locale_is_not_english() {
        // `LANG=C` means "no preference"; it must not be read as English.
        for tag in ["C", "POSIX", "", "   "] {
            assert_eq!(Lang::from_tag(tag), None, "{tag:?} names no language");
        }
    }

    #[test]
    fn detection_failure_falls_back_to_chinese() {
        // A machine with nothing set at all is the case the fallback exists for.
        assert_eq!(Lang::default(), Lang::ZhHans);
        assert_eq!(Lang::resolve_with(None, |_| None), Lang::ZhHans);
        // Variables that name no supported language are skipped, not treated as English.
        assert_eq!(
            Lang::resolve_with(None, |_| Some("C".to_owned())),
            Lang::ZhHans
        );
        assert_eq!(
            Lang::resolve_with(None, |_| Some(String::new())),
            Lang::ZhHans
        );
    }

    #[test]
    fn an_explicit_override_wins() {
        let always_english = |_: &str| Some("en_US.UTF-8".to_owned());
        assert_eq!(
            Lang::resolve_with(Some("zh-CN"), always_english),
            Lang::ZhHans
        );
        let no_env = |_: &str| None;
        assert_eq!(Lang::resolve_with(Some("en-GB"), no_env), Lang::En);
        // An unusable override falls through rather than failing.
        assert_eq!(Lang::resolve_with(Some("C"), no_env), Lang::ZhHans);
    }

    #[test]
    fn the_environment_is_consulted_in_posix_order() {
        // LC_ALL beats LC_MESSAGES beats LANG.
        let env = |key: &str| match key {
            "LC_ALL" => Some("en_GB.UTF-8".to_owned()),
            "LC_MESSAGES" => Some("zh_CN.UTF-8".to_owned()),
            "LANG" => Some("zh_CN.UTF-8".to_owned()),
            _ => None,
        };
        assert_eq!(Lang::resolve_with(None, env), Lang::En);

        let env = |key: &str| match key {
            "LC_MESSAGES" => Some("en_GB.UTF-8".to_owned()),
            "LANG" => Some("zh_CN.UTF-8".to_owned()),
            _ => None,
        };
        assert_eq!(Lang::resolve_with(None, env), Lang::En);

        let env = |key: &str| match key {
            "LANG" => Some("en_GB.UTF-8".to_owned()),
            _ => None,
        };
        assert_eq!(Lang::resolve_with(None, env), Lang::En);

        // WXAPKG_LANG beats every locale variable.
        let env = |key: &str| match key {
            "WXAPKG_LANG" => Some("zh-Hans".to_owned()),
            "LC_ALL" => Some("en_US.UTF-8".to_owned()),
            _ => None,
        };
        assert_eq!(Lang::resolve_with(None, env), Lang::ZhHans);
    }

    #[test]
    fn accept_language_uses_the_first_understood_entry() {
        assert_eq!(
            Lang::from_accept_language("en-US,en;q=0.9,zh-CN;q=0.8"),
            Some(Lang::En)
        );
        assert_eq!(
            Lang::from_accept_language("zh-CN,zh;q=0.9"),
            Some(Lang::ZhHans)
        );
        assert_eq!(Lang::from_accept_language(""), None);
    }

    #[test]
    fn placeholders_are_substituted_by_name() {
        let text = Key::InvalidContainer.format(
            Lang::ZhHans,
            &[
                ("path", "app.wxapkg"),
                ("reason", "entry table overruns the file"),
            ],
        );
        assert!(text.contains("app.wxapkg"), "{text}");
        assert!(text.contains("entry table overruns the file"), "{text}");
        assert!(!text.contains("{path}"), "{text}");
        assert!(!text.contains("{reason}"), "{text}");
    }

    #[test]
    fn a_missing_argument_leaves_a_visible_placeholder() {
        // Better an obviously broken message than a plausible one with a hole in it.
        let text = Key::IoError.format(Lang::En, &[("path", "/tmp/x")]);
        assert!(text.contains("{source}"), "{text}");
    }

    #[test]
    fn every_key_has_both_translations() {
        // Guards against a copy-paste that fills the English slot by accident.
        for key in [
            Key::IoError,
            Key::PlainIoError,
            Key::InvalidContainer,
            Key::MissingDecryptionKey,
            Key::DecryptionFailed,
            Key::ArchiveError,
            Key::JsonError,
            Key::RegexError,
            Key::EntryNotFound,
            Key::ProgramNotFound,
            Key::ProgramNotFoundWithVersion,
            Key::InvalidArgument,
            Key::Unsupported,
            Key::HintRunScan,
            Key::HintPassWxid,
            Key::HintPassPath,
            Key::HintCheckContainer,
            Key::HintLoginAgain,
            Key::HintRetryLater,
            Key::ScanSummary,
            Key::StatusImplementation,
            Key::StatusStorage,
            Key::StatusAccount,
            Key::StatusNoAccount,
            Key::StatusState,
            Key::RecentEmpty,
            Key::LoginAs,
            Key::DeleteDone,
            Key::DeleteNeedsYes,
            Key::StateOnline,
            Key::StateStale,
            Key::StateOffline,
            Key::SessionExpired,
            Key::NotSignedIn,
            Key::HintCheckPermission,
            Key::WarnSkippedPaths,
            Key::ScanNothingFound,
            Key::HintPassPathRoot,
        ] {
            let (zh, en) = key.texts();
            assert!(!zh.trim().is_empty(), "{key:?} has no Chinese text");
            assert!(!en.trim().is_empty(), "{key:?} has no English text");
            assert_ne!(zh, en, "{key:?} has the same text in both languages");
            // The placeholder set must match, or one language will print a raw brace.
            for placeholder in [
                "{path}",
                "{reason}",
                "{appid}",
                "{count}",
                "{source}",
                "{name}",
                "{version}",
            ] {
                assert_eq!(
                    zh.contains(placeholder),
                    en.contains(placeholder),
                    "{key:?} uses {placeholder} in only one language"
                );
            }
        }
    }

    #[test]
    fn an_error_renders_in_both_languages() {
        let err = Error::ProgramNotFound {
            appid: "wx0000000000000001".to_owned(),
            version: None,
        };
        let zh = err.message(Lang::ZhHans);
        let en = err.message(Lang::En);
        assert!(zh.contains("wx0000000000000001") && en.contains("wx0000000000000001"));
        assert_ne!(zh, en);
        // The hint is translated too, and the code is not.
        assert_ne!(err.hint(Lang::ZhHans), err.hint(Lang::En));
        assert_eq!(err.code(), "program_not_found");
    }

    #[test]
    fn a_named_version_appears_in_both_languages() {
        let err = Error::ProgramNotFound {
            appid: "wx0000000000000001".to_owned(),
            version: Some("364".to_owned()),
        };
        for lang in [Lang::ZhHans, Lang::En] {
            let text = err.message(lang);
            assert!(text.contains("wx0000000000000001"), "{text}");
            assert!(text.contains("364"), "{text}");
        }
        assert_ne!(err.message(Lang::ZhHans), err.message(Lang::En));
    }

    #[test]
    fn no_message_mixes_two_languages() {
        // The regression this guards: an interpolated explanation used to be rendered in
        // English at the raise site and spliced into a Chinese sentence.
        let error = Error::ProgramNotFound {
            appid: "wx1".to_owned(),
            version: None,
        };
        let text = error.message(Lang::ZhHans);
        assert!(
            !text.contains("was not found") && !text.contains("no installation"),
            "an English fragment leaked into the Chinese message: {text}"
        );
    }
}
