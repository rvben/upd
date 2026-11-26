//! `[audit] ignore-advisories`: accepted advisories pass the audit, stay
//! visible with their reason, and never hide behind a malformed or expired
//! entry.

use std::fs;
use std::path::Path;
use std::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ADVISORY: &str = "RUSTSEC-2026-0543";

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

fn run(args: &[&str], cwd: &Path, osv: &str) -> (String, String, i32) {
    let output = Command::new(upd_bin())
        .args(args)
        .current_dir(cwd)
        .env("OSV_API_URL", osv)
        // `--fix-audit` asks the package registries which fixes are
        // published; they answer from the same mock, never a live registry.
        .env_remove("UV_EXTRA_INDEX_URL")
        .env_remove("PIP_EXTRA_INDEX_URL")
        .env("UV_INDEX_URL", osv)
        .output()
        .expect("run upd");
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
        output.status.code().unwrap_or(-1),
    )
}

/// OSV mock reporting `ids` for the one package in the fixture. The first has
/// no fixed version, like an unmaintained-crate notice; `fixable` is the
/// exception and names a fix.
async fn osv(ids: &[(&str, Option<&str>)]) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{ "vulns": ids.iter().map(|(id, _)| serde_json::json!({"id": id})).collect::<Vec<_>>() }]
        })))
        .mount(&server)
        .await;
    for (id, fixed) in ids {
        let events = match fixed {
            Some(fixed) => serde_json::json!([{ "introduced": "0" }, { "fixed": fixed }]),
            None => serde_json::json!([{ "introduced": "0" }]),
        };
        Mock::given(method("GET"))
            .and(path(format!("/vulns/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": id,
                "summary": format!("{id} summary"),
                "aliases": [format!("CVE-{id}")],
                "affected": [{
                    "package": { "name": "examplepkg", "ecosystem": "PyPI" },
                    "ranges": [{ "type": "ECOSYSTEM", "events": events }]
                }],
            })))
            .mount(&server)
            .await;
    }
    server
}

fn project(config: Option<&str>) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "examplepkg==1.0.0\n").unwrap();
    if let Some(config) = config {
        fs::write(tmp.path().join(".updrc.toml"), config).unwrap();
    }
    tmp
}

fn suppression(extra: &str) -> String {
    format!(
        "[audit]\nignore-advisories = [{{ id = \"{ADVISORY}\", reason = \"no successor release\"{extra} }}]\n"
    )
}

fn json(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {stdout}"))
}

#[tokio::test]
async fn unsuppressed_advisory_fails_the_audit() {
    let server = osv(&[(ADVISORY, None)]).await;
    let tmp = project(None);
    let (_, _, code) = run(&["audit", "--no-cache"], tmp.path(), &server.uri());
    assert_eq!(code, 6);
}

