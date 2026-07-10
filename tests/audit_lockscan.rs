//! Lock-only transitive dependencies surface in audit findings.

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

/// pyproject.toml declares nothing vulnerable; uv.lock pins a lock-only
/// transitive that OSV flags. The finding must appear, sourced from the lock.
#[tokio::test]
async fn lock_only_package_is_audited() {
    let server = wiremock::MockServer::start().await;
    // Exactly ONE audit package exists (the manifest declares no
    // dependencies; the lock holds one registry package), so the batch
    // response is order-independent by construction.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/querybatch"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [ { "vulns": [ { "id": "GHSA-lock-only-1" } ] } ]
            })),
        )
        .expect(1)
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/vulns/GHSA-lock-only-1"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "GHSA-lock-only-1",
                "summary": "lock-only vuln",
                "aliases": ["CVE-2026-11111"],
                "affected": [{
                    "package": { "name": "lockonly", "ecosystem": "PyPI" },
                    "ranges": [{ "type": "ECOSYSTEM",
                        "events": [{ "introduced": "0" }, { "fixed": "0.49.1" }] }]
                }]
            })),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = []\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("uv.lock"),
        r#"version = 1

[[package]]
name = "lockonly"
version = "0.40.0"
source = { registry = "https://pypi.org/simple" }
"#,
    )
    .unwrap();

    let (stdout, _stderr, code) = run_with_env(
        &["audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );
    assert_eq!(code, 6, "vulnerable audit exits 6");
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let vulns = json["vulnerabilities"].as_array().unwrap();
    assert!(
        vulns
            .iter()
            .any(|v| v["package"] == "lockonly" && v["id"] == "GHSA-lock-only-1"),
        "lock-only package must be audited: {vulns:?}"
    );
    // Prove exactly one query went out, and for the lock package.
    let requests = server.received_requests().await.unwrap();
    let batch: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let queries = batch["queries"].as_array().unwrap();
    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0]["package"]["name"], "lockonly");
}

/// Same package declared in the manifest and resolved in the lock: the lock
/// version is ground truth, so exactly ONE query goes out - for the locked
/// version - and no fabricated manifest-fragment query is sent.
#[tokio::test]
async fn manifest_entry_suppressed_by_lock_resolution() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/querybatch"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [ { "vulns": [] } ]
            })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = [\"Same_Pkg==2.0.0\"]\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("uv.lock"),
        "version = 1\n\n[[package]]\nname = \"same-pkg\"\nversion = \"2.0.0\"\nsource = { registry = \"https://pypi.org/simple\" }\n",
    )
    .unwrap();

    let (stdout, _stderr, code) = run_with_env(
        &["audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );
    assert_eq!(code, 0);
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["summary"]["packages_checked"], 1);
    // The proof is in the REQUEST, not just the summary (missing results
    // silently drop, so packages_checked alone cannot prove suppression):
    // exactly one query, for the lock's spelling and exact version.
    let requests = server.received_requests().await.unwrap();
    let batch: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let queries = batch["queries"].as_array().unwrap();
    assert_eq!(queries.len(), 1, "manifest fragment must be suppressed");
    assert_eq!(queries[0]["package"]["name"], "same-pkg");
    assert_eq!(queries[0]["version"], "2.0.0");
}

/// Responds to `/querybatch` by inspecting the request body's `queries`
/// array and building a per-query result: `scan_packages` groups packages
/// in a `HashMap`, so the order the two `dupcrate` duplicate versions and
/// `directcrate` land in the batch is NOT deterministic. A static
/// order-aligned results array would be tautological (it could pass by
/// accident with the wrong query mapped to the wrong answer); this responder
/// proves the right answer went to the right query regardless of order.
struct DupcrateResponder;

