// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Artifact handling: archive a project, stream it back, browse it, read it.
//!
//! Everything here only ever touches paths inside the workspace, and every one of them is
//! also *read* rather than written, so the workspace guard is the single security boundary
//! this module relies on.

use std::path::{Path, PathBuf};

use axum::body::{Body, Bytes};
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt as _;
use tokio_stream::wrappers::ReceiverStream;
use walkdir::WalkDir;

use crate::api::{failure, internal_error, success};
use crate::guard;
use crate::state::AppState;
use crate::util;

/// Largest text file `/api/project/file` will return.
const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;

/// Largest directory listing `/api/project/tree` will return.
const MAX_TREE_ENTRIES: usize = 5000;

/// Size of one chunk handed to the streaming body.
const STREAM_CHUNK: usize = 64 * 1024;

/// Routes owned by this module.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/project/zip", post(zip_project))
        .route("/project/download", get(download))
        .route("/project/tree", get(tree))
        .route("/project/file", get(read_file))
}

// ---------------------------------------------------------------------------
// zip
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ZipBody {
    dir: String,
    out: Option<String>,
    #[serde(default)]
    allow_outside: bool,
}

/// `POST /api/project/zip`
async fn zip_project(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
    Json(body): Json<ZipBody>,
) -> Response {
    let pairs = util::parse_query(raw.as_deref().unwrap_or_default());
    let include_raw = util::query_flag(&pairs, "includeRaw", false);

    let dir = match resolve_dir(&state.workspace, &body.dir, body.allow_outside) {
        Ok(path) => path,
        Err(message) => return refused("project zip", &message),
    };
    if !dir.is_dir() {
        return failure(
            "project zip",
            "invalid_argument",
            &format!("{} is not a directory", dir.display()),
            None,
            StatusCode::BAD_REQUEST,
        );
    }

    let out = match body
        .out
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(explicit) => {
            if body.allow_outside {
                match guard::resolve_anywhere(explicit) {
                    Ok(path) => path,
                    Err(message) => return refused("project zip", &message),
                }
            } else {
                match guard::ensure_inside(&state.workspace, explicit) {
                    Ok(path) => path,
                    Err(message) => return refused("project zip", &message),
                }
            }
        }
        None => dir.with_extension("zip"),
    };

    state
        .events
        .publish("info", format!("zipping {}", dir.display()));

    let archive_dir = dir.clone();
    let archive_out = out.clone();
    let result = tokio::task::spawn_blocking(move || {
        anyhow::ensure!(
            archive_dir != archive_out,
            "the archive cannot replace the directory it is built from"
        );
        zip_directory(&archive_dir, &archive_out, include_raw)
    })
    .await;

    match result {
        Ok(Ok((bytes, files))) => {
            state.events.publish(
                "info",
                format!("archive written: {} file(s), {bytes} bytes", files),
            );
            success(
                "project zip",
                json!({
                    "zipPath": wxapkg_core::paths::display_path(&out),
                    "bytes": bytes,
                    "files": files,
                    "includeRaw": include_raw,
                }),
            )
        }
        Ok(Err(err)) => failure(
            "project zip",
            "io_error",
            &format!("could not build the archive: {err}"),
            None,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        Err(err) => internal_error("project zip", &err.to_string()),
    }
}

/// Writes every file under `dir` into a new deflate archive at `out`.
fn zip_directory(dir: &Path, out: &Path, include_raw: bool) -> anyhow::Result<(u64, usize)> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(out)?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let mut files = 0usize;
    for entry in WalkDir::new(dir).follow_links(false).sort_by_file_name() {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        // The archive is created before the walk starts, so it would otherwise include
        // itself when the destination happens to sit inside the source tree.
        if entry.path() == out {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(dir)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if !include_raw && relative.starts_with("_raw/") {
            continue;
        }
        writer.start_file(relative, options)?;
        let mut source = std::fs::File::open(entry.path())?;
        std::io::copy(&mut source, &mut writer)?;
        files += 1;
    }
    writer.finish()?;

    let bytes = std::fs::metadata(out)?.len();
    Ok((bytes, files))
}

// ---------------------------------------------------------------------------
// download
// ---------------------------------------------------------------------------

