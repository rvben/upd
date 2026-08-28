//! Executable contract tests for the reusable GitHub dependency workflow.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const WORKFLOW: &str = include_str!("../.github/workflows/dependency-health.yml");
const RUST_WORKFLOW: &str = include_str!("../.github/workflows/dependencies.yml");
const ACTIONS_WORKFLOW: &str = include_str!("../.github/workflows/upd.yml");
const GITLAB_TEMPLATE: &str = include_str!("../ci/gitlab-dependency-update.yml");
const BRANCH: &str = "automation/upd-github-actions";

fn workflow_script(name: &str) -> String {
    let heading = format!("      - name: {name}\n");
    let step = WORKFLOW
        .split_once(&heading)
        .unwrap_or_else(|| panic!("workflow has the {name} step"))
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

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
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
        write_executable(
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
        );

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
        let report = if changed {
            r#"{"files":[{"path":"dependency.txt","updates":[{"package":"example","current":"1.0.0","latest":"1.1.0","bump":"minor"}]}],"summary":{"updates_total":1,"files_with_changes":1,"held_back":0,"capped":0,"skipped":0,"warnings":0}}"#
        } else {
            r#"{"files":[],"summary":{"updates_total":0,"files_with_changes":0,"held_back":0,"capped":0,"skipped":0,"warnings":0}}"#
        };
        self.run_publish_with_report(changed, content, auto_merge, report);
    }

    fn run_publish_with_report(
        &self,
        changed: bool,
        content: &str,
        auto_merge: bool,
        report_json: &str,
    ) {
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
        fs::write(&report, report_json).unwrap();
        let presentation = self.runner_temp.join("upd-presentation.json");
        let output_file = self.runner_temp.join("github-output");
        let summary_file = self.runner_temp.join("github-summary");
        fs::write(&output_file, "").unwrap();
        fs::write(&summary_file, "").unwrap();

        let path = format!(
            "{}:{}",
            self.fake_bin.display(),
            std::env::var("PATH").unwrap()
        );
        let presentation_output = Command::new("bash")
            .arg("-c")
            .arg(workflow_script(
                "Build provider-neutral review presentation",
            ))
            .current_dir(&self.checkout)
            .env("AUTO_MERGE", auto_merge.to_string())
            .env("CHANGED", changed.to_string())
            .env("LOCK", "false")
            .env("MAX_BUMP", "minor")
            .env("MIN_AGE", "7d")
            .env("PRESENTATION", &presentation)
            .env("REPORT", &report)
            .env("VALIDATION_CONFIGURED", "true")
            .output()
            .expect("presentation script starts");
        assert!(
            presentation_output.status.success(),
            "presentation failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&presentation_output.stdout),
            String::from_utf8_lossy(&presentation_output.stderr)
        );

        let output = Command::new("bash")
            .arg("-c")
            .arg(workflow_script("Publish rolling pull request"))
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
            .env("MERGE_METHOD", "squash")
            .env("PATH", path)
            .env("PRESENTATION", presentation)
            .env("PR_TITLE", "")
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

    fn presentation(&self) -> serde_json::Value {
        serde_json::from_str(
            &fs::read_to_string(self.runner_temp.join("upd-presentation.json")).unwrap(),
        )
        .unwrap()
    }

    fn body(&self) -> String {
        fs::read_to_string(self.runner_temp.join("upd-pr-description.md")).unwrap()
    }
}

#[test]
fn default_install_resolves_and_verifies_the_canonical_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let fake_bin = temp.path().join("bin");
    let runner_temp = temp.path().join("runner");
    let github_path = temp.path().join("github-path");
    let log = temp.path().join("curl-log");
    fs::create_dir(&fake_bin).unwrap();
    fs::create_dir(&runner_temp).unwrap();
    fs::write(&github_path, "").unwrap();

    write_executable(
        &fake_bin.join("curl"),
        r#"#!/usr/bin/env bash
set -euo pipefail
output=
url=
while (($#)); do
  case "$1" in
    --output) output="$2"; shift 2 ;;
    http*) url="$1"; shift ;;
    *) shift ;;
  esac
done
printf '%s\n' "$url" >> "$FAKE_CURL_LOG"
printf '{}\n' > "$output"
"#,
    );
    write_executable(
        &fake_bin.join("jq"),
        r#"#!/usr/bin/env bash
set -euo pipefail
query="${@: -2:1}"
case "$query" in
  .schema) printf '%s\n' 1 ;;
  .version) printf '%s\n' v9.8.7 ;;
  *'.name') printf '%s\n' upd-v9.8.7-x86_64-unknown-linux-gnu.tar.gz ;;
  *'.sha256') printf '%064d\n' 0 ;;
  *) echo "unexpected jq query: $query" >&2; exit 2 ;;
