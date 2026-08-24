//! Executable contract tests for the reusable GitHub dependency workflow.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const WORKFLOW: &str = include_str!("../.github/workflows/dependency-health.yml");
const RELEASE_PINS: &str = include_str!("../release-pins.json");
const BRANCH: &str = "automation/upd-github-actions";

fn release_pin(path: &[&str]) -> String {
    let manifest: serde_json::Value = serde_json::from_str(RELEASE_PINS).unwrap();
    let mut value = &manifest;
    for key in path {
        value = &value[*key];
    }
    value.as_str().unwrap().to_string()
}

fn publish_script() -> String {
    let step = WORKFLOW
        .split_once("      - name: Publish rolling pull request\n")
        .expect("workflow has the publish step")
        .1;
    let block = step
        .split_once("        run: |\n")
        .expect("publish step has a literal script")
        .1;
    block
        .lines()
        .take_while(|line| !line.starts_with("      - name: "))
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                line.strip_prefix("          ")
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
    runner_temp: PathBuf,
    state: PathBuf,
    fake_bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let checkout = temp.path().join("checkout");
        let remote = temp.path().join("remote.git");
        let runner_temp = temp.path().join("runner-temp");
        let state = temp.path().join("gh-state");
        let fake_bin = temp.path().join("bin");
        fs::create_dir(&checkout).unwrap();
        fs::create_dir(&runner_temp).unwrap();
        fs::create_dir(&state).unwrap();
        fs::create_dir(&fake_bin).unwrap();

        git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        git(&checkout, &["init"]);
        git(&checkout, &["config", "user.name", "Test User"]);
        git(&checkout, &["config", "user.email", "test@example.com"]);
        fs::write(checkout.join("dependency.txt"), "old\n").unwrap();
        git(&checkout, &["add", "dependency.txt"]);
        git(&checkout, &["commit", "-m", "test: initial"]);
        git(&checkout, &["branch", "-M", "main"]);
        git(
            &checkout,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&checkout, &["push", "origin", "main"]);

        let gh = fake_bin.join("gh");
        fs::write(
            &gh,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$GH_STATE_DIR/log"
state="$GH_STATE_DIR/pr-state"
auto="$GH_STATE_DIR/auto-merge"
case "${1:-}:${2:-}" in
  pr:list)
    if [[ -f "$state" && "$(cat "$state")" == open ]]; then
      printf '%s\n' '[{"number":7,"url":"https://github.example.test/project/pull/7"}]'
    else
      printf '%s\n' '[]'
    fi
    ;;
  pr:create)
    printf '%s\n' open > "$state"
    printf '%s\n' 'https://github.example.test/project/pull/7'
    ;;
  pr:edit)
    ;;
  pr:close)
    printf '%s\n' closed > "$state"
    ;;
  pr:view)
    if [[ -f "$auto" ]]; then cat "$auto"; else printf '%s\n' false; fi
    ;;
  pr:merge)
    if [[ " $* " == *' --disable-auto '* ]]; then
      printf '%s\n' false > "$auto"
    else
      printf '%s\n' true > "$auto"
    fi
    ;;
  *)
    echo "Unexpected gh invocation: $*" >&2
    exit 2
    ;;
