//! Executable contract tests for the distributed GitLab CI template.

#![cfg(unix)]

mod isolated;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TEMPLATE: &str = include_str!("../ci/gitlab-dependency-update.yml");
const RELEASE_PINS: &str = include_str!("../release-pins.json");
const BRANCH: &str = "automation/upd-dependencies";
const MAJOR_BRANCH: &str = "automation/upd-dependencies-major";

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

/// The commit `branch` points at in the bare `remote`, if it exists.
fn remote_tip(remote: &Path, branch: &str) -> Option<String> {
    let output = isolated::command("git")
        .arg(format!("--git-dir={}", remote.display()))
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .expect("git rev-parse starts");
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).unwrap().trim().to_string())
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    run(isolated::command("git").current_dir(cwd).args(args))
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
    commit_message: String,
    git_name: String,
    git_email: String,
    major_mr: bool,
    major_branch: String,
    major_commit_message: String,
    /// What the fake updater does when the major lane invokes it: the same
    /// controls as above, for that invocation only. An empty `major_report`
    /// selects a report of one major upgrade in `major_file`.
    major_change: bool,
    major_content: String,
    major_file: String,
    major_report: String,
    major_exit: Option<i32>,
    /// An npm registry to run the real updater against, in place of the fake
    /// one; the fake updater's controls above then go unused.
    npm_registry: Option<String>,
    /// OSV server the real updater queries; without one it is pointed at the
    /// npm registry, which knows no advisories.
    osv: Option<String>,
    security_remediation: String,
    packages: String,
    /// What the fake updater does when asked for security fixes: the report
    /// it prints (empty selects a report with no advisories), the exit status
    /// it ends with, and `audit_content` it writes to `audit_file` first
    /// when `audit_file` is set.
    audit_report: String,
    audit_exit: Option<i32>,
    audit_file: String,
    audit_content: String,
    /// What the fake updater prints when asked to audit the updated tree
    /// (empty selects a report with no advisories) and the exit status it
    /// ends with.
    recheck_report: String,
    recheck_exit: Option<i32>,
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
            commit_message: "chore(deps): test update".to_string(),
            git_name: "upd test".to_string(),
            git_email: "upd-test@example.com".to_string(),
            major_mr: false,
            major_branch: MAJOR_BRANCH.to_string(),
            major_commit_message: "chore(deps): test major update".to_string(),
            major_change: true,
            major_content: "major".to_string(),
            major_file: "major.txt".to_string(),
            major_report: String::new(),
            major_exit: None,
            npm_registry: None,
            osv: None,
            security_remediation: "true".to_string(),
            packages: String::new(),
            audit_report: String::new(),
            audit_exit: None,
            audit_file: String::new(),
            audit_content: String::new(),
            recheck_report: String::new(),
            recheck_exit: None,
        }
    }
}

/// A report of one major upgrade in `file`, as the major lane's updater
/// prints it.
fn major_report(file: &str) -> String {
    json!({
        "command": "update",
        "mode": "applied",
        "files": [{
            "path": file,
            "file_type": "test",
            "lang": "test",
            "updates": [{"package": "breaking", "current": "1.4.0", "latest": "2.0.0", "bump": "major"}],
            "pinned": [], "ignored": [], "errors": [], "warnings": [],
        }],
        "summary": {
            "files_scanned": 1, "files_with_changes": 1, "updates_total": 1,
            "updates_major": 1, "updates_minor": 0, "updates_patch": 0,
            "pinned": 0, "ignored": 0, "errors": 0, "warnings": 0,
        },
    })
    .to_string()
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
        let hook = remote.join("hooks/update");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\necho \"$1\" >> '{}'\n",
                temp.path().join("pushes.log").display()
            ),
        )
        .expect("push log hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

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
printf '%s\n' "$*" >> "$FAKE_UPD_ARGV_LOG"
if [ "${1:-}" = audit ] && [ "${2:-}" != --fix-audit ]; then
  if [ -s "${FAKE_RECHECK_REPORT_FILE:-}" ]; then
    cat "$FAKE_RECHECK_REPORT_FILE"
  else
    cat <<JSON
{"command":"audit","errors":[],"status":"complete","summary":{"errors":0,"packages_checked":1,"vulnerabilities":0,"vulnerable_packages":0},"vulnerabilities":[]}
JSON
  fi
  exit "${FAKE_RECHECK_EXIT:-0}"
fi
if [ "${1:-}" = audit ]; then
  if [ -n "${FAKE_AUDIT_FILE:-}" ]; then
    mkdir -p "$(dirname "$FAKE_AUDIT_FILE")"
    printf '%s\n' "$FAKE_AUDIT_CONTENT" > "$FAKE_AUDIT_FILE"
  fi
  if [ -s "${FAKE_AUDIT_REPORT_FILE:-}" ]; then
    cat "$FAKE_AUDIT_REPORT_FILE"
  else
    cat <<JSON
{"command":"audit","errors":[],"fixes":[],"status":"complete","summary":{"errors":0,"packages_checked":1,"vulnerabilities":0,"vulnerable_packages":0},"vulnerabilities":[]}
JSON
  fi
  exit "${FAKE_AUDIT_EXIT:-0}"
fi
case " $* " in
  *" --only-bump major "*)
    FAKE_UPD_EXIT="${FAKE_UPD_MAJOR_EXIT:-}"
    FAKE_UPD_CHANGE="$FAKE_UPD_MAJOR_CHANGE"
    FAKE_UPD_CONTENT="$FAKE_UPD_MAJOR_CONTENT"
    FAKE_UPD_FILE="$FAKE_UPD_MAJOR_FILE"
    FAKE_UPD_REPORT_FILE="$FAKE_UPD_MAJOR_REPORT_FILE"
    ;;