esac
"#,
    );
    write_executable(
        &fake_bin.join("sha256sum"),
        "#!/usr/bin/env bash\nprintf '%064d  %s\\n' 0 \"$1\"\n",
    );
    write_executable(
        &fake_bin.join("tar"),
        r#"#!/usr/bin/env bash
set -euo pipefail
directory=
while (($#)); do
  if [[ "$1" == --directory ]]; then directory="$2"; shift 2; else shift; fi
done
printf '%s\n' '#!/usr/bin/env bash' 'echo "upd 9.8.7"' > "$directory/upd"
chmod 0755 "$directory/upd"
"#,
    );

    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let run_install = |version: &str| {
        Command::new("bash")
            .arg("-c")
            .arg(workflow_script("Install verified upd release"))
            .env("FAKE_CURL_LOG", &log)
            .env("GITHUB_PATH", &github_path)
            .env("PATH", &path)
            .env("REQUESTED_SHA256", "")
            .env("REQUESTED_TARGET", "x86_64-unknown-linux-gnu")
            .env("REQUESTED_VERSION", version)
            .env("RUNNER_TEMP", &runner_temp)
            .output()
            .unwrap()
    };

    let output = run_install("");
    assert!(
        output.status.success(),
        "install failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = fs::read_to_string(&log).unwrap();
    assert!(requests.contains("/rvben/upd/main/release-pins.json"));
    assert!(
        requests.contains("/releases/download/v9.8.7/upd-v9.8.7-x86_64-unknown-linux-gnu.tar.gz")
    );
    assert!(
        fs::read_to_string(&github_path)
            .unwrap()
            .contains("runner/upd-bin")
    );

    let rejected = run_install("v9.8.6");
    assert_eq!(rejected.status.code(), Some(4));
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("Set upd-sha256 when selecting v9.8.6")
    );
}

#[test]
fn workflow_creates_a_single_commit_rolling_pull_request() {
    let fixture = Fixture::new();
    fixture.run_publish(true, "new", false);

    assert_eq!(fixture.branch_file().as_deref(), Some("new\n"));
    assert_eq!(fixture.branch_commit_count(), 1);
    assert!(fixture.log().contains("pr create --base main"));
    assert!(
        fixture
            .log()
            .contains("--title chore(deps): update example to 1.1.0")
    );
    assert_eq!(fixture.presentation()["schema"], 1);
    assert_eq!(fixture.presentation()["state"], "ready");
    let body = fixture.body();
    assert!(body.contains("**Ready for review.**"));
    assert!(body.contains("<code>example</code>"));
    assert!(body.contains("<code>1.0.0</code>"));
    assert!(body.contains("<code>1.1.0</code>"));
    assert!(body.contains("Freshness: <code>7d</code>"));
    assert!(body.contains("maximum bump: <code>minor</code>"));
    assert!(body.len() <= 32 * 1024);
}

#[test]
fn github_presentation_explains_policy_holds_and_escapes_untrusted_text() {
    let fixture = Fixture::new();
    let report = r#"{
      "files": [{
        "path": "dependency.txt",
        "updates": [{
          "package": "bad|pkg</code>", "current": "1.0.0`", "latest": "1.1.0",
          "bump": "minor", "status": "applied"
        }],
        "annotations": [{"package":"annotated","version":"2.0.0"}],
        "held_back": [{"package":"fresh_pkg","current":"1.0.0","chosen":"1.0.1","skipped_latest":"1.1.0"}],
        "skipped_by_cooldown": [],
        "capped": [{"package":"major_pkg","current":"1.0.0","available":"2.0.0"}],
        "skipped": [{
          "package":"blocked<script>", "current":"3.0.0", "status":"blocked",
          "reason":"missing-version-comment",
          "message":"Add *trusted* metadata | before updating \u202ethis pin"
        }]
      }],
      "summary": {"updates_total":1,"files_with_changes":1,"warnings":2,"not_examined":1}
    }"#;
    fixture.run_publish_with_report(true, "new", false, report);

    let presentation = fixture.presentation();
    assert_eq!(presentation["title"], "chore(deps): update dependency");
    assert_eq!(presentation["counts"]["policy_holds"], 2);
    assert_eq!(presentation["counts"]["blocked"], 1);
    assert_eq!(presentation["counts"]["annotations"], 1);
    let body = fixture.body();
    assert!(body.contains("**Ready for review, with follow-up.**"));
    assert!(body.contains("Held back by policy (2)"));
    assert!(body.contains("### Needs attention"));
    assert!(body.contains("bad&#124;pkg&lt;/code&gt;"));
    assert!(body.contains("blocked&lt;script&gt;"));
    assert!(body.contains("&#42;trusted&#42;"));
    assert!(!body.contains("<script>"));
    assert!(!body.contains("*trusted*"));
    assert!(!body.contains('\u{202e}'));
    assert!(body.len() <= 32 * 1024);
}

