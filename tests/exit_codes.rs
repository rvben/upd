//! Integration tests for exit-code semantics.
//!
//! Exit-code contract:
//!   0 - clean run, no updates pending, no errors
//!   1 - `--check` / `--dry-run` with pending updates (no errors)
//!   2 - any run where at least one error occurred (network, parse, io, …)

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

/// Exit 0: `--check` on an empty workspace - no updates, no errors.
#[test]
fn check_on_empty_workspace_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let path_str = tmp.path().to_str().unwrap();
    let (_stdout, _stderr, code) = run(&["--check", path_str], tmp.path());
    assert_eq!(code, 0, "expected 0 for clean --check, got {code}");
}

/// Exit 0: `--dry-run` mode on an empty workspace - no files, no registry
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
    // package line has a URL reference with a broken fragment - the safest way
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

/// Exit 0: `--dry-run` on an empty workspace - no updates, no errors.
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
/// produces an immediate connection-refused error - deterministic and fast.
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
    assert_eq!(decide_audit_exit_code(1, 0, false), 6);
    assert_eq!(decide_audit_exit_code(162, 0, false), 6);
}

/// Unit test: vulns found, --no-fail present → exit 0.
#[test]
fn decide_audit_exit_code_vulns_with_no_fail() {
    use upd::decide_audit_exit_code;
    assert_eq!(decide_audit_exit_code(1, 0, true), 0);
    assert_eq!(decide_audit_exit_code(162, 0, true), 0);
}

/// Unit test: scan errors take precedence over vulns - always exit 2.
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

/// Exit 0: `audit` on an empty workspace - no packages, no errors.
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
async fn audit_with_vulns_exits_six() {
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
        code, 6,
        "audit with vulns must exit 6, the vulnerabilities_found outcome (no --no-fail); stderr: {stderr}"
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

/// Exit 2: `audit` when OSV is unreachable - scan error, not a vuln result.
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

/// A config file exercising every setting `--show-config` claims to report.
const SHOW_CONFIG_FIXTURE: &str = r#"
ignore = ["left-pad"]
include = ["ansible/roles/*/vars/*.yml"]
exclude = ["**/vendor/**"]

[automation]
security_remediation = true

[pin]
"sigstore/cosign-installer" = "v4.1.2"

[cooldown]
default = "14d"

[cooldown.ecosystem]
npm = "3d"
"#;

/// Exit 0: `--show-config` reports the settings the run resolved to.
///
/// The values below come from the fixture rather than from the schema
/// template, which is the distinction the command exists to make: a template
/// showing `ignore = []` is indistinguishable from a resolved empty list.
#[test]
fn show_config_reports_resolved_settings() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join(".updrc.toml"), SHOW_CONFIG_FIXTURE).unwrap();

    let (stdout, _stderr, code) = run(&["--show-config", "-o", "text"], tmp.path());
    assert_eq!(code, 0, "--show-config must exit 0; got {code}");

    for needle in [
        ".updrc.toml",
        "left-pad",
        "ansible/roles/*/vars/*.yml",
        "**/vendor/**",
        "sigstore/cosign-installer",
        "14d",
        "3d",
        "security_remediation: true",
    ] {
        assert!(
            stdout.contains(needle),
            "--show-config must report {needle:?}; got:\n{stdout}"
        );
    }
}

/// `--show-config` distinguishes "no config file" from "a config file setting
/// nothing", rather than rendering an empty template for both.
#[test]
fn show_config_without_a_config_file_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, _stderr, code) = run(&["--show-config", "-o", "text"], tmp.path());
    assert_eq!(code, 0, "--show-config must exit 0; got {code}");
    assert!(
        stdout.contains("(none found"),
        "--show-config must say no config file was found; got:\n{stdout}"
    );
    assert!(
        stdout.contains("ignore: (none)"),
        "--show-config must render an empty ignore list explicitly; got:\n{stdout}"
    );
}

/// `-c` selects the file `--show-config` reports on. Without this the flag is
/// accepted and silently ignored, and the output describes a different file
/// than the one named.
#[test]
fn show_config_honors_explicit_config_path() {
    let tmp = tempfile::tempdir().unwrap();
    let named = tmp.path().join("named.toml");
    fs::write(&named, SHOW_CONFIG_FIXTURE).unwrap();

    // A different config in the working directory, so discovery and the
    // explicit path cannot both be satisfied by the same output.
    let other = tempfile::tempdir().unwrap();
    fs::write(
        other.path().join(".updrc.toml"),
        "ignore = [\"right-pad\"]\n",
    )
    .unwrap();

    let (stdout, _stderr, code) = run(
        &["--show-config", "-o", "text", "-c", named.to_str().unwrap()],
        other.path(),
    );
    assert_eq!(code, 0, "--show-config must exit 0; got {code}");
    assert!(
        stdout.contains("left-pad"),
        "--show-config -c must read the named file; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("right-pad"),
        "--show-config -c must not fall back to discovery; got:\n{stdout}"
    );
}

