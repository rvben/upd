//! Integration tests for audit severity normalization and sort order.
//!
//! These tests verify that:
//! - CVSS vector strings are converted to human-readable severity labels
//! - `database_specific.severity` strings are normalised (e.g. "MODERATE" → "Medium")
//! - Multiple vulnerabilities are sorted Critical → High → Medium → Low
//! - JSON output carries the normalized label, not the raw vector

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

fn parse_json(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not valid JSON ({e}):\n{stdout}"))
}

/// OSV returns a CVSS vector → audit text output shows the severity label, not the raw vector.
#[tokio::test]
async fn audit_cvss_vector_shows_severity_label_not_raw_vector() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": [{ "id": "GHSA-sev-001" }] }]
        })))
        .mount(&server)
        .await;

    // Severity is a CVSS vector → should map to "High"
    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-sev-001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-sev-001",
            "summary": "a high-severity vulnerability",
            "severity": [{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:N" }],
            "references": [{ "url": "https://example.com/sev-001" }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    let (stdout, stderr, _code) = run_with_env(
        &["audit", "--no-cache"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    // Should show "High" not the raw vector string
    assert!(
        stdout.contains("High"),
        "output should contain 'High' label; stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("CVSS:"),
        "output must not contain raw CVSS vector; stdout: {stdout}"
    );
}

/// OSV returns `database_specific.severity = "CRITICAL"` (no CVSS vector) → shows "Critical".
#[tokio::test]
async fn audit_database_specific_severity_shows_normalized_label() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": [{ "id": "GHSA-sev-002" }] }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-sev-002"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-sev-002",
            "summary": "a critical vulnerability",
            "database_specific": { "severity": "CRITICAL" },
            "references": [{ "url": "https://example.com/sev-002" }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    let (stdout, stderr, _code) = run_with_env(
        &["audit", "--no-cache"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    assert!(
        stdout.contains("Critical"),
        "output should show 'Critical' from database_specific.severity; stdout: {stdout}\nstderr: {stderr}"
    );
    // Raw "CRITICAL" string should not appear in output (normalized to title-case)
    assert!(
        !stdout.contains("CRITICAL"),
        "output must not contain raw 'CRITICAL'; stdout: {stdout}"
    );
}

/// OSV returns `database_specific.severity = "MODERATE"` → normalized to "Medium".
#[tokio::test]
async fn audit_moderate_severity_normalized_to_medium() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": [{ "id": "GHSA-sev-003" }] }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-sev-003"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-sev-003",
            "summary": "moderate vulnerability",
            "database_specific": { "severity": "MODERATE" },
            "references": [{ "url": "https://example.com/sev-003" }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    let (stdout, stderr, _code) = run_with_env(
        &["audit", "--no-cache"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    assert!(
        stdout.contains("Medium"),
        "MODERATE should normalize to 'Medium'; stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("MODERATE"),
        "raw 'MODERATE' must not appear; stdout: {stdout}"
    );
}

/// Multiple vulnerabilities with different severities → output order is Critical → High → Medium → Low.
#[tokio::test]
async fn audit_multiple_vulns_sorted_by_severity_descending() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // One package with two vulnerabilities returned as two separate IDs
    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [
                { "vulns": [
                    { "id": "GHSA-low-001" },
                    { "id": "GHSA-crit-001" },
                    { "id": "GHSA-med-001" },
                    { "id": "GHSA-high-001" }
                ]}
            ]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-low-001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-low-001",
            "summary": "low severity",
            "database_specific": { "severity": "LOW" },
            "references": [{ "url": "https://example.com/low" }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-crit-001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-crit-001",
            "summary": "critical severity",
            "database_specific": { "severity": "CRITICAL" },
            "references": [{ "url": "https://example.com/crit" }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-med-001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-med-001",
            "summary": "medium severity",
            "database_specific": { "severity": "MODERATE" },
            "references": [{ "url": "https://example.com/med" }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-high-001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-high-001",
            "summary": "high severity",
            "database_specific": { "severity": "HIGH" },
            "references": [{ "url": "https://example.com/high" }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    let (stdout, _stderr, _code) = run_with_env(
        &["audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    let json = parse_json(&stdout);
    let vulns = json["vulnerabilities"]
        .as_array()
        .expect("vulnerabilities must be an array");
    assert_eq!(vulns.len(), 4, "expected 4 vulnerabilities");

    let severities: Vec<&str> = vulns
        .iter()
        .map(|v| v["severity"].as_str().expect("severity must be a string"))
        .collect();

    assert_eq!(
        severities,
        vec!["Critical", "High", "Medium", "Low"],
        "vulnerabilities must be sorted Critical → High → Medium → Low; got: {severities:?}"
    );
}

/// JSON output carries the normalized severity label, not the raw CVSS vector string.
#[tokio::test]
async fn audit_json_output_contains_normalized_severity_label() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": [{ "id": "GHSA-json-001" }] }]
        })))
        .mount(&server)
        .await;

    // CVSS:3.1 vector that maps to Critical (9.8)
    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-json-001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "GHSA-json-001",
            "summary": "critical network vuln",
            "severity": [{
                "type": "CVSS_V3",
                "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"
            }],
            "references": [{ "url": "https://example.com/json-001" }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    let (stdout, stderr, _code) = run_with_env(
        &["audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );

    let json = parse_json(&stdout);
    let vulns = json["vulnerabilities"]
        .as_array()
        .expect("vulnerabilities must be an array");
    assert!(
        !vulns.is_empty(),
        "should have at least one vulnerability; stderr: {stderr}"
    );

    let sev = vulns[0]["severity"]
        .as_str()
        .expect("severity must be a string");
    assert_eq!(
        sev, "Critical",
        "JSON severity should be normalized label 'Critical', got: {sev}"
    );
    assert!(
        !sev.contains("CVSS:"),
        "JSON severity must not contain raw vector"
    );
}
