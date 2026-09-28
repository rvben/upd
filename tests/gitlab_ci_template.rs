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

/// Job inputs for one template run. Defaults mirror the template's own input
/// defaults, except for the fake updater controls.
#[derive(Clone)]
struct Run {
    /// Whether the fake updater writes `content` to `file`.
    change: bool,
    content: String,
    file: String,
    /// Report the fake updater prints; empty selects its built-in reports.
    report: String,
    /// Exit status the fake updater fails with, before writing anything.
    upd_exit: Option<i32>,
    auto_merge: bool,
    lock: String,
    min_age: String,
    max_bump: String,
    prepare_command: String,
    validation_command: String,
    mr_title: String,
    branch: String,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            change: true,
            content: "new".to_string(),
            file: "dependency.txt".to_string(),
            report: String::new(),
            upd_exit: None,
            auto_merge: false,
            lock: "false".to_string(),
            min_age: "7d".to_string(),
            max_bump: "minor".to_string(),
            prepare_command: String::new(),
            validation_command: String::new(),
            mr_title: String::new(),
            branch: BRANCH.to_string(),
        }
    }
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
if [ "${1:-}" = gitlab ]; then
  exec "$REAL_UPD" "$@"
fi
if [ -n "${UPD_GITLAB_TOKEN+set}" ]; then
  echo "fake upd received the GitLab token" >&2
  exit 9
fi
if [ -n "${FAKE_UPD_EXIT:-}" ]; then
  echo "fake upd failure" >&2
  exit "$FAKE_UPD_EXIT"
fi
if [ "${FAKE_UPD_CHANGE}" = "true" ]; then
  target="${FAKE_UPD_FILE:-dependency.txt}"
  mkdir -p "$(dirname "$target")"
  printf '%s\n' "${FAKE_UPD_CONTENT}" > "$target"
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
        self.run(
            server,
            &Run {
                change,
                content: content.to_string(),
                auto_merge,
                report: report.to_string(),
                ..Run::default()
            },
        );
    }

    /// Runs the job and requires it to succeed.
    fn run(&self, server: &MockServer, run: &Run) -> Output {
        let output = self.execute(server, run);
        assert!(
            output.status.success(),
            "template failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// Runs the job with the given inputs and returns its outcome unjudged.
    fn execute(&self, server: &MockServer, run: &Run) -> Output {
        let mut command = Command::new("bash");
        command.arg("-c").arg(embedded_script());
        self.job(&mut command, server, run);
        command.output().expect("template starts")
    }

    /// Runs `upd gitlab run` directly, with the job's environment.
    fn execute_upd(&self, server: &MockServer, run: &Run, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_upd"));
        command.args(["gitlab", "run"]).args(args);
        self.job(&mut command, server, run);
        command.output().expect("upd starts")
    }

    /// Gives `command` the CI job environment for `run`.
    fn job(&self, command: &mut Command, server: &MockServer, run: &Run) {
        let report_file = self._temp.path().join("fake-upd-report.json");
        fs::write(&report_file, &run.report).expect("fixture report");
        command
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
            .env("UPD_MIN_AGE", &run.min_age)
            .env("UPD_MAX_BUMP", &run.max_bump)
            .env("UPD_LOCK", &run.lock)
            .env("UPD_PREPARE_COMMAND", &run.prepare_command)
            .env("UPD_VALIDATION_COMMAND", &run.validation_command)
            .env("UPD_BRANCH", &run.branch)
            .env("UPD_COMMIT_MESSAGE", "chore(deps): test update")
            .env("UPD_MR_TITLE", &run.mr_title)
            .env("UPD_GIT_NAME", "upd test")
            .env("UPD_GIT_EMAIL", "upd-test@example.com")
            .env("UPD_AUTO_MERGE", run.auto_merge.to_string())
            .env("UPD_EXECUTABLE", &self.updater)
            .env("REAL_UPD", env!("CARGO_BIN_EXE_upd"))
            .env("FAKE_UPD_CHANGE", run.change.to_string())
            .env("FAKE_UPD_CONTENT", &run.content)
            .env("FAKE_UPD_FILE", &run.file)
            .env("FAKE_UPD_REPORT_FILE", report_file)
            .env("FIXTURE_REMOTE", &self.remote);
        if let Some(code) = run.upd_exit {
            command.env("FAKE_UPD_EXIT", code.to_string());
        }
    }

    fn remote_tip(&self) -> Option<String> {
        let output = Command::new("git")
            .arg(format!("--git-dir={}", self.remote.display()))
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{BRANCH}"))
            .output()
            .expect("git rev-parse starts");
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_string())
    }

    fn remote_author(&self) -> String {
        let output = run(Command::new("git")
            .arg(format!("--git-dir={}", self.remote.display()))
            .args(["show", "-s", "--format=%an <%ae>|%cn <%ce>|%s"])
            .arg(format!("refs/heads/{BRANCH}")));
        String::from_utf8(output.stdout).unwrap().trim().to_string()
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
    assert!(description.contains("**1 needs attention**"));
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
async fn gitlab_presentation_reports_normalized_specifiers_as_changes() {
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
        "path":"pyproject.toml","file_type":"pyproject","lang":"python",
        "normalized":[
          {"line":3,"package":"click","previous_spec":null,"new_spec":">=8.5.0","version":"8.5.0","pinned":false},
          {"line":4,"package":"urllib3","previous_spec":"<= 2.0.0","new_spec":">=2.7.0","version":"2.7.0","pinned":false,"skipped_latest":"2.8.0"}
        ],
        "errors":[],"warnings":[]
      }],
      "summary":{"files_scanned":1,"files_with_changes":1,"updates_total":0,"normalized":2,"errors":0,"warnings":0}
    }"#;
    fixture.run_template_with_report(&server, true, "new", false, report);

    let presentation = fixture.presentation();
    assert_eq!(
        presentation["title"],
        "chore(deps): normalize 2 dependency specifiers"
    );
    assert_eq!(presentation["counts"]["normalized"], 2);
    assert_eq!(presentation["counts"]["updates"], 0);
    assert_eq!(presentation["counts"]["policy_holds"], 1);
    assert_eq!(presentation["normalized"][0]["previous"], "(no specifier)");

    let description = fixture.description();
    assert!(description.contains("upd prepared 2 normalized specifiers"));
    assert!(description.contains("**2 normalized**"));
    assert!(!description.contains("**0 moved forward**"));
    assert!(description.contains("### Normalized specifiers"));
    assert!(description.contains("<code>&lt;= 2.0.0</code>"));
    assert!(description.contains("<code>&gt;=2.7.0</code>"));
    assert!(description.contains("Saved for a deliberate upgrade (1)"));
    assert!(!description.contains("without changing selected versions"));
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

