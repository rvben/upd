//! End-to-end tests for `--format json`.
//!
//! These tests spawn the real `upd` binary so they catch wiring regressions
//! (CLI parsing, stdout routing, schema shape) that pure unit tests miss.
//! To keep them hermetic and fast, they operate on temp directories that
//! contain no dependency files, which exercises the "empty" JSON emission
//! paths for every subcommand.

use serde_json::Value;
use std::path::Path;
use std::process::Command;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

fn run(args: &[&str], cwd: &Path) -> (String, String, i32) {
    let output = Command::new(upd_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("failed to run upd");
    (
        String::from_utf8(output.stdout).expect("stdout not UTF-8"),
        String::from_utf8(output.stderr).expect("stderr not UTF-8"),
        output.status.code().unwrap_or(-1),
    )
}

fn parse_json(stdout: &str) -> Value {
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not valid JSON ({e}):\n{stdout}"))
}

#[test]
fn update_format_json_on_empty_workspace_emits_valid_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, _stderr, code) = run(&["--format", "json", "--dry-run"], tmp.path());
    assert_eq!(code, 0, "exit code should be 0 on empty workspace");

    let json = parse_json(&stdout);
    assert_eq!(json["command"], "update");
    assert_eq!(json["mode"], "dry-run");
    assert!(json["files"].as_array().unwrap().is_empty());
    let summary = &json["summary"];
    assert_eq!(summary["files_scanned"], 0);
    assert_eq!(summary["updates_total"], 0);
    assert_eq!(summary["errors"], 0);
}

#[test]
fn align_format_json_on_empty_workspace_emits_valid_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, _stderr, code) = run(&["--format", "json", "align"], tmp.path());
    assert_eq!(code, 0);

    let json = parse_json(&stdout);
    assert_eq!(json["command"], "align");
    assert!(json["packages"].as_array().unwrap().is_empty());
    assert_eq!(json["summary"]["files_scanned"], 0);
    assert_eq!(json["summary"]["packages"], 0);
    assert_eq!(json["summary"]["misaligned_packages"], 0);
}

#[test]
fn audit_format_json_on_empty_workspace_emits_valid_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, _stderr, code) = run(&["--format", "json", "audit"], tmp.path());
    assert_eq!(code, 0);

    let json = parse_json(&stdout);
    assert_eq!(json["command"], "audit");
    assert_eq!(json["status"], "complete");
    assert!(json["vulnerabilities"].as_array().unwrap().is_empty());
    assert!(json["errors"].as_array().unwrap().is_empty());
    assert_eq!(json["summary"]["packages_checked"], 0);
    assert_eq!(json["summary"]["vulnerabilities"], 0);
}

#[test]
fn interactive_with_format_json_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, stderr, code) = run(
        &["--format", "json", "--interactive", "--dry-run"],
        tmp.path(),
    );
    assert_ne!(code, 0, "interactive + json should fail");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("--interactive") && combined.contains("--format json"),
        "error should mention the conflicting flags, got: {combined}"
    );
}

#[test]
fn format_text_is_default_and_not_json() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, _stderr, code) = run(&["--dry-run"], tmp.path());
    assert_eq!(code, 0);
    assert!(
        serde_json::from_str::<Value>(stdout.trim()).is_err(),
        "default text output should not parse as JSON, got: {stdout}"
    );
}
