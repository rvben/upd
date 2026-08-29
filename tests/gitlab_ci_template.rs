//! Executable contract tests for the distributed GitLab CI template.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEMPLATE: &str = include_str!("../ci/gitlab-dependency-update.yml");
const RELEASE_PINS: &str = include_str!("../release-pins.json");
const BRANCH: &str = "automation/upd-dependencies";

fn release_version() -> String {
    serde_json::from_str::<serde_json::Value>(RELEASE_PINS).unwrap()["version"]
        .as_str()
        .unwrap()
        .to_string()
}

fn embedded_script() -> String {
    let marker = "  script:\n    - |\n";
    let block = TEMPLATE
        .split_once(marker)
        .expect("template has one literal script block")
        .1;
    block
        .lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                line.strip_prefix("      ")
                    .expect("script line keeps YAML indentation")
                    .to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn run(command: &mut Command) -> Output {
    let output = command.output().expect("command starts");
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    run(Command::new("git").current_dir(cwd).args(args))
}

struct Fixture {
    _temp: TempDir,
    checkout: PathBuf,
    remote: PathBuf,
    updater: PathBuf,
    server_url: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let checkout = temp.path().join("checkout");
        let remote = temp.path().join("remote.git");
        let updater = temp.path().join("fake-upd");
        fs::create_dir(&checkout).expect("checkout directory");

        git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        git(&checkout, &["init"]);
        git(&checkout, &["config", "user.name", "Test User"]);
        git(&checkout, &["config", "user.email", "test@example.com"]);
        fs::write(checkout.join("dependency.txt"), "old\n").expect("fixture manifest");
        git(&checkout, &["add", "dependency.txt"]);
        git(&checkout, &["commit", "-m", "test: initial"]);
        git(&checkout, &["branch", "-M", "main"]);
        git(
            &checkout,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&checkout, &["push", "origin", "main"]);

        fs::write(
            &updater,
            r#"#!/usr/bin/env bash
set -euo pipefail
if [ "${FAKE_UPD_CHANGE}" = "true" ]; then
  printf '%s\n' "${FAKE_UPD_CONTENT}" > dependency.txt
fi
if [ -s "${FAKE_UPD_REPORT_FILE:-}" ]; then
  cat "$FAKE_UPD_REPORT_FILE"
elif [ "${FAKE_UPD_CHANGE}" = "true" ]; then
  cat <<JSON
{"command":"update","mode":"applied","files":[{"path":"dependency.txt","file_type":"test","lang":"test","updates":[{"package":"example","current":"1.0.0","latest":"1.1.0","bump":"minor"}],"pinned":[],"ignored":[],"errors":[],"warnings":[]}],"summary":{"files_scanned":1,"files_with_changes":1,"updates_total":1,"updates_major":0,"updates_minor":1,"updates_patch":0,"pinned":0,"ignored":0,"errors":0,"warnings":0}}
JSON
else
  cat <<JSON
{"command":"update","mode":"applied","files":[],"summary":{"files_scanned":1,"files_with_changes":0,"updates_total":0,"updates_major":0,"updates_minor":0,"updates_patch":0,"pinned":0,"ignored":0,"errors":0,"warnings":0}}
JSON
fi
"#,
        )
        .expect("fake updater");
        let mut permissions = fs::metadata(&updater).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&updater, permissions).unwrap();

        Self {
            server_url: format!("file://{}", temp.path().display()),
            _temp: temp,
            checkout,
            remote,
            updater,
        }
    }

    fn run_template(&self, server: &MockServer, change: bool, content: &str, auto_merge: bool) {
        self.run_template_with_report(server, change, content, auto_merge, "");
    }

    fn run_template_with_report(
        &self,
        server: &MockServer,
        change: bool,
        content: &str,
        auto_merge: bool,
        report: &str,
    ) {
        let report_file = self._temp.path().join("fake-upd-report.json");
        fs::write(&report_file, report).expect("fixture report");
        let output = Command::new("bash")
            .arg("-c")
            .arg(embedded_script())
            .current_dir(&self.checkout)
            .env("UPD_GITLAB_TOKEN", "test-token")
            .env("CI_API_V4_URL", format!("{}/api/v4", server.uri()))
            .env("CI_DEFAULT_BRANCH", "main")
            .env("CI_PROJECT_DIR", &self.checkout)
            .env("CI_PROJECT_ID", "1")
            .env("CI_PROJECT_PATH", "remote")
            .env("CI_SERVER_URL", &self.server_url)
            .env("UPD_VERSION", release_version())
            .env("UPD_SHA256", "")
            .env("UPD_TARGET", "")
            .env("UPD_PATHS", ".")
            .env("UPD_LANGS", "")
            .env("UPD_PACKAGES", "")
            .env("UPD_MIN_AGE", "7d")
            .env("UPD_MAX_BUMP", "minor")
            .env("UPD_LOCK", "false")
            .env("UPD_PREPARE_COMMAND", "")
            .env("UPD_VALIDATION_COMMAND", "")
            .env("UPD_BRANCH", BRANCH)
            .env("UPD_COMMIT_MESSAGE", "chore(deps): test update")
            .env("UPD_MR_TITLE", "")
            .env("UPD_GIT_NAME", "upd test")
            .env("UPD_GIT_EMAIL", "upd-test@example.com")
            .env("UPD_AUTO_MERGE", auto_merge.to_string())
            .env("UPD_EXECUTABLE", &self.updater)
            .env("FAKE_UPD_CHANGE", change.to_string())
            .env("FAKE_UPD_CONTENT", content)
            .env("FAKE_UPD_REPORT_FILE", report_file)
            .output()
            .expect("template starts");
        assert!(
            output.status.success(),
            "template failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn branch_file(&self) -> Option<String> {
        let output = Command::new("git")
            .arg(format!("--git-dir={}", self.remote.display()))
            .args(["show", &format!("refs/heads/{BRANCH}:dependency.txt")])
            .output()
            .expect("git show starts");
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap())
    }

    fn branch_commit_count(&self) -> usize {
        let output = run(Command::new("git")
            .arg(format!("--git-dir={}", self.remote.display()))
            .args([
                "rev-list",
                "--count",
                &format!("refs/heads/main..refs/heads/{BRANCH}"),
            ]));
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn presentation(&self) -> serde_json::Value {
        serde_json::from_str(
            &fs::read_to_string(self.checkout.join(".upd-ci/upd-presentation.json")).unwrap(),
        )
        .unwrap()
    }

    fn description(&self) -> String {
        fs::read_to_string(self.checkout.join(".upd-ci/upd-mr-description.md")).unwrap()
    }
}