esac
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
        let mut command = isolated::command("bash");
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
        let major_report_file = self._temp.path().join("fake-upd-major-report.json");
        let major = if run.major_report.is_empty() && run.major_change {
            major_report(&run.major_file)
        } else {
            run.major_report.clone()
        };
        fs::write(&major_report_file, major).expect("fixture major report");
        let audit_report_file = self._temp.path().join("fake-upd-audit-report.json");
        fs::write(&audit_report_file, &run.audit_report).expect("fixture audit report");
        let recheck_report_file = self._temp.path().join("fake-upd-recheck-report.json");
        fs::write(&recheck_report_file, &run.recheck_report).expect("fixture recheck report");
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
            .env("UPD_PACKAGES", &run.packages)
            .env("UPD_MIN_AGE", &run.min_age)
            .env("UPD_MAX_BUMP", &run.max_bump)
            .env("UPD_LOCK", &run.lock)
            .env("UPD_PREPARE_COMMAND", &run.prepare_command)
            .env("UPD_VALIDATION_COMMAND", &run.validation_command)
            .env("UPD_BRANCH", &run.branch)
            .env("UPD_COMMIT_MESSAGE", &run.commit_message)
            .env("UPD_MR_TITLE", &run.mr_title)
            .env("UPD_GIT_NAME", &run.git_name)
            .env("UPD_GIT_EMAIL", &run.git_email)
            .env("UPD_AUTO_MERGE", run.auto_merge.to_string())
            .env("UPD_EXECUTABLE", &self.updater)
            .env("REAL_UPD", env!("CARGO_BIN_EXE_upd"))
            .env("FAKE_UPD_CHANGE", run.change.to_string())
            .env("FAKE_UPD_CONTENT", &run.content)
            .env("FAKE_UPD_FILE", &run.file)
            .env("FAKE_UPD_REPORT_FILE", report_file)
            .env("UPD_MAJOR_MR", run.major_mr.to_string())
            .env("UPD_MAJOR_BRANCH", &run.major_branch)
            .env("UPD_MAJOR_COMMIT_MESSAGE", &run.major_commit_message)
            .env("FAKE_UPD_MAJOR_CHANGE", run.major_change.to_string())
            .env("FAKE_UPD_MAJOR_CONTENT", &run.major_content)
            .env("FAKE_UPD_MAJOR_FILE", &run.major_file)
            .env("FAKE_UPD_MAJOR_REPORT_FILE", major_report_file)
            .env("FAKE_UPD_ARGV_LOG", self.argv_log())
            .env("UPD_SECURITY_REMEDIATION", &run.security_remediation)
            .env("FAKE_AUDIT_REPORT_FILE", audit_report_file)
            .env("FAKE_AUDIT_CONTENT", &run.audit_content)
            .env("FAKE_RECHECK_REPORT_FILE", recheck_report_file)
            .env("FIXTURE_REMOTE", &self.remote);
        if let Some(code) = run.upd_exit {
            command.env("FAKE_UPD_EXIT", code.to_string());
        }
        if let Some(code) = run.major_exit {
            command.env("FAKE_UPD_MAJOR_EXIT", code.to_string());
        }
        if let Some(code) = run.audit_exit {
            command.env("FAKE_AUDIT_EXIT", code.to_string());
        }
        if let Some(code) = run.recheck_exit {
            command.env("FAKE_RECHECK_EXIT", code.to_string());
        }
        if !run.audit_file.is_empty() {
            command.env("FAKE_AUDIT_FILE", &run.audit_file);
        }
        if let Some(registry) = &run.npm_registry {
            command
                .env("UPD_EXECUTABLE", env!("CARGO_BIN_EXE_upd"))
                .env("NPM_REGISTRY", registry)
                .env("OSV_API_URL", run.osv.as_deref().unwrap_or(registry))
                .env("UPD_CACHE_DIR", self._temp.path().join("cache"));
        }
    }

    fn argv_log(&self) -> PathBuf {
        self._temp.path().join("fake-upd-argv.log")
    }

    /// The arguments of every updater invocation so far, one per line.
    fn updater_invocations(&self) -> Vec<String> {
        fs::read_to_string(self.argv_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The tip of `branch` on the remote, if it exists.
    fn tip_of(&self, branch: &str) -> Option<String> {
        remote_tip(&self.remote, branch)
    }

    /// `file` as `branch` on the remote has it, if it does.
    fn file_on(&self, branch: &str, file: &str) -> Option<String> {
        let output = isolated::command("git")
            .arg(format!("--git-dir={}", self.remote.display()))
            .args(["show", &format!("refs/heads/{branch}:{file}")])
            .output()
            .expect("git show starts");
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap())
    }

    /// A pipeline artifact the last run wrote.
    fn artifact(&self, name: &str) -> String {
        fs::read_to_string(self.checkout.join(".upd-ci").join(name))
            .unwrap_or_else(|error| panic!("artifact {name}: {error}"))
    }

    fn remote_tip(&self) -> Option<String> {
        self.tip_of(BRANCH)
    }

    /// How many pushes updated the rolling branch.
    fn branch_pushes(&self) -> usize {
        fs::read_to_string(self._temp.path().join("pushes.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == format!("refs/heads/{BRANCH}"))
            .count()
    }

    fn remote_author(&self) -> String {
        let output = run(isolated::command("git")
            .arg(format!("--git-dir={}", self.remote.display()))
            .args(["show", "-s", "--format=%an <%ae>|%cn <%ce>|%s"])
            .arg(format!("refs/heads/{BRANCH}")));
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn branch_file(&self) -> Option<String> {
        self.file_on(BRANCH, "dependency.txt")
    }

    fn branch_commit_count(&self) -> usize {
        let output = run(isolated::command("git")
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

/// Answers a merge request read as GitLab does once it has processed a push:
/// merge request 7 heads the rolling branch and 8 the major branch, each at
/// the commit the remote holds, with mergeability checked.
struct MergeRequestHeads(PathBuf);

impl Respond for MergeRequestHeads {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let iid: u64 = request
            .url
            .path()
            .rsplit('/')
            .next()
            .and_then(|segment| segment.parse().ok())
            .expect("merge request path ends in its iid");
        let branch = match iid {
            7 => BRANCH,
            8 => MAJOR_BRANCH,
            other => panic!("no merge request {other} in this fixture"),
        };
        let mut body = mr_response(iid, false);
        body["sha"] = json!(remote_tip(&self.0, branch));
        body["detailed_merge_status"] = json!("mergeable");
        ResponseTemplate::new(200).set_body_json(body)
    }
}

async fn serve_merge_request_heads(server: &MockServer, fixture: &Fixture) {
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v4/projects/1/merge_requests/\d+$"))
        .respond_with(MergeRequestHeads(fixture.remote.clone()))
        .mount(server)
        .await;
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
    list_mock_for(BRANCH, response)
}

/// Answers the one lookup of `branch`'s open merge requests.
fn list_mock_for(branch: &str, response: serde_json::Value) -> Mock {
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .and(query_param("state", "opened"))
        .and(query_param("source_branch", branch))
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
    serve_merge_request_heads(&update_server, &fixture).await;
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
        run(isolated::command("git")
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
        run(isolated::command("git")
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
    assert_lane_matches_golden(case, run, false).await
}

/// Pins the presentation and description of one lane: the major lane's when
/// `major`, the ordinary lane's otherwise.
async fn assert_lane_matches_golden(case: &str, run: Run, major: bool) -> String {
    let (branch, artifact_prefix) = if major {
        (MAJOR_BRANCH, "upd-major-")
    } else {
        (BRANCH, "upd-")
    };
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .and(body_partial_json(json!({"source_branch": BRANCH})))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    if run.auto_merge {
        serve_merge_request_heads(&server, &fixture).await;
        Mock::given(method("PUT"))
            .and(path("/api/v4/projects/1/merge_requests/7/merge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, true)))
            .expect(1)
            .mount(&server)
            .await;
    }
    if run.major_mr {
        list_mock_for(MAJOR_BRANCH, json!([])).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v4/projects/1/merge_requests"))
            .and(body_partial_json(json!({"source_branch": MAJOR_BRANCH})))
            .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(8, false)))
            .expect(u64::from(run.major_change))
            .mount(&server)
            .await;
    }

    fixture.run(&server, &run);

    let presentation: serde_json::Value =
        serde_json::from_str(&fixture.artifact(&format!("{artifact_prefix}presentation.json")))
            .unwrap();
    // The fixture's server lives in a fresh temporary directory.
    let stable = |text: &str| text.replace(&fixture.server_url, "<server>");
    assert_golden(
        case,
        "presentation.json",
        &stable(&(serde_json::to_string_pretty(&presentation).unwrap() + "\n")),
    );
    let description = fixture.artifact(&format!("{artifact_prefix}mr-description.md"));
    // The commit id changes with every run's timestamps.
    let tip = fixture.tip_of(branch).expect("rolling branch pushed");
    assert_golden(
        case,
        "description.md",
        &stable(&description.replace(&tip, "<tip>")),
    );

    let requests = server.received_requests().await.unwrap();
    let create = requests
        .iter()
        .find(|request| {
            request.method.as_str() == "POST" && json_body(request)["source_branch"] == branch
        })
        .expect("merge request created");
    // The title override belongs to the ordinary lane alone.
    let title = if run.mr_title.is_empty() || major {
        presentation["title"].as_str().unwrap().to_string()
    } else {
        run.mr_title.clone()
    };
    assert_eq!(
        json_body(create),
        json!({
            "title": title,
            "description": description,
            "source_branch": branch,
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
    serve_merge_request_heads(&server, &fixture).await;
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

async fn assert_lease_rejects_a_racing_push(existing_branch: bool, content: &str) {
    let fixture = Fixture::new();
    if existing_branch {
        create_rolling_branch(&fixture, "first").await;
    }
    let server = MockServer::start().await;

    let output = fixture.execute(
        &server,
        &Run {
            content: content.to_string(),
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
    assert_lease_rejects_a_racing_push(true, "second").await;
}

#[tokio::test]
async fn template_lease_catches_a_push_racing_an_unchanged_branch() {
    assert_lease_rejects_a_racing_push(true, "first").await;
}

#[tokio::test]
async fn template_lease_rejects_a_push_racing_branch_creation() {
    assert_lease_rejects_a_racing_push(false, "second").await;
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
            "pushed": true,
            "commit": fixture.remote_tip().unwrap(),
            "auto_merge": "off",
            "security": {
                "fixes": 0,
                "pending_relock": 0,
                "blocked": 0,
                "skipped": 0,
                "not_applied": 0,
                "unfixable": 0,
                "advisories": 0,
            },
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
    run(isolated::command("git")
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

/// The rolling branch and merge request as a first run publishes them.
struct Published {
    tip: String,
    title: String,
    description: String,
}

/// Publishes `run` to a fresh branch and merge request 7.
async fn publish(fixture: &Fixture, run: &Run) -> Published {
    let server = MockServer::start().await;
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    serve_merge_request_heads(&server, fixture).await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7/merge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, true)))
        .mount(&server)
        .await;
    fixture.run(&server, run);
    let requests = server.received_requests().await.unwrap();
    let create = requests
        .iter()
        .find(|request| request.method.as_str() == "POST")
        .expect("merge request created");
    let body = json_body(create);
    Published {
        tip: fixture.remote_tip().expect("rolling branch pushed"),
        title: body["title"].as_str().unwrap().to_string(),
        description: body["description"].as_str().unwrap().to_string(),
    }
}

/// The open merge request 7 as GitLab lists it; auto-merge, when armed,
/// removes the source branch.
fn listed(title: &str, description: &str, auto_merge: bool) -> serde_json::Value {
    json!([{
        "iid": 7,
        "web_url": "https://gitlab.example.test/project/-/merge_requests/7",
        "title": title,
        "description": description,
        "merge_when_pipeline_succeeds": auto_merge,
        "should_remove_source_branch": auto_merge,
    }])
}

fn writes(requests: &[wiremock::Request]) -> Vec<String> {
    requests
        .iter()
        .filter(|request| request.method.as_str() != "GET")
        .map(|request| format!("{} {}", request.method, request.url.path()))
        .collect()
}

fn outcome_of(output: &Output) -> serde_json::Value {
    assert!(output.status.success(), "{}", describe(output));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn rerunning_an_unchanged_update_leaves_branch_and_merge_request_alone() {
    let fixture = Fixture::new();
    let first = publish(&fixture, &Run::default()).await;
    // A commit rebuilt in a later second gets a new id; waiting makes a
    // rewritten branch visible even to a run that pushes an identical tree.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let server = MockServer::start().await;
    list_mock(listed(&first.title, &first.description, false))
        .mount(&server)
        .await;
    let output = fixture.execute_upd(&server, &Run::default(), &["--output", "json"]);

    assert_eq!(
        outcome_of(&output),
        json!({
            "command": "gitlab run",
            "branch": BRANCH,
            "outcome": "published",
            "merge_request": "https://gitlab.example.test/project/-/merge_requests/7",
            "created": false,
            "pushed": false,
            "commit": first.tip,
            "auto_merge": "off",
            "security": {
                "fixes": 0,
                "pending_relock": 0,
                "blocked": 0,
                "skipped": 0,
                "not_applied": 0,
                "unfixable": 0,
                "advisories": 0,
            },
        })
    );
    assert_eq!(fixture.remote_tip().as_deref(), Some(first.tip.as_str()));
    assert_eq!(
        fixture.branch_pushes(),
        1,
        "the unchanged branch was pushed again"
    );
    let writes = writes(&server.received_requests().await.unwrap());
    assert!(
        writes.is_empty(),
        "an unchanged rerun wrote to GitLab: {writes:?}"
    );
}

#[tokio::test]
async fn an_unchanged_update_still_refreshes_a_stale_description() {
    let fixture = Fixture::new();
    let first = publish(&fixture, &Run::default()).await;

    let server = MockServer::start().await;
    list_mock(listed(&first.title, "edited by hand", false))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    let output = fixture.execute_upd(&server, &Run::default(), &["--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["pushed"], false, "{outcome}");
    assert_eq!(outcome["commit"], first.tip.as_str(), "{outcome}");
    assert_eq!(fixture.branch_pushes(), 1);
    let requests = server.received_requests().await.unwrap();
    let edit = requests
        .iter()
        .find(|request| request.method.as_str() == "PUT")
        .unwrap();
    assert_eq!(
        json_body(edit),
        json!({"title": first.title, "description": first.description})
    );
}

#[tokio::test]
async fn an_unchanged_branch_without_a_merge_request_gets_one_without_a_push() {
    let fixture = Fixture::new();
    let first = publish(&fixture, &Run::default()).await;

    let server = MockServer::start().await;
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    let output = fixture.execute_upd(&server, &Run::default(), &["--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["created"], true, "{outcome}");
    assert_eq!(outcome["pushed"], false, "{outcome}");
    assert_eq!(outcome["commit"], first.tip.as_str(), "{outcome}");
    assert_eq!(fixture.branch_pushes(), 1);
}

#[tokio::test]
async fn an_unchanged_update_keeps_an_armed_auto_merge() {
    let fixture = Fixture::new();
    let run = Run {
        auto_merge: true,
        ..Run::default()
    };
    let first = publish(&fixture, &run).await;

    let server = MockServer::start().await;
    list_mock(listed(&first.title, &first.description, true))
        .mount(&server)
        .await;
    let output = fixture.execute_upd(&server, &run, &["--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["auto_merge"], "enabled", "{outcome}");
    assert_eq!(outcome["pushed"], false, "{outcome}");
    let writes = writes(&server.received_requests().await.unwrap());
    assert!(writes.is_empty(), "auto-merge was re-armed: {writes:?}");
}

#[tokio::test]
async fn an_unchanged_update_rearms_auto_merge_that_keeps_the_source_branch() {
    let fixture = Fixture::new();
    let run = Run {
        auto_merge: true,
        ..Run::default()
    };
    let first = publish(&fixture, &run).await;

    // Someone re-armed auto-merge by hand without removing the source branch.
    let mut merge_request = listed(&first.title, &first.description, true);
    merge_request[0]["should_remove_source_branch"] = json!(false);
    let server = MockServer::start().await;
    list_mock(merge_request).mount(&server).await;
    serve_merge_request_heads(&server, &fixture).await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7/merge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, true)))
        .expect(1)
        .mount(&server)
        .await;
    let output = fixture.execute_upd(&server, &run, &["--output", "json"]);

    assert_eq!(outcome_of(&output)["pushed"], false);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        json_body(&requests[requests.len() - 1]),
        json!({"auto_merge": true, "sha": first.tip, "should_remove_source_branch": true})
    );
}

#[tokio::test]
async fn an_unchanged_update_arms_auto_merge_on_the_existing_commit() {
    let fixture = Fixture::new();
    let run = Run {
        auto_merge: true,
        ..Run::default()
    };
    let first = publish(&fixture, &run).await;

    // Auto-merge armed earlier was cancelled since, e.g. by a failed pipeline.

    let server = MockServer::start().await;
    list_mock(listed(&first.title, &first.description, false))
        .mount(&server)
        .await;
    serve_merge_request_heads(&server, &fixture).await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7/merge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, true)))
        .expect(1)
        .mount(&server)
        .await;
    let output = fixture.execute_upd(&server, &run, &["--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["auto_merge"], "enabled", "{outcome}");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        json_body(&requests[requests.len() - 1]),
        json!({"auto_merge": true, "sha": first.tip, "should_remove_source_branch": true})
    );
    assert_eq!(fixture.branch_pushes(), 1);
}

#[tokio::test]
async fn an_advanced_default_branch_is_built_on_even_without_new_updates() {
    let fixture = Fixture::new();
    let first = publish(&fixture, &Run::default()).await;
    // A commit that leaves the tree as it was: only the parent can tell the
    // branch is behind.
    git(&fixture.checkout, &["switch", "main"]);
    git(
        &fixture.checkout,
        &["commit", "--allow-empty", "-m", "chore: tree unchanged"],
    );
    git(&fixture.checkout, &["push", "origin", "main"]);

    let server = MockServer::start().await;
    list_mock(listed(&first.title, &first.description, false))
        .mount(&server)
        .await;
    expect_description_edit(&server).await;
    let output = fixture.execute_upd(&server, &Run::default(), &["--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["pushed"], true, "{outcome}");
    let tip = fixture.remote_tip().unwrap();
    assert_ne!(tip, first.tip);
    assert_eq!(outcome["commit"], tip.as_str(), "{outcome}");
    assert_eq!(fixture.branch_pushes(), 2);
    assert_eq!(fixture.branch_commit_count(), 1);
    assert_records(&server, &tip).await;
}

#[tokio::test]
async fn a_changed_automation_name_rewrites_the_commit() {
    let fixture = Fixture::new();
    let first = publish(&fixture, &Run::default()).await;

    // The name is no part of ownership, so no recorded commit is needed.
    let server = MockServer::start().await;
    list_mock(listed(&first.title, "Review notes", false))
        .mount(&server)
        .await;
    expect_description_edit(&server).await;
    let run = Run {
        git_name: "renamed bot".to_string(),
        ..Run::default()
    };
    let output = fixture.execute_upd(&server, &run, &["--output", "json"]);

    assert_eq!(outcome_of(&output)["pushed"], true);
    assert_eq!(
        fixture.remote_author(),
        "renamed bot <upd-test@example.com>|renamed bot <upd-test@example.com>|chore(deps): test update"
    );
    assert_records(&server, &fixture.remote_tip().unwrap()).await;
}

#[tokio::test]
async fn a_dry_run_says_when_the_branch_is_already_up_to_date() {
    let fixture = Fixture::new();
    let first = publish(&fixture, &Run::default()).await;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(listed(
            &first.title,
            &first.description,
            false,
        )))
        .mount(&server)
        .await;
    let output = fixture.execute_upd(&server, &Run::default(), &["--dry-run", "--output", "json"]);
    let outcome = outcome_of(&output);
    assert_eq!(outcome["outcome"], "would_publish", "{outcome}");
    assert_eq!(outcome["push"], false, "{outcome}");

    // A dry run leaves the update in the working tree, as a job would.
    git(&fixture.checkout, &["reset", "--hard", "--quiet"]);
    let changed = Run {
        content: "newer".to_string(),
        ..Run::default()
    };
    let output = fixture.execute_upd(&server, &changed, &["--dry-run", "--output", "json"]);
    assert_eq!(outcome_of(&output)["push"], true);
    assert_eq!(fixture.branch_pushes(), 1);
}

#[tokio::test]
async fn a_multi_line_commit_message_keeps_the_branch_owned() {
    // Blank-line runs and trailing spaces are what a default `git commit`
    // would rewrite; the message must survive exactly as configured.
    let run = Run {
        commit_message: "chore(deps): test update\n\n\nWritten by upd.  \nSee the merge request."
            .to_string(),
        ..Run::default()
    };
    let fixture = Fixture::new();
    let first = publish(&fixture, &run).await;

    // With no merge request left to consult, only the commit itself can say
    // that automation wrote it.
    let server = MockServer::start().await;
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    let output = fixture.execute_upd(&server, &run, &["--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["outcome"], "published", "{outcome}");
    assert_eq!(outcome["pushed"], false, "{outcome}");
    assert_eq!(outcome["commit"], first.tip.as_str(), "{outcome}");
    let message = run_git_in_remote(&fixture, &["show", "-s", "--format=%B", &first.tip]);
    assert_eq!(message.trim_end(), run.commit_message);
}

fn run_git_in_remote(fixture: &Fixture, args: &[&str]) -> String {
    let output = run(isolated::command("git")
        .arg(format!("--git-dir={}", fixture.remote.display()))
        .args(args));
    String::from_utf8(output.stdout).unwrap()
}

fn commit_marker(commit: &str) -> String {
    format!("<!-- upd-commit: {commit} -->")
}

/// Serves merge request 7 as `listed` for every lookup and accepts edits.
async fn serve_merge_request(server: &MockServer, listed: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(listed))
        .mount(server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, false)))
        .mount(server)
        .await;
}

async fn assert_rewrites_the_recorded_commit(changed: Run, author: &str) {
    let fixture = Fixture::new();
    let first = publish(&fixture, &Run::default()).await;
    assert!(
        first
            .description
            .ends_with(&format!("\n\n{}\n", commit_marker(&first.tip))),
        "the description does not record the commit it proposes:\n{}",
        first.description
    );

    let server = MockServer::start().await;
    serve_merge_request(&server, listed(&first.title, &first.description, false)).await;
    let output = fixture.execute_upd(&server, &changed, &["--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["outcome"], "published", "{outcome}");
    assert_eq!(outcome["pushed"], true, "{outcome}");
    let tip = fixture.remote_tip().unwrap();
    assert_ne!(tip, first.tip);
    assert_eq!(outcome["commit"], tip.as_str(), "{outcome}");
    assert_eq!(fixture.remote_author(), author);
    assert_eq!(fixture.branch_commit_count(), 1);
    assert_records(&server, &tip).await;
}

/// Accepts exactly one edit of merge request 7.
async fn expect_description_edit(server: &MockServer) {
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(server)
        .await;
}

/// Requires the merge request edit `server` received to record `tip`, and
/// no other commit.
async fn assert_records(server: &MockServer, tip: &str) {
    let requests = server.received_requests().await.unwrap();
    let edit = requests
        .iter()
        .find(|request| request.method.as_str() == "PUT")
        .expect("the merge request is refreshed");
    let description = json_body(edit)["description"].as_str().unwrap().to_string();
    assert!(
        description.ends_with(&format!("\n\n{}\n", commit_marker(tip))),
        "{description}"
    );
    assert_eq!(
        description.matches("<!-- upd-commit: ").count(),
        1,
        "{description}"
    );
}

#[tokio::test]
async fn a_changed_commit_message_rewrites_the_commit_its_merge_request_records() {
    assert_rewrites_the_recorded_commit(
        Run {
            commit_message: "build(deps): refresh with upd".to_string(),
            ..Run::default()
        },
        "upd test <upd-test@example.com>|upd test <upd-test@example.com>|build(deps): refresh with upd",
    )
    .await;
}

#[tokio::test]
async fn a_changed_automation_email_rewrites_the_commit_its_merge_request_records() {
    assert_rewrites_the_recorded_commit(
        Run {
            git_email: "bot@example.com".to_string(),
            ..Run::default()
        },
        "upd test <bot@example.com>|upd test <bot@example.com>|chore(deps): test update",
    )
    .await;
}

/// Runs `run` against merge request 7 described as `description` and
/// requires it to pause, leaving the branch at `tip`.
async fn assert_pauses(fixture: &Fixture, run: &Run, description: &str, tip: &str) {
    let server = MockServer::start().await;
    let mut existing = mr_list_response(7, false);
    existing[0]["description"] = json!(description);
    list_mock(existing).mount(&server).await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;

    let output = fixture.execute_upd(&server, run, &["--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["outcome"], "paused", "{outcome}");
    assert_eq!(outcome["notice_added"], true, "{outcome}");
    assert_eq!(fixture.remote_tip().as_deref(), Some(tip));
}

#[tokio::test]
async fn a_changed_message_without_a_recorded_commit_still_pauses() {
    let fixture = Fixture::new();
    let first = publish(&fixture, &Run::default()).await;
    let changed = Run {
        commit_message: "build(deps): refresh with upd".to_string(),
        ..Run::default()
    };

    // A description that records nothing, as releases before the record wrote.
    assert_pauses(&fixture, &changed, "Review notes", &first.tip).await;
}

#[tokio::test]
async fn a_human_commit_on_the_recorded_commit_still_pauses() {
    let fixture = Fixture::new();
    let first = publish(&fixture, &Run::default()).await;
    let human_tip = push_human_commit(&fixture);
    let changed = Run {
        commit_message: "build(deps): refresh with upd".to_string(),
        ..Run::default()
    };

    assert_pauses(&fixture, &changed, &first.description, &human_tip).await;
}

#[tokio::test]
async fn an_older_recorded_commit_does_not_claim_a_replaced_branch() {
    let fixture = Fixture::new();
    let first = publish(&fixture, &Run::default()).await;
    // Someone replaces the branch with their own single commit.
    git(
        &fixture.checkout,
        &["switch", "--force-create", "human-work", "main"],
    );
    fs::write(fixture.checkout.join("dependency.txt"), "by hand\n").unwrap();
    git(&fixture.checkout, &["add", "dependency.txt"]);
    git(
        &fixture.checkout,
        &[
            "-c",
            "user.name=Human Maintainer",
            "-c",
            "user.email=human@example.com",
            "commit",
            "-m",
            "fix: pin by hand",
        ],
    );
    git(
        &fixture.checkout,
        &[
            "push",
            "--force",
            "origin",
            &format!("HEAD:refs/heads/{BRANCH}"),
        ],
    );
    git(&fixture.checkout, &["switch", "main"]);
    let human_tip = fixture.remote_tip().unwrap();

    assert_pauses(&fixture, &Run::default(), &first.description, &human_tip).await;
}

#[tokio::test]
async fn a_recorded_commit_off_the_default_branch_history_still_pauses() {
    let fixture = Fixture::new();
    let first = publish(&fixture, &Run::default()).await;
    // The default branch is rewritten under the rolling branch.
    git(&fixture.checkout, &["switch", "main"]);
    git(
        &fixture.checkout,
        &["commit", "--amend", "-m", "test: initial, rewritten"],
    );
    git(&fixture.checkout, &["push", "--force", "origin", "main"]);
    let changed = Run {
        commit_message: "build(deps): refresh with upd".to_string(),
        ..Run::default()
    };

    assert_pauses(&fixture, &changed, &first.description, &first.tip).await;
}

#[tokio::test]
async fn a_dry_run_reports_a_recorded_commit_as_rewritten() {
    let fixture = Fixture::new();
    let first = publish(&fixture, &Run::default()).await;
    let server = MockServer::start().await;
    serve_merge_request(&server, listed(&first.title, &first.description, false)).await;
    let changed = Run {
        commit_message: "build(deps): refresh with upd".to_string(),
        ..Run::default()
    };

    let output = fixture.execute_upd(&server, &changed, &["--dry-run", "--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["outcome"], "would_publish", "{outcome}");
    assert_eq!(outcome["push"], true, "{outcome}");
    assert_eq!(fixture.remote_tip().as_deref(), Some(first.tip.as_str()));
    let writes = writes(&server.received_requests().await.unwrap());
    assert!(writes.is_empty(), "a dry run wrote to GitLab: {writes:?}");
}

// The major lane: a second rolling merge request, on its own branch, that
// proposes only major-version upgrades and is never merged by upd.

/// Serves both lanes' first run: no open merge requests, the ordinary lane's
/// created as 7 and the major lane's as 8.
async fn serve_both_lanes(server: &MockServer) {
    list_mock(json!([])).mount(server).await;
    list_mock_for(MAJOR_BRANCH, json!([])).mount(server).await;
    for (branch, iid) in [(BRANCH, 7), (MAJOR_BRANCH, 8)] {
        Mock::given(method("POST"))
            .and(path("/api/v4/projects/1/merge_requests"))
            .and(body_partial_json(json!({"source_branch": branch})))
            .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(iid, false)))
            .expect(1)
            .mount(server)
            .await;
    }
}

fn created_on<'a>(requests: &'a [wiremock::Request], branch: &str) -> &'a wiremock::Request {
    requests
        .iter()
        .find(|request| {
            request.method.as_str() == "POST"
                && request.url.path() == "/api/v4/projects/1/merge_requests"
                && json_body(request)["source_branch"] == branch
        })
        .unwrap_or_else(|| panic!("no merge request created for {branch}"))
}

fn major_run() -> Run {
    Run {
        major_mr: true,
        ..Run::default()
    }
}

#[tokio::test]
async fn the_major_lane_proposes_its_own_merge_request_that_upd_never_merges() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_both_lanes(&server).await;
    serve_merge_request_heads(&server, &fixture).await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7/merge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, true)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/8/merge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(8, true)))
        .expect(0)
        .mount(&server)
        .await;

    fixture.run(
        &server,
        &Run {
            auto_merge: true,
            mr_title: "Ordinary title override".to_string(),
            ..major_run()
        },
    );

    // Each lane's branch starts from the default branch and carries only its
    // own lane's change.
    assert_eq!(
        fixture.file_on(BRANCH, "dependency.txt").as_deref(),
        Some("new\n")
    );
    assert_eq!(fixture.file_on(BRANCH, "major.txt"), None);
    assert_eq!(
        fixture.file_on(MAJOR_BRANCH, "major.txt").as_deref(),
        Some("major\n")
    );
    assert_eq!(
        fixture.file_on(MAJOR_BRANCH, "dependency.txt").as_deref(),
        Some("old\n")
    );
    let subject = run(isolated::command("git")
        .arg(format!("--git-dir={}", fixture.remote.display()))
        .args(["show", "-s", "--format=%s"])
        .arg(format!("refs/heads/{MAJOR_BRANCH}")));
    assert_eq!(
        String::from_utf8(subject.stdout).unwrap().trim(),
        "chore(deps): test major update"
    );

    let requests = server.received_requests().await.unwrap();
    let ordinary = json_body(created_on(&requests, BRANCH));
    let major = json_body(created_on(&requests, MAJOR_BRANCH));
    assert_eq!(ordinary["title"], "Ordinary title override");
    assert_eq!(major["title"], "chore(deps): upgrade breaking to 2.0.0");
    let major_description = major["description"].as_str().unwrap();
    assert!(major_description.contains("upd never merges this merge request"));
    assert!(
        !ordinary["description"]
            .as_str()
            .unwrap()
            .contains("upd never merges")
    );

    // Each lane keeps its own evidence.
    assert_eq!(
        fixture.artifact("upd-major-mr-description.md"),
        major_description
    );
    assert_eq!(
        fixture.artifact("upd-mr-description.md"),
        ordinary["description"].as_str().unwrap()
    );
    let major_report: serde_json::Value =
        serde_json::from_str(&fixture.artifact("upd-major-report.json")).unwrap();
    assert_eq!(
        major_report["files"][0]["updates"][0]["package"],
        "breaking"
    );
}

#[tokio::test]
async fn the_major_lane_asks_the_updater_for_major_upgrades_alone() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_both_lanes(&server).await;

    fixture.run(&server, &major_run());

    let invocations = fixture.updater_invocations();
    assert_eq!(invocations.len(), 3, "{invocations:?}");
    // Security fixes belong to the ordinary lane, which runs first.
    assert!(invocations[0].starts_with("audit "), "{invocations:?}");
    let ordinary = format!(" {} ", invocations[1]);
    let major = format!(" {} ", invocations[2]);
    assert!(ordinary.starts_with(" update "), "{ordinary}");
    assert!(major.starts_with(" update "), "{major}");
    assert!(ordinary.contains(" --max-bump minor "), "{ordinary}");
    assert!(!ordinary.contains("--only-bump"), "{ordinary}");
    assert!(!ordinary.contains("--strict-bump"), "{ordinary}");
    assert!(
        major.contains(" --only-bump major --strict-bump "),
        "{major}"
    );
    assert!(!major.contains("--max-bump"), "{major}");
    // Every other input reaches both lanes alike.
    assert!(major.contains(" --min-age 7d "), "{major}");
}

#[tokio::test]
async fn without_the_major_lane_the_major_branch_is_never_touched() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;

    fixture.run(&server, &Run::default());

    let invocations = fixture.updater_invocations();
    assert_eq!(
        invocations
            .iter()
            .filter(|args| args.starts_with("update "))
            .count(),
        1,
        "{invocations:?}"
    );
    assert_eq!(fixture.tip_of(MAJOR_BRANCH), None);
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|request| !request.url.query().unwrap_or_default().contains("major")),
        "a run without the major lane looked it up"
    );
    assert!(
        !fixture
            .checkout
            .join(".upd-ci/upd-major-report.json")
            .exists()
    );
}

/// Reruns the major lane against its merge request 8, found with auto-merge
/// armed, with the updater now writing `major_content`; requires the run to
/// cancel the auto-merge and never merge. Returns the fixture and the major
/// branch's tip before the rerun.
async fn rerun_against_an_armed_major_merge_request(major_content: &str) -> (Fixture, String) {
    let fixture = Fixture::new();
    let first = MockServer::start().await;
    serve_both_lanes(&first).await;
    fixture.run(&first, &major_run());
    let first_tip = fixture.tip_of(MAJOR_BRANCH).expect("major branch");

    let server = MockServer::start().await;
    list_mock(mr_list_response(7, false)).mount(&server).await;
    list_mock_for(MAJOR_BRANCH, mr_list_response(8, true))
        .mount(&server)
        .await;
    // GitLab answers the description edit with the merge request as it now
    // stands: still armed.
    for (iid, armed) in [(7, false), (8, true)] {
        Mock::given(method("PUT"))
            .and(path(format!("/api/v4/projects/1/merge_requests/{iid}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(iid, armed)))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(
            "/api/v4/projects/1/merge_requests/8/cancel_merge_when_pipeline_succeeds",
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(8, false)))
        .expect(1)
        .mount(&server)
        .await;
    serve_merge_request_heads(&server, &fixture).await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7/merge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, true)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/8/merge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(8, true)))
        .expect(0)
        .mount(&server)
        .await;

    let output = fixture.run(
        &server,
        &Run {
            auto_merge: true,
            major_content: major_content.to_string(),
            ..major_run()
        },
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("upd never merges a major upgrade"),
        "{}",
        describe(&output)
    );
    (fixture, first_tip)
}

#[tokio::test]
async fn the_major_lane_cancels_an_armed_auto_merge_when_it_pushes() {
    let (fixture, first_tip) = rerun_against_an_armed_major_merge_request("major again").await;
    assert_ne!(fixture.tip_of(MAJOR_BRANCH), Some(first_tip));
    assert_eq!(
        fixture.file_on(MAJOR_BRANCH, "major.txt").as_deref(),
        Some("major again\n")
    );
}

#[tokio::test]
async fn the_major_lane_cancels_an_armed_auto_merge_with_nothing_to_push() {
    let (fixture, first_tip) = rerun_against_an_armed_major_merge_request("major").await;
    assert_eq!(fixture.tip_of(MAJOR_BRANCH), Some(first_tip));
}

#[tokio::test]
async fn the_major_lane_closes_its_merge_request_once_no_major_upgrade_remains() {
    let fixture = Fixture::new();
    let first = MockServer::start().await;
    serve_both_lanes(&first).await;
    fixture.run(&first, &major_run());
    let ordinary_tip = fixture.tip_of(BRANCH).expect("ordinary branch");

    let server = MockServer::start().await;
    list_mock(mr_list_response(7, false)).mount(&server).await;
    list_mock_for(MAJOR_BRANCH, mr_list_response(8, false))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(7, false)))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/1/merge_requests/8"))
        .and(body_partial_json(json!({"state_event": "close"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(8, false)))
        .expect(1)
        .mount(&server)
        .await;

    fixture.run(
        &server,
        &Run {
            major_change: false,
            ..major_run()
        },
    );

    assert_eq!(fixture.tip_of(MAJOR_BRANCH), None);
    assert_eq!(
        fixture.tip_of(BRANCH).as_deref(),
        Some(ordinary_tip.as_str())
    );
}

#[tokio::test]
async fn a_human_commit_pauses_only_the_lane_it_landed_on() {
    let fixture = Fixture::new();
    let first = MockServer::start().await;
    serve_both_lanes(&first).await;
    fixture.run(&first, &major_run());

    git(&fixture.checkout, &["fetch", "origin", MAJOR_BRANCH]);
    git(
        &fixture.checkout,
        &["switch", "--force-create", "human-work", "FETCH_HEAD"],
    );
    fs::write(fixture.checkout.join("migration.txt"), "adapted\n").unwrap();
    git(&fixture.checkout, &["add", "migration.txt"]);
    git(
        &fixture.checkout,
        &[
            "-c",
            "user.name=Human Maintainer",
            "-c",
            "user.email=human@example.com",
            "commit",
            "-m",
            "fix: adapt to breaking 2.0",
        ],
    );
    git(
        &fixture.checkout,
        &["push", "origin", &format!("HEAD:refs/heads/{MAJOR_BRANCH}")],
    );
    let human_tip = String::from_utf8(git(&fixture.checkout, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    git(&fixture.checkout, &["switch", "main"]);

    let server = MockServer::start().await;
    list_mock(mr_list_response(7, false)).mount(&server).await;
    let mut major = mr_list_response(8, false);
    major[0]["description"] = json!("Major review work");
    list_mock_for(MAJOR_BRANCH, major).mount(&server).await;
    for iid in [7, 8] {
        Mock::given(method("PUT"))
            .and(path(format!("/api/v4/projects/1/merge_requests/{iid}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(mr_response(iid, false)))
            .expect(1)
            .mount(&server)
            .await;
    }

    fixture.run(
        &server,
        &Run {
            content: "newer".to_string(),
            major_content: "major again".to_string(),
            ..major_run()
        },
    );

    assert_eq!(
        fixture.tip_of(MAJOR_BRANCH).as_deref(),
        Some(human_tip.as_str())
    );
    assert_eq!(
        fixture.file_on(BRANCH, "dependency.txt").as_deref(),
        Some("newer\n")
    );
    let requests = server.received_requests().await.unwrap();
    let pause_edit = requests
        .iter()
        .find(|request| {
            request.method.as_str() == "PUT"
                && request.url.path() == "/api/v4/projects/1/merge_requests/8"
        })
        .expect("major merge request edited");
    assert!(String::from_utf8_lossy(&pause_edit.body).contains("upd-human-commit-pause"));
    let ordinary_edit = requests
        .iter()
        .find(|request| {
            request.method.as_str() == "PUT"
                && request.url.path() == "/api/v4/projects/1/merge_requests/7"
        })
        .expect("ordinary merge request edited");
    assert!(!String::from_utf8_lossy(&ordinary_edit.body).contains("upd-human-commit-pause"));
}

#[tokio::test]
async fn a_failed_major_lane_fails_the_job_after_the_ordinary_lane_published() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock(json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .and(body_partial_json(json!({"source_branch": BRANCH})))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;

    let output = fixture.execute_upd(
        &server,
        &Run {
            major_exit: Some(3),
            ..major_run()
        },
        &["--format", "json"],
    );

    assert!(!output.status.success(), "{}", describe(&output));
    assert_eq!(
        fixture.file_on(BRANCH, "dependency.txt").as_deref(),
        Some("new\n")
    );
    assert_eq!(fixture.tip_of(MAJOR_BRANCH), None);
    let outcome: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["command"], "gitlab run");
    assert_eq!(outcome["outcome"], "published", "{outcome}");
    assert_eq!(outcome["major"]["outcome"], "failed", "{outcome}");
    assert_eq!(outcome["major"]["branch"], MAJOR_BRANCH, "{outcome}");
    assert!(outcome["major"].get("command").is_none(), "{outcome}");
}

#[tokio::test]
async fn a_failed_ordinary_lane_still_lets_the_major_lane_publish() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock_for(MAJOR_BRANCH, json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .and(body_partial_json(json!({"source_branch": MAJOR_BRANCH})))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(8, false)))
        .expect(1)
        .mount(&server)
        .await;

    let output = fixture.execute_upd(
        &server,
        &Run {
            upd_exit: Some(3),
            ..major_run()
        },
        &["--format", "json"],
    );

    assert!(!output.status.success(), "{}", describe(&output));
    assert_eq!(fixture.tip_of(BRANCH), None);
    assert_eq!(
        fixture.file_on(MAJOR_BRANCH, "major.txt").as_deref(),
        Some("major\n")
    );
    let outcome: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["outcome"], "failed", "{outcome}");
    assert_eq!(outcome["major"]["outcome"], "published", "{outcome}");
}

#[tokio::test]
async fn a_major_lane_that_would_repeat_the_ordinary_one_is_refused_before_any_work() {
    for (max_bump, major_branch) in [
        ("major", MAJOR_BRANCH),
        ("", MAJOR_BRANCH),
        ("minor", BRANCH),
        ("minor", "main"),
    ] {
        let server = MockServer::start().await;
        let fixture = Fixture::new();
        let output = fixture.execute(
            &server,
            &Run {
                max_bump: max_bump.to_string(),
                major_branch: major_branch.to_string(),
                ..major_run()
            },
        );
        assert!(
            failed_with(&output, 4),
            "{max_bump:?} {major_branch}: {}",
            describe(&output)
        );
        assert!(fixture.updater_invocations().is_empty());
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn golden_major_lane() {
    assert_lane_matches_golden(
        "major-lane",
        Run {
            auto_merge: true,
            ..major_run()
        },
        true,
    )
    .await;
}

/// An ordinary report whose ceiling held a major release, a cooldown hold and,
/// under a patch ceiling, a minor release.
fn held_report(minor_capped: bool) -> String {
    let mut capped = vec![json!({
        "package": "breaking", "current": "1.4.0", "available": "2.0.0", "bump": "major"
    })];
    if minor_capped {
        capped.push(json!({
            "package": "featureful", "current": "3.1.0", "available": "3.2.0", "bump": "minor"
        }));
    }
    json!({
        "command": "update",
        "mode": "applied",
        "files": [{
            "path": "dependency.txt",
            "file_type": "test",
            "lang": "test",
            "updates": [{"package": "example", "current": "1.0.0", "latest": "1.0.1", "bump": "patch"}],
            "held_back": [{"package": "fresh", "current": "1.0.0", "chosen": "1.0.0", "skipped_latest": "1.1.0"}],
            "capped": capped,
            "pinned": [], "ignored": [], "errors": [], "warnings": [],
        }],
        "summary": {
            "files_scanned": 1, "files_with_changes": 1, "updates_total": 1,
            "updates_major": 0, "updates_minor": 0, "updates_patch": 1,
            "pinned": 0, "ignored": 0, "errors": 0, "warnings": 0,
        },
    })
    .to_string()
}

#[tokio::test]
async fn golden_ordinary_lane_links_the_majors_its_ceiling_held() {
    let description = assert_lane_matches_golden(
        "held-linked",
        Run {
            report: held_report(false),
            ..major_run()
        },
        false,
    )
    .await;
    assert!(
        description.contains(
            "merge_requests?state=opened&source_branch=automation%2Fupd-dependencies-major"
        )
    );
}

#[tokio::test]
async fn golden_ordinary_lane_under_a_patch_ceiling_keeps_its_minor_hold() {
    let description = assert_lane_matches_golden(
        "held-linked-patch-cap",
        Run {
            report: held_report(true),
            max_bump: "patch".to_string(),
            ..major_run()
        },
        false,
    )
    .await;
    assert!(description.contains("<code>featureful</code>"));
}

#[tokio::test]
async fn the_major_lane_starts_clean_after_the_ordinary_lane_stopped_midway() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    list_mock_for(MAJOR_BRANCH, json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .and(body_partial_json(json!({"source_branch": MAJOR_BRANCH})))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(8, false)))
        .expect(1)
        .mount(&server)
        .await;

    // Validation rejects the ordinary lane's update, leaving that update in
    // the tree and an untracked file beside it; the major lane's tree
    // passes it untouched.
    let output =
        fixture.execute(
            &server,
            &Run {
                validation_command:
                    "if grep -q new dependency.txt; then echo stray > stray.txt; exit 1; fi"
                        .to_string(),
                ..major_run()
            },
        );

    assert!(!output.status.success(), "{}", describe(&output));
    assert_eq!(fixture.tip_of(BRANCH), None);
    assert_eq!(
        fixture.file_on(MAJOR_BRANCH, "dependency.txt").as_deref(),
        Some("old\n")
    );
    assert_eq!(fixture.file_on(MAJOR_BRANCH, "stray.txt"), None);
    assert_eq!(
        fixture.file_on(MAJOR_BRANCH, "major.txt").as_deref(),
        Some("major\n")
    );
}

#[tokio::test]
async fn a_checkout_with_local_changes_is_refused_before_either_lane_touches_it() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    fs::write(fixture.checkout.join("dependency.txt"), "edited by hand\n").unwrap();
    fs::write(fixture.checkout.join("notes.txt"), "not committed yet\n").unwrap();

    for args in [&["--dry-run"][..], &[]] {
        let output = fixture.execute_upd(&server, &major_run(), args);

        assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("uncommitted changes"),
            "{}",
            describe(&output)
        );
        assert_eq!(
            fs::read_to_string(fixture.checkout.join("dependency.txt")).unwrap(),
            "edited by hand\n",
            "{args:?}"
        );
        assert_eq!(
            fs::read_to_string(fixture.checkout.join("notes.txt")).unwrap(),
            "not committed yet\n",
            "{args:?}"
        );
    }
    assert_eq!(fixture.tip_of(BRANCH), None);
    assert_eq!(fixture.tip_of(MAJOR_BRANCH), None);
    assert!(server.received_requests().await.unwrap().is_empty());
}

// Lane separation with the real updater: the ordinary lane carries every
// change that is not a major upgrade, and the major lane carries nothing
// else.

/// An npm registry document for `name` publishing `versions`, each old enough
/// to clear any cooldown.
fn npm_document(name: &str, versions: &[&str]) -> serde_json::Value {
    let latest = versions.last().expect("at least one version");
    json!({
        "name": name,
        "dist-tags": {"latest": latest},
        "versions": versions
            .iter()
            .map(|version| (version.to_string(), json!({"name": name, "version": version})))
            .collect::<serde_json::Map<_, _>>(),
        "time": versions
            .iter()
            .map(|version| (version.to_string(), json!("2025-01-01T00:00:00.000Z")))
            .collect::<serde_json::Map<_, _>>(),
    })
}

/// Answers an OSV batch query with one empty result per query: no package
/// has an advisory.
struct NoAdvisories;

impl Respond for NoAdvisories {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let queries = json_body(request)["queries"].as_array().map_or(0, Vec::len);
        ResponseTemplate::new(200)
            .set_body_json(json!({"results": vec![json!({"vulns": []}); queries]}))
    }
}

/// Serves `packages` as an npm registry, which also answers OSV batch
/// queries with no advisories.
async fn npm_registry(packages: &[(&str, &[&str])]) -> MockServer {
    let registry = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(NoAdvisories)
        .mount(&registry)
        .await;
    for (name, versions) in packages {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(npm_document(name, versions)))
            .mount(&registry)
            .await;
    }
    registry
}

/// Commits a `package.json` depending on `pinned` 1.0.0 and `breaking`
/// 1.4.0, with `pinned` configured to 1.2.0, to the default branch.
fn commit_pinned_project(fixture: &Fixture) {
    fs::write(
        fixture.checkout.join("package.json"),
        "{\n  \"name\": \"lanes\",\n  \"private\": true,\n  \"dependencies\": {\n    \"breaking\": \"1.4.0\",\n    \"pinned\": \"1.0.0\"\n  }\n}\n",
    )
    .expect("package.json");
    fs::write(
        fixture.checkout.join(".updrc.toml"),
        "[pin]\npinned = \"1.2.0\"\n",
    )
    .expect("upd config");
    git(&fixture.checkout, &["add", "package.json", ".updrc.toml"]);
    git(&fixture.checkout, &["commit", "-m", "test: npm project"]);
    git(&fixture.checkout, &["push", "origin", "main"]);
}

/// `git diff --numstat` from the default branch to `branch` on the remote.
fn numstat_from_main(fixture: &Fixture, branch: &str) -> String {
    let output = run(isolated::command("git")
        .arg(format!("--git-dir={}", fixture.remote.display()))
        .args(["diff", "--numstat", "refs/heads/main"])
        .arg(format!("refs/heads/{branch}")));
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_pin_alone_reaches_only_the_ordinary_lane() {
    let registry = npm_registry(&[
        ("breaking", &["1.4.0"]),
        ("pinned", &["1.0.0", "1.2.0", "1.3.0"]),
    ])
    .await;
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    commit_pinned_project(&fixture);
    list_mock(json!([])).mount(&server).await;
    list_mock_for(MAJOR_BRANCH, json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .and(body_partial_json(json!({"source_branch": BRANCH})))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .and(body_partial_json(json!({"source_branch": MAJOR_BRANCH})))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(8, false)))
        .expect(0)
        .mount(&server)
        .await;

    fixture.run(
        &server,
        &Run {
            npm_registry: Some(registry.uri()),
            ..major_run()
        },
    );

    let ordinary = fixture.file_on(BRANCH, "package.json").unwrap();
    assert!(ordinary.contains("\"pinned\": \"1.2.0\""), "{ordinary}");
    assert_eq!(numstat_from_main(&fixture, BRANCH), "1\t1\tpackage.json\n");
    assert_eq!(fixture.tip_of(MAJOR_BRANCH), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_major_lane_carries_the_held_major_and_nothing_else() {
    let registry = npm_registry(&[
        ("breaking", &["1.4.0", "2.0.0"]),
        ("pinned", &["1.0.0", "1.2.0", "1.3.0"]),
    ])
    .await;
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    commit_pinned_project(&fixture);
    serve_both_lanes(&server).await;

    fixture.run(
        &server,
        &Run {
            npm_registry: Some(registry.uri()),
            ..major_run()
        },
    );

    let ordinary = fixture.file_on(BRANCH, "package.json").unwrap();
    assert!(ordinary.contains("\"breaking\": \"1.4.0\""), "{ordinary}");
    assert!(ordinary.contains("\"pinned\": \"1.2.0\""), "{ordinary}");
    let major = fixture.file_on(MAJOR_BRANCH, "package.json").unwrap();
    assert!(major.contains("\"breaking\": \"2.0.0\""), "{major}");
    assert!(major.contains("\"pinned\": \"1.0.0\""), "{major}");
    assert_eq!(numstat_from_main(&fixture, BRANCH), "1\t1\tpackage.json\n");
    assert_eq!(
        numstat_from_main(&fixture, MAJOR_BRANCH),
        "1\t1\tpackage.json\n"
    );

    let requests = server.received_requests().await.unwrap();
    let description = json_body(created_on(&requests, MAJOR_BRANCH))["description"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(description.contains("breaking"), "{description}");
    assert!(!description.contains("pinned"), "{description}");
}

// Security remediation: before the update, the ordinary lane asks the
// updater to fix every dependency with a published advisory, whatever the
// update policy would allow.

/// The report `upd audit --fix-audit` prints for `vulnerabilities` and
/// `fixes`, recording `errors` errors.
fn audit_report(
    vulnerabilities: serde_json::Value,
    fixes: serde_json::Value,
    errors: u64,
) -> String {
    let count = vulnerabilities.as_array().map_or(0, Vec::len);
    json!({
        "command": "audit",
        "status": if errors == 0 { "complete" } else { "incomplete" },
        "vulnerabilities": vulnerabilities,
        "errors": (0..errors).map(|n| format!("osv query {n} failed")).collect::<Vec<_>>(),
        "summary": {
            "packages_checked": 3,
            "vulnerable_packages": count,
            "vulnerabilities": count,
            "errors": errors,
        },
        "fixes": fixes,
    })
    .to_string()
}

/// One RustSec advisory for the crate `package` at `version`.
fn crate_advisory(package: &str, version: &str, id: &str, severity: &str) -> serde_json::Value {
    let mut advisory = advisory(package, version, id, severity);
    advisory["ecosystem"] = json!("crates.io");
    advisory["source"] = json!("RUSTSEC");
    advisory
}

/// One npm advisory for `package` at `version`.
fn advisory(package: &str, version: &str, id: &str, severity: &str) -> serde_json::Value {
    json!({
        "package": package,
        "version": version,
        "ecosystem": "npm",
        "id": id,
        "severity": severity,
        "summary": format!("advisory for {package}"),
        "source": "GHSA",
    })
}

/// A run whose update changes nothing and whose security step fixes one
/// advisory in `lodash`, writing the fix to `security.txt`.
fn security_fix_run() -> Run {
    Run {
        change: false,
        audit_file: "security.txt".to_string(),
        audit_content: "lodash 4.17.21".to_string(),
        audit_report: audit_report(
            json!([advisory("lodash", "4.17.20", "GHSA-35jh-r3h4-6jhm", "High")]),
            json!([{
                "package": "lodash", "ecosystem": "npm", "from_version": "4.17.20", "to_version": "4.17.21",
                "method": "manifest", "path": "security.txt", "status": "pending_relock",
            }]),
            0,
        ),
        ..Run::default()
    }
}

fn golden_audit(case: &str) -> String {
    fs::read_to_string(Path::new(GOLDEN_DIR).join(case).join("audit.json"))
        .expect("golden case audit report")
}

/// Mounts the merge request list and creation for the ordinary lane.
async fn serve_ordinary_lane(server: &MockServer) {
    list_mock(json!([])).mount(server).await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr_response(7, false)))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn security_fixes_run_before_the_update_and_without_its_policy() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_ordinary_lane(&server).await;

    fixture.run(
        &server,
        &Run {
            packages: "example".to_string(),
            ..Run::default()
        },
    );

    let invocations = fixture.updater_invocations();
    assert_eq!(invocations.len(), 2, "{invocations:?}");
    // The cooldown reaches the security step only to read its relocks back
    // against; the bump ceiling and package filter never do.
    assert_eq!(
        invocations[0],
        "audit --fix-audit --apply --full-precision --format json --no-lock --min-age 7d ."
    );
    // The update keeps the policy the security step leaves out.
    let update = format!(" {} ", invocations[1]);
    assert!(update.starts_with(" update "), "{update}");
    for policy in [
        " --min-age 7d ",
        " --max-bump minor ",
        " --package example ",
    ] {
        assert!(update.contains(policy), "{policy} missing from {update}");
    }
}

#[tokio::test]
async fn security_fixes_relock_when_lockfile_regeneration_is_on() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_ordinary_lane(&server).await;

    fixture.run(
        &server,
        &Run {
            lock: "true".to_string(),
            ..Run::default()
        },
    );

    let invocations = fixture.updater_invocations();
    assert_eq!(
        invocations[0],
        "audit --fix-audit --apply --full-precision --format json --min-age 7d ."
    );
    assert!(
        format!(" {} ", invocations[1]).contains(" --lock "),
        "{invocations:?}"
    );
}

#[tokio::test]
async fn disabled_security_remediation_never_audits() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_ordinary_lane(&server).await;

    let output = fixture.execute_upd(
        &server,
        &Run {
            security_remediation: "false".to_string(),
            change: true,
            ..security_fix_run()
        },
        &["--output", "json"],
    );

    let outcome = outcome_of(&output);
    let invocations = fixture.updater_invocations();
    assert_eq!(invocations.len(), 1, "{invocations:?}");
    assert!(invocations[0].starts_with("update "), "{invocations:?}");
    assert!(outcome.get("security").is_none(), "{outcome}");
}

#[tokio::test]
async fn template_rejects_a_non_boolean_security_remediation_input() {
    assert_rejects_input(Run {
        security_remediation: "yes".to_string(),
        ..Run::default()
    })
    .await;
}

#[tokio::test]
async fn a_security_fix_alone_is_published_as_a_fix() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_ordinary_lane(&server).await;

    let output = fixture.execute_upd(&server, &security_fix_run(), &["--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["outcome"], "published", "{outcome}");
    assert_eq!(
        outcome["security"],
        json!({
            "fixes": 1,
            "pending_relock": 1,
            "blocked": 0,
            "skipped": 0,
            "not_applied": 0,
            "unfixable": 0,
            "advisories": 1,
        })
    );
    assert_eq!(
        fixture.file_on(BRANCH, "security.txt").as_deref(),
        Some("lodash 4.17.21\n")
    );
    let requests = server.received_requests().await.unwrap();
    let create = json_body(created_on(&requests, BRANCH));
    assert_eq!(
        create["title"], "fix(security): resolve advisory in lodash",
        "{create}"
    );
    let description = create["description"].as_str().unwrap();
    assert!(description.contains("GHSA-35jh-r3h4-6jhm"), "{description}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "upd audit: 1 fixed, 1 pending relock, 0 skipped, 0 blocked, 0 not applied, 0 without a fix, 0 error(s)"
        ),
        "{}",
        describe(&output)
    );
}

#[tokio::test]
async fn a_dry_run_applies_security_fixes_and_names_them_in_the_title() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    Mock::given(method("GET"))
        .and(path("/api/v4/projects/1/merge_requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let output = fixture.execute_upd(
        &server,
        &security_fix_run(),
        &["--dry-run", "--output", "json"],
    );

    let outcome = outcome_of(&output);
    assert_eq!(outcome["outcome"], "would_publish", "{outcome}");
    assert_eq!(
        outcome["title"], "fix(security): resolve advisory in lodash",
        "{outcome}"
    );
    assert_eq!(outcome["security"]["fixes"], 1, "{outcome}");
    assert_eq!(fixture.remote_tip(), None);
    assert!(only_reads(&server.received_requests().await.unwrap()));
}

#[tokio::test]
async fn the_security_report_is_kept_as_a_pipeline_artifact() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_ordinary_lane(&server).await;
    let run = security_fix_run();

    fixture.run(&server, &run);

    let kept: serde_json::Value =
        serde_json::from_str(&fixture.artifact("upd-security-report.json")).unwrap();
    let printed: serde_json::Value = serde_json::from_str(&run.audit_report).unwrap();
    assert_eq!(kept, printed);
}

#[tokio::test]
async fn a_blocked_security_fix_publishes_the_rest_and_says_what_needs_attention() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_ordinary_lane(&server).await;
    let report = audit_report(
        json!([
            crate_advisory("time", "0.3.20", "RUSTSEC-2024-0010", "Low"),
            crate_advisory("lru", "0.16.0", "RUSTSEC-2024-0011", "High"),
        ]),
        json!([
            {"package": "time", "ecosystem": "crates.io", "from_version": "0.3.20", "to_version": "0.3.36",
             "method": "cargo-precise", "path": "Cargo.lock", "status": "applied"},
            {"package": "lru", "ecosystem": "crates.io", "from_version": "0.16.0", "to_version": "0.16.3",
             "method": "cargo-precise", "path": "Cargo.lock", "status": "blocked",
             "error": "ratatui-core requires lru ^0.16.0, <0.16.2"},
        ]),
        0,
    );

    let output = fixture.execute_upd(
        &server,
        &Run {
            lock: "true".to_string(),
            audit_file: "Cargo.lock".to_string(),
            audit_content: "time 0.3.36".to_string(),
            audit_report: report,
            audit_exit: Some(6),
            ..Run::default()
        },
        &["--output", "json"],
    );

    let outcome = outcome_of(&output);
    assert_eq!(outcome["outcome"], "published", "{outcome}");
    assert_eq!(outcome["security"]["fixes"], 1, "{outcome}");
    assert_eq!(outcome["security"]["blocked"], 1, "{outcome}");
    // Only advisories whose package was fixed entirely count as resolved.
    assert_eq!(outcome["security"]["advisories"], 1, "{outcome}");
    assert_eq!(
        fixture.file_on(BRANCH, "Cargo.lock").as_deref(),
        Some("time 0.3.36\n")
    );
    assert_eq!(fixture.branch_file().as_deref(), Some("new\n"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("lru") && stderr.contains("ratatui-core requires lru ^0.16.0, <0.16.2"),
        "{}",
        describe(&output)
    );
    let requests = server.received_requests().await.unwrap();
    let description = json_body(created_on(&requests, BRANCH))["description"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(description.contains("### Needs attention"), "{description}");
}

#[tokio::test]
async fn a_failed_security_fix_stops_the_run_before_the_update() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    let report = audit_report(
        json!([advisory("lodash", "4.17.20", "GHSA-35jh-r3h4-6jhm", "High")]),
        json!([{
            "package": "lodash", "ecosystem": "npm", "from_version": "4.17.20", "to_version": "4.17.21",
            "method": "manifest", "path": "package.json", "status": "rolled_back",
            "error": "npm install failed",
        }]),
        0,
    );

    let output = fixture.execute(
        &server,
        &Run {
            audit_report: report,
            audit_exit: Some(2),
            ..Run::default()
        },
    );

    assert!(failed_with(&output, 2), "{}", describe(&output));
    let invocations = fixture.updater_invocations();
    assert_eq!(invocations.len(), 1, "{invocations:?}");
    assert_eq!(fixture.remote_tip(), None);
    assert!(server.received_requests().await.unwrap().is_empty());
    // What the updater reported is kept for diagnosis all the same.
    assert!(
        fixture
            .artifact("upd-security-report.json")
            .contains("rolled_back")
    );
}

#[tokio::test]
async fn a_security_step_that_cannot_reach_its_advisories_is_a_network_failure() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();

    let output = fixture.execute(
        &server,
        &Run {
            audit_exit: Some(3),
            ..Run::default()
        },
    );

    assert!(failed_with(&output, 3), "{}", describe(&output));
    assert_eq!(fixture.updater_invocations().len(), 1);
    assert_eq!(fixture.remote_tip(), None);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_security_report_with_errors_is_refused() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    // Partial advisory data: the fix that did apply cannot be trusted to be
    // the whole story, and the exit status alone does not say so.
    let report = audit_report(
        json!([advisory("lodash", "4.17.20", "GHSA-35jh-r3h4-6jhm", "High")]),
        json!([{
            "package": "lodash", "ecosystem": "npm", "from_version": "4.17.20", "to_version": "4.17.21",
            "method": "manifest", "path": "security.txt", "status": "pending_relock",
        }]),
        1,
    );

    let output = fixture.execute(
        &server,
        &Run {
            audit_report: report,
            ..security_fix_run()
        },
    );

    assert!(failed_with(&output, 2), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("while applying security fixes"),
        "{}",
        describe(&output)
    );
    assert_eq!(fixture.updater_invocations().len(), 1);
    assert_eq!(fixture.remote_tip(), None);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_security_report_that_is_not_json_is_refused() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();

    let output = fixture.execute(
        &server,
        &Run {
            audit_report: "not json".to_string(),
            ..Run::default()
        },
    );

    assert!(failed_with(&output, 2), "{}", describe(&output));
    assert_eq!(fixture.updater_invocations().len(), 1);
    assert_eq!(fixture.remote_tip(), None);
}

