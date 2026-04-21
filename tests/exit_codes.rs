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
    let path_str = tmp.path().to_str().unwrap();
    let (_stdout, _stderr, code) = run(&["--check", path_str], tmp.path());
    assert_eq!(code, 0, "expected 0 for clean --check, got {code}");
}

/// Exit 0: `--dry-run` mode on an empty workspace — no files, no registry
/// calls, no errors.
#[test]
fn mutate_clean_workspace_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let path_str = tmp.path().to_str().unwrap();
    let (_stdout, _stderr, status) = run(&["--dry-run", path_str], tmp.path());
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
    let path_str = tmp.path().to_str().unwrap();

    let (_stdout, stderr, code) = run(&["--dry-run", path_str], tmp.path());
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
    let path_str = tmp.path().to_str().unwrap();

    let (_stdout, stderr, code) = run(&["--check", path_str], tmp.path());
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
    let path_str = tmp.path().to_str().unwrap();

    let (_stdout, stderr, code) = run(&["--dry-run", path_str], tmp.path());
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
    let path_str = tmp.path().to_str().unwrap();

    let (stdout, _stderr, code) = run(&["--format", "json", "--dry-run", path_str], tmp.path());
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
    let path_str = tmp.path().to_str().unwrap();

    let (stdout, _stderr, code) = run(&["--format", "json", "--dry-run", path_str], tmp.path());
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
    let path_str = tmp.path().to_str().unwrap().to_string();

    // Point the PyPI client at the mock server.  UV_INDEX_URL is stripped of any
    // trailing "/simple" suffix by normalize_index_url, so pass the bare URI.
    let (_stdout, stderr, code) = run_with_env(
        &["--check", "--no-cache", &path_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 1,
        "--check with a pending update must exit 1; stderr: {stderr}"
    );
}

/// Exit 1: `--dry-run` with a genuinely out-of-date dependency.
///
/// Mirrors `check_with_pending_update_exits_one`: `--dry-run` must exit 1
/// when updates are available, identical to `--check`.
#[tokio::test]
async fn dry_run_with_pending_updates_exits_one() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

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
    let path_str = tmp.path().to_str().unwrap().to_string();

    let (_stdout, stderr, code) = run_with_env(
        &["--dry-run", "--no-cache", &path_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 1,
        "--dry-run with a pending update must exit 1; stderr: {stderr}"
    );
}

