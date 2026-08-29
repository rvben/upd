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
        self.run_publish_with_report_and_validation(
            changed,
            content,
            auto_merge,
            report_json,
            true,
        );
    }

    fn run_publish_with_report_and_validation(
        &self,
        changed: bool,
        content: &str,
        auto_merge: bool,
        report_json: &str,
        validation_configured: bool,
    ) {
        self.run_publish_with_report_validation_and_credential(
            changed,
            content,
            auto_merge,
            report_json,
            validation_configured,
            true,
        );
    }

    fn run_publish_with_report_validation_and_credential(
        &self,
        changed: bool,
        content: &str,
        auto_merge: bool,
        report_json: &str,
        validation_configured: bool,
        has_publishing_token: bool,
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
            .env("VALIDATION_CONFIGURED", validation_configured.to_string())
            .output()
            .expect("presentation script starts");
        assert!(
            presentation_output.status.success(),
            "presentation failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&presentation_output.stdout),
            String::from_utf8_lossy(&presentation_output.stderr)
        );

        if changed {
            git(
                &self.checkout,
                &["commit", "-m", "chore(deps): test update"],
            );
        }

        let output = Command::new("bash")
            .arg("-c")
            .arg(workflow_script("Publish rolling pull request"))
            .current_dir(&self.checkout)
            .env("AUTO_MERGE", auto_merge.to_string())
            .env("BASE_BRANCH", "main")
            .env("BRANCH", BRANCH)
            .env("CHANGED", changed.to_string())
            .env("EXPECTED_REMOTE_SHA", expected_remote_sha)
            .env("GH_STATE_DIR", &self.state)
            .env("GITHUB_OUTPUT", output_file)
            .env("GITHUB_STEP_SUMMARY", summary_file)
            .env("GH_TOKEN", "test-publication-token")
            .env("HAS_PUBLISHING_TOKEN", has_publishing_token.to_string())
            .env("MERGE_METHOD", "squash")
            .env("PATH", path)
            .env("PRESENTATION", presentation)
            .env("PR_TITLE", "")
            .env("RUNNER_TEMP", &self.runner_temp)
            .output()
            .expect("publish script starts");
        if has_publishing_token {
            assert!(
                output.status.success(),
                "publish failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        } else {
            assert_eq!(
                output.status.code(),
                Some(4),
                "credential-less stale cleanup did not fail safely\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
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
            .contains("--title chore(deps): refresh example to 1.1.0")
    );
    assert_eq!(fixture.presentation()["schema"], 1);
    assert_eq!(fixture.presentation()["state"], "ready");
    let body = fixture.body();
    assert!(body.contains("**A tidy upgrade, already prepared.**"));
    assert!(body.contains("84109eaf36c739dc11af0452c6218abb7e47a8e3/assets/logo-wide.svg"));
    assert!(body.contains("**1 moved forward** · **1 worth a look**"));
    assert!(body.contains("### Worth a look"));
    assert!(body.contains("### Why this is a comfortable review"));
    assert!(body.contains("<summary><strong>Proof and provenance</strong></summary>"));
    assert!(body.contains("<code>example</code>"));
    assert!(body.contains("<code>1.0.0</code>"));
    assert!(body.contains("<code>1.1.0</code>"));
    assert!(body.contains("Freshness <code>7d</code>"));
    assert!(body.contains("maximum bump <code>minor</code>"));
    assert!(body.len() <= 32 * 1024);
}

#[test]
fn github_presentation_keeps_unvalidated_patch_updates_truthful() {
    let fixture = Fixture::new();
    let report = r#"{
      "files": [{"path":"dependency.txt","updates":[{
        "package":"example","current":"1.0.0","latest":"1.0.1","bump":"patch"
      }]}],
      "summary":{"updates_total":1,"files_with_changes":1,"warnings":0}
    }"#;
    fixture.run_publish_with_report_and_validation(true, "new", false, report, false);

    let body = fixture.body();
    assert!(body.contains("### What changed"));
    assert!(!body.contains("### Worth a look"));
    assert!(!body.contains("Quiet patch updates"));
    assert!(body.contains("### What upd verified"));
    assert!(body.contains("No repository-specific command was configured"));
    assert!(!body.contains("### Why this is a comfortable review"));
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
    assert_eq!(presentation["title"], "chore(deps): refresh dependency");
    assert_eq!(presentation["counts"]["policy_holds"], 2);
    assert_eq!(presentation["counts"]["blocked"], 1);
    assert_eq!(presentation["counts"]["annotations"], 1);
    let body = fixture.body();
    assert!(body.contains("**A careful upgrade, with follow-up.**"));
    assert!(body.contains("Saved for a deliberate upgrade (2)"));
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
                "package": format!("package-{index}-{}", "x".repeat(1200)),
                "current": format!("1.0.0-{}", "c".repeat(1200)),
                "latest": format!("1.1.0-{}", "l".repeat(1200)),
                "bump": if index < 40 { "minor" } else { "patch" }
            })
        })
        .collect::<Vec<_>>();
    let held = (0..40)
        .map(|index| {
            serde_json::json!({
                "package": format!("fresh-{index}-{}", "y".repeat(1200)),
                "current": format!("1.0.0-{}", "c".repeat(1200)),
                "chosen": format!("1.0.1-{}", "s".repeat(1200)),
                "skipped_latest": format!("1.1.0-{}", "a".repeat(1200))
            })
        })
        .collect::<Vec<_>>();
    let blocked = (0..30)
        .map(|index| {
            serde_json::json!({
                "package": format!("blocked-{index}"),
                "current": format!("1.0.0-{}", "c".repeat(1200)),
                "status": "blocked",
                "reason": "fixture",
                "message": "z".repeat(1200)
            })
        })
        .collect::<Vec<_>>();
    let report = serde_json::json!({
        "files": [{
            "path": format!("dependency-{}.txt", "p".repeat(1200)),
            "updates": updates,
            "held_back": held,
            "skipped": blocked
        }],
        "summary": {"updates_total":80,"files_with_changes":1,"warnings":0}
    });
    fixture.run_publish_with_report(true, "new", false, &report.to_string());

    let body = fixture.body();
    assert!(body.len() <= 32 * 1024, "body was {} bytes", body.len());
    assert!(body.contains("**A careful upgrade, with follow-up.**"));
    assert!(
        body.contains("Saved for a deliberate upgrade: 40"),
        "unexpected large-update body:\n{body}"
    );
    assert!(body.contains("Needs attention: 30"));
    assert!(body.contains("Major-version jumps: 0"));
    assert!(body.contains("repository validation and proposal integrity passed"));
    assert!(!body.contains("**A tidy upgrade, already prepared.**"));
    assert!(
        fixture
            .log()
            .contains("--title chore(deps): refresh 80 dependencies")
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
            "updates_review_worthy:",
            "updates_quiet:",
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
        assert!(source.contains("\\($title_prefix): refresh \\(.counts.updates) dependencies"));
        assert!(source.contains("wc -c"));
        assert!(source.contains("32768"));
    }
    assert!(WORKFLOW.contains("> [!IMPORTANT]"));
    assert!(!GITLAB_TEMPLATE.contains("> [!IMPORTANT]"));
}