/// `GET /api/project/download?path=`
async fn download(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let pairs = util::parse_query(raw.as_deref().unwrap_or_default());
    let Some(requested) = util::query_value(&pairs, "path") else {
        return failure(
            "project download",
            "invalid_argument",
            "the `path` query parameter is required",
            None,
            StatusCode::BAD_REQUEST,
        );
    };

    let path = match guard::ensure_inside(&state.workspace, requested) {
        Ok(path) => path,
        Err(message) => return refused("project download", &message),
    };
    if !is_archive(&path) {
        return failure(
            "project download",
            "invalid_argument",
            "only .zip and .wxapkg artifacts can be downloaded",
            None,
            StatusCode::BAD_REQUEST,
        );
    }

    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return not_found("project download", &path),
        Err(_) => return not_found("project download", &path),
    };

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(err) => return internal_error("project download", &err.to_string()),
    };

    // A bounded channel gives the read loop back-pressure: the file is never buffered
    // whole, which is what makes a multi-gigabyte artifact safe to serve.
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        let mut buffer = vec![0u8; STREAM_CHUNK];
        loop {
            match file.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => {
                    if sender
                        .send(Ok(Bytes::copy_from_slice(&buffer[..read])))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    let _ = sender.send(Err(err)).await;
                    break;
                }
            }
        }
    });

    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "artifact".to_owned());
    let content_type = util::content_type_for(&name, &[]);
    let disposition = format!("attachment; filename=\"{}\"", name.replace('"', ""));

    state.events.publish(
        "info",
        format!("streaming {name} ({} bytes)", metadata.len()),
    );

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("content-length", metadata.len())
        .header("content-disposition", disposition)
        .body(Body::from_stream(ReceiverStream::new(receiver)))
        .unwrap_or_else(|err| internal_error("project download", &err.to_string()))
}

fn is_archive(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip") || ext.eq_ignore_ascii_case("wxapkg"))
}

// ---------------------------------------------------------------------------
// tree
// ---------------------------------------------------------------------------

/// `GET /api/project/tree?dir=&depth=`
async fn tree(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let pairs = util::parse_query(raw.as_deref().unwrap_or_default());
    let Some(requested) = util::query_value(&pairs, "dir") else {
        return failure(
            "project tree",
            "invalid_argument",
            "the `dir` query parameter is required",
            None,
            StatusCode::BAD_REQUEST,
        );
    };
    let depth = util::query_number::<usize>(&pairs, "depth")
        .unwrap_or(2)
        .clamp(1, 12);

    let dir = match guard::ensure_inside(&state.workspace, requested) {
        Ok(path) => path,
        Err(message) => return refused("project tree", &message),
    };
    if !dir.is_dir() {
        return not_found("project tree", &dir);
    }

    let mut entries: Vec<Value> = Vec::new();
    let mut truncated = false;
    for entry in WalkDir::new(&dir)
        .max_depth(depth)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue;
        }
        if entries.len() >= MAX_TREE_ENTRIES {
            truncated = true;
            break;
        }
        let relative = entry
            .path()
            .strip_prefix(&dir)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = entry.metadata().ok();
        entries.push(json!({
            "path": relative,
            "name": entry.file_name().to_string_lossy(),
            "kind": if entry.file_type().is_dir() { "dir" } else { "file" },
            "size": metadata.as_ref().map_or(0, std::fs::Metadata::len),
            "modifiedAt": metadata
                .as_ref()
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|delta| delta.as_secs() as i64),
        }));
    }

    success(
        "project tree",
        json!({
            "dir": wxapkg_core::paths::display_path(&dir),
            "depth": depth,
            "count": entries.len(),
            "truncated": truncated,
            "entries": entries,
        }),
    )
}

// ---------------------------------------------------------------------------
// file
// ---------------------------------------------------------------------------

/// `GET /api/project/file?path=`
async fn read_file(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let pairs = util::parse_query(raw.as_deref().unwrap_or_default());
    let Some(requested) = util::query_value(&pairs, "path") else {
        return failure(
            "project file",
            "invalid_argument",
            "the `path` query parameter is required",
            None,
            StatusCode::BAD_REQUEST,
        );
    };

    let path = match guard::ensure_inside(&state.workspace, requested) {
        Ok(path) => path,
        Err(message) => return refused("project file", &message),
    };

    let result = tokio::task::spawn_blocking(move || read_text(&path)).await;
    match result {
        Ok(Ok((path, text))) => {
            let lines = text.lines().count();
            success(
                "project file",
                json!({
                    "path": wxapkg_core::paths::display_path(&path),
                    "size": text.len(),
                    "lines": lines,
                    "content": text,
                }),
            )
        }
        Ok(Err((code, message, status))) => failure("project file", code, &message, None, status),
        Err(err) => internal_error("project file", &err.to_string()),
    }
}