#[tokio::test]
async fn the_major_lane_never_applies_security_fixes() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_both_lanes(&server).await;

    fixture.run(&server, &major_run());

    let audits = fixture
        .updater_invocations()
        .into_iter()
        .filter(|args| args.starts_with("audit "))
        .count();
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn golden_security_fix_alone() {
    assert_rendering_matches_golden(
        "security-only",
        Run {
            change: false,
            audit_file: "dependency.txt".to_string(),
            audit_content: "lodash 4.17.21".to_string(),
            audit_report: golden_audit("security-only"),
            ..Run::default()
        },
    )
    .await;
}

/// A young release the fix's relock locked besides the fix is listed for
/// review, and the fix still stands.
#[tokio::test]
async fn golden_security_fix_pulling_in_a_young_release() {
    assert_rendering_matches_golden(
        "security-young-transitive",
        Run {
            lock: "true".to_string(),
            change: false,
            audit_file: "dependency.txt".to_string(),
            audit_content: "lodash 4.17.21".to_string(),
            audit_report: golden_audit("security-young-transitive"),
            ..Run::default()
        },
    )
    .await;
}

/// A run whose security step fixes `lodash` in `package.json` with its
/// lockfile regenerated, whose update then changes `dependency.txt`, and
/// whose audit of the updated tree prints `recheck`.
fn recheck_run(recheck: String) -> Run {
    Run {
        lock: "true".to_string(),
        audit_file: "package.json".to_string(),
        audit_content: "lodash 4.17.21".to_string(),
        audit_report: audit_report(
            json!([
                advisory("lodash", "4.17.20", "GHSA-35jh-r3h4-6jhm", "High"),
                advisory("lodash", "4.17.20", "GHSA-29mw-wpgm-hmr9", "Medium"),
            ]),
            json!([{
                "package": "lodash", "ecosystem": "npm", "from_version": "4.17.20", "to_version": "4.17.21",
                "method": "manifest", "path": "package.json", "status": "applied",
            }]),
            0,
        ),
        recheck_report: recheck,
        ..Run::default()
    }
}