#[tokio::test]
async fn suppressed_advisory_passes_and_is_listed_with_its_reason() {
    let server = osv(&[(ADVISORY, None)]).await;
    let tmp = project(Some(&suppression(", expires = \"2999-12-31\"")));

    let (stdout, stderr, code) = run(
        &["audit", "--no-cache", "--format", "text"],
        tmp.path(),
        &server.uri(),
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("No vulnerabilities found"), "{stdout}");
    assert!(stdout.contains(ADVISORY) && stdout.contains("examplepkg 1.0.0"));
    assert!(stdout.contains("no successor release") && stdout.contains("2999-12-31"));
    assert!(!stderr.contains("Warning"), "{stderr}");

    let (stdout, _, code) = run(
        &["audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &server.uri(),
    );
    assert_eq!(code, 0);
    let report = json(&stdout);
    assert_eq!(report["vulnerabilities"], serde_json::json!([]));
    assert_eq!(report["summary"]["vulnerabilities"], 0);
    assert_eq!(report["summary"]["vulnerable_packages"], 0);
    assert_eq!(report["summary"]["suppressed"], 1);
    assert_eq!(report["summary"]["packages_checked"], 1);
    assert_eq!(report["status"], "complete");
    let suppressed = &report["suppressed"][0];
    assert_eq!(suppressed["id"], ADVISORY);
    assert_eq!(suppressed["package"], "examplepkg");
    assert_eq!(suppressed["reason"], "no successor release");
    assert_eq!(suppressed["expires"], "2999-12-31");
    assert!(report.get("suppression_warnings").is_none());
}

#[tokio::test]
async fn an_alias_and_a_toml_date_suppress_too() {
    let server = osv(&[(ADVISORY, None)]).await;
    let tmp = project(Some(&format!(
        "[audit]\nignore-advisories = [{{ id = \"cve-{ADVISORY}\", reason = \"accepted\", expires = 2999-12-31 }}]\n"
    )));
    let (stdout, _, code) = run(
        &["audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &server.uri(),
    );
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(json(&stdout)["suppressed"][0]["id"], ADVISORY);
}

#[tokio::test]
async fn only_the_named_advisory_is_suppressed() {
    let server = osv(&[(ADVISORY, None), ("GHSA-other-0001", None)]).await;
    let tmp = project(Some(&suppression("")));
    let (stdout, _, code) = run(
        &["audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &server.uri(),
    );
    assert_eq!(code, 6);
    let report = json(&stdout);
    assert_eq!(report["vulnerabilities"].as_array().unwrap().len(), 1);
    assert_eq!(report["vulnerabilities"][0]["id"], "GHSA-other-0001");
    assert_eq!(report["suppressed"][0]["id"], ADVISORY);
    assert_eq!(report["summary"]["vulnerable_packages"], 1);
}

#[tokio::test]
async fn an_expired_suppression_stops_applying_and_says_so() {
    let server = osv(&[(ADVISORY, None)]).await;
    let tmp = project(Some(&suppression(", expires = \"2020-01-01\"")));
    let (stdout, stderr, code) = run(
        &["audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &server.uri(),
    );
    assert_eq!(code, 6, "an expired suppression must not hide the advisory");
    assert!(stderr.contains("expired on 2020-01-01"), "{stderr}");
    assert!(stderr.contains("reported again"), "{stderr}");
    let report = json(&stdout);
    assert_eq!(report["vulnerabilities"][0]["id"], ADVISORY);
    assert!(report.get("suppressed").is_none());
    assert!(
        report["suppression_warnings"][0]
            .as_str()
            .unwrap()
            .contains("expired")
    );
}

#[tokio::test]
async fn a_suppression_matching_nothing_warns_without_failing() {
    let server = osv(&[]).await;
    let tmp = project(Some(&suppression("")));
    let (stdout, stderr, code) = run(
        &["audit", "--no-cache", "--format", "json"],
        tmp.path(),
        &server.uri(),
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("matches no advisory"), "{stderr}");
    let report = json(&stdout);
    assert!(
        report["suppression_warnings"][0]
            .as_str()
            .unwrap()
            .contains(ADVISORY)
    );
    assert_eq!(
        report["status"], "complete",
        "a stale suppression does not make the audit incomplete"
    );
}

#[tokio::test]
async fn a_language_filtered_audit_does_not_call_a_suppression_unmatched() {
    let server = osv(&[]).await;
    let tmp = project(Some(&suppression("")));
    let (_, stderr, code) = run(
        &["audit", "--no-cache", "--lang", "python"],
        tmp.path(),
        &server.uri(),
    );
    assert_eq!(code, 0);
    assert!(!stderr.contains("matches no advisory"), "{stderr}");
}

#[tokio::test]
async fn a_malformed_entry_is_a_config_error_and_suppresses_nothing() {
    let server = osv(&[(ADVISORY, None)]).await;
    let cases = [
        // reason is required
        format!("[audit]\nignore-advisories = [{{ id = \"{ADVISORY}\" }}]\n"),
        format!("[audit]\nignore-advisories = [{{ id = \"{ADVISORY}\", reason = \"  \" }}]\n"),
        // an id names one advisory
        "[audit]\nignore-advisories = [{ id = \"\", reason = \"x\" }]\n".to_string(),
        "[audit]\nignore-advisories = [{ id = \"RUSTSEC-1 RUSTSEC-2\", reason = \"x\" }]\n"
            .to_string(),
        "[audit]\nignore-advisories = [{ id = \"RUSTSEC-*\", reason = \"x\" }]\n".to_string(),
        // expiry is a plain date
        suppression(", expires = \"soon\""),
        suppression(", expires = \"2027-5-1\""),
        suppression(", expires = \"2027-05-31T10:00:00Z\""),
        suppression(", expires = 20270531"),
        suppression(", expires = \"+2999-1-01\""),
        suppression(", expires = \"2999- 1-01\""),
        suppression(", expires = \"+10000-01-01\""),
        // a value that spells another error class must still be a config error
        suppression(", expires = \"timeout\""),
        suppression(", expires = \"No such file\""),
        // a misspelled key must not leave a suppression half-written
        suppression(", expiry = \"2027-05-31\""),
        "[audit]\nignore-advisory = []\n".to_string(),
        // one id twice
        format!(
            "[audit]\nignore-advisories = [{{ id = \"{ADVISORY}\", reason = \"a\" }}, {{ id = \"{}\", reason = \"b\" }}]\n",
            ADVISORY.to_lowercase()
        ),
    ];
    for config in cases {
        let tmp = project(Some(&config));
        let (stdout, stderr, code) = run(&["audit", "--no-cache"], tmp.path(), &server.uri());
        assert_eq!(code, 4, "{config}\nstdout: {stdout}\nstderr: {stderr}");
        assert!(
            stderr.contains("[audit]") || stderr.contains("audit"),
            "{stderr}"
        );
        assert!(!stdout.contains(ADVISORY), "{config}\n{stdout}");
    }
}

#[tokio::test]
async fn sarif_omits_accepted_findings_and_lists_them_for_other_consumers() {
    let server = osv(&[(ADVISORY, None), ("GHSA-other-0001", None)]).await;
    let tmp = project(Some(&suppression(", expires = \"2999-12-31\"")));
    let (stdout, stderr, code) = run(
        &["audit", "--no-cache", "--format", "sarif"],
        tmp.path(),
        &server.uri(),
    );
    assert_eq!(code, 6);
    let sarif = json(&stdout);
    let run_ = &sarif["runs"][0];
    let results = run_["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["ruleId"], "GHSA-other-0001");
    assert!(
        results[0].get("suppressions").is_none(),
        "GitHub code scanning ignores result suppressions, so an accepted \
         finding must be absent rather than marked"
    );
    let rules = run_["tool"]["driver"]["rules"].as_array().unwrap();
    assert!(rules.iter().all(|rule| rule["id"] != ADVISORY));
    let accepted = &run_["properties"]["suppressedAdvisories"][0];
    assert_eq!(accepted["ruleId"], ADVISORY);
    assert_eq!(accepted["justification"], "no successor release");
    assert_eq!(accepted["expires"], "2999-12-31");
    // The failing advisory is named in the log even though the SARIF goes to
    // stdout for upload.
    assert!(
        stderr.contains("GHSA-other-0001 (examplepkg 1.0.0)"),
        "{stderr}"
    );
    assert!(!stderr.contains(&format!("{ADVISORY} (examplepkg")) || stderr.contains("Suppressed"));
}

#[tokio::test]
async fn sarif_without_suppressions_has_no_run_properties() {
    let server = osv(&[("GHSA-other-0001", None)]).await;
    let tmp = project(None);
    let (stdout, _, code) = run(
        &["audit", "--no-cache", "--format", "sarif"],
        tmp.path(),
        &server.uri(),
    );
    assert_eq!(code, 6);
    assert!(json(&stdout)["runs"][0].get("properties").is_none());
}

#[tokio::test]
async fn fix_audit_leaves_a_suppressed_advisory_alone_and_exits_clean() {
    // Fixable advisory: without the suppression `--fix-audit --apply` rewrites
    // the pin.
    let fixable = osv(&[("GHSA-fixable-1", Some("2.0.0"))]).await;
    let control = project(None);
    let (_, _, code) = run(
        &["audit", "--fix-audit", "--apply", "--no-cache"],
        control.path(),
        &fixable.uri(),
    );
    assert_eq!(code, 0);
    assert!(
        fs::read_to_string(control.path().join("requirements.txt"))
            .unwrap()
            .contains("2.0.0")
    );

    let suppressed = project(Some(
        "[audit]\nignore-advisories = [{ id = \"GHSA-fixable-1\", reason = \"accepted\" }]\n",
    ));
    let (stdout, stderr, code) = run(
        &["audit", "--fix-audit", "--apply", "--no-cache"],
        suppressed.path(),
        &fixable.uri(),
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        fs::read_to_string(suppressed.path().join("requirements.txt")).unwrap(),
        "examplepkg==1.0.0\n",
        "an accepted advisory is not fixed"
    );
}

#[tokio::test]
async fn an_advisory_with_no_fix_is_not_an_unresolved_remediation_residual() {
    let server = osv(&[(ADVISORY, None)]).await;

    let control = project(None);
    let (_, _, code) = run(
        &["audit", "--fix-audit", "--apply", "--no-cache"],
        control.path(),
        &server.uri(),
    );
    assert_eq!(code, 6, "control: an unfixable advisory is left unresolved");

    let suppressed = project(Some(&suppression("")));
    let (stdout, _, code) = run(
        &[
            "audit",
            "--fix-audit",
            "--apply",
            "--no-cache",
            "--format",
            "json",
        ],
        suppressed.path(),
        &server.uri(),
    );
    assert_eq!(code, 0, "{stdout}");
    let report = json(&stdout);
    assert_eq!(report["summary"]["vulnerabilities"], 0);
    assert!(report.get("fixes").is_none(), "nothing to fix: {report}");
    assert_eq!(report["suppressed"][0]["id"], ADVISORY);
}

#[tokio::test]
async fn several_paths_accept_an_advisory_only_when_each_one_does() {
    let server = osv(&[(ADVISORY, None)]).await;
    let accepting = project(Some(&suppression("")));
    let plain = project(None);
    for pair in [
        [accepting.path(), plain.path()],
        [plain.path(), accepting.path()],
    ] {
        let args = [
            "audit",
            "--no-cache",
            pair[0].to_str().unwrap(),
            pair[1].to_str().unwrap(),
        ];
        let (stdout, stderr, code) = run(&args, accepting.path(), &server.uri());
        assert_eq!(
            code, 6,
            "order {pair:?}\nstdout: {stdout}\nstderr: {stderr}"
        );
    }
    let both = project(Some(&suppression("")));
    let args = [
        "audit",
        "--no-cache",
        accepting.path().to_str().unwrap(),
        both.path().to_str().unwrap(),
    ];
    let (stdout, _, code) = run(&args, accepting.path(), &server.uri());
    assert_eq!(code, 0, "{stdout}");
}

#[tokio::test]
async fn limit_bounds_the_suppressed_list_too() {
    let server = osv(&[(ADVISORY, None), ("GHSA-second-001", None)]).await;
    let tmp = project(Some(
        "[audit]\nignore-advisories = [{ id = \"RUSTSEC-2026-0543\", reason = \"a\" }, { id = \"GHSA-second-001\", reason = \"b\" }]\n",
    ));
    let (stdout, _, code) = run(
        &["audit", "--no-cache", "--format", "json", "--limit", "1"],
        tmp.path(),
        &server.uri(),
    );
    assert_eq!(code, 0);
    let report = json(&stdout);
    assert_eq!(report["suppressed"].as_array().unwrap().len(), 1);
    assert_eq!(report["suppressed_total"], 2);
    assert_eq!(report["summary"]["suppressed"], 2);
}

#[tokio::test]
async fn an_audit_with_a_coverage_warning_does_not_call_a_suppression_unmatched() {
    // jsr: imports are not audited, so the tree was not fully covered and the
    // absence of a match proves nothing.
    let server = osv(&[]).await;
    let tmp = project(Some(&suppression("")));
    fs::write(
        tmp.path().join("deno.json"),
        r#"{ "imports": { "@std/path": "jsr:@std/path@1.0.0" } }"#,
    )
    .unwrap();
    let (_, stderr, _) = run(&["audit", "--no-cache"], tmp.path(), &server.uri());
    assert!(
        stderr.contains("jsr"),
        "control: coverage warning shown: {stderr}"
    );
    assert!(!stderr.contains("matches no advisory"), "{stderr}");
}

#[tokio::test]
async fn suppressed_rows_page_and_respect_the_field_mask() {
    let server = osv(&[(ADVISORY, None), ("GHSA-second-001", None)]).await;
    let tmp = project(Some(
        "[audit]\nignore-advisories = [{ id = \"RUSTSEC-2026-0543\", reason = \"a\" }, { id = \"GHSA-second-001\", reason = \"b\" }]\n",
    ));
    let first = json(
        &run(
            &["audit", "--no-cache", "--format", "json"],
            tmp.path(),
            &server.uri(),
        )
        .0,
    );
    let ids: Vec<_> = first["suppressed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(ids.len(), 2);

    let (stdout, _, _) = run(
        &["audit", "--no-cache", "--format", "json", "--offset", "1"],
        tmp.path(),
        &server.uri(),
    );
    let page = json(&stdout);
    assert_eq!(page["suppressed"].as_array().unwrap().len(), 1);
    assert_eq!(page["suppressed"][0]["id"], ids[1]);

    let (stdout, _, _) = run(
        &[
            "audit",
            "--no-cache",
            "--format",
            "json",
            "--limit",
            "0",
            "--fields",
            "suppressed",
        ],
        tmp.path(),
        &server.uri(),
    );
    let projected = json(&stdout);
    assert_eq!(projected.as_object().unwrap().len(), 1, "{projected}");
}