fn mr_list_response(iid: u64, auto_merge: bool) -> serde_json::Value {
    json!([{
        "iid": iid,
        "web_url": format!("https://gitlab.example.test/project/-/merge_requests/{iid}"),
        "merge_when_pipeline_succeeds": auto_merge,
    }])
}

fn mr_response(iid: u64, auto_merge: bool) -> serde_json::Value {
    json!({
        "iid": iid,
        "web_url": format!("https://gitlab.example.test/project/-/merge_requests/{iid}"),
        "merge_when_pipeline_succeeds": auto_merge,
    })
}

fn list_mock(response: serde_json::Value) -> Mock {
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .and(query_param("state", "opened"))
        .and(query_param("source_branch", BRANCH))
        .and(query_param("target_branch", "main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .expect(1)
}

#[tokio::test]
async fn template_creates_a_single_commit_rolling_merge_request() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;

    fixture.run_template(&server, true, "new", false);

    assert_eq!(fixture.branch_file().as_deref(), Some("new\n"));
    assert_eq!(fixture.branch_commit_count(), 1);
    assert_eq!(fixture.presentation()["schema"], 1);
    assert_eq!(fixture.presentation()["state"], "ready");
    let description = fixture.description();
    assert!(description.contains("**A tidy upgrade, already prepared.**"));
    assert!(description.contains("84109eaf36c739dc11af0452c6218abb7e47a8e3/assets/logo-wide.svg"));
    assert!(description.contains("**1 moved forward** · **1 worth a look**"));
    assert!(description.contains("### Worth a look"));
    assert!(description.contains("### What upd verified"));
    assert!(description.contains("<summary><strong>Proof and provenance</strong></summary>"));
    assert!(description.contains("<code>example</code>"));
    assert!(description.contains("<code>1.0.0</code>"));
    assert!(description.contains("<code>1.1.0</code>"));
    assert!(description.contains("Freshness <code>7d</code>"));
    assert!(!description.contains("[!IMPORTANT]"));
    assert!(description.len() <= 32 * 1024);
}

#[tokio::test]
async fn gitlab_presentation_matches_the_contract_and_escapes_untrusted_text() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    let report = r#"{
      "command":"update","mode":"applied",
      "files":[{
        "path":"dependency.txt","file_type":"test","lang":"test",
        "updates":[{"package":"bad|pkg</code>","current":"1.0.0`","latest":"1.1.0","bump":"minor","status":"applied"}],
        "annotations":[{"package":"annotated","version":"2.0.0"}],
        "held_back":[{"package":"fresh_pkg","current":"1.0.0","chosen":"1.0.1","skipped_latest":"1.1.0"}],
        "capped":[{"package":"major_pkg","current":"1.0.0","available":"2.0.0"}],
        "skipped":[{"package":"blocked<script>","current":"3.0.0","status":"blocked","reason":"missing-version-comment","message":"Add *trusted* metadata | before updating \u202ethis pin"}],
        "errors":[],"warnings":[]
      }],
      "summary":{"files_scanned":1,"files_with_changes":1,"updates_total":1,"errors":0,"warnings":2,"not_examined":1}
    }"#;
    fixture.run_template_with_report(&server, true, "new", false, report);

    let presentation = fixture.presentation();
    assert_eq!(presentation["title"], "chore(deps): refresh dependency");
    assert_eq!(presentation["counts"]["policy_holds"], 2);
    assert_eq!(presentation["counts"]["blocked"], 1);
    assert_eq!(presentation["counts"]["annotations"], 1);
    let description = fixture.description();
    assert!(description.contains("**A careful upgrade, with follow-up.**"));
    assert!(description.contains("Saved for a deliberate upgrade (2)"));
    assert!(description.contains("### Needs attention"));
    assert!(description.contains("bad&#124;pkg&lt;/code&gt;"));
    assert!(description.contains("blocked&lt;script&gt;"));
    assert!(description.contains("&#42;trusted&#42;"));
    assert!(!description.contains("<script>"));
    assert!(!description.contains("*trusted*"));
    assert!(!description.contains('\u{202e}'));
    assert!(!description.contains("[!IMPORTANT]"));
    assert!(description.len() <= 32 * 1024);
}