/// The audit of the updated tree, which the security step's own flags
/// scope and nothing else: it neither fixes nor applies.
const RECHECK_INVOCATION: &str = "audit --format json .";

#[tokio::test]
async fn an_update_that_moves_a_fixed_dependency_back_to_a_vulnerable_release_is_flagged() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_ordinary_lane(&server).await;
    let recheck = audit_report(
        json!([advisory(
            "lodash",
            "4.17.22",
            "GHSA-test-7x2q-reintroduced",
            "High"
        )]),
        json!([]),
        0,
    );
    let run = Run {
        // The audit exits 6 when it finds a vulnerability; that is the
        // finding, not a failure.
        recheck_exit: Some(6),
        ..recheck_run(recheck.clone())
    };

    let output = fixture.execute_upd(&server, &run, &["--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["outcome"], "published", "{outcome}");
    assert_eq!(outcome["security"]["fixes"], 1, "{outcome}");
    assert_eq!(outcome["security"]["reintroduced"], 1, "{outcome}");
    // The fix's advisories no longer count as resolved.
    assert_eq!(outcome["security"]["advisories"], 0, "{outcome}");
    let invocations = fixture.updater_invocations();
    assert_eq!(invocations.len(), 3, "{invocations:?}");
    assert_eq!(invocations[2], RECHECK_INVOCATION);
    assert_eq!(
        fixture.artifact("upd-security-recheck.json").trim(),
        recheck
    );
    let reason = "the dependency update moved lodash to 4.17.22, which GHSA-test-7x2q-reintroduced still affects";
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!(
            "warning: lodash (fixed in package.json): {reason}"
        )),
        "{}",
        describe(&output)
    );
    let requests = server.received_requests().await.unwrap();
    let create = json_body(created_on(&requests, BRANCH));
    assert_eq!(
        create["title"], "fix(security): update vulnerable lodash and refresh dependencies",
        "{create}"
    );
    let description = create["description"].as_str().unwrap();
    assert!(description.contains(reason), "{description}");
    assert!(
        description.contains("**1 vulnerable again**"),
        "{description}"
    );
}

