// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! `wxapkg-mcp` — the Model Context Protocol server embedded in `wxapkg-dump --mcp`.
//!
//! Speaks JSON-RPC 2.0 over stdin/stdout with one JSON object per line, which is the
//! stdio transport defined by MCP. Kept as a library so the CLI binary is the only
//! executable that has to be shipped.
//!
//! ```jsonc
//! // ~/.config/.../mcp.json
//! { "mcpServers": { "wxapkg-dump": { "command": "wxapkg-dump", "args": ["--mcp"] } } }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::module_name_repetitions,
    clippy::too_many_lines,
    clippy::missing_errors_doc,
    clippy::uninlined_format_args,
    clippy::needless_pass_by_value
)]

mod protocol;
mod tools;

use std::io::{BufRead, Write};

use protocol::{Request, Response, RpcError};
use serde_json::{json, Value};

/// Protocol revision this server implements.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Server name reported during `initialize`.
const SERVER_NAME: &str = "wxapkg-dump";

/// Runs the stdio MCP server until stdin closes.
///
/// # Errors
/// Returns an error when stdout cannot be written, matching the CLI's failure contract.
pub fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        // stdout carries the protocol; anything diagnostic must go to stderr.
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                tracing::error!(target: "wxapkg-dump::mcp", error = %err, "stdin read failed");
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(trimmed) {
            Ok(request) => request,
            Err(err) => {
                write_message(
                    &mut out,
                    &Response::error(Value::Null, RpcError::parse_error(err.to_string())),
                )?;
                continue;
            }
        };

        // A request must declare the protocol revision it speaks.
        if request.jsonrpc != "2.0" {
            write_message(
                &mut out,
                &Response::error(
                    request.id.clone().unwrap_or(Value::Null),
                    RpcError::invalid_request(format!(
                        "expected jsonrpc \"2.0\", received {:?}",
                        request.jsonrpc
                    )),
                ),
            )?;
            continue;
        }

        // Notifications carry no `id` and must never be answered.
        let is_notification = request.id.is_none();
        let id = request.id.clone().unwrap_or(Value::Null);

        match handle(&request) {
            Ok(Some(result)) => {
                if !is_notification {
                    write_message(&mut out, &Response::success(id, result))?;
                }
            }
            Ok(None) => {
                if !is_notification {
                    write_message(&mut out, &Response::success(id, json!({})))?;
                }
            }
            Err(error) => {
                if !is_notification {
                    write_message(&mut out, &Response::error(id, error))?;
                }
            }
        }
    }
    Ok(())
}

/// Wraps an unexpected failure so the client always gets a well-formed error.
#[allow(dead_code)]
fn internal(message: impl Into<String>) -> RpcError {
    RpcError::internal(message)
}

fn write_message(out: &mut impl Write, response: &Response) -> anyhow::Result<()> {
    let encoded = serde_json::to_string(response)?;
    writeln!(out, "{encoded}")?;
    out.flush()?;
    Ok(())
}

/// Dispatches one JSON-RPC method.
fn handle(request: &Request) -> Result<Option<Value>, RpcError> {
    match request.method.as_str() {
        "initialize" => Ok(Some(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": wxapkg_core::VERSION,
            },
            "instructions": INSTRUCTIONS,
        }))),

        // Lifecycle notifications: acknowledged by producing no result at all.
        "notifications/initialized" | "initialized" | "notifications/cancelled" => Ok(None),

        "ping" => Ok(Some(json!({}))),

        "tools/list" => Ok(Some(json!({ "tools": tools::descriptors() }))),

        "tools/call" => {
            let params = request.params.clone().unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("`name` is required"))?;
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match tools::call(name, &arguments) {
                Ok(result) => Ok(Some(json!({
                    "content": [ { "type": "text", "text": result } ],
                    "isError": false,
                }))),
                Err(message) => Ok(Some(json!({
                    "content": [ { "type": "text", "text": message } ],
                    "isError": true,
                }))),
            }
        }

        // Resource support is not advertised, so a request for it is a client bug.
        other => Err(RpcError::method_not_found(other)),
    }
}

/// Guidance surfaced to the assistant right after `initialize`.
const INSTRUCTIONS: &str = "\
WeChat Mini Program toolkit. Use `wxapkg_scan_local_programs` to see what the WeChat \
client has cached on this machine, `wxapkg_unpack_package` to reconstruct a project the \
official DevTools can open, and `ilink_download_mini_program` to fetch a Mini Program from \
the WeChat CDN using an already-authorized session.

Notes that prevent common mistakes:
- Remote download needs a session established beforehand with `wxapkg-dump login`; this \
server will not and cannot scan a QR code for the user.
- The original id returned by `ilink_recent_mini_programs` keeps its `@app` suffix on \
purpose. Pass it through verbatim.
- Unpacking a Mini Program you do not own or have not been authorised to analyse is not \
acceptable use. Prefer `wxapkg_inspect_package` when only structural information is needed.";

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, params: Option<Value>) -> Request {
        Request {
            jsonrpc: "2.0".to_owned(),
            id: Some(json!(1)),
            method: method.to_owned(),
            params,
        }
    }

    #[test]
    fn initialize_advertises_tools() {
        let value = handle(&request("initialize", None)).unwrap().unwrap();
        assert_eq!(value["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(value["serverInfo"]["name"], SERVER_NAME);
        assert!(value["capabilities"]["tools"].is_object());
    }

    #[test]
    fn lifecycle_notifications_produce_no_result() {
        assert!(handle(&request("notifications/initialized", None))
            .unwrap()
            .is_none());
    }

    #[test]
    fn tools_list_includes_the_core_tools() {
        let value = handle(&request("tools/list", None)).unwrap().unwrap();
        let names: Vec<String> = value["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect();
        assert!(names.contains(&"wxapkg_scan_local_programs".to_owned()));
        assert!(names.contains(&"wxapkg_unpack_package".to_owned()));
    }

    #[test]
    fn unknown_methods_are_rejected() {
        assert!(handle(&request("tools/nope", None)).is_err());
    }

    #[test]
    fn tools_call_without_a_name_is_invalid() {
        assert!(handle(&request("tools/call", Some(json!({})))).is_err());
    }
}