#[test]
fn github_presentation_prioritizes_review_worthy_updates() {
    let fixture = Fixture::new();
    let report = r#"{
      "files": [{
        "path": "dependency.txt",
        "updates": [
          {"package":"quiet-one","current":"1.0.0","latest":"1.0.1","bump":"patch"},
          {"package":"review-one","current":"1.0.0","latest":"1.1.0","bump":"minor"},
          {"package":"quiet-two","current":"2.0.0","latest":"2.0.1","bump":"patch"},
          {"package":"review-two","current":"3.0.0","latest":"4.0.0","bump":"major"}
        ],
        "capped": [{"package":"later","current":"1.0.0","available":"2.0.0"}]
      }],
      "summary": {"updates_total":4,"files_with_changes":1,"warnings":0}
    }"#;
    fixture.run_publish_with_report(true, "new", false, report);

    let presentation = fixture.presentation();
    assert_eq!(presentation["counts"]["updates_review_worthy"], 2);
    assert_eq!(presentation["counts"]["updates_quiet"], 2);
    let body = fixture.body();
    assert!(body.contains(
        "**4 moved forward** · **2 worth a look** · **2 quiet patches** · **1 saved for later**"
    ));
    let worth = body.find("### Worth a look").unwrap();
    let review_one = body.find("<code>review-one</code>").unwrap();
    let quiet = body.find("Quiet patch updates (2)").unwrap();
    let quiet_one = body.find("<code>quiet-one</code>").unwrap();
    assert!(worth < review_one && review_one < quiet && quiet < quiet_one);
    assert!(body.contains("Includes 1 major-version jump."));
}