#[tokio::test]
async fn template_preserves_human_commits_even_when_no_updates_remain() {
    let create_server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&create_server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .mount(&create_server)
        .await;
    fixture.run_template(&create_server, true, "first", false);

    git(
        &fixture.checkout,
        &["config", "user.name", "Human Maintainer"],
    );
    git(
        &fixture.checkout,
        &["config", "user.email", "human@example.com"],
    );
    fs::write(fixture.checkout.join("human-fix.txt"), "keep me\n").unwrap();
    git(&fixture.checkout, &["add", "human-fix.txt"]);
    git(&fixture.checkout, &["commit", "-m", "fix: adapt to update"]);
    git(&fixture.checkout, &["push", "origin", BRANCH]);
    let human_tip = String::from_utf8(git(&fixture.checkout, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();

    let paused_server = MockServer::start().await;
    let mut existing = mr_list_response(7, false);
    existing[0]["description"] = json!("Existing review evidence");
    list_mock(existing).mount(&paused_server).await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&paused_server)
        .await;

    fixture.run_template(&paused_server, true, "second", false);

    let cleanup_server = MockServer::start().await;
    let mut paused_mr = mr_list_response(7, false);
    paused_mr[0]["description"] = json!(
        "Existing review evidence\n\n<!-- upd-human-commit-pause -->\n> **Automation paused**"
    );
    list_mock(paused_mr).mount(&cleanup_server).await;
    fixture.run_template(&cleanup_server, false, "unused", false);

    let remote_tip = String::from_utf8(
        run(Command::new("git")
            .arg(format!("--git-dir={}", fixture.remote.display()))
            .args(["rev-parse", &format!("refs/heads/{BRANCH}")]))
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(remote_tip, human_tip);
    assert_eq!(fixture.branch_file().as_deref(), Some("first\n"));
    assert_eq!(fixture.branch_commit_count(), 2);

    let requests = paused_server.received_requests().await.unwrap();
    assert!(requests.iter().any(|request| {
        String::from_utf8_lossy(&request.body).contains("upd-human-commit-pause")
    }));
}

#[tokio::test]
async fn template_does_not_adopt_a_single_human_commit_as_its_own() {
    let fixture = Fixture::new();
    git(&fixture.checkout, &["switch", "-c", BRANCH]);
    fs::write(fixture.checkout.join("dependency.txt"), "human change\n").unwrap();
    git(&fixture.checkout, &["add", "dependency.txt"]);
    git(
        &fixture.checkout,
        &["commit", "-m", "chore(deps): test update"],
    );
    git(&fixture.checkout, &["push", "origin", BRANCH]);
    let human_tip = String::from_utf8(git(&fixture.checkout, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();

    let server = MockServer::start().await;
    let mut existing = mr_list_response(7, false);
    existing[0]["description"] = json!("Human review work");
    list_mock(existing).mount(&server).await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    fixture.run_template(&server, true, "bot change", false);

    let remote_tip = String::from_utf8(
        run(Command::new("git")
            .arg(format!("--git-dir={}", fixture.remote.display()))
            .args(["rev-parse", &format!("refs/heads/{BRANCH}")]))
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(remote_tip, human_tip);
    assert_eq!(fixture.branch_file().as_deref(), Some("human change\n"));
}

#[test]
fn template_defaults_are_reproducible_and_safe() {
    assert!(TEMPLATE.contains("debian:bookworm-slim@sha256:"));
    assert!(TEMPLATE.contains("UPD_VERSION: \"$[[ inputs.upd_version ]]\""));
    assert!(TEMPLATE.contains("\"$upd_bin\" gitlab run"));
    assert!(!TEMPLATE.contains("JOB-TOKEN:"));
    assert!(!TEMPLATE.contains("UPD_VERSION: \"latest\""));
}

// Golden rendering. Each case pins the exact presentation model and merge
// request description the template produces for a fixed update report, so any
// reimplementation of the rendering can be held to byte-identical output.
// Regenerate deliberately with `UPD_BLESS=1` and review the diff.

const GOLDEN_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/gitlab-template"
);

fn assert_golden(case: &str, name: &str, actual: &str) {
    let path = Path::new(GOLDEN_DIR).join(case).join(name);
    if std::env::var_os("UPD_BLESS").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "missing golden file {}: {error}; generate it with UPD_BLESS=1",
            path.display()
        )
    });
    assert_eq!(
        expected,
        actual,
        "{} differs from the rendered output",
        path.display()
    );
}

fn golden_report(case: &str) -> String {
    fs::read_to_string(Path::new(GOLDEN_DIR).join(case).join("report.json"))
        .expect("golden case report")
}

/// The JSON body of a request.
fn json_body(request: &wiremock::Request) -> serde_json::Value {
    serde_json::from_slice(&request.body).unwrap_or_else(|error| {
        panic!(
            "request body is not JSON ({error}): {}",
            String::from_utf8_lossy(&request.body)
        )
    })
}

async fn assert_rendering_matches_golden(case: &str, run: Run) -> String {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    if run.auto_merge {
        Mock::given(method("PUT"))
            .and(path("/api/v4/projects/1/merge_requests/7/merge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, true)))
            .expect(1)
            .mount(&server)
            .await;
    }

    fixture.run(&server, &run);

    let presentation = fixture.presentation();
    assert_golden(
        case,
        "presentation.json",
        &(serde_json::to_string_pretty(&presentation).unwrap() + "\n"),
    );
    let description = fixture.description();
    assert_golden(case, "description.md", &description);

    let requests = server.received_requests().await.unwrap();
    let create = requests
        .iter()
        .find(|request| request.method.as_str() == "POST")
        .expect("merge request created");
    let title = if run.mr_title.is_empty() {
        presentation["title"].as_str().unwrap().to_string()
    } else {
        run.mr_title.clone()
    };
    assert_eq!(
        json_body(create),
        json!({
            "title": title,
            "description": description,
            "source_branch": BRANCH,
            "target_branch": "main",
            "remove_source_branch": true,
        })
    );
    description
}

#[tokio::test]
async fn golden_single_minor_update() {
    assert_rendering_matches_golden("single-minor", Run::default()).await;
}

#[tokio::test]
async fn golden_untrusted_text_with_holds_and_blocked() {
    let report = golden_report("untrusted-holds-blocked");
    assert_rendering_matches_golden(
        "untrusted-holds-blocked",
        Run {
            report,
            ..Run::default()
        },
    )
    .await;
}

#[tokio::test]
async fn golden_normalized_specifiers_only() {
    let report = golden_report("normalized-only");
    assert_rendering_matches_golden(
        "normalized-only",
        Run {
            report,
            ..Run::default()
        },
    )
    .await;
}

#[tokio::test]
async fn golden_updates_and_normalized_specifiers() {
    let report = golden_report("updates-and-normalized");
    assert_rendering_matches_golden(
        "updates-and-normalized",
        Run {
            report,
            ..Run::default()
        },
    )
    .await;
}

#[tokio::test]
async fn golden_review_worthy_and_quiet_updates() {
    let report = golden_report("review-and-quiet");
    assert_rendering_matches_golden(
        "review-and-quiet",
        Run {
            report,
            ..Run::default()
        },
    )
    .await;
}

#[tokio::test]
async fn golden_patch_only_update() {
    let report = golden_report("patch-only");
    assert_rendering_matches_golden(
        "patch-only",
        Run {
            report,
            ..Run::default()
        },
    )
    .await;
}

#[tokio::test]
async fn golden_annotations_only() {
    let report = golden_report("annotations-only");
    assert_rendering_matches_golden(
        "annotations-only",
        Run {
            report,
            file: ".github/workflows/ci.yml".to_string(),
            ..Run::default()
        },
    )
    .await;
}

#[tokio::test]
async fn template_publishes_an_annotation_only_change() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;

    fixture.run(
        &server,
        &Run {
            report: golden_report("annotations-only"),
            file: ".github/workflows/ci.yml".to_string(),
            ..Run::default()
        },
    );

    assert_eq!(
        fixture.presentation()["title"],
        "ci(deps): annotate dependency metadata"
    );
    assert!(
        fixture
            .description()
            .contains("upd prepared 1 dependency metadata annotation across 1 file")
    );
}

#[tokio::test]
async fn golden_workflow_only_change_uses_the_ci_prefix() {
    let report = golden_report("workflow-only");
    assert_rendering_matches_golden(
        "workflow-only",
        Run {
            report,
            file: ".github/workflows/ci.yml".to_string(),
            ..Run::default()
        },
    )
    .await;
}

#[tokio::test]
async fn golden_validated_repository_policy_with_auto_merge() {
    let report = golden_report("validated-auto-merge");
    assert_rendering_matches_golden(
        "validated-auto-merge",
        Run {
            report,
            validation_command: "true".to_string(),
            lock: "true".to_string(),
            min_age: String::new(),
            max_bump: String::new(),
            auto_merge: true,
            ..Run::default()
        },
    )
    .await;
}

#[tokio::test]
async fn golden_truncates_long_tables_within_the_body_budget() {
    let mut updates = Vec::new();
    for index in 0..15 {
        updates.push(
            json!({"package": format!("review-{index:02}"), "current": "1.0.0",
            "latest": "1.1.0", "bump": "minor", "status": "applied"}),
        );
    }
    for index in 0..25 {
        updates.push(
            json!({"package": format!("quiet-{index:02}"), "current": "1.0.0",
            "latest": "1.0.1", "bump": "patch", "status": "applied"}),
        );
    }
    let held = (0..25)
        .map(|index| {
            json!({"package": format!("held-{index:02}"), "current": "1.0.0",
            "available": "2.0.0"})
        })
        .collect::<Vec<_>>();
    let blocked = (0..14)
        .map(|index| {
            json!({"package": format!("blocked-{index:02}"), "current": "1.0.0",
            "status": "blocked", "message": "needs a person"})
        })
        .collect::<Vec<_>>();
    let report = json!({
        "command": "update", "mode": "applied",
        "files": [{"path": "dependency.txt", "file_type": "test", "lang": "test",
            "updates": updates, "capped": held, "skipped": blocked,
            "errors": [], "warnings": []}],
        "summary": {"files_scanned": 1, "files_with_changes": 1, "updates_total": 40,
            "errors": 0, "warnings": 0}
    });
    let description = assert_rendering_matches_golden(
        "truncated-tables",
        Run {
            report: report.to_string(),
            ..Run::default()
        },
    )
    .await;
    assert!(description.contains("_3 more review-worthy updates are preserved"));
    assert!(description.contains("_5 more patch updates are preserved"));
    assert!(description.contains("_5 more policy decisions are preserved"));
    assert!(description.contains("_2 more blocked decisions are preserved"));
}

#[tokio::test]
async fn golden_oversized_description_falls_back_to_a_summary() {
    let long = |prefix: String, fill: &str| format!("{prefix}-{}", fill.repeat(300));
    let updates = (0..40)
        .map(|index| {
            json!({
                "package": long(format!("package-{index:02}"), "x"),
                "current": long("1.0.0".to_string(), "c"),
                "latest": long("1.1.0".to_string(), "l"),
                "bump": if index < 20 { "minor" } else { "patch" }, "status": "applied"
            })
        })
        .collect::<Vec<_>>();
    let held = (0..20)
        .map(|index| {
            json!({
                "package": long(format!("held-{index:02}"), "y"),
                "current": long("1.0.0".to_string(), "c"),
                "chosen": long("1.0.1".to_string(), "s"),
                "skipped_latest": long("1.1.0".to_string(), "a")
            })
        })
        .collect::<Vec<_>>();
    let blocked = (0..12)
        .map(|index| {
            json!({
                "package": format!("blocked-{index:02}"),
                "current": long("1.0.0".to_string(), "c"),
                "status": "blocked", "message": "z".repeat(300)
            })
        })
        .collect::<Vec<_>>();
    let report = json!({
        "command": "update", "mode": "applied",
        "files": [{"path": long("dependency".to_string(), "p"), "file_type": "test",
            "lang": "test", "updates": updates, "held_back": held, "skipped": blocked,
            "errors": [], "warnings": []}],
        "summary": {"files_scanned": 1, "files_with_changes": 1, "updates_total": 40,
            "errors": 0, "warnings": 0}
    });
    let description = assert_rendering_matches_golden(
        "oversized-fallback",
        Run {
            report: report.to_string(),
            ..Run::default()
        },
    )
    .await;
    assert!(description.contains("exceeded the configured body budget"));
    assert!(description.len() <= 32 * 1024);
}

#[tokio::test]
async fn golden_title_override_replaces_only_the_merge_request_title() {
    assert_rendering_matches_golden(
        "title-override",
        Run {
            mr_title: "chore: custom dependency title".to_string(),
            ..Run::default()
        },
    )
    .await;
}

// Lifecycle contract. These pin what the rolling branch and the GitLab API see
// in each situation, independently of how the description is worded.

fn only_reads(requests: &[wiremock::Request]) -> bool {
    requests
        .iter()
        .all(|request| request.method.as_str() == "GET")
}

fn failed_with(output: &Output, code: i32) -> bool {
    output.status.code() == Some(code)
}

fn describe(output: &Output) -> String {
    format!(
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

async fn create_rolling_branch(fixture: &Fixture, content: &str) {
    let server = MockServer::start().await;
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .mount(&server)
        .await;
    fixture.run(
        &server,
        &Run {
            content: content.to_string(),
            ..Run::default()
        },
    );
}

fn push_human_commit(fixture: &Fixture) -> String {
    git(&fixture.checkout, &["fetch", "origin", BRANCH]);
    git(
        &fixture.checkout,
        &["switch", "--force-create", "human-work", "FETCH_HEAD"],
    );
    fs::write(fixture.checkout.join("human-fix.txt"), "keep me\n").unwrap();
    git(&fixture.checkout, &["add", "human-fix.txt"]);
    git(
        &fixture.checkout,
        &[
            "-c",
            "user.name=Human Maintainer",
            "-c",
            "user.email=human@example.com",
            "commit",
            "-m",
            "fix: adapt to update",
        ],
    );
    git(
        &fixture.checkout,
        &["push", "origin", &format!("HEAD:refs/heads/{BRANCH}")],
    );
    let tip = String::from_utf8(git(&fixture.checkout, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    git(&fixture.checkout, &["switch", "main"]);
    tip
}

/// A validation command that pushes a commit to the rolling branch from
/// another clone, standing in for a concurrent writer between fetch and push.
const RACING_PUSH: &str = r#"racer="$(mktemp -d)"
git clone --quiet "$FIXTURE_REMOTE" "$racer"
if git -C "$racer" rev-parse --verify --quiet "refs/remotes/origin/automation/upd-dependencies" >/dev/null; then
  git -C "$racer" switch --quiet automation/upd-dependencies
fi
echo race > "$racer/race.txt"
git -C "$racer" add race.txt
git -C "$racer" -c user.name=Racer -c user.email=racer@example.com commit --quiet -m race
git -C "$racer" push --quiet origin HEAD:refs/heads/automation/upd-dependencies
git -C "$racer" rev-parse HEAD > "$FIXTURE_REMOTE/../racer-tip""#;

#[tokio::test]
async fn template_commits_as_the_automation_identity() {
    let fixture = Fixture::new();
    create_rolling_branch(&fixture, "new").await;

    assert_eq!(
        fixture.remote_author(),
        "upd test <upd-test@example.com>|upd test <upd-test@example.com>|chore(deps): test update"
    );
}

#[tokio::test]
async fn template_does_nothing_when_clean_and_no_branch_exists() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&server).await;

    fixture.run(
        &server,
        &Run {
            change: false,
            ..Run::default()
        },
    );

    assert_eq!(fixture.remote_tip(), None);
    assert!(only_reads(&server.received_requests().await.unwrap()));
}

#[tokio::test]
async fn template_refuses_duplicate_merge_requests_when_publishing() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    let mut duplicates = mr_list_response(7, false);
    duplicates
        .as_array_mut()
        .unwrap()
        .push(mr_list_response(8, false)[0].clone());
    list_mock(duplicates).mount(&server).await;

    let output = fixture.execute(&server, &Run::default());

    assert!(failed_with(&output, 2), "{}", describe(&output));
    assert!(only_reads(&server.received_requests().await.unwrap()));
}

#[tokio::test]
async fn template_refuses_duplicate_merge_requests_when_cleaning_up() {
    let fixture = Fixture::new();
    create_rolling_branch(&fixture, "new").await;
    let tip = fixture.remote_tip();
    let server = MockServer::start().await;
    let mut duplicates = mr_list_response(7, false);
    duplicates
        .as_array_mut()
        .unwrap()
        .push(mr_list_response(8, false)[0].clone());
    list_mock(duplicates).mount(&server).await;

    let output = fixture.execute(
        &server,
        &Run {
            change: false,
            ..Run::default()
        },
    );

    assert!(failed_with(&output, 2), "{}", describe(&output));
    assert_eq!(fixture.remote_tip(), tip);
    assert!(only_reads(&server.received_requests().await.unwrap()));
}

/// Answers the open merge-request lookup after pushing a human commit to the
/// rolling branch, standing in for a maintainer who pushes while a run is
/// deciding what to clean up.
struct PushWhileListing {
    remote: PathBuf,
    racer: PathBuf,
    response: serde_json::Value,
}

impl wiremock::Respond for PushWhileListing {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        git(
            self.remote.parent().unwrap(),
            &[
                "clone",
                "--quiet",
                "--branch",
                BRANCH,
                self.remote.to_str().unwrap(),
                self.racer.to_str().unwrap(),
            ],
        );
        fs::write(self.racer.join("human-fix.txt"), "keep me\n").unwrap();
        git(&self.racer, &["add", "human-fix.txt"]);
        git(
            &self.racer,
            &[
                "-c",
                "user.name=Human Maintainer",
                "-c",
                "user.email=human@example.com",
                "commit",
                "--quiet",
                "-m",
                "fix: adapt to update",
            ],
        );
        git(&self.racer, &["push", "--quiet", "origin", "HEAD"]);
        ResponseTemplate::new(200).set_body_json(self.response.clone())
    }
}

#[tokio::test]
async fn cleanup_leaves_the_merge_request_open_when_a_human_pushes_first() {
    let fixture = Fixture::new();
    create_rolling_branch(&fixture, "new").await;
    let racer = fixture._temp.path().join("racer");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(PushWhileListing {
            remote: fixture.remote.clone(),
            racer: racer.clone(),
            response: mr_list_response(7, false),
        })
        .mount(&server)
        .await;

    let output = fixture.execute(
        &server,
        &Run {
            change: false,
            ..Run::default()
        },
    );

    assert!(failed_with(&output, 5), "{}", describe(&output));
    let human_tip = String::from_utf8(git(&racer, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(fixture.remote_tip(), Some(human_tip));
    assert!(
        only_reads(&server.received_requests().await.unwrap()),
        "the merge request carrying the human commit must stay open"
    );
}

async fn assert_cancels_previous_auto_merge(existing: serde_json::Value) {
    let fixture = Fixture::new();
    create_rolling_branch(&fixture, "first").await;
    let server = MockServer::start().await;
    list_mock(json!([existing.clone()])).mount(&server).await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(existing))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/api/v4/projects/1/merge_requests/7/cancel_merge_when_pipeline_succeeds",
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;

    fixture.run(
        &server,
        &Run {
            content: "second".to_string(),
            ..Run::default()
        },
    );
}

#[tokio::test]
async fn template_cancels_auto_merge_reported_by_the_legacy_field() {
    assert_cancels_previous_auto_merge(mr_response(7, true)).await;
}

#[tokio::test]
async fn template_cancels_auto_merge_reported_by_the_current_field() {
    assert_cancels_previous_auto_merge(json!({
        "iid": 7,
        "web_url": "https://gitlab.example.test/project/-/merge_requests/7",
        "auto_merge_enabled": true,
    }))
    .await;
}

#[tokio::test]
async fn template_binds_auto_merge_to_the_pushed_commit() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7/merge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, true)))
        .expect(1)
        .mount(&server)
        .await;

    fixture.run(
        &server,
        &Run {
            auto_merge: true,
            ..Run::default()
        },
    );

    let tip = fixture.remote_tip().expect("rolling branch pushed");
    let requests = server.received_requests().await.unwrap();
    let merge = requests
        .iter()
        .find(|request| request.url.path().ends_with("/merge"))
        .expect("auto-merge requested");
    assert_eq!(
        json_body(merge),
        json!({"auto_merge": true, "sha": tip, "should_remove_source_branch": true})
    );
}

