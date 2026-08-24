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
const BRANCH: &str = "automation/upd-dependencies";

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
  cat <<JSON
{"command":"update","mode":"applied","files":[{"path":"dependency.txt","file_type":"test","lang":"test","updates":[{"package":"example","current":"1.0.0","latest":"2.0.0","bump":"major"}],"pinned":[],"ignored":[],"errors":[],"warnings":[]}],"summary":{"files_scanned":1,"files_with_changes":1,"updates_total":1,"updates_major":1,"updates_minor":0,"updates_patch":0,"pinned":0,"ignored":0,"errors":0,"warnings":0}}
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
            .env("UPD_VERSION", "v0.6.2")
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
            .env("UPD_MR_TITLE", "chore(deps): test update")
            .env("UPD_GIT_NAME", "upd test")
            .env("UPD_GIT_EMAIL", "upd-test@example.com")
            .env("UPD_AUTO_MERGE", auto_merge.to_string())
            .env("UPD_EXECUTABLE", &self.updater)
            .env("FAKE_UPD_CHANGE", change.to_string())
            .env("FAKE_UPD_CONTENT", content)
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