#[tokio::test]
async fn gitlab_presentation_prioritizes_review_worthy_updates() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    let report = r#"{
      "command":"update","mode":"applied",
      "files":[{
        "path":"dependency.txt","file_type":"test","lang":"test",
        "updates":[
          {"package":"quiet-one","current":"1.0.0","latest":"1.0.1","bump":"patch","status":"applied"},
          {"package":"review-one","current":"1.0.0","latest":"1.1.0","bump":"minor","status":"applied"},
          {"package":"quiet-two","current":"2.0.0","latest":"2.0.1","bump":"patch","status":"applied"},
          {"package":"review-two","current":"3.0.0","latest":"4.0.0","bump":"major","status":"applied"}
        ],
        "capped":[{"package":"later","current":"1.0.0","available":"2.0.0"}],
        "errors":[],"warnings":[]
      }],
      "summary":{"files_scanned":1,"files_with_changes":1,"updates_total":4,"errors":0,"warnings":0}
    }"#;
    fixture.run_template_with_report(&server, true, "new", false, report);

    let presentation = fixture.presentation();
    assert_eq!(presentation["counts"]["updates_review_worthy"], 2);
    assert_eq!(presentation["counts"]["updates_quiet"], 2);
    let description = fixture.description();
    assert!(description.contains(
        "**4 moved forward** · **2 worth a look** · **2 quiet patches** · **1 saved for later**"
    ));
    let worth = description.find("### Worth a look").unwrap();
    let review_one = description.find("<code>review-one</code>").unwrap();
    let quiet = description.find("Quiet patch updates (2)").unwrap();
    let quiet_one = description.find("<code>quiet-one</code>").unwrap();
    assert!(worth < review_one && review_one < quiet && quiet < quiet_one);
    assert!(description.contains("Includes 1 major-version jump."));
}