#[tokio::test]
async fn an_update_that_keeps_a_fixed_dependency_safe_leaves_its_advisories_resolved() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_ordinary_lane(&server).await;

    let output = fixture.execute_upd(&server, &recheck_run(String::new()), &["--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["outcome"], "published", "{outcome}");
    assert_eq!(outcome["security"]["advisories"], 2, "{outcome}");
    assert!(
        outcome["security"].get("reintroduced").is_none(),
        "{outcome}"
    );
    // The recheck ran and found the tree clean.
    let invocations = fixture.updater_invocations();
    assert_eq!(invocations.len(), 3, "{invocations:?}");
    assert_eq!(invocations[2], RECHECK_INVOCATION);
    let requests = server.received_requests().await.unwrap();
    let create = json_body(created_on(&requests, BRANCH));
    assert_eq!(
        create["title"], "fix(security): resolve 2 advisories and refresh dependencies",
        "{create}"
    );
}

#[tokio::test]
async fn an_update_that_changes_nothing_is_not_audited_again() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_ordinary_lane(&server).await;
    let vulnerable = audit_report(
        json!([advisory(
            "lodash",
            "4.17.22",
            "GHSA-test-7x2q-reintroduced",
            "High"
        )]),
        json!([]),
        0,
    );

    let output = fixture.execute_upd(
        &server,
        &Run {
            change: false,
            ..recheck_run(vulnerable)
        },
        &["--output", "json"],
    );

    let outcome = outcome_of(&output);
    assert_eq!(outcome["security"]["advisories"], 2, "{outcome}");
    let invocations = fixture.updater_invocations();
    assert_eq!(invocations.len(), 2, "{invocations:?}");
    assert!(!invocations.iter().any(|line| line == RECHECK_INVOCATION));
}

