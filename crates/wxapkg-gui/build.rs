// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Tauri build script.
//!
//! Generates the platform manifest (icons, permissions, capabilities) from
//! `tauri.conf.json`, and re-runs whenever an embedded frontend file changes:
//! `generate_context!` reads the `frontendDist` files at compile time, but cargo
//! only knows about Rust sources — without this, a rebuilt app would still ship
//! the previous UI.

use std::path::Path;

/// The single source of the window UI; the crate copy exists so `frontendDist` resolves
/// inside the crate and `tauri build` embeds a bundle-ready snapshot.
const SOURCE_WEBUI: &str = "../wxapkg-server/webui";
const TARGET_WEBUI: &str = "webui";

fn main() {
    println!("cargo:rerun-if-changed=tauri.conf.json");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(SOURCE_WEBUI);
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join(TARGET_WEBUI);
    // The snapshot directory is generated, so a clean checkout does not have it yet.
    std::fs::create_dir_all(&target)
        .unwrap_or_else(|err| panic!("cannot create {}: {err}", target.display()));
    for name in ["app.js", "index.html", "styles.css"] {
        let from = source.join(name);
        let to = target.join(name);
        println!("cargo:rerun-if-changed={}", from.display());
        if let Err(err) = std::fs::copy(&from, &to) {
            panic!("cannot sync {} into the crate: {err}", from.display());
        }
    }
    tauri_build::build();
}