#[tokio::test]
async fn template_recovers_when_the_push_succeeded_but_merge_request_creation_failed() {
    let fixture = Fixture::new();
    let failing = MockServer::start().await;
    list_mock(json!([])).mount(&failing).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(500).set_body_string("unavailable"))
        .expect(1)
        .mount(&failing)
        .await;

    let output = fixture.execute(&failing, &Run::default());
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(fixture.remote_tip().is_some());

    let retry = MockServer::start().await;
    list_mock(json!([])).mount(&retry).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&retry)
        .await;
    fixture.run(
        &retry,
        &Run {
            content: "retried".to_string(),
            ..Run::default()
        },
    );

    assert_eq!(fixture.branch_file().as_deref(), Some("retried\n"));
    assert_eq!(fixture.branch_commit_count(), 1);
}

async fn assert_lease_rejects_a_racing_push(existing_branch: bool) {
    let fixture = Fixture::new();
    if existing_branch {
        create_rolling_branch(&fixture, "first").await;
    }
    let server = MockServer::start().await;

    let output = fixture.execute(
        &server,
        &Run {
            content: "second".to_string(),
            validation_command: RACING_PUSH.to_string(),
            ..Run::default()
        },
    );

    assert!(failed_with(&output, 5), "{}", describe(&output));
    let racer_tip = fs::read_to_string(fixture._temp.path().join("racer-tip"))
        .expect("racing push ran")
        .trim()
        .to_string();
    assert_eq!(fixture.remote_tip(), Some(racer_tip));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn template_lease_rejects_a_push_racing_an_existing_branch() {
    assert_lease_rejects_a_racing_push(true).await;
}

#[tokio::test]
async fn template_lease_rejects_a_push_racing_branch_creation() {
    assert_lease_rejects_a_racing_push(false).await;
}

#[tokio::test]
async fn template_refuses_to_publish_a_report_with_errors() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    let report = r#"{"command":"update","mode":"applied",
      "files":[{"path":"dependency.txt","file_type":"test","lang":"test",
        "updates":[{"package":"example","current":"1.0.0","latest":"1.1.0","bump":"minor"}],
        "errors":["registry unavailable"],"warnings":[]}],
      "summary":{"files_scanned":1,"files_with_changes":1,"updates_total":1,"errors":1,"warnings":0}}"#;

    let output = fixture.execute(
        &server,
        &Run {
            report: report.to_string(),
            ..Run::default()
        },
    );

    assert!(failed_with(&output, 2), "{}", describe(&output));
    assert_eq!(fixture.remote_tip(), None);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn template_stops_when_upd_fails() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();

    let output = fixture.execute(
        &server,
        &Run {
            upd_exit: Some(3),
            ..Run::default()
        },
    );

    assert!(failed_with(&output, 3), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("fake upd failure"),
        "the updater's diagnostics reach the job log\n{}",
        describe(&output)
    );
    assert_eq!(fixture.remote_tip(), None);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn template_refuses_a_prepare_command_that_changes_repository_files() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();

    let output = fixture.execute(
        &server,
        &Run {
            prepare_command: "echo setup > stray.txt".to_string(),
            ..Run::default()
        },
    );

    assert!(failed_with(&output, 2), "{}", describe(&output));
    assert_eq!(fixture.remote_tip(), None);
    assert!(server.received_requests().await.unwrap().is_empty());
}