impl wiremock::Respond for DupcrateResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let queries = body["queries"].as_array().expect("queries array");
        let results: Vec<serde_json::Value> = queries
            .iter()
            .map(|q| {
                let name = q["package"]["name"].as_str().unwrap_or_default();
                let version = q["version"].as_str().unwrap_or_default();
                if name == "dupcrate" && version == "1.2.3" {
                    serde_json::json!({ "vulns": [ { "id": "GHSA-dup-1" } ] })
                } else {
                    serde_json::json!({ "vulns": [] })
                }
            })
            .collect();
        wiremock::ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({ "results": results }))
    }
}

/// Cargo.lock resolves `directcrate` (also declared in Cargo.toml, so the
/// lock version is ground truth and the manifest fragment is suppressed)
/// plus TWO registry versions of `dupcrate` (lock-only, not declared in the
/// manifest at all). Only the 1.2.3 duplicate is vulnerable; the finding
/// must land on the right version, not smear across both.
#[tokio::test]
async fn cargo_lock_only_duplicate_version_is_audited() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/querybatch"))
        .respond_with(DupcrateResponder)
        .expect(1)
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/vulns/GHSA-dup-1"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "GHSA-dup-1",
                "summary": "dup vuln",
                "aliases": ["CVE-2026-22222"],
                "affected": [{
                    "package": { "name": "dupcrate", "ecosystem": "crates.io" },
                    "ranges": [{ "type": "ECOSYSTEM",
                        "events": [{ "introduced": "0" }, { "fixed": "1.3.0" }] }]
                }]
            })),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\n\n[dependencies]\ndirectcrate = \"1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("Cargo.lock"),
        r#"version = 4

[[package]]
name = "t"
version = "0.1.0"
dependencies = ["directcrate"]

[[package]]
name = "directcrate"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "dupcrate"
version = "1.2.3"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "dupcrate"
version = "2.0.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#,
    )
    .unwrap();

    let (stdout, _stderr, code) = run_with_env(
        &["audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );
    assert_eq!(code, 6, "vulnerable audit exits 6");
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let vulns = json["vulnerabilities"].as_array().unwrap();
    assert!(
        vulns.iter().any(|v| v["package"] == "dupcrate"
            && v["version"] == "1.2.3"
            && v["id"] == "GHSA-dup-1"),
        "dupcrate 1.2.3 finding must appear: {vulns:?}"
    );
    assert!(
        !vulns
            .iter()
            .any(|v| v["package"] == "dupcrate" && v["version"] == "2.0.1"),
        "dupcrate 2.0.1 must not be flagged: {vulns:?}"
    );
    assert!(
        !vulns.iter().any(|v| v["package"] == "directcrate"),
        "directcrate must not be flagged: {vulns:?}"
    );
    assert_eq!(
        json["summary"]["packages_checked"], 3,
        "directcrate + both dupcrate duplicate versions checked separately: {json:?}"
    );
}

/// pyproject.toml declares nothing vulnerable; poetry.lock resolves a
/// lock-only transitive (`poetrydep`) that OSV flags. Mirrors the uv
/// lock-only case above but through the poetry.lock reader.
#[tokio::test]
async fn poetry_lock_only_package_is_audited() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/querybatch"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [ { "vulns": [ { "id": "GHSA-poetry-1" } ] } ]
            })),
        )
        .expect(1)
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/vulns/GHSA-poetry-1"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "GHSA-poetry-1",
                "summary": "poetry lock-only vuln",
                "aliases": ["CVE-2026-33333"],
                "affected": [{
                    "package": { "name": "poetrydep", "ecosystem": "PyPI" },
                    "ranges": [{ "type": "ECOSYSTEM",
                        "events": [{ "introduced": "0" }, { "fixed": "1.0.1" }] }]
                }]
            })),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = []\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("poetry.lock"),
        r#"[[package]]
name = "poetrydep"
version = "1.0.0"
description = "test"
optional = false
python-versions = ">=3.8"