#[tokio::test]
async fn a_fix_awaiting_a_relock_is_not_undone_by_its_stale_lockfile() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_ordinary_lane(&server).await;
    // Until a relock the lockfile still records the vulnerable version the
    // security step found; the update did not put it there.
    let stale = audit_report(
        json!([advisory("lodash", "4.17.20", "GHSA-35jh-r3h4-6jhm", "High")]),
        json!([]),
        0,
    );

    let output = fixture.execute_upd(
        &server,
        &Run {
            change: true,
            recheck_report: stale,
            recheck_exit: Some(6),
            ..security_fix_run()
        },
        &["--output", "json"],
    );

    let outcome = outcome_of(&output);
    assert_eq!(outcome["security"]["pending_relock"], 1, "{outcome}");
    assert_eq!(outcome["security"]["advisories"], 1, "{outcome}");
    assert!(
        outcome["security"].get("reintroduced").is_none(),
        "{outcome}"
    );
    let invocations = fixture.updater_invocations();
    assert_eq!(invocations.len(), 3, "{invocations:?}");
    assert_eq!(invocations[2], RECHECK_INVOCATION);
}

#[tokio::test]
async fn an_audit_of_the_updated_tree_with_errors_is_refused() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();

    let output = fixture.execute(&server, &recheck_run(audit_report(json!([]), json!([]), 1)));

    assert!(failed_with(&output, 2), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("while auditing the updated tree"),
        "{}",
        describe(&output)
    );
    assert_eq!(fixture.updater_invocations().len(), 3);
    assert_eq!(fixture.remote_tip(), None);
}