#[tokio::test]
async fn gitlab_presentation_keeps_unvalidated_patch_updates_truthful() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    let report = r#"{
      "command":"update","mode":"applied",
      "files":[{"path":"dependency.txt","file_type":"test","lang":"test",
        "updates":[{"package":"example","current":"1.0.0","latest":"1.0.1","bump":"patch","status":"applied"}],
        "errors":[],"warnings":[]}],
      "summary":{"files_scanned":1,"files_with_changes":1,"updates_total":1,"errors":0,"warnings":0}
    }"#;
    fixture.run_template_with_report(&server, true, "new", false, report);

    let description = fixture.description();
    assert!(description.contains("### What changed"));
    assert!(!description.contains("### Worth a look"));
    assert!(!description.contains("Quiet patch updates"));
    assert!(description.contains("### What upd verified"));
    assert!(description.contains("No project-specific command was configured"));
    assert!(!description.contains("### Why this is a comfortable review"));
}

#[tokio::test]
async fn gitlab_large_body_fallback_preserves_risk_state() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    let updates = (0..80)
        .map(|index| {
            json!({
                "package": format!("package-{index}-{}", "x".repeat(1200)),
                "current": format!("1.0.0-{}", "c".repeat(1200)),
                "latest": format!("1.1.0-{}", "l".repeat(1200)),
                "bump": if index < 40 { "minor" } else { "patch" }, "status": "applied"
            })
        })
        .collect::<Vec<_>>();
    let held = (0..40)
        .map(|index| {
            json!({
                "package": format!("held-{index}-{}", "y".repeat(1200)),
                "current": format!("1.0.0-{}", "c".repeat(1200)),
                "chosen": format!("1.0.1-{}", "s".repeat(1200)),
                "skipped_latest": format!("1.1.0-{}", "a".repeat(1200))
            })
        })
        .collect::<Vec<_>>();
    let blocked = (0..30)
        .map(|index| {
            json!({
                "package": format!("blocked-{index}"),
                "current": format!("1.0.0-{}", "c".repeat(1200)),
                "status": "blocked", "message": "z".repeat(1200)
            })
        })
        .collect::<Vec<_>>();
    let report = json!({
        "command":"update", "mode":"applied",
        "files":[{"path":format!("dependency-{}.txt", "p".repeat(1200)),"file_type":"test","lang":"test",
          "updates":updates,"held_back":held,"skipped":blocked,"errors":[],"warnings":[]}],
        "summary":{"files_scanned":1,"files_with_changes":1,"updates_total":80,"errors":0,"warnings":0}
    });
    fixture.run_template_with_report(&server, true, "new", false, &report.to_string());

    let description = fixture.description();
    assert!(description.len() <= 32 * 1024);
    assert!(description.contains("**A careful upgrade, with follow-up.**"));
    assert!(description.contains("Saved for a deliberate upgrade: 40"));
    assert!(description.contains("Needs attention: 30"));
    assert!(description.contains("Major-version jumps: 0"));
    assert!(description.contains("no project-specific command was configured"));
    assert!(!description.contains("**A tidy upgrade, already prepared.**"));
}

#[tokio::test]
async fn template_updates_the_rolling_branch_and_enables_sha_bound_automerge() {
    let create_server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&create_server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .mount(&create_server)
        .await;
    fixture.run_template(&create_server, true, "first", false);

    let update_server = MockServer::start().await;
    list_mock(mr_list_response(7, false))
        .mount(&update_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&update_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7/merge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, true)))
        .expect(1)
        .mount(&update_server)
        .await;

    fixture.run_template(&update_server, true, "second", true);

    assert_eq!(fixture.branch_file().as_deref(), Some("second\n"));
    assert_eq!(fixture.branch_commit_count(), 1);
    assert!(TEMPLATE.contains("--form \"sha=${commit_sha}\""));
}

#[tokio::test]
async fn template_closes_an_obsolete_merge_request_and_deletes_its_branch() {
    let create_server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&create_server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .mount(&create_server)
        .await;
    fixture.run_template(&create_server, true, "new", false);

    let cleanup_server = MockServer::start().await;
    list_mock(mr_list_response(7, false))
        .mount(&cleanup_server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&cleanup_server)
        .await;

    fixture.run_template(&cleanup_server, false, "unused", false);

    assert_eq!(fixture.branch_file(), None);
}

#[test]
fn template_defaults_are_reproducible_and_safe() {
    assert!(TEMPLATE.contains("debian:bookworm-slim@sha256:"));
    assert!(TEMPLATE.contains("UPD_VERSION: \"$[[ inputs.upd_version ]]\""));
    assert!(TEMPLATE.contains("--force-with-lease="));
    assert!(!TEMPLATE.contains("JOB-TOKEN:"));
    assert!(!TEMPLATE.contains("UPD_VERSION: \"latest\""));
}