#[test]
fn github_presentation_stays_within_the_review_budget_for_large_updates() {
    let fixture = Fixture::new();
    let updates = (0..80)
        .map(|index| {
            serde_json::json!({
                "package": format!("package-{index}-{}", "x".repeat(120)),
                "current": "1.0.0",
                "latest": "1.1.0",
                "bump": "minor"
            })
        })
        .collect::<Vec<_>>();
    let held = (0..40)
        .map(|index| {
            serde_json::json!({
                "package": format!("fresh-{index}-{}", "y".repeat(120)),
                "current": "1.0.0",
                "chosen": "1.0.1",
                "skipped_latest": "1.1.0"
            })
        })
        .collect::<Vec<_>>();
    let blocked = (0..30)
        .map(|index| {
            serde_json::json!({
                "package": format!("blocked-{index}"),
                "current": "1.0.0",
                "status": "blocked",
                "reason": "fixture",
                "message": "z".repeat(400)
            })
        })
        .collect::<Vec<_>>();
    let report = serde_json::json!({
        "files": [{
            "path": "dependency.txt",
            "updates": updates,
            "held_back": held,
            "skipped": blocked
        }],
        "summary": {"updates_total":80,"files_with_changes":1,"warnings":0}
    });
    fixture.run_publish_with_report(true, "new", false, &report.to_string());

    let body = fixture.body();
    assert!(body.len() <= 32 * 1024, "body was {} bytes", body.len());
    assert!(
        fixture
            .log()
            .contains("--title chore(deps): update 80 dependencies")
    );
}

#[test]
fn normal_update_presentation_contract_is_shared_across_providers() {
    for source in [WORKFLOW, GITLAB_TEMPLATE] {
        for field in [
            "schema: 1",
            "state:",
            "updates:",
            "updates_major:",
            "updates_minor:",
            "updates_patch:",
            "annotations:",
            "policy_holds:",
            "blocked:",
            "changed_paths:",
            "counts:",
            "policy:",
            "validation:",
            "auto_merge_requested:",
        ] {
            assert!(source.contains(field), "missing presentation field {field}");
        }
        assert!(source.contains("\\($title_prefix): update \\(.counts.updates) dependencies"));
        assert!(source.contains("wc -c"));
        assert!(source.contains("32768"));
    }
    assert!(WORKFLOW.contains("> [!IMPORTANT]"));
    assert!(!GITLAB_TEMPLATE.contains("> [!IMPORTANT]"));
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
    assert_eq!(fixture.presentation()["state"], "clean");
}

#[test]
fn workflow_defaults_are_reproducible_and_safe() {
    assert!(
        WORKFLOW.contains("https://raw.githubusercontent.com/rvben/upd/main/release-pins.json")
    );
    assert!(WORKFLOW.contains(".assets[$target].name"));
    assert!(WORKFLOW.contains(".assets[$target].sha256"));
    assert!(WORKFLOW.contains("manifest_asset\" != \"$expected_asset"));
    assert!(WORKFLOW.contains("--retry 3 --retry-all-errors"));
    assert!(WORKFLOW.contains("actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"));
    assert!(WORKFLOW.contains("actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a"));
    assert!(WORKFLOW.contains("--force-with-lease="));
    assert!(!WORKFLOW.contains("default: latest"));
    assert!(!WORKFLOW.contains("default: v0."));
    assert!(!WORKFLOW.contains("git push --force "));
}

#[test]
fn repository_dependency_jobs_share_the_hardened_workflow() {
    assert!(RUST_WORKFLOW.contains("uses: ./.github/workflows/dependency-health.yml"));
    assert!(RUST_WORKFLOW.contains("langs: rust"));
    assert!(RUST_WORKFLOW.contains("lock: true"));
    assert!(RUST_WORKFLOW.contains("branch: deps/upd"));
    assert!(!RUST_WORKFLOW.contains("git push"));
    assert!(!RUST_WORKFLOW.contains("upd-version:"));

    assert!(ACTIONS_WORKFLOW.contains("uses: ./.github/workflows/dependency-health.yml"));
    assert!(!ACTIONS_WORKFLOW.contains("upd-version:"));
}