async fn assert_refuses_dirty_validation(command: &str) {
    let server = MockServer::start().await;
    let fixture = Fixture::new();

    let output = fixture.execute(
        &server,
        &Run {
            validation_command: command.to_string(),
            ..Run::default()
        },
    );

    assert!(failed_with(&output, 2), "{}", describe(&output));
    assert_eq!(fixture.remote_tip(), None);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn template_refuses_validation_that_modifies_a_tracked_file() {
    assert_refuses_dirty_validation("echo more >> dependency.txt").await;
}

#[tokio::test]
async fn template_refuses_validation_that_creates_an_untracked_file() {
    assert_refuses_dirty_validation("echo build > output.txt").await;
}

#[tokio::test]
async fn template_pause_requires_exactly_one_open_merge_request() {
    let fixture = Fixture::new();
    create_rolling_branch(&fixture, "first").await;
    let human_tip = push_human_commit(&fixture);
    let server = MockServer::start().await;
    list_mock(json!([])).mount(&server).await;

    let output = fixture.execute(&server, &Run::default());

    assert!(failed_with(&output, 2), "{}", describe(&output));
    assert_eq!(fixture.remote_tip(), Some(human_tip));
    assert!(only_reads(&server.received_requests().await.unwrap()));
}

#[tokio::test]
async fn template_pause_refuses_to_overwrite_an_unreadable_description() {
    let fixture = Fixture::new();
    create_rolling_branch(&fixture, "first").await;
    let human_tip = push_human_commit(&fixture);
    let server = MockServer::start().await;
    let mut existing = mr_list_response(7, false);
    existing[0]["description"] = serde_json::Value::Null;
    list_mock(existing).mount(&server).await;

    let output = fixture.execute(&server, &Run::default());

    assert!(failed_with(&output, 2), "{}", describe(&output));
    assert_eq!(fixture.remote_tip(), Some(human_tip));
    assert!(only_reads(&server.received_requests().await.unwrap()));
}

#[tokio::test]
async fn template_pause_notice_is_published_once() {
    let fixture = Fixture::new();
    create_rolling_branch(&fixture, "first").await;
    let human_tip = push_human_commit(&fixture);
    let server = MockServer::start().await;
    let mut existing = mr_list_response(7, false);
    existing[0]["description"] =
        json!("Review notes\n\n<!-- upd-human-commit-pause -->\n> **Automation paused**");
    list_mock(existing).mount(&server).await;

    fixture.run(&server, &Run::default());

    assert_eq!(fixture.remote_tip(), Some(human_tip));
    assert!(only_reads(&server.received_requests().await.unwrap()));
}

#[tokio::test]
async fn template_pause_notice_appends_to_the_existing_description() {
    let fixture = Fixture::new();
    create_rolling_branch(&fixture, "first").await;
    push_human_commit(&fixture);
    let server = MockServer::start().await;
    let mut existing = mr_list_response(7, false);
    existing[0]["description"] = json!("Review notes");
    list_mock(existing).mount(&server).await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;

    fixture.run(&server, &Run::default());

    let requests = server.received_requests().await.unwrap();
    let update = requests
        .iter()
        .find(|request| request.method.as_str() == "PUT")
        .unwrap();
    assert_eq!(
        json_body(update),
        json!({
            "description": "Review notes\n\n\n<!-- upd-human-commit-pause -->\n> **Automation paused:** this branch has commits outside the generated upd commit. Preserve them or remove them before automation resumes.\n"
        })
    );
}

async fn assert_rejects_input(run: Run) {
    let server = MockServer::start().await;
    let fixture = Fixture::new();

    let output = fixture.execute(&server, &run);

    assert!(failed_with(&output, 4), "{}", describe(&output));
    assert_eq!(fixture.remote_tip(), None);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn template_rejects_a_non_boolean_lock_input() {
    assert_rejects_input(Run {
        lock: "yes".to_string(),
        ..Run::default()
    })
    .await;
}

#[tokio::test]
async fn template_rejects_the_default_branch_as_automation_branch() {
    assert_rejects_input(Run {
        branch: "main".to_string(),
        ..Run::default()
    })
    .await;
}

#[tokio::test]
async fn template_rejects_an_invalid_branch_name() {
    assert_rejects_input(Run {
        branch: "automation/bad..name".to_string(),
        ..Run::default()
    })
    .await;
}

// Behavior of `upd gitlab run` itself: its token boundary, its outcome
// report, and the repository state it leaves behind.

const TOKEN_ABSENT: &str =
    r#"if [ -n "${UPD_GITLAB_TOKEN+set}" ]; then echo "token visible" >&2; exit 1; fi"#;

#[tokio::test]
async fn repository_commands_and_the_updater_never_see_the_token() {
    let fixture = Fixture::new();
    let server = MockServer::start().await;
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;

    // The fake updater refuses to run when the token reaches it.
    fixture.run(
        &server,
        &Run {
            prepare_command: TOKEN_ABSENT.to_string(),
            validation_command: TOKEN_ABSENT.to_string(),
            ..Run::default()
        },
    );

    assert_eq!(fixture.branch_file().as_deref(), Some("new\n"));
}

#[tokio::test]
async fn run_reports_its_outcome_as_json() {
    let fixture = Fixture::new();
    let server = MockServer::start().await;
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;

    let output = fixture.execute_upd(&server, &Run::default(), &["--output", "json"]);

    assert!(output.status.success(), "{}", describe(&output));
    let outcome: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        outcome,
        json!({
            "command": "gitlab run",
            "branch": BRANCH,
            "outcome": "published",
            "merge_request": "https://gitlab.example.test/project/-/merge_requests/7",
            "created": true,
            "commit": fixture.remote_tip().unwrap(),
            "auto_merge": "off",
        })
    );
}

#[tokio::test]
async fn a_dry_run_reports_the_proposal_without_publishing_it() {
    let fixture = Fixture::new();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let output = fixture.execute_upd(&server, &Run::default(), &["--dry-run", "--output", "json"]);

    assert!(output.status.success(), "{}", describe(&output));
    let outcome: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["outcome"], "would_publish", "{outcome}");
    assert_eq!(fixture.remote_tip(), None, "a dry run pushed the branch");
    let writes: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() != "GET")
        .map(|request| format!("{} {}", request.method, request.url.path()))
        .collect();
    assert!(writes.is_empty(), "a dry run wrote to GitLab: {writes:?}");
}

