//! End-to-end: aliases + source surface in JSON, and fixed_version comes
//! from the range branch containing the installed version.

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

#[tokio::test]
async fn aliases_source_and_branch_scoped_fix_flow_through() {
    let server = wiremock::MockServer::start().await;

    // querybatch: one vulnerable package.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/querybatch"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [ { "vulns": [ { "id": "GHSA-aaaa-bbbb-cccc" } ] } ]
            })),
        )
        .mount(&server)
        .await;

    // Vulnerability detail: multi-branch affected ranges; installed 2.0.5
    // must resolve to the 2.x branch fix 2.1.3, not the 1.x fix 1.2.5.
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/vulns/GHSA-aaaa-bbbb-cccc"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "GHSA-aaaa-bbbb-cccc",
                "summary": "Multi-branch test advisory",
                "aliases": ["CVE-2026-99999", "PYSEC-2026-999"],
                "affected": [
                    {
                        "package": { "name": "examplepkg", "ecosystem": "PyPI" },
                        "ranges": [ {
                            "type": "ECOSYSTEM",
                            "events": [
                                { "introduced": "1.0.0" }, { "fixed": "1.2.5" },
                                { "introduced": "2.0.0" }, { "fixed": "2.1.3" }
                            ]
                        } ]
                    },
                    {
                        "package": { "name": "otherpkg", "ecosystem": "PyPI" },
                        "ranges": [ {
                            "type": "ECOSYSTEM",
                            "events": [ { "introduced": "0" }, { "fixed": "9.9.9" } ]
                        } ]
                    }
                ]
            })),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("requirements.txt"), "examplepkg==2.0.5\n").unwrap();

    // Run the real binary exactly as tests/audit_severity.rs does: copy its
    // `upd_bin()` and `run_with_env()` helpers VERBATIM (run_with_env returns
    // a (stdout, stderr, exit_code) tuple, not std::process::Output), and use
    // --no-cache like the canonical JSON audit calls so the on-disk cache
    // never interferes.
    let (stdout, _stderr, code) = run_with_env(
        &["audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &[("OSV_API_URL", &server.uri())],
    );
    assert_eq!(code, 6, "vulnerable audit exits 6 (vulnerabilities_found)");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let vuln = &json["vulnerabilities"][0];
    assert_eq!(vuln["id"], "GHSA-aaaa-bbbb-cccc");
    assert_eq!(vuln["source"], "GHSA");
    assert_eq!(vuln["aliases"][0], "CVE-2026-99999");
    assert_eq!(
        vuln["fixed_version"], "2.1.3",
        "fix must come from the 2.x branch containing 2.0.5"
    );
}
