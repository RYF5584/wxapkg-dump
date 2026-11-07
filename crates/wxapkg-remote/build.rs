// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Compiles the staged protocol definitions into Rust types.
//!
//! `protox` is a pure-Rust protobuf compiler, so building needs no system `protoc`.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = crate_dir.join("proto");
    let mut files = Vec::new();
    for name in ["base", "ilink", "wxa_apis", "wxapkg"] {
        files.push(proto_dir.join(format!("{name}.proto")));
    }

    let set = protox::compile(
        &files,
        &[crate_dir], // protos import each other as "proto/<name>.proto"
    )?;

    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let mut config = prost_build::Config::new();
    config.out_dir(&out);
    config.compile_fds(set)?;

    println!("cargo:rerun-if-changed={}", proto_dir.display());
    Ok(())
}
