//! Integration tests for `upd audit --fix-audit`.
//!
//! Verifies:
//! - `--fix-audit --apply` rewrites a vulnerable package to the OSV fixed version.
//! - `--fix-audit` without `--apply` prints a dry-run preview, exits 1, leaves files unchanged.
//! - When `fixed_version` is absent for a vulnerability, emits a warning and leaves the file
//!   unchanged; falls through to the normal audit exit code (3 for unfixed vulns).
//! - `--fix-audit --no-fail` exits 0 even when pending fixes exist in dry-run mode.
//! - An already-clean audit (no vulnerabilities) with `--fix-audit` exits 0.

use std::fs;
use std::process::Command;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

fn run_with_env(
    args: &[&str],
    cwd: &std::path::Path,
    env: &[(&str, &str)],
) -> (String, String, i32) {
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

/// OSV mock: one vulnerable package with a fixed_version.
/// `--fix-audit --apply` should rewrite the version in requirements.txt.
#[tokio::test]
async fn fix_audit_apply_rewrites_vulnerable_package() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": [{ "id": "GHSA-fix-001" }] }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-fix-001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-fix-001",
            "summary": "test vulnerability",
            "database_specific": { "severity": "HIGH" },
            "affected": [{
                "ranges": [{
                    "events": [{ "fixed": "2.28.0" }]
                }]
            }],
            "references": [{ "url": "https://example.com/fix-001" }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let req_path = tmp.path().join("requirements.txt");
    fs::write(&req_path, "requests==1.0.0\n").unwrap();

    let (stdout, stderr, code) = run_with_env(
        &["audit", "--fix-audit", "--apply", "--no-cache"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    assert_eq!(
        code, 0,
        "--fix-audit --apply should exit 0 on success; stdout: {stdout}\nstderr: {stderr}"
    );

    let content = fs::read_to_string(&req_path).unwrap();
    assert!(
        content.contains("2.28.0"),
        "requirements.txt should be updated to the fixed version; got: {content}"
    );
    assert!(
        !content.contains("1.0.0"),
        "old vulnerable version should be replaced; got: {content}"
    );

    assert!(
        stdout.contains("Fixed") || stdout.contains("fix"),
        "output should mention the fix; stdout: {stdout}"
    );
}

/// `--fix-audit` without `--apply` is a dry-run: exits 1, file unchanged.
#[tokio::test]
async fn fix_audit_dry_run_exits_1_and_leaves_file_unchanged() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": [{ "id": "GHSA-fix-002" }] }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-fix-002"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-fix-002",
            "summary": "test vulnerability",
            "database_specific": { "severity": "HIGH" },
            "affected": [{
                "ranges": [{
                    "events": [{ "fixed": "2.28.0" }]
                }]
            }],
            "references": [{ "url": "https://example.com/fix-002" }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let req_path = tmp.path().join("requirements.txt");
    let original_content = "requests==1.0.0\n";
    fs::write(&req_path, original_content).unwrap();

    let (stdout, stderr, code) = run_with_env(
        &["audit", "--fix-audit", "--no-cache"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    assert_eq!(
        code, 1,
        "--fix-audit without --apply should exit 1 (pending fixes); stdout: {stdout}\nstderr: {stderr}"
    );

    let content = fs::read_to_string(&req_path).unwrap();
    assert_eq!(
        content, original_content,
        "file must not be modified in dry-run mode; got: {content}"
    );

    // Dry-run output should mention the would-be fix.
    assert!(
        stdout.contains("Would fix") || stdout.contains("would fix") || stdout.contains("2.28.0"),
        "dry-run output should indicate what would be fixed; stdout: {stdout}"
    );
}

/// When a vulnerability has no `fixed_version`, emit a warning and don't touch the file.
/// Falls through to normal audit exit code (3 = vulnerable, !no_fail).
#[tokio::test]
async fn fix_audit_no_fixed_version_warns_and_exits_3() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": [{ "id": "GHSA-fix-003" }] }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-fix-003"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-fix-003",
            "summary": "test vulnerability with no fix",
            "database_specific": { "severity": "CRITICAL" },
            "references": [{ "url": "https://example.com/fix-003" }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let req_path = tmp.path().join("requirements.txt");
    let original_content = "requests==1.0.0\n";
    fs::write(&req_path, original_content).unwrap();

    let (stdout, stderr, code) = run_with_env(
        &["audit", "--fix-audit", "--no-cache"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    // No fixable packages → falls through to normal audit exit code.
    // Normal audit with vulnerabilities and !no_fail → exit 3.
    assert_eq!(
        code, 3,
        "should exit 3 (vulnerable, no fix available); stdout: {stdout}\nstderr: {stderr}"
    );

    let content = fs::read_to_string(&req_path).unwrap();
    assert_eq!(
        content, original_content,
        "file must not be touched when no fix is available; got: {content}"
    );

    // Warning should appear on stderr.
    assert!(
        stderr.contains("Cannot auto-fix") || stderr.contains("no fixed version"),
        "stderr should warn about unfixable vuln; stderr: {stderr}"
    );
}

/// `--fix-audit --no-fail` exits 0 even when there are pending fixes (dry-run).
#[tokio::test]
async fn fix_audit_no_fail_exits_0_on_pending_fixes() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": [{ "id": "GHSA-fix-004" }] }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-fix-004"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-fix-004",
            "summary": "test vulnerability",
            "database_specific": { "severity": "HIGH" },
            "affected": [{
                "ranges": [{
                    "events": [{ "fixed": "2.28.0" }]
                }]
            }],
            "references": [{ "url": "https://example.com/fix-004" }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    let (_stdout, stderr, code) = run_with_env(
        &["audit", "--fix-audit", "--no-fail", "--no-cache"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    assert_eq!(
        code, 0,
        "--fix-audit --no-fail should exit 0 even with pending fixes; stderr: {stderr}"
    );
}

/// A clean audit (no vulnerabilities) with `--fix-audit` exits 0 and does nothing.
#[tokio::test]
async fn fix_audit_clean_audit_exits_0() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": [] }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let req_path = tmp.path().join("requirements.txt");
    let original_content = "requests==2.31.0\n";
    fs::write(&req_path, original_content).unwrap();

    let (stdout, stderr, code) = run_with_env(
        &["audit", "--fix-audit", "--no-cache"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    assert_eq!(
        code, 0,
        "--fix-audit on clean audit should exit 0; stdout: {stdout}\nstderr: {stderr}"
    );

    let content = fs::read_to_string(&req_path).unwrap();
    assert_eq!(
        content, original_content,
        "file must be unchanged on clean audit; got: {content}"
    );
}