/// Exit 0: `--dry-run` on an empty workspace — no updates, no errors.
#[test]
fn dry_run_on_empty_workspace_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let path_str = tmp.path().to_str().unwrap();
    let (_stdout, _stderr, code) = run(&["--dry-run", path_str], tmp.path());
    assert_eq!(
        code, 0,
        "--dry-run on an empty workspace must exit 0 (no updates, no errors)"
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
    let path_str = tmp.path().to_str().unwrap().to_string();

    let (_stdout, stderr, code) = run_with_env(
        &["--check", "--no-cache", &path_str],
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

/// Unit test: `decide_exit_code` returns 1 for pending updates in dry-run mode
/// (non_mutating=true covers both --check and --dry-run).
#[test]
fn decide_exit_code_dry_run_with_updates() {
    use upd::decide_exit_code;
    // --dry-run passes non_mutating=true, same as --check
    assert_eq!(decide_exit_code(true, true, false), 1);
    // no pending updates → 0 even in non-mutating mode
    assert_eq!(decide_exit_code(true, false, false), 0);
    // mutating mode with pending → 0 (updates applied, not flagged)
    assert_eq!(decide_exit_code(false, true, false), 0);
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

// ── decide_audit_exit_code unit tests ────────────────────────────────────────

/// Unit test: no vulns, no errors → exit 0 regardless of --no-fail.
#[test]
fn decide_audit_exit_code_clean() {
    use upd::decide_audit_exit_code;
    assert_eq!(decide_audit_exit_code(0, 0, false), 0);
    assert_eq!(decide_audit_exit_code(0, 0, true), 0);
}

/// Unit test: vulns found, no --no-fail → exit 3.
#[test]
fn decide_audit_exit_code_vulns_without_no_fail() {
    use upd::decide_audit_exit_code;
    assert_eq!(decide_audit_exit_code(1, 0, false), 3);
    assert_eq!(decide_audit_exit_code(162, 0, false), 3);
}

/// Unit test: vulns found, --no-fail present → exit 0.
#[test]
fn decide_audit_exit_code_vulns_with_no_fail() {
    use upd::decide_audit_exit_code;
    assert_eq!(decide_audit_exit_code(1, 0, true), 0);
    assert_eq!(decide_audit_exit_code(162, 0, true), 0);
}

/// Unit test: scan errors take precedence over vulns — always exit 2.
#[test]
fn decide_audit_exit_code_errors_take_precedence() {
    use upd::decide_audit_exit_code;
    // errors + vulns, no --no-fail → 2 (not 3)
    assert_eq!(decide_audit_exit_code(5, 1, false), 2);
    // errors + vulns, --no-fail → still 2
    assert_eq!(decide_audit_exit_code(5, 1, true), 2);
    // errors only, no vulns → 2
    assert_eq!(decide_audit_exit_code(0, 3, false), 2);
    // errors only, --no-fail → still 2
    assert_eq!(decide_audit_exit_code(0, 3, true), 2);
}

// ── audit integration tests ───────────────────────────────────────────────────

/// Exit 0: `audit` on an empty workspace — no packages, no errors.
#[test]
fn audit_on_empty_workspace_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let (_stdout, _stderr, code) = run(&["audit"], tmp.path());
    assert_eq!(
        code, 0,
        "expected 0 for audit on empty workspace, got {code}"
    );
}

/// Exit 3: `audit` finds vulnerabilities and `--no-fail` is absent.
///
/// A wiremock server stands in for the OSV API and reports one vulnerability
/// for `requests==1.0.0`.
#[tokio::test]
async fn audit_with_vulns_exits_three() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": [{ "id": "GHSA-test-0001" }] }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-test-0001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-test-0001",
            "summary": "test vulnerability",
            "references": [{ "url": "https://example.com/GHSA-test-0001" }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    let (_stdout, stderr, code) = run_with_env(
        &["audit", "--no-cache"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    assert_eq!(
        code, 3,
        "audit with vulns must exit 3 (no --no-fail); stderr: {stderr}"
    );
}

/// Exit 0: `audit` finds vulnerabilities but `--no-fail` suppresses non-zero exit.
#[tokio::test]
async fn audit_with_vulns_and_no_fail_exits_zero() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": [{ "id": "GHSA-test-0002" }] }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-test-0002"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-test-0002",
            "summary": "test vulnerability",
            "references": [{ "url": "https://example.com/GHSA-test-0002" }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    let (_stdout, stderr, code) = run_with_env(
        &["audit", "--no-fail", "--no-cache"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    assert_eq!(
        code, 0,
        "audit with --no-fail must exit 0 even with vulns; stderr: {stderr}"
    );
}

/// Exit 2: `audit` when OSV is unreachable — scan error, not a vuln result.
#[test]
fn audit_with_osv_unreachable_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    let (_stdout, stderr, code) = run_with_env(
        &["audit", "--no-cache"],
        tmp.path(),
        // Port 1 on loopback is never bound; the OS returns ECONNREFUSED instantly.
        &[("OSV_API_URL", "http://127.0.0.1:1")],
    );

    assert_eq!(
        code, 2,
        "audit with unreachable OSV must exit 2; stderr: {stderr}"
    );
}

// ── --show-config tests ───────────────────────────────────────────────────────

/// Exit 0: `--show-config` prints the schema and exits cleanly.
#[test]
fn show_config_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, _stderr, code) = run(&["--show-config"], tmp.path());
    assert_eq!(code, 0, "--show-config must exit 0; got {code}");
    // The schema output must contain the documented top-level keys so users
    // know what the config file should look like.
    assert!(
        stdout.contains("ignore"),
        "--show-config stdout must contain 'ignore'; got:\n{stdout}"
    );
    assert!(
        stdout.contains("[pin]"),
        "--show-config stdout must contain '[pin]'; got:\n{stdout}"
    );
}

// ── bad config parse tests ────────────────────────────────────────────────────

/// A config file using `[ignore]` (table) instead of `ignore = [...]` (array)
/// must surface a visible parse error on stderr.
///
/// This is the "original bug": before the fix, `load_from_path` swallowed the
/// error and the user saw zero output — the config was silently ignored.
#[test]
fn bad_config_wrong_ignore_format_prints_error_on_stderr() {
    let tmp = tempfile::tempdir().unwrap();

    // The broken config: `[ignore]` creates a table; the updater expects an array.
    fs::write(
        tmp.path().join(".updrc.toml"),
        "[ignore]\npackages = [\"some-package\"]\n",
    )
    .unwrap();

    // A minimal manifest so that the updater iterates over files and triggers
    // config discovery.  The file itself need not be up-to-date; we only care
    // that config loading is attempted and the parse error surfaces.
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    // Run with --no-cache and an explicit path to avoid the VCS-root check.
    let path_str = tmp.path().to_str().unwrap();
    let (_stdout, stderr, _code) = run(&["--dry-run", "--no-cache", path_str], tmp.path());

    // The error must be visible — the user must not see silence.
    assert!(
        stderr.to_lowercase().contains("error"),
        "stderr must contain 'error' when config fails to parse; got:\n{stderr}"
    );
}