[metadata]
lock-version = "2.0"
python-versions = ">=3.8"
content-hash = "0000"
"#,
    )
    .unwrap();

    let (stdout, _stderr, code) = run_with_env(
        &["audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );
    assert_eq!(code, 6, "vulnerable audit exits 6");
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let vulns = json["vulnerabilities"].as_array().unwrap();
    assert!(
        vulns
            .iter()
            .any(|v| v["package"] == "poetrydep" && v["id"] == "GHSA-poetry-1"),
        "poetry lock-only package must be audited: {vulns:?}"
    );
    // Prove exactly one query went out, and for the lock package - the
    // same non-vacuousness check used for the uv lock-only case above.
    let requests = server.received_requests().await.unwrap();
    let batch: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let queries = batch["queries"].as_array().unwrap();
    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0]["package"]["name"], "poetrydep");
}

/// SARIF anchoring: a lock-only finding's location must point at the
/// lockfile entry, not a nonexistent manifest occurrence. Mirrors the uv
/// lock-only scenario above but requests `--format sarif` and inspects the
/// result the way `tests/audit_sarif.rs` does.
#[tokio::test]
async fn sarif_lock_only_finding_anchors_to_lockfile() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/querybatch"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [ { "vulns": [ { "id": "GHSA-sarif-lock-1" } ] } ]
            })),
        )
        .expect(1)
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/vulns/GHSA-sarif-lock-1"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "GHSA-sarif-lock-1",
                "summary": "sarif lock-only vuln",
                "affected": [{
                    "package": { "name": "sariflockpkg", "ecosystem": "PyPI" },
                    "ranges": [{ "type": "ECOSYSTEM",
                        "events": [{ "introduced": "0" }, { "fixed": "0.2.0" }] }]
                }]
            })),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = []\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("uv.lock"),
        r#"version = 1

[[package]]
name = "sariflockpkg"
version = "0.1.0"
source = { registry = "https://pypi.org/simple" }
"#,
    )
    .unwrap();

    let (stdout, _stderr, code) = run_with_env(
        &["audit", "--no-cache", "--format", "sarif"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );
    assert_eq!(code, 6, "vulnerable audit exits 6");
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let run = &json["runs"][0];
    let results = run["results"].as_array().unwrap();
    let result = results
        .iter()
        .find(|r| r["ruleId"] == "GHSA-sarif-lock-1")
        .expect("sarif result for the lock-only finding");
    let locations = result["locations"].as_array().unwrap();
    assert!(!locations.is_empty(), "at least one location expected");
    let uri = locations[0]["physicalLocation"]["artifactLocation"]["uri"]
        .as_str()
        .unwrap();
    assert!(
        uri.ends_with("uv.lock"),
        "lock-only finding must anchor to the lockfile, got: {uri}"
    );
}

/// npm workspaces: lock skipped, warning emitted, status incomplete, exit 0.
#[tokio::test]
async fn npm_workspace_lock_is_skipped_with_warning() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("package.json"),
        r#"{ "name": "t", "version": "1.0.0", "workspaces": ["packages/*"] }"#,
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("package-lock.json"),
        r#"{ "name": "t", "lockfileVersion": 3, "packages": { "": {},
            "node_modules/somepkg": { "version": "1.0.0" } } }"#,
    )
    .unwrap();

    // No OSV server: the manifest has no dependencies and the lock is
    // skipped, so nothing is queried. --offline keeps it airtight.
    let (stdout, _stderr, code) =
        run_with_env(&["audit", "--offline", "--format", "json"], tmp.path(), &[]);
    assert_eq!(code, 0, "warnings never change the exit code");
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["status"], "incomplete");
    let warnings = json["warnings"].as_array().expect("warnings present");
    assert!(warnings[0].as_str().unwrap().contains("npm workspaces"));
    assert!(
        json["vulnerabilities"].as_array().unwrap().is_empty()
            && json["summary"]["packages_checked"] == 0,
        "no lock packages scanned"
    );
}