#[tokio::test]
async fn a_dry_run_names_the_cleanup_it_would_do_without_doing_it() {
    let create_server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&create_server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .mount(&create_server)
        .await;
    fixture.run_template(&create_server, true, "new", false);
    let tip = fixture
        .remote_tip()
        .expect("the first run pushed the branch");

    let server = MockServer::start().await;
    list_mock(mr_list_response(7, false)).mount(&server).await;
    let output = fixture.execute_upd(
        &server,
        &Run {
            change: false,
            ..Run::default()
        },
        &["--dry-run", "--output", "json"],
    );

    assert!(output.status.success(), "{}", describe(&output));
    let outcome: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["outcome"], "would_close", "{outcome}");
    assert_eq!(
        outcome["merge_request"],
        "https://gitlab.example.test/project/-/merge_requests/7"
    );
    assert_eq!(outcome["delete_branch"], true, "{outcome}");
    assert_eq!(
        fixture.remote_tip(),
        Some(tip),
        "a dry run removed the branch"
    );
    let writes: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() != "GET")
        .map(|request| format!("{} {}", request.method, request.url.path()))
        .collect();
    assert!(writes.is_empty(), "a dry run wrote to GitLab: {writes:?}");
}

#[tokio::test]
async fn run_reports_a_missing_setting_as_a_json_input_error() {
    let fixture = Fixture::new();
    let server = MockServer::start().await;

    let mut command = Command::new(env!("CARGO_BIN_EXE_upd"));
    command.args(["gitlab", "run", "--output", "json"]);
    fixture.job(&mut command, &server, &Run::default());
    let output = command.env_remove("UPD_GITLAB_TOKEN").output().unwrap();

    assert!(failed_with(&output, 4), "{}", describe(&output));
    assert!(output.stdout.is_empty(), "{}", describe(&output));
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "parse_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("UPD_GITLAB_TOKEN"),
        "{error}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_unrelated_commit_with_the_automation_identity_pauses() {
    let fixture = Fixture::new();
    git(&fixture.checkout, &["switch", "--orphan", BRANCH]);
    fs::write(fixture.checkout.join("dependency.txt"), "orphan\n").unwrap();
    git(&fixture.checkout, &["add", "dependency.txt"]);
    run(Command::new("git")
        .current_dir(&fixture.checkout)
        .args(["commit", "-m", "chore(deps): test update"])
        .env("GIT_AUTHOR_NAME", "upd test")
        .env("GIT_AUTHOR_EMAIL", "upd-test@example.com")
        .env("GIT_COMMITTER_NAME", "upd test")
        .env("GIT_COMMITTER_EMAIL", "upd-test@example.com"));
    git(&fixture.checkout, &["push", "origin", BRANCH]);
    git(&fixture.checkout, &["switch", "main"]);
    let orphan_tip = fixture.remote_tip().unwrap();

    let server = MockServer::start().await;
    let mut existing = mr_list_response(7, false);
    existing[0]["description"] = json!("Review notes");
    list_mock(existing).mount(&server).await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;

    let output = fixture.execute_upd(&server, &Run::default(), &["--output", "json"]);

    assert!(output.status.success(), "{}", describe(&output));
    let outcome: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["outcome"], "paused", "{outcome}");
    assert_eq!(outcome["notice_added"], true, "{outcome}");
    assert_eq!(fixture.remote_tip().as_deref(), Some(orphan_tip.as_str()));
}

#[tokio::test]
async fn changed_paths_are_reported_as_written() {
    let fixture = Fixture::new();
    let server = MockServer::start().await;
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;

    fixture.run(
        &server,
        &Run {
            file: "dépendances/paquet.txt".to_string(),
            ..Run::default()
        },
    );

    assert_eq!(
        fixture.presentation()["changed_paths"],
        json!(["dépendances/paquet.txt"])
    );
}

#[tokio::test]
async fn the_artifact_directory_is_excluded_once() {
    let fixture = Fixture::new();
    let exclude = fixture.checkout.join(".git/info/exclude");
    fs::write(&exclude, "# existing rule without a trailing newline").unwrap();
    for _ in 0..2 {
        let server = MockServer::start().await;
        list_mock(json!([])).mount(&server).await;
        fixture.run(
            &server,
            &Run {
                change: false,
                ..Run::default()
            },
        );
    }

    assert_eq!(
        fs::read_to_string(&exclude).unwrap(),
        "# existing rule without a trailing newline\n/.upd-ci/\n"
    );
}
