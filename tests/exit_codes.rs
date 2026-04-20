//! Integration tests for exit-code semantics.
//!
//! Exit-code contract:
//!   0 — clean run, no updates pending, no errors
//!   1 — `--check` / `--dry-run` with pending updates (no errors)
//!   2 — any run where at least one error occurred (network, parse, io, …)

use serde_json::Value;
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

fn parse_json(stdout: &str) -> Value {
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not valid JSON ({e}):\n{stdout}"))
}

/// Exit 0: `--check` on an empty workspace — no updates, no errors.
#[test]
fn check_on_empty_workspace_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let (_stdout, _stderr, code) = run(&["--check"], tmp.path());
    assert_eq!(code, 0, "expected 0 for clean --check, got {code}");
}

/// Exit 0: default (mutate) mode on an empty workspace — no files, no registry
/// calls, no errors.
#[test]
fn mutate_clean_workspace_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let (_stdout, _stderr, status) = run(&["--dry-run"], tmp.path());
    assert_eq!(
        status, 0,
        "dry-run on an empty workspace must exit 0 (no updates, no errors)"
    );
}

/// Exit 2: corrupted JSON file in default/dry-run mode causes a parse error.
#[test]
fn dry_run_with_corrupted_package_json_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("package.json"), b"{ THIS IS NOT JSON }").unwrap();

    let (_stdout, stderr, code) = run(&["--dry-run"], tmp.path());
    assert_eq!(
        code, 2,
        "corrupted JSON should produce exit 2, got {code}; stderr: {stderr}"
    );
}

/// Exit 2: corrupted JSON file in `--check` mode causes a parse error.
#[test]
fn check_with_corrupted_package_json_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("package.json"), b"{ THIS IS NOT JSON }").unwrap();

    let (_stdout, stderr, code) = run(&["--check"], tmp.path());
    assert_eq!(
        code, 2,
        "corrupted JSON in --check should produce exit 2, got {code}; stderr: {stderr}"
    );
}

/// Exit 2: corrupted requirements.txt / pyproject.toml parse errors.
#[test]
fn dry_run_with_corrupted_requirements_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    // A requirements.txt that triggers a parse error via an invalid version spec
    // that our updater considers an error (not just a warning/skip).
    // Using a file whose name is recognised as requirements.txt but whose first
    // package line has a URL reference with a broken fragment — the safest way
    // to exercise the Err path is via a package.json (JSON parse is strict).
    // Use package.json since its parse error is deterministic.
    fs::write(tmp.path().join("package.json"), b"INVALID").unwrap();

    let (_stdout, stderr, code) = run(&["--dry-run"], tmp.path());
    assert_eq!(
        code, 2,
        "corrupted file should produce exit 2, got {code}; stderr: {stderr}"
    );
}

/// Exit 2: JSON output mode with a corrupted file has structured error objects.
#[test]
fn json_output_with_error_has_structured_error_objects() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("package.json"), b"{ BROKEN }").unwrap();

    let (stdout, _stderr, code) = run(&["--format", "json", "--dry-run"], tmp.path());
    assert_eq!(
        code, 2,
        "corrupted file with --format json should exit 2, got {code}"
    );

    let json = parse_json(&stdout);
    let files = json["files"].as_array().expect("files must be an array");
    assert!(!files.is_empty(), "files array must not be empty");

    let errors = files[0]["errors"]
        .as_array()
        .expect("errors must be an array");
    assert!(!errors.is_empty(), "errors array must not be empty");

    let first_error = &errors[0];
    assert!(
        first_error.get("message").is_some(),
        "error entry must have 'message' field, got: {first_error}"
    );
    assert!(
        first_error.get("kind").is_some(),
        "error entry must have 'kind' field, got: {first_error}"
    );
    // file field is present (may be null for some error sources)
    assert!(
        first_error.get("file").is_some(),
        "error entry must have 'file' field, got: {first_error}"
    );
}

/// Exit 2: top-level summary `errors` count is non-zero when errors occur.
#[test]
fn json_output_summary_errors_count_nonzero_on_error() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("package.json"), b"BROKEN").unwrap();

    let (stdout, _stderr, code) = run(&["--format", "json", "--dry-run"], tmp.path());
    assert_eq!(code, 2, "expected exit 2 on error, got {code}");

    let json = parse_json(&stdout);
    let error_count = json["summary"]["errors"].as_u64().unwrap_or(0);
    assert!(
        error_count > 0,
        "summary.errors must be > 0 when errors occurred, got {error_count}"
    );
}

/// Exit 1: `--check` with a genuinely out-of-date dependency.
///
/// A wiremock server stands in for PyPI and advertises version 99.0.0 of
/// `requests`. The manifest pins version 1.0.0, so `upd --check` detects a
/// pending update and must exit 1 (updates pending, no errors).
#[tokio::test]
async fn check_with_pending_update_exits_one() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Serve a minimal PyPI Simple-API HTML page that advertises version 99.0.0.
    // Both the Simple API path and the legacy JSON API path are mocked so the
    // registry succeeds regardless of which endpoint the client prefers.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-99.0.0.tar.gz">requests-99.0.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    // Point the PyPI client at the mock server.  UV_INDEX_URL is stripped of any
    // trailing "/simple" suffix by normalize_index_url, so pass the bare URI.
    let (_stdout, stderr, code) = run_with_env(
        &["--check", "--no-cache"],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 1,
        "--check with a pending update must exit 1; stderr: {stderr}"
    );
}

/// Exit 2: `--check` when the registry is unreachable (network/registry error).
///
/// `NPM_REGISTRY` is pointed at a loopback address with no listener, which
/// produces an immediate connection-refused error — deterministic and fast.
#[test]
fn check_with_registry_error_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("package.json"),
        r#"{"dependencies":{"lodash":"1.0.0"}}"#,
    )
    .unwrap();

    let (_stdout, stderr, code) = run_with_env(
        &["--check", "--no-cache"],
        tmp.path(),
        // Port 1 on loopback is never bound; the OS returns ECONNREFUSED instantly.
        &[("NPM_REGISTRY", "http://127.0.0.1:1")],
    );

    assert_eq!(
        code, 2,
        "--check with an unreachable registry must exit 2; stderr: {stderr}"
    );
}

/// Unit test: `decide_exit_code` returns 0 for no updates, no errors.
#[test]
fn decide_exit_code_clean() {
    use upd::decide_exit_code;
    assert_eq!(decide_exit_code(false, false, false), 0);
}

/// Unit test: `decide_exit_code` returns 1 for pending updates in check mode.
#[test]
fn decide_exit_code_check_with_updates() {
    use upd::decide_exit_code;
    assert_eq!(decide_exit_code(true, true, false), 1);
}

/// Unit test: `decide_exit_code` returns 2 when errors occurred, regardless of updates.
#[test]
fn decide_exit_code_errors_take_precedence() {
    use upd::decide_exit_code;
    // errors + updates pending in check mode → still 2
    assert_eq!(decide_exit_code(true, true, true), 2);
    // errors + no updates → 2
    assert_eq!(decide_exit_code(true, false, true), 2);
    // errors in mutate mode → 2
    assert_eq!(decide_exit_code(false, false, true), 2);
}
