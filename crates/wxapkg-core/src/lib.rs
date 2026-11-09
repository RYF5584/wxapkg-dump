// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! `wxapkg-core` — container parsing, decryption, decompilation and beautification for
//! WeChat Mini Program packages.
//!
//! The crate is deliberately self-contained: it implements the container format, the
//! `wcc` conventions that Mini Programs are compiled with and a miniature evaluator for
//! the resulting template artefacts, without embedding a JavaScript engine or depending
//! on the WeChat DevTools.
//!
//! # Pipeline
//!
//! ```text
//!   .wxapkg  ──container──▶  entries  ──decompile──▶  sources  ──beautify──▶  readable tree
//!   (V1MMWX)     parse        files      bundle/wxml/wxss      format        DevTools project
//! ```
//!
//! # Entry points
//!
//! Most callers want [`pipeline`], which wires the stages together and returns
//! serialisable reports:
//!
//! ```no_run
//! use std::path::PathBuf;
//! use wxapkg_core::pipeline::{unpack, Source, UnpackRequest};
//!
//! # fn main() -> wxapkg_core::Result<()> {
//! let request = UnpackRequest::new(
//!     Source::AppId {
//!         appid: "wx0000000000000002".to_owned(),
//!         version: None,
//!         roots: None,
//!     },
//!     PathBuf::from("./out"),
//! );
//! let report = unpack(&request)?;
//! println!("{} files written, {} warnings", report.total_files(), report.warnings.len());
//! # Ok(())
//! # }
//! ```
//!
//! Finer-grained control is available through [`container`], [`decompile`],
//! [`project`] and [`beautify`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::module_name_repetitions,
    clippy::too_many_lines,
    clippy::struct_excessive_bools,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::return_self_not_must_use,
    clippy::uninlined_format_args,
    clippy::redundant_closure_for_method_calls,
    clippy::items_after_statements,
    clippy::needless_pass_by_value,
    clippy::map_unwrap_or,
    clippy::manual_let_else
)]

pub mod beautify;
pub mod container;
pub mod crypto;
pub mod decompile;
pub mod discover;
pub mod error;
pub mod i18n;
pub mod js;
pub mod paths;
pub mod pipeline;
pub mod project;
pub mod report;
pub mod verify;

pub use container::{PkgEntry, PkgSummary, Wxapkg};
pub use discover::{PackageFile, Program};
pub use error::{Error, Result};
pub use pipeline::{ContainerInfo, Source, UnpackRequest};
pub use report::{
    BeautifyReport, BuildReport, Counts, PartKind, PartReport, ProgramSummary, RawExtractReport,
};
pub use verify::{verify_project, Issue, Severity, VerifyOptions, VerifyReport, VerifyStats};

/// Version reported by `--version` and the HTTP `Server` header.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
