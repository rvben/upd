//! End-to-end tests for `upd audit --format sarif`.
//!
//! A wiremock server acts as OSV so the test runs offline with a controlled
//! vulnerability payload. We parse the stdout JSON and check that the SARIF
//! 2.1.0 structure is correct including `$schema`, `version`, tool metadata,
//! result rule IDs, and physical location with line numbers.

use std::fs;
use std::process::Command;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

fn parse_json(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not valid JSON ({e}):\n{stdout}"))
}

/// Write a minimal requirements.txt with a pinned package at a known line.
fn write_requirements(dir: &TempDir, content: &str) {
    fs::write(dir.path().join("requirements.txt"), content).unwrap();
}

#[tokio::test]
async fn audit_sarif_on_empty_workspace_emits_valid_sarif() {
    let tmp = tempfile::tempdir().unwrap();
    // No dependency files → early-exit SARIF.
    let (stdout, _stderr, code) = run_with_env(
        &["--format", "sarif", "audit"],
        tmp.path(),
        &[("UPD_CACHE_DIR", tmp.path().to_str().unwrap())],
    );
    assert_eq!(code, 0, "exit 0 on empty workspace; stderr: {_stderr}");

    let json = parse_json(&stdout);
    assert_eq!(
        json["$schema"],
        "https://json.schemastore.org/sarif-2.1.0.json"
    );
    assert_eq!(json["version"], "2.1.0");
    let runs = json["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["tool"]["driver"]["name"], "upd");
    assert!(runs[0]["results"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn audit_sarif_with_vulnerability_emits_correct_structure() {
    let tmp = tempfile::tempdir().unwrap();

    // requirements.txt: "requests" on line 1.
    write_requirements(&tmp, "requests==2.27.0\n");

    // Start wiremock as a fake OSV API.
    let server = MockServer::start().await;

    // POST /querybatch → one vuln reference.
    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": [{ "id": "GHSA-test-sarif-xxxx" }] }]
        })))
        .mount(&server)
        .await;

    // GET /vulns/GHSA-test-sarif-xxxx → full vuln details.
    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-test-sarif-xxxx"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-test-sarif-xxxx",
            "summary": "Test vulnerability in requests",
            "database_specific": { "severity": "High" },
            "affected": [{
                "ranges": [{ "events": [{ "fixed": "2.28.0" }] }]
            }],
            "references": [{ "url": "https://example.com/GHSA-test-sarif-xxxx" }]
        })))
        .mount(&server)
        .await;

    let osv_url = server.uri();

    let (stdout, _stderr, _code) = run_with_env(
        &[
            "--format",
            "sarif",
            "audit",
            "--no-fail",
            tmp.path().to_str().unwrap(),
        ],
        tmp.path(),
        &[
            ("OSV_API_URL", &osv_url),
            ("UPD_CACHE_DIR", tmp.path().to_str().unwrap()),
        ],
    );

    let json = parse_json(&stdout);

    // Top-level SARIF envelope.
    assert_eq!(
        json["$schema"], "https://json.schemastore.org/sarif-2.1.0.json",
        "SARIF $schema must be set"
    );
    assert_eq!(json["version"], "2.1.0", "SARIF version must be 2.1.0");

    let run = &json["runs"][0];

    // Tool driver metadata.
    let driver = &run["tool"]["driver"];
    assert_eq!(driver["name"], "upd");
    assert_eq!(driver["informationUri"], "https://github.com/rvben/upd");

    // Rules: one entry for the single vulnerability ID.
    let rules = run["tool"]["driver"]["rules"].as_array().unwrap();
    assert_eq!(
        rules.len(),
        1,
        "exactly one rule for one unique vulnerability"
    );
    assert_eq!(rules[0]["id"], "GHSA-test-sarif-xxxx");

    // Results: one result for the one vulnerability.
    let results = run["results"].as_array().unwrap();
    assert!(!results.is_empty(), "results must not be empty");
    let result = &results[0];

    assert_eq!(result["ruleId"], "GHSA-test-sarif-xxxx");
    assert_eq!(
        result["level"], "error",
        "High severity should map to error"
    );

    // Location: must point at requirements.txt with a line number.
    let locations = result["locations"].as_array().unwrap();
    assert!(!locations.is_empty(), "at least one location expected");
    let phys = &locations[0]["physicalLocation"];
    let uri = phys["artifactLocation"]["uri"].as_str().unwrap();
    assert_eq!(
        uri, "requirements.txt",
        "artifact URI should be relative to the invocation dir, got: {uri}"
    );
    let start_line = phys["region"]["startLine"].as_u64().unwrap();
    assert_eq!(start_line, 1, "requests is on line 1");

    // Properties.
    let props = &result["properties"];
    assert_eq!(props["package"], "requests");
    assert_eq!(props["version"], "2.27.0");
    assert_eq!(props["ecosystem"], "PyPI");
    assert_eq!(props["fixedVersion"], "2.28.0");
}

#[tokio::test]
async fn audit_sarif_no_fail_flag_suppresses_nonzero_exit() {
    let tmp = tempfile::tempdir().unwrap();
    // A workspace with no dependency files returns exit 0 regardless.
    let (stdout, _stderr, code) = run_with_env(
        &["--format", "sarif", "audit", "--no-fail"],
        tmp.path(),
        &[("UPD_CACHE_DIR", tmp.path().to_str().unwrap())],
    );
    assert_eq!(code, 0);
    let json = parse_json(&stdout);
    assert_eq!(json["version"], "2.1.0");
}