esac
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&gh).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&gh, permissions).unwrap();

        Self {
            _temp: temp,
            checkout,
            remote,
            runner_temp,
            state,
            fake_bin,
        }
    }

    fn run_publish(&self, changed: bool, content: &str, auto_merge: bool) {
        git(
            &self.checkout,
            &[
                "fetch",
                "--no-tags",
                "origin",
                "+refs/heads/main:refs/remotes/origin/main",
            ],
        );
        let branch_ref = format!("refs/heads/{BRANCH}:refs/remotes/origin/{BRANCH}");
        let fetch = Command::new("git")
            .current_dir(&self.checkout)
            .args(["fetch", "--no-tags", "origin", &format!("+{branch_ref}")])
            .output()
            .unwrap();
        let expected_remote_sha = if fetch.status.success() {
            String::from_utf8(
                git(
                    &self.checkout,
                    &["rev-parse", &format!("refs/remotes/origin/{BRANCH}")],
                )
                .stdout,
            )
            .unwrap()
            .trim()
            .to_string()
        } else {
            String::new()
        };
        git(
            &self.checkout,
            &[
                "switch",
                "--force-create",
                BRANCH,
                "refs/remotes/origin/main",
            ],
        );
        if changed {
            fs::write(self.checkout.join("dependency.txt"), format!("{content}\n")).unwrap();
            git(&self.checkout, &["add", "--all"]);
        }

        let report = self.runner_temp.join("upd-report.json");
        fs::write(
            &report,
            if changed {
                r#"{"files":[{"path":"dependency.txt","updates":[{"package":"example","current":"1.0.0","latest":"2.0.0","bump":"major"}]}],"summary":{"updates_total":1,"files_with_changes":1,"held_back":0,"capped":0,"skipped":0,"warnings":0}}"#
            } else {
                r#"{"files":[],"summary":{"updates_total":0,"files_with_changes":0,"held_back":0,"capped":0,"skipped":0,"warnings":0}}"#
            },
        )
        .unwrap();
        let output_file = self.runner_temp.join("github-output");
        let summary_file = self.runner_temp.join("github-summary");
        fs::write(&output_file, "").unwrap();
        fs::write(&summary_file, "").unwrap();

        let path = format!(
            "{}:{}",
            self.fake_bin.display(),
            std::env::var("PATH").unwrap()
        );
        let output = Command::new("bash")
            .arg("-c")
            .arg(publish_script())
            .current_dir(&self.checkout)
            .env("AUTO_MERGE", auto_merge.to_string())
            .env("BASE_BRANCH", "main")
            .env("BRANCH", BRANCH)
            .env("CHANGED", changed.to_string())
            .env("COMMIT_MESSAGE", "chore(deps): test update")
            .env("EXPECTED_REMOTE_SHA", expected_remote_sha)
            .env("GH_STATE_DIR", &self.state)
            .env("GITHUB_OUTPUT", output_file)
            .env("GITHUB_STEP_SUMMARY", summary_file)
            .env("LOCK", "false")
            .env("MERGE_METHOD", "squash")
            .env("PATH", path)
            .env("PR_TITLE", "chore(deps): test update")
            .env("REPORT", report)
            .env("RUNNER_TEMP", &self.runner_temp)
            .output()
            .expect("publish script starts");
        assert!(
            output.status.success(),
            "publish failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn branch_file(&self) -> Option<String> {
        let output = Command::new("git")
            .arg(format!("--git-dir={}", self.remote.display()))
            .args(["show", &format!("refs/heads/{BRANCH}:dependency.txt")])
            .output()
            .unwrap();
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

    fn log(&self) -> String {
        fs::read_to_string(self.state.join("log")).unwrap_or_default()
    }
}

#[test]
fn workflow_creates_a_single_commit_rolling_pull_request() {
    let fixture = Fixture::new();
    fixture.run_publish(true, "new", false);

    assert_eq!(fixture.branch_file().as_deref(), Some("new\n"));
    assert_eq!(fixture.branch_commit_count(), 1);
    assert!(fixture.log().contains("pr create --base main"));
}

#[test]
fn workflow_updates_the_branch_and_enables_sha_bound_automerge() {
    let fixture = Fixture::new();
    fixture.run_publish(true, "first", false);
    fixture.run_publish(true, "second", true);

    assert_eq!(fixture.branch_file().as_deref(), Some("second\n"));
    assert_eq!(fixture.branch_commit_count(), 1);
    let head = String::from_utf8(
        git(
            &fixture.checkout,
            &["rev-parse", &format!("refs/remotes/origin/{BRANCH}")],
        )
        .stdout,
    )
    .unwrap();
    assert!(fixture.log().contains("pr edit 7"));
    assert!(fixture.log().contains(&format!(
        "pr merge 7 --auto --squash --match-head-commit {}",
        head.trim()
    )));
}

#[test]
fn workflow_closes_an_obsolete_pr_and_lease_deletes_its_branch() {
    let fixture = Fixture::new();
    fixture.run_publish(true, "new", false);
    fixture.run_publish(false, "unused", false);

    assert_eq!(fixture.branch_file(), None);
    assert!(fixture.log().contains("pr close 7 --comment"));
}

#[test]
fn workflow_defaults_are_reproducible_and_safe() {
    let version = release_pin(&["version"]);
    let checksum = release_pin(&["assets", "x86_64-unknown-linux-gnu", "sha256"]);
    assert!(WORKFLOW.contains(&format!("default: {version}")));
    assert!(WORKFLOW.contains(&checksum));
    assert!(WORKFLOW.contains("actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"));
    assert!(WORKFLOW.contains("actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a"));
    assert!(WORKFLOW.contains("--force-with-lease="));
    assert!(!WORKFLOW.contains("default: latest"));
    assert!(!WORKFLOW.contains("git push --force "));
}
