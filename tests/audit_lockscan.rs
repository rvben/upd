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