/// A fixed dependency the update moved back to a vulnerable release is
/// listed for review, and the merge request is still published.
#[tokio::test]
async fn golden_security_fix_the_update_moved_back() {
    assert_rendering_matches_golden(
        "security-reintroduced",
        recheck_run(
            fs::read_to_string(
                Path::new(GOLDEN_DIR)
                    .join("security-reintroduced")
                    .join("recheck.json"),
            )
            .expect("golden case recheck report"),
        ),
    )
    .await;
}

/// What the security step's cooldown check could not verify.
const FIX_AUDIT_WARNING: &str = "package-lock.json: companion 1.0.0 could not be checked against the 7d cooldown (the registry lookup failed)";
/// What the audit of the updated tree could not check.
const RECHECK_WARNING: &str =
    "yarn.lock: the lockfile could not be parsed, so its dependencies were not audited";

/// A security fix whose audit, and the audit after the update, each report
/// something they could not check; the recheck repeats the fix audit's.
fn warned_run() -> Run {
    let mut recheck: serde_json::Value =
        serde_json::from_str(&audit_report(json!([]), json!([]), 0)).unwrap();
    recheck["warnings"] = json!([FIX_AUDIT_WARNING, RECHECK_WARNING]);
    let run = recheck_run(recheck.to_string());
    let mut report: serde_json::Value = serde_json::from_str(&run.audit_report).unwrap();
    report["warnings"] = json!([FIX_AUDIT_WARNING]);
    Run {
        audit_report: report.to_string(),
        ..run
    }
}