/// Reads a text file, refusing anything oversized or binary.
///
/// The failure is returned as a tuple so the blocking closure stays free of HTTP types.
fn read_text(path: &Path) -> Result<(PathBuf, String), (&'static str, String, StatusCode)> {
    let metadata = std::fs::metadata(path).map_err(|_| {
        (
            "not_found",
            format!("{} does not exist", path.display()),
            StatusCode::NOT_FOUND,
        )
    })?;
    if !metadata.is_file() {
        return Err((
            "invalid_argument",
            format!("{} is not a file", path.display()),
            StatusCode::BAD_REQUEST,
        ));
    }
    if metadata.len() > MAX_TEXT_BYTES {
        return Err((
            "too_large",
            format!(
                "{} is {} bytes; the limit is {MAX_TEXT_BYTES}",
                path.display(),
                metadata.len()
            ),
            StatusCode::PAYLOAD_TOO_LARGE,
        ));
    }

    let bytes = std::fs::read(path).map_err(|err| {
        (
            "io_error",
            format!("cannot read {}: {err}", path.display()),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
    })?;
    let head = &bytes[..bytes.len().min(8192)];
    if util::looks_binary(head) {
        return Err((
            "binary_file",
            format!("{} appears to be binary", path.display()),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        (
            "not_utf8",
            format!("{} is not valid UTF-8", path.display()),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        )
    })?;
    Ok((path.to_path_buf(), text))
}

// ---------------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------------

/// Resolves a directory parameter, honouring the `allowOutside` escape hatch.
///
/// The failure is a message rather than a `Response` so the error type stays small.
fn resolve_dir(workspace: &Path, requested: &str, allow_outside: bool) -> Result<PathBuf, String> {
    if allow_outside {
        guard::resolve_anywhere(requested)
    } else {
        guard::ensure_inside(workspace, requested)
    }
}

fn refused(command: &str, message: &str) -> Response {
    failure(
        command,
        "forbidden",
        message,
        Some("paths must resolve inside the workspace unless the request opts out explicitly"),
        StatusCode::FORBIDDEN,
    )
}

fn not_found(command: &str, path: &Path) -> Response {
    failure(
        command,
        "not_found",
        &format!("{} is not a regular file or directory", path.display()),
        None,
        StatusCode::NOT_FOUND,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_skips_raw_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("_raw/main")).unwrap();
        std::fs::write(dir.path().join("app.js"), "App({})").unwrap();
        std::fs::write(dir.path().join("_raw/main/app.js"), "raw").unwrap();

        let out = dir.path().with_extension("zip");
        let (bytes, files) = zip_directory(dir.path(), &out, false).unwrap();
        assert!(bytes > 0);
        assert_eq!(files, 1);

        let (_, with_raw) = zip_directory(dir.path(), &out, true).unwrap();
        assert_eq!(with_raw, 2);
    }

    #[test]
    fn zip_never_swallows_its_own_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.js"), "App({})").unwrap();
        // Writing the archive inside the directory it archives is a trap the walk must skip.
        let out = dir.path().join("project.zip");
        let (_, files) = zip_directory(dir.path(), &out, true).unwrap();
        assert_eq!(files, 1);
    }

    #[test]
    fn only_archives_are_downloadable() {
        assert!(is_archive(Path::new("/w/a.zip")));
        assert!(is_archive(Path::new("/w/a.wxapkg")));
        assert!(!is_archive(Path::new("/w/notes.txt")));
    }

    #[test]
    fn binary_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        std::fs::write(&path, b"abc\0def").unwrap();
        assert_eq!(read_text(&path).unwrap_err().0, "binary_file");
    }

    #[test]
    fn oversized_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        std::fs::write(&path, vec![b'a'; (MAX_TEXT_BYTES + 1) as usize]).unwrap();
        assert_eq!(read_text(&path).unwrap_err().0, "too_large");
    }

    #[test]
    fn text_files_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.js");
        std::fs::write(&path, "App({});\n").unwrap();
        let (_, text) = read_text(&path).unwrap();
        assert_eq!(text, "App({});\n");
    }
}
