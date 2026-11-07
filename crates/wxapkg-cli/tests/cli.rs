// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Deterministic CLI contract tests.
//!
//! These run without a WeChat installation: every case uses a temporary directory,
//! a deliberately invalid appid, or the protocol-only `--mcp` mode.

use assert_cmd::Command;
use predicates::prelude::*;

fn cli() -> Command {
    Command::cargo_bin("wxapkg-dump").expect("the CLI binary must build")
}

#[test]
fn version_and_help_are_stable() {
    cli()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("wxapkg-dump 1.0.0"));
    cli()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("unpack"))
        .stdout(predicate::str::contains("--mcp"));
}

#[test]
fn doctor_reports_the_running_version() {
    let output = cli().args(["doctor", "--json"]).output().unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["version"], "1.0.0");
}

#[test]
fn an_empty_appid_is_a_usage_error() {
    cli()
        .args(["unpack", "--appid", "", "--json"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"ok\": false"));
}

#[test]
fn a_missing_container_is_a_usage_error() {
    cli()
        .args(["info", "/definitely/not/a/container.wxapkg", "--json"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("\"ok\": false"));
}

#[test]
fn scan_of_an_empty_directory_is_an_empty_success() {
    let dir = tempfile::tempdir().unwrap();
    let output = cli()
        .args(["scan", dir.path().to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"].as_array().map(Vec::len), Some(0));
}

#[test]
fn completions_are_rendered_for_every_supported_shell() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        cli()
            .args(["completions", shell])
            .assert()
            .success()
            .stdout(predicate::str::is_empty().not());
    }
}

#[test]
fn english_messages_follow_the_language_flag() {
    let output = cli()
        .args(["--lang", "en", "unpack", "--appid", "", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("empty"), "{message}");
}

#[test]
fn mcp_initialize_and_tools_list_are_well_formed() {
    let request = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":",
        "{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":",
        "{\"name\":\"test\",\"version\":\"1\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n"
    );
    let output = cli().arg("--mcp").write_stdin(request).output().unwrap();
    assert!(output.status.success());
    let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[0]["result"]["serverInfo"]["version"], "1.0.0");
    assert_eq!(
        lines[1]["result"]["tools"].as_array().map(Vec::len),
        Some(9)
    );
}
