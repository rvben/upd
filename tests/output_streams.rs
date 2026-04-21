//! Integration tests for output-stream routing and --quiet flag.
//!
//! Covers:
//!  - Error lines go to stderr in text mode, not stdout
//!  - `--quiet` suppresses progress/summary on stdout
//!  - `--quiet --format json` still emits JSON on stdout (JSON is unaffected)
//!  - Normal text run (no --quiet) still prints summary to stdout

use std::fs;
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

fn run_with_env(args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> (String, String, i32) {
    let mut cmd = Command::new(upd_bin());
    cmd.args(args).current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to run upd");
    (
        String::from_utf8(output.stdout).expect("stdout not UTF-8"),
        String::from_utf8(output.stderr).expect("stderr not UTF-8"),
        output.status.code().unwrap_or(-1),
    )
}

/// Error messages from a file parse failure go to stderr, not stdout.
///
/// A corrupted package.json produces a parse error. The error text must appear
/// on stderr; stdout must not contain the word "Error".
#[test]
fn parse_error_goes_to_stderr_not_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("package.json"), b"{ BROKEN JSON }").unwrap();

    let (stdout, stderr, code) = run(&["--dry-run", "--no-cache"], tmp.path());

    assert_eq!(code, 2, "parse error must exit 2; stderr: {stderr}");
    assert!(
        stderr.to_lowercase().contains("error"),
        "error text must appear on stderr; stderr: {stderr}"
    );
    assert!(
        !stdout.to_lowercase().contains("error"),
        "error text must NOT appear on stdout; stdout: {stdout}"
    );
}

/// In text mode without --quiet, the summary line appears on stdout.
#[test]
fn normal_text_run_prints_summary_to_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    // Empty workspace → "all dependencies up to date"
    let (stdout, _stderr, code) = run(&["--dry-run"], tmp.path());
    assert_eq!(
        code, 0,
        "expected exit 0 on empty workspace; stdout: {stdout}"
    );
    assert!(
        stdout.contains("No dependency files found."),
        "stdout must contain summary message in normal text mode; got: {stdout:?}"
    );
}

/// `--quiet` on an empty workspace: stdout is empty, exit 0.
#[test]
fn quiet_on_empty_workspace_produces_empty_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, _stderr, code) = run(&["-q", "--dry-run"], tmp.path());
    assert_eq!(code, 0, "expected exit 0; stderr was not checked");
    assert!(
        stdout.trim().is_empty(),
        "--quiet must suppress all stdout in text mode on clean workspace; got: {stdout:?}"
    );
}

/// `--quiet --format json` still emits JSON on stdout.
///
/// JSON output is machine-facing and must not be suppressed by --quiet.
#[test]
fn quiet_with_json_format_still_emits_json() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, _stderr, code) = run(&["-q", "--format", "json", "--dry-run"], tmp.path());
    assert_eq!(code, 0, "expected exit 0");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout must be valid JSON with --quiet --format json ({e}); got: {stdout}")
    });
    assert_eq!(
        parsed["command"], "update",
        "JSON command field must be 'update'"
    );
}

/// `--quiet` does not silence errors: a corrupted file still produces stderr output.
///
/// --quiet is not --silent. Errors must always surface.
#[test]
fn quiet_does_not_silence_errors_on_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("package.json"), b"NOT JSON AT ALL").unwrap();

    let (stdout, stderr, code) = run(&["-q", "--dry-run", "--no-cache"], tmp.path());

    assert_eq!(code, 2, "parse error must exit 2 even with --quiet");
    assert!(
        stderr.to_lowercase().contains("error"),
        "--quiet must not suppress error messages on stderr; stderr: {stderr}"
    );
    assert!(
        stdout.trim().is_empty(),
        "--quiet must suppress non-error stdout even on error run; stdout: {stdout:?}"
    );
}

/// Registry errors appear on stderr, not stdout, during a text-mode update run.
///
/// NPM_REGISTRY is pointed at a loopback address with no listener, which
/// produces an immediate connection-refused error.
#[test]
fn registry_error_goes_to_stderr_not_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("package.json"),
        r#"{"dependencies":{"lodash":"1.0.0"}}"#,
    )
    .unwrap();

    let (stdout, stderr, code) = run_with_env(
        &["--dry-run", "--no-cache"],
        tmp.path(),
        &[("NPM_REGISTRY", "http://127.0.0.1:1")],
    );

    assert_eq!(code, 2, "registry error must exit 2; stderr: {stderr}");
    assert!(
        stderr.to_lowercase().contains("error"),
        "error text must appear on stderr; stderr: {stderr}"
    );
    assert!(
        !stdout.to_lowercase().contains("error"),
        "error text must NOT appear on stdout; stdout: {stdout}"
    );
}

/// `--quiet` with `--check` on an empty workspace: stdout is empty, exit 0.
#[test]
fn quiet_check_on_empty_workspace_produces_empty_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, _stderr, code) = run(&["-q", "--check"], tmp.path());
    assert_eq!(code, 0, "expected exit 0");
    assert!(
        stdout.trim().is_empty(),
        "--quiet must suppress check output; got: {stdout:?}"
    );
}
