// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Decompilation of compiled Mini Program artefacts back into source files.
//!
//! WeChat uploads every page, component and third-party dependency as a single
//! `app-service.js` file. Modules are registered with
//!
//! ```js
//! define("pages/index/index.js", function(require, module, exports) { /* original source */ });
//! ```
//!
//! so the function body *is* the developer's source code with a thin wrapper around
//! it. Slicing it out yields the original file.
//!
//! A second artefact is the wxss/style side, compiled by `wcc` into segment arrays
//! consumed by `setCssToHead`. A third is the WXML template, compiled into render
//! functions whose operand arrays are the original expressions.
//!
//! The submodules here reverse each of those three encodings.

pub mod bundle;
pub mod wxml;
pub mod wxss;

pub use bundle::{
    find_appcode_json, rewrite_requires, scan_requires, strip_define_bodies, JsModule,
};
pub use wxml::{collect_ops, decompile_app_service, DecompiledTemplate, OperandPool};
pub use wxss::extract_wxss;