#[test]
fn workflow_requires_a_check_triggering_publishing_credential() {
    let temp = tempfile::tempdir().unwrap();
    let summary = temp.path().join("summary");
    let output = temp.path().join("output");
    let script = workflow_script("Require a check-triggering publishing credential");

    let missing = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .env("BASE_SHA", "unused")
        .env("CHANGED", "false")
        .env("HAS_BROKER", "false")
        .env("HAS_PAT", "false")
        .env("GITHUB_OUTPUT", &output)
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("hosted token broker"));

    let configured = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .env("BASE_SHA", "unused")
        .env("CHANGED", "false")
        .env("HAS_BROKER", "true")
        .env("HAS_PAT", "false")
        .env("GITHUB_OUTPUT", &output)
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .unwrap();
    assert!(configured.status.success());
    assert!(
        fs::read_to_string(&output)
            .unwrap()
            .contains("use-broker=true")
    );

    let pat_output = temp.path().join("pat-output");
    let pat_configured = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .env("BASE_SHA", "unused")
        .env("CHANGED", "false")
        .env("HAS_BROKER", "true")
        .env("HAS_PAT", "true")
        .env("GITHUB_OUTPUT", &pat_output)
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .unwrap();
    assert!(pat_configured.status.success());
    assert!(
        fs::read_to_string(pat_output)
            .unwrap()
            .contains("use-broker=false")
    );
}

#[test]
fn workflow_requests_and_masks_a_scoped_broker_token() {
    let temp = tempfile::tempdir().unwrap();
    let fake_bin = temp.path().join("bin");
    let runner_temp = temp.path().join("runner");
    fs::create_dir(&fake_bin).unwrap();
    fs::create_dir(&runner_temp).unwrap();
    write_executable(
        &fake_bin.join("curl"),
        r#"#!/usr/bin/env bash
set -euo pipefail
output=""
request=""
url=""
while [[ "$#" -gt 0 ]]; do
  case "$1" in
    --output) output="$2"; shift 2 ;;
    --data-binary) request="${2#@}"; shift 2 ;;
    --header|--data-urlencode|--request|--write-out) shift 2 ;;
    --silent|--show-error|--fail|--get) shift ;;
    *) url="$1"; shift ;;
  esac
done
if [[ "$url" == https://oidc.example.test/* ]]; then
  printf '%s\n' '{"value":"signed-oidc-secret"}' > "$output"
else
  cp "$request" "$BROKER_CAPTURE"
  printf '%s\n' '{"token":"installation-secret","expires_at":"2026-08-29T12:00:00Z"}' > "$output"
  printf '200'
fi
"#,
    );
    let output = temp.path().join("github-output");
    let capture = temp.path().join("broker-request");
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let result = Command::new("bash")
        .arg("-c")
        .arg(workflow_script("Request publication token"))
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "oidc-request-secret")
        .env(
            "ACTIONS_ID_TOKEN_REQUEST_URL",
            "https://oidc.example.test/token?request=1",
        )
        .env("BROKER_AUDIENCE", "upd-token-broker")
        .env("BROKER_CAPTURE", &capture)
        .env("BROKER_URL", "https://broker.example.test/v1/token")
        .env("GITHUB_OUTPUT", &output)
        .env("PATH", path)
        .env("RUNNER_TEMP", &runner_temp)
        .env("WORKFLOW_FILES_CHANGED", "true")
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(stdout.contains("::add-mask::signed-oidc-secret"));
    assert!(stdout.contains("::add-mask::installation-secret"));
    assert_eq!(
        fs::read_to_string(output).unwrap(),
        "token=installation-secret\n"
    );
    let request: serde_json::Value = serde_json::from_slice(&fs::read(capture).unwrap()).unwrap();
    assert_eq!(request["oidc_token"], "signed-oidc-secret");
    assert_eq!(request["permissions"]["contents"], "write");
    assert_eq!(request["permissions"]["pull_requests"], "write");
    assert_eq!(request["permissions"]["workflows"], "write");
}