#[tokio::test]
async fn what_the_security_audits_could_not_check_is_logged_and_listed_for_review() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    serve_ordinary_lane(&server).await;

    let output = fixture.execute_upd(&server, &warned_run(), &["--output", "json"]);

    let outcome = outcome_of(&output);
    assert_eq!(outcome["outcome"], "published", "{outcome}");
    assert_eq!(outcome["security"]["audit_warnings"], 2, "{outcome}");
    // A warning is a gap in the check, not a vulnerability.
    assert_eq!(outcome["security"]["advisories"], 2, "{outcome}");
    let log = String::from_utf8_lossy(&output.stderr);
    for warning in [FIX_AUDIT_WARNING, RECHECK_WARNING] {
        assert_eq!(
            log.matches(&format!("warning: {warning}")).count(),
            1,
            "{}",
            describe(&output)
        );
    }
    let requests = server.received_requests().await.unwrap();
    let description = json_body(created_on(&requests, BRANCH))["description"]
        .as_str()
        .unwrap()
        .to_string();
    let attention = &description[description
        .find("### Needs attention")
        .expect("a Needs attention section")..];
    for warning in [FIX_AUDIT_WARNING, RECHECK_WARNING] {
        assert_eq!(
            attention.matches(&format!("- {warning}")).count(),
            1,
            "{description}"
        );
    }
}

/// What the security audits could not check is listed for review.
#[tokio::test]
async fn golden_security_audit_warnings() {
    assert_rendering_matches_golden("security-audit-warnings", warned_run()).await;
}

#[tokio::test]
async fn golden_security_fix_and_updates() {
    assert_rendering_matches_golden(
        "security-and-updates",
        Run {
            audit_file: "requirements.txt".to_string(),
            audit_content: "requests==2.32.0".to_string(),
            audit_report: golden_audit("security-and-updates"),
            ..Run::default()
        },
    )
    .await;
}

#[tokio::test]
async fn golden_security_fixes_without_lockfile_regeneration() {
    assert_rendering_matches_golden(
        "security-no-lock",
        Run {
            change: false,
            audit_file: "Cargo.toml".to_string(),
            audit_content: "serde_yaml = \"0.8.4\"".to_string(),
            audit_report: golden_audit("security-no-lock"),
            audit_exit: Some(6),
            ..Run::default()
        },
    )
    .await;
}

#[tokio::test]
async fn golden_security_fix_blocked_by_a_requirement() {
    assert_rendering_matches_golden(
        "security-blocked",
        Run {
            lock: "true".to_string(),
            change: false,
            audit_file: "Cargo.lock".to_string(),
            audit_content: "time 0.3.36".to_string(),
            audit_report: golden_audit("security-blocked"),
            audit_exit: Some(6),
            ..Run::default()
        },
    )
    .await;
}

/// Answers an OSV batch query with `id` for every query about `package`,
/// and nothing for any other.
struct AdvisoryFor {
    package: &'static str,
    id: &'static str,
}

impl Respond for AdvisoryFor {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let results: Vec<serde_json::Value> = json_body(request)["queries"]
            .as_array()
            .map(|queries| {
                queries
                    .iter()
                    .map(|query| {
                        if query["package"]["name"] == self.package {
                            json!({"vulns": [{"id": self.id}]})
                        } else {
                            json!({"vulns": []})
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        ResponseTemplate::new(200).set_body_json(json!({"results": results}))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_real_updater_applies_a_security_fix_the_cooldown_would_hold_back() {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let registry = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/vulnerable"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "vulnerable",
            "dist-tags": {"latest": "1.0.1"},
            "versions": {
                "1.0.0": {"name": "vulnerable", "version": "1.0.0"},
                "1.0.1": {"name": "vulnerable", "version": "1.0.1"},
            },
            "time": {"1.0.0": "2025-01-01T00:00:00.000Z", "1.0.1": now},
        })))
        .mount(&registry)
        .await;
    let osv = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(AdvisoryFor {
            package: "vulnerable",
            id: "GHSA-test-0001-0001",
        })
        .mount(&osv)
        .await;
    Mock::given(method("GET"))
        .and(path("/vulns/GHSA-test-0001-0001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "GHSA-test-0001-0001",
            "summary": "test vulnerability",
            "database_specific": {"severity": "HIGH"},
            "affected": [{
                "package": {"name": "vulnerable", "ecosystem": "npm"},
                "ranges": [{"type": "SEMVER", "events": [{"introduced": "0"}, {"fixed": "1.0.1"}]}],
            }],
        })))
        .mount(&osv)
        .await;
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    fs::write(
        fixture.checkout.join("package.json"),
        "{\n  \"name\": \"security\",\n  \"private\": true,\n  \"dependencies\": {\n    \"vulnerable\": \"1.0.0\"\n  }\n}\n",
    )
    .expect("package.json");
    git(&fixture.checkout, &["add", "package.json"]);
    git(&fixture.checkout, &["commit", "-m", "test: npm project"]);
    git(&fixture.checkout, &["push", "origin", "main"]);
    serve_ordinary_lane(&server).await;

    fixture.run(
        &server,
        &Run {
            npm_registry: Some(registry.uri()),
            osv: Some(osv.uri()),
            min_age: "7d".to_string(),
            ..Run::default()
        },
    );

    let manifest = fixture.file_on(BRANCH, "package.json").unwrap();
    assert!(manifest.contains("\"vulnerable\": \"1.0.1\""), "{manifest}");
    assert_eq!(numstat_from_main(&fixture, BRANCH), "1\t1\tpackage.json\n");
    let requests = server.received_requests().await.unwrap();
    let create = json_body(created_on(&requests, BRANCH));
    assert_eq!(
        create["title"], "fix(security): resolve advisory in vulnerable",
        "{create}"
    );
    let report: serde_json::Value =
        serde_json::from_str(&fixture.artifact("upd-security-report.json")).unwrap();
    assert_eq!(report["fixes"][0]["status"], "pending_relock", "{report}");
}