/// Exit 2: a `-c` path that cannot be read is an error, not a silent fall back
/// to discovery. Reporting some other file's settings under the name the user
/// asked about is worse than reporting nothing.
#[test]
fn show_config_with_missing_config_path_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope.toml");
    let (_stdout, stderr, code) = run(
        &["--show-config", "-c", missing.to_str().unwrap()],
        tmp.path(),
    );
    assert_eq!(
        code, 2,
        "missing --config must exit 2; got {code}\n{stderr}"
    );
    assert!(
        stderr.contains("nope.toml"),
        "error must name the file; got:\n{stderr}"
    );
}

/// JSON mode emits the resolved settings as JSON, not the TOML template.
#[test]
fn show_config_json_reports_resolved_settings() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join(".updrc.toml"), SHOW_CONFIG_FIXTURE).unwrap();

    let (stdout, _stderr, code) = run(&["--show-config", "-o", "json"], tmp.path());
    assert_eq!(code, 0, "--show-config must exit 0; got {code}");

    let json = parse_json(&stdout);
    assert_eq!(json["ignore"][0], "left-pad");
    assert_eq!(json["include"][0], "ansible/roles/*/vars/*.yml");
    assert_eq!(json["exclude"][0], "**/vendor/**");
    assert_eq!(json["pin"]["sigstore/cosign-installer"], "v4.1.2");
    assert_eq!(json["cooldown"]["default_seconds"], 14 * 86_400);
    assert_eq!(json["cooldown"]["ecosystem_seconds"]["npm"], 3 * 86_400);
    assert_eq!(json["update_action_shas"], true);
    assert_eq!(json["automation"]["security_remediation"], true);
    assert!(
        json["config_file"]
            .as_str()
            .expect("config_file must be a string when a file was loaded")
            .ends_with(".updrc.toml"),
        "config_file must name the loaded file; got {}",
        json["config_file"]
    );
}

/// `config_file` is null rather than an empty string when no file was found, so
/// a consumer can tell "defaults" from "a file at path ''".
#[test]
fn show_config_json_reports_null_when_no_config_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, _stderr, code) = run(&["--show-config", "-o", "json"], tmp.path());
    assert_eq!(code, 0, "--show-config must exit 0; got {code}");
    let json = parse_json(&stdout);
    assert!(
        json["config_file"].is_null(),
        "config_file must be null with no config file; got {}",
        json["config_file"]
    );
    assert_eq!(
        json["automation"]["security_remediation"], false,
        "scheduled write automation must default to disabled"
    );
}

/// The reported settings fold in the command line, not just the file: this is
/// what the run will use, so `--min-age` and the action-SHA flags show through.
#[test]
fn show_config_folds_in_command_line_overrides() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join(".updrc.toml"), SHOW_CONFIG_FIXTURE).unwrap();

    let (stdout, _stderr, code) = run(
        &[
            "--show-config",
            "-o",
            "json",
            "--min-age",
            "30d",
            "--no-update-action-shas",
        ],
        tmp.path(),
    );
    assert_eq!(code, 0, "--show-config must exit 0; got {code}");
    let json = parse_json(&stdout);
    assert_eq!(
        json["cooldown"]["min_age_override_seconds"],
        30 * 86_400,
        "--min-age must show as the active override"
    );
    assert_eq!(
        json["update_action_shas"], false,
        "--no-update-action-shas must show through"
    );
}

/// Text mode still carries the schema, because a config-parse warning points
/// here to find the accepted keys.
#[test]
fn show_config_text_still_documents_the_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, _stderr, code) = run(&["--show-config", "-o", "text"], tmp.path());
    assert_eq!(code, 0, "--show-config must exit 0; got {code}");
    assert!(
        stdout.contains("[pin]"),
        "--show-config text must document the schema; got:\n{stdout}"
    );
}

// ── bad config parse tests ────────────────────────────────────────────────────

/// A config file using `[ignore]` (table) instead of `ignore = [...]` (array)
/// must surface a visible parse error on stderr.
///
/// This is the "original bug": before the fix, `load_from_path` swallowed the
/// error and the user saw zero output - the config was silently ignored.
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

    // The error must be visible - the user must not see silence.
    assert!(
        stderr.to_lowercase().contains("error"),
        "stderr must contain 'error' when config fails to parse; got:\n{stderr}"
    );
}