#[test]
fn workflow_change_detection_limits_workflow_permission_to_workflow_proposals() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    git(&repo, &["config", "user.name", "Test User"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    fs::write(repo.join("dependency.txt"), "old\n").unwrap();
    git(&repo, &["add", "dependency.txt"]);
    git(&repo, &["commit", "-m", "test: base"]);
    let base = String::from_utf8(git(&repo, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    fs::write(repo.join("dependency.txt"), "new\n").unwrap();
    git(&repo, &["add", "dependency.txt"]);
    git(&repo, &["commit", "-m", "test: ordinary proposal"]);
    let ordinary_output = temp.path().join("ordinary-output");
    let ordinary = Command::new("bash")
        .arg("-c")
        .arg(workflow_script(
            "Require a check-triggering publishing credential",
        ))
        .current_dir(&repo)
        .env("BASE_SHA", &base)
        .env("CHANGED", "true")
        .env("HAS_BROKER", "true")
        .env("HAS_PAT", "false")
        .env("GITHUB_OUTPUT", &ordinary_output)
        .env("GITHUB_STEP_SUMMARY", temp.path().join("ordinary-summary"))
        .output()
        .unwrap();
    assert!(ordinary.status.success());
    assert!(
        fs::read_to_string(ordinary_output)
            .unwrap()
            .contains("workflow-files-changed=false")
    );
    let workflow_base = String::from_utf8(git(&repo, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    fs::create_dir_all(repo.join(".github/workflows")).unwrap();
    fs::write(repo.join(".github/workflows/check.yml"), "name: Check\n").unwrap();
    git(&repo, &["add", ".github/workflows/check.yml"]);
    git(&repo, &["commit", "-m", "test: workflow proposal"]);

    let output = temp.path().join("output");
    let summary = temp.path().join("summary");
    let result = Command::new("bash")
        .arg("-c")
        .arg(workflow_script(
            "Require a check-triggering publishing credential",
        ))
        .current_dir(&repo)
        .env("BASE_SHA", workflow_base)
        .env("CHANGED", "true")
        .env("HAS_BROKER", "true")
        .env("HAS_PAT", "false")
        .env("GITHUB_OUTPUT", &output)
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(
        fs::read_to_string(output)
            .unwrap()
            .contains("workflow-files-changed=true")
    );
}

#[test]
fn validated_proposal_crosses_the_job_boundary_as_one_commit() {
    let fixture = Fixture::new();
    let base = String::from_utf8(git(&fixture.checkout, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    fs::write(fixture.checkout.join("dependency.txt"), "new\n").unwrap();
    git(&fixture.checkout, &["add", "dependency.txt"]);

    let package = Command::new("bash")
        .arg("-c")
        .arg(workflow_script("Package validated proposal"))
        .current_dir(&fixture.checkout)
        .env("BASE_SHA", &base)
        .env("COMMIT_MESSAGE", "chore(deps): test boundary")
        .env("RUNNER_TEMP", &fixture.runner_temp)
        .output()
        .unwrap();
    assert!(
        package.status.success(),
        "package failed: {}",
        String::from_utf8_lossy(&package.stderr)
    );

    git(&fixture.checkout, &["reset", "--hard", &base]);
    let restore = Command::new("bash")
        .arg("-c")
        .arg(workflow_script("Restore validated proposal"))
        .current_dir(&fixture.checkout)
        .env("BASE_SHA", &base)
        .env("BRANCH", BRANCH)
        .env("RUNNER_TEMP", &fixture.runner_temp)
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&restore.stderr)
    );
    assert_eq!(
        fs::read_to_string(fixture.checkout.join("dependency.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        String::from_utf8(git(&fixture.checkout, &["rev-parse", "HEAD^"]).stdout)
            .unwrap()
            .trim(),
        base
    );
}

#[test]
fn isolated_publisher_rejects_a_merge_commit_bundle() {
    let fixture = Fixture::new();
    let base = String::from_utf8(git(&fixture.checkout, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    git(
        &fixture.checkout,
        &["switch", "--create", "malicious-side", &base],
    );
    fs::write(
        fixture.checkout.join("injected.txt"),
        "unexpected history\n",
    )
    .unwrap();
    git(&fixture.checkout, &["add", "injected.txt"]);
    git(&fixture.checkout, &["commit", "-m", "test: second parent"]);
    git(&fixture.checkout, &["switch", "--detach", &base]);
    git(
        &fixture.checkout,
        &[
            "merge",
            "--no-ff",
            "malicious-side",
            "-m",
            "test: merge proposal",
        ],
    );

    let bundle = fixture.runner_temp.join("upd-proposal.bundle");
    let exclusion = format!("^{base}");
    git(
        &fixture.checkout,
        &[
            "bundle",
            "create",
            bundle.to_str().unwrap(),
            "HEAD",
            &exclusion,
        ],
    );
    let checksum = run(Command::new("sha256sum")
        .current_dir(&fixture.runner_temp)
        .arg("upd-proposal.bundle"));
    fs::write(
        fixture.runner_temp.join("upd-proposal.bundle.sha256"),
        checksum.stdout,
    )
    .unwrap();
    git(&fixture.checkout, &["reset", "--hard", &base]);

    let restore = Command::new("bash")
        .arg("-c")
        .arg(workflow_script("Restore validated proposal"))
        .current_dir(&fixture.checkout)
        .env("BASE_SHA", &base)
        .env("BRANCH", BRANCH)
        .env("RUNNER_TEMP", &fixture.runner_temp)
        .output()
        .unwrap();
    assert_eq!(restore.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&restore.stderr)
            .contains("not a single commit on the prepared base")
    );
    assert!(!fixture.checkout.join("injected.txt").exists());
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
fn credential_less_cleanup_preserves_the_existing_pr_and_branch() {
    let fixture = Fixture::new();
    fixture.run_publish(true, "new", false);
    let clean_report = r#"{"files":[],"summary":{"updates_total":0,"files_with_changes":0,"held_back":0,"capped":0,"skipped":0,"warnings":0}}"#;
    fixture.run_publish_with_report_validation_and_credential(
        false,
        "unused",
        false,
        clean_report,
        true,
        false,
    );

    assert_eq!(fixture.branch_file().as_deref(), Some("new\n"));
    assert!(!fixture.log().contains("pr close 7"));
}

#[test]
fn workflow_defaults_are_reproducible_and_safe() {
    assert!(!WORKFLOW.contains("  workflow_dispatch:"));
    assert!(
        WORKFLOW.contains("https://raw.githubusercontent.com/rvben/upd/main/release-pins.json")
    );
    assert!(WORKFLOW.contains(".assets[$target].name"));
    assert!(WORKFLOW.contains(".assets[$target].sha256"));
    assert!(WORKFLOW.contains("manifest_asset\" != \"$expected_asset"));
    assert!(WORKFLOW.contains("--retry 3 --retry-all-errors"));
    assert!(WORKFLOW.contains("actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"));
    assert!(WORKFLOW.contains("actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a"));
    assert!(
        WORKFLOW.contains("actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c")
    );
    assert!(WORKFLOW.contains("--force-with-lease="));
    assert!(WORKFLOW.contains("persist-credentials: false"));
    assert!(WORKFLOW.contains("GIT_ASKPASS=\"$askpass\""));
    assert!(!WORKFLOW.contains("http.extraheader"));
    assert!(!WORKFLOW.contains("AUTHORIZATION: basic"));
    assert!(WORKFLOW.contains("  publish:\n    needs: update"));
    assert!(WORKFLOW.contains("never executes checked-out repository code"));
    assert!(WORKFLOW.contains("GITHUB_TOKEN: ${{ github.token }}"));
    assert!(WORKFLOW.contains(r#"{workflows: "write"}"#));
    assert!(WORKFLOW.contains("ACTIONS_ID_TOKEN_REQUEST_TOKEN"));
    assert!(!WORKFLOW.contains("app-private-key"));
    assert!(!WORKFLOW.contains("create-github-app-token"));
    let validation = WORKFLOW
        .find("Verify validation did not mutate the proposal")
        .unwrap();
    let publication_token = WORKFLOW.find("Request publication token").unwrap();
    assert!(validation < publication_token);
    assert!(!WORKFLOW.contains("default: latest"));
    assert!(!WORKFLOW.contains("default: v0."));
    assert!(!WORKFLOW.contains("git push --force "));
    for caller in [RUST_WORKFLOW, ACTIONS_WORKFLOW] {
        assert!(caller.contains("contents: read"));
        assert!(caller.contains("pull-requests: read"));
        assert!(caller.contains("id-token: write"));
        assert!(!caller.contains("contents: write"));
        assert!(!caller.contains("UPD_APP_PRIVATE_KEY"));
    }
}

#[test]
fn repository_dependency_jobs_share_the_hardened_workflow() {
    assert!(RUST_WORKFLOW.contains("uses: ./.github/workflows/dependency-health.yml"));
    assert!(RUST_WORKFLOW.contains("langs: rust"));
    assert!(RUST_WORKFLOW.contains("lock: true"));
    assert!(RUST_WORKFLOW.contains("branch: deps/upd"));
    assert!(RUST_WORKFLOW.contains("broker-url: ${{ vars.UPD_BROKER_URL }}"));
    assert!(!RUST_WORKFLOW.contains("git push"));
    assert!(!RUST_WORKFLOW.contains("upd-version:"));

    assert!(ACTIONS_WORKFLOW.contains("uses: ./.github/workflows/dependency-health.yml"));
    assert!(ACTIONS_WORKFLOW.contains("broker-url: ${{ vars.UPD_BROKER_URL }}"));
    assert!(!ACTIONS_WORKFLOW.contains("upd-version:"));
}
