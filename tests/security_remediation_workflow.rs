//! Security and lifecycle contracts for the reusable remediation workflow.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const WORKFLOW: &str = include_str!("../.github/workflows/dependency-remediation.yml");
const CALLER: &str = include_str!("../.github/workflows/remediate-dependencies.yml");
const UPDATE_WORKFLOW: &str = include_str!("../.github/workflows/dependency-health.yml");
const BRANCH: &str = "security/upd";

fn workflow_script(name: &str) -> String {
    let heading = format!("      - name: {name}\n");
    let step = WORKFLOW
        .split_once(&heading)
        .unwrap_or_else(|| panic!("workflow has the {name} step"))
        .1;
    let block = step
        .split_once("        run: |\n")
        .expect("step has a literal script")
        .1;
    block
        .lines()
        .take_while(|line| line.is_empty() || line.starts_with("          "))
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
    body: PathBuf,
    state: PathBuf,
    fake_bin: PathBuf,
    output: PathBuf,
    summary: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let checkout = temp.path().join("checkout");
        let remote = temp.path().join("remote.git");
        let state = temp.path().join("gh-state");
        let fake_bin = temp.path().join("bin");
        let body = temp.path().join("body.md");
        let output = temp.path().join("github-output");
        let summary = temp.path().join("github-summary");
        fs::create_dir(&checkout).unwrap();
        fs::create_dir(&state).unwrap();
        fs::create_dir(&fake_bin).unwrap();
        fs::write(&body, "Validated security remediation.\n").unwrap();
        fs::write(&output, "").unwrap();
        fs::write(&summary, "").unwrap();

        git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        git(&checkout, &["init"]);
        git(&checkout, &["config", "user.name", "Test User"]);
        git(&checkout, &["config", "user.email", "test@example.invalid"]);
        fs::write(checkout.join("Cargo.toml"), "version = \"1.0.0\"\n").unwrap();
        fs::write(checkout.join("README.md"), "trusted\n").unwrap();
        git(&checkout, &["add", "Cargo.toml", "README.md"]);
        git(&checkout, &["commit", "-m", "test: initial"]);
        git(&checkout, &["branch", "-M", "main"]);
        git(
            &checkout,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&checkout, &["push", "origin", "main"]);

        write_executable(
            &fake_bin.join("gh"),
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$GH_STATE_DIR/log"
state="$GH_STATE_DIR/pr-state"
case "${1:-}:${2:-}" in
  auth:setup-git) ;;
  pr:list)
    case "$(cat "$state" 2>/dev/null || true)" in
      open) printf '%s\n' '[{"number":7,"url":"https://github.example.test/project/pull/7"}]' ;;
      duplicate) printf '%s\n' '[{"number":7,"url":"https://github.example.test/project/pull/7"},{"number":8,"url":"https://github.example.test/project/pull/8"}]' ;;
      *) printf '%s\n' '[]' ;;
    esac
    ;;
  pr:create) printf '%s\n' open > "$state" ;;
  pr:edit) ;;
  pr:close) printf '%s\n' closed > "$state" ;;
  pr:view) printf '%s\n' false ;;
  pr:merge) ;;
  *) echo "Unexpected gh invocation: $*" >&2; exit 2 ;;
esac
"#,
        );

        Self {
            _temp: temp,
            checkout,
            remote,
            body,
            state,
            fake_bin,
            output,
            summary,
        }
    }

    fn base_sha(&self) -> String {
        String::from_utf8(git(&self.checkout, &["rev-parse", "main"]).stdout)
            .unwrap()
            .trim()
            .to_string()
    }

    fn reset_to_main(&self) {
        git(&self.checkout, &["switch", "--detach", "main"]);
    }

    fn stage_change(&self, version: &str) {
        fs::write(
            self.checkout.join("Cargo.toml"),
            format!("version = \"{version}\"\n"),
        )
        .unwrap();
        git(&self.checkout, &["add", "Cargo.toml"]);
    }

    fn run_publish(&self, disposition: &str, base_sha: &str) -> Output {
        self.run_publish_with_auto_merge(disposition, base_sha, false, "squash")
    }

    fn run_publish_with_auto_merge(
        &self,
        disposition: &str,
        base_sha: &str,
        auto_merge: bool,
        merge_method: &str,
    ) -> Output {
        let path = format!(
            "{}:{}",
            self.fake_bin.display(),
            std::env::var("PATH").unwrap()
        );
        Command::new("bash")
            .arg("-c")
            .arg(workflow_script("Publish the rolling security pull request"))
            .current_dir(&self.checkout)
            .env("BASE_BRANCH", "main")
            .env("BASE_SHA", base_sha)
            .env("BODY_FILE", &self.body)
            .env("BRANCH", BRANCH)
            .env("AUTO_MERGE", auto_merge.to_string())
            .env(
                "COMMIT_MESSAGE",
                "fix(deps): remediate vulnerable dependencies with upd",
            )
            .env("DISPOSITION", disposition)
            .env("GH_STATE_DIR", &self.state)
            .env("GH_TOKEN", "test-token")
            .env("GITHUB_OUTPUT", &self.output)
            .env("GITHUB_REPOSITORY", "owner/project")
            .env("GITHUB_STEP_SUMMARY", &self.summary)
            .env("MERGE_METHOD", merge_method)
            .env("PATH", path)
            .env(
                "PR_TITLE",
                "fix(deps): remediate vulnerable dependencies with upd",
            )
            .output()
            .expect("publish script starts")
    }

    fn branch_file(&self) -> Option<String> {
        let output = Command::new("git")
            .arg(format!("--git-dir={}", self.remote.display()))
            .args(["show", &format!("refs/heads/{BRANCH}:Cargo.toml")])
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
fn reusable_workflow_has_a_provider_neutral_least_privilege_boundary() {
    assert!(WORKFLOW.contains("workflow_call:"));
    assert!(!WORKFLOW.contains("pull_request_target:"));
    assert!(!WORKFLOW.contains("issue_comment:"));
    assert!(!WORKFLOW.contains("github.token"));
    assert!(!WORKFLOW.contains("pull-request-token"));
    assert!(WORKFLOW.contains("persist-credentials: false"));
    assert!(WORKFLOW.contains("id-token: write"));
    assert!(WORKFLOW.contains(
        "UPD_HOSTED_BROKER_URL: https://upd-token-broker-6e5p6y25aa-ez.a.run.app/v1/token"
    ));
    assert!(WORKFLOW.contains("UPD_HOSTED_BROKER_AUDIENCE: upd-token-broker"));
    assert!(!WORKFLOW.contains("inputs.broker-url"));
    assert!(!WORKFLOW.contains("inputs.broker-audience"));
    assert!(WORKFLOW.contains("ACTIONS_ID_TOKEN_REQUEST_TOKEN"));
    assert!(WORKFLOW.contains(r#"contents: "write""#));
    assert!(WORKFLOW.contains(r#"pull_requests: "write""#));
    assert!(!WORKFLOW.contains("app-private-key"));
    assert!(!WORKFLOW.contains("create-github-app-token"));
    assert!(WORKFLOW.contains("gh auth setup-git"));
    assert!(!WORKFLOW.contains("AUTHORIZATION: bearer"));

    let verify = WORKFLOW
        .find("name: Verify and stage the proposal")
        .unwrap();
    let require = WORKFLOW
        .find("name: Validate hosted broker configuration")
        .unwrap();
    let mint = WORKFLOW
        .find("name: Request a least-privilege installation token")
        .unwrap();
    let publish = WORKFLOW
        .find("name: Publish the rolling security pull request")
        .unwrap();
    assert!(verify < require && require < mint && mint < publish);
}

#[test]
fn remediation_requires_structured_results_and_a_fresh_post_fix_audit() {
    assert!(WORKFLOW.contains("audit --fix-audit --apply --lock"));
    assert!(WORKFLOW.contains(".status == \"complete\""));
    assert!(WORKFLOW.contains(".summary.errors // 0"));
    assert!(WORKFLOW.contains(".status == \"rolled_back\""));
    assert!(WORKFLOW.contains(".status == \"pending_relock\""));
    assert!(WORKFLOW.contains("name: Audit the final proposed dependency graph"));
    assert!(WORKFLOW.contains("args=(audit --no-fail --no-cache --format json)"));
    assert!(WORKFLOW.contains("git add --intent-to-add"));
    assert!(WORKFLOW.contains("git diff --binary HEAD"));
    assert!(WORKFLOW.contains("git diff --quiet HEAD"));
    assert!(WORKFLOW.contains("literal_allowed+=(\":(literal)$path\")"));
    assert!(WORKFLOW.contains("before_untracked="));
    assert!(WORKFLOW.contains("partial_changed"));
    assert!(WORKFLOW.contains("residual_no_change"));
    assert!(WORKFLOW.contains("AUTO_MERGE"));
    assert!(WORKFLOW.contains("$DISPOSITION\" == clean_changed"));
    assert!(WORKFLOW.contains("--match-head-commit \"$commit_sha\""));
    assert!(!WORKFLOW.contains("gh pr merge \"$pr_number\" --admin"));
    assert!(!WORKFLOW.contains("upload-sarif"));
}

#[test]
fn repository_caller_is_thin_scoped_and_safe_by_default() {
    assert!(CALLER.contains("uses: ./.github/workflows/dependency-remediation.yml"));
    assert!(CALLER.contains("langs: rust"));
    assert!(CALLER.contains("allowed-paths: Cargo.toml Cargo.lock"));
    assert!(CALLER.contains("validation-command: make check"));
    assert!(CALLER.contains("default: false"));
    assert!(CALLER.contains("UPD_SECURITY_REMEDIATION_ENABLED"));
    assert!(CALLER.contains("id-token: write"));
    assert!(!CALLER.contains("broker-url:"));
    assert!(!CALLER.contains("UPD_BROKER_URL"));
    assert!(!CALLER.contains("UPD_APP_PRIVATE_KEY"));
    assert!(WORKFLOW.contains("group: upd-dependency-writes-${{ github.repository }}"));
    assert!(UPDATE_WORKFLOW.contains("group: upd-dependency-writes-${{ github.repository }}"));
}

#[test]
fn validation_cannot_hide_a_mutation_in_the_git_index() {
    let fixture = Fixture::new();
    fixture.stage_change("1.0.1");
    let output = Command::new("bash")
        .arg("-c")
        .arg(workflow_script("Constrain and validate the proposal"))
        .current_dir(&fixture.checkout)
        .env("ALLOWED_PATHS", "Cargo.toml")
        .env(
            "VALIDATION_COMMAND",
            "printf 'changed\\n' > README.md && git add README.md",
        )
        .output()
        .expect("constraint script starts");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Validation changed the remediation proposal")
    );
}

#[test]
fn classifier_builds_valid_metadata_patch_and_pull_request_body() {
    let fixture = Fixture::new();
    fixture.stage_change("1.0.1");
    let report_dir = fixture.state.join("reports");
    fs::create_dir(&report_dir).unwrap();
    fs::write(
        report_dir.join("pre-fix.json"),
        r#"{
  "summary": {"packages_checked": 211, "vulnerabilities": 2, "vulnerable_packages": 1},
  "vulnerabilities": [
    {
      "package": "quinn-proto",
      "version": "0.11.14",
      "ecosystem": "crates.io",
      "id": "GHSA-4w2j-m93h-cj5j",
      "aliases": ["CVE-2026-25800"],
      "severity": "High",
      "summary": "Remote memory exhaustion from unbounded stream reassembly"
    },
    {
      "package": "quinn-proto",
      "version": "0.11.14",
      "ecosystem": "crates.io",
      "id": "RUSTSEC-2026-0185",
      "aliases": ["CVE-2026-25800", "GHSA-4w2j-m93h-cj5j"],
      "severity": "High",
      "summary": "Remote memory exhaustion from unbounded stream reassembly"
    }
  ],
  "fixes": [{
    "package": "quinn-proto",
    "from_version": "0.11.14",
    "to_version": "0.11.15",
    "path": "Cargo.lock",
    "status": "applied"
  }]
}"#,
    )
    .unwrap();
    fs::write(
        report_dir.join("post-fix.json"),
        r#"{"summary":{"vulnerabilities":0},"vulnerabilities":[]}"#,
    )
    .unwrap();

    let output = Command::new("bash")
        .arg("-c")
        .arg(workflow_script("Classify the validated result"))
        .current_dir(&fixture.checkout)
        .env("ALLOWED_PATHS", "Cargo.toml")
        .env("AUTO_MERGE", "false")
        .env("GITHUB_OUTPUT", &fixture.output)
        .env("GITHUB_STEP_SUMMARY", &fixture.summary)
        .env("REPORT_DIR", &report_dir)
        .output()
        .expect("classifier script starts");

    assert!(
        output.status.success(),
        "classifier failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report_dir.join("metadata.json")).unwrap())
            .unwrap();
    assert_eq!(metadata["disposition"], "clean_changed");
    assert_eq!(metadata["residual_advisory_records"], 0);
    assert_eq!(
        metadata["pull_request_title"],
        "fix(security): resolve CVE-2026-25800 in quinn-proto"
    );
    assert!(
        !fs::read(report_dir.join("proposal.patch"))
            .unwrap()
            .is_empty()
    );
    let presentation: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report_dir.join("presentation.json")).unwrap())
            .unwrap();
    assert_eq!(presentation["raw_advisory_records_before"], 2);
    assert_eq!(presentation["underlying_vulnerabilities_before"], 1);
    assert_eq!(presentation["issues"][0]["id"], "CVE-2026-25800");
    assert_eq!(
        presentation["issues"][0]["occurrences"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let body = fs::read_to_string(report_dir.join("pull-request.md")).unwrap();
    assert!(body.contains("**Ready for review.**"));
    assert!(body.contains("resolved 1 High-severity vulnerability in 1 package"));
    assert!(body.contains("[CVE-2026-25800](https://nvd.nist.gov/vuln/detail/CVE-2026-25800)"));
    assert!(body.contains("<code>quinn-proto</code>"));
    assert!(body.contains("<code>0.11.14</code>"));
    assert!(body.contains("<code>0.11.15</code>"));
    assert!(
        body.contains("Before: 2 OSV advisory records, correlated to 1 underlying vulnerability")
    );
    assert!(body.contains("After: 0 OSV advisory records"));
    assert!(body.contains("Auto-merge off"));
    assert!(body.len() <= 32 * 1024);
}

#[test]
fn publisher_enables_auto_merge_only_for_a_clean_exact_head() {
    let fixture = Fixture::new();
    let base = fixture.base_sha();
    fixture.stage_change("1.0.1");
    let output = fixture.run_publish_with_auto_merge("clean_changed", &base, true, "squash");
    assert!(
        output.status.success(),
        "publisher failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let commit_sha = String::from_utf8(
        git(
            &fixture.checkout,
            &["rev-parse", &format!("refs/remotes/origin/{BRANCH}")],
        )
        .stdout,
    )
    .unwrap();
    let expected = format!(
        "pr merge 7 --repo owner/project --auto --squash --match-head-commit {}",
        commit_sha.trim()
    );
    assert!(fixture.log().contains(&expected), "log:\n{}", fixture.log());
    assert!(!fixture.log().contains("--admin"));
}

#[test]
fn publisher_refuses_auto_merge_for_partial_remediation() {
    let fixture = Fixture::new();
    let base = fixture.base_sha();
    fixture.stage_change("1.0.1");
    let output = fixture.run_publish_with_auto_merge("partial_changed", &base, true, "squash");
    assert!(
        output.status.success(),
        "publisher failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.log().contains("--auto"), "log:\n{}", fixture.log());
}

#[test]
fn presentation_neutralizes_untrusted_markdown_and_control_characters() {
    let fixture = Fixture::new();
    fixture.stage_change("1.0.1");
    let report_dir = fixture.state.join("hostile-reports");
    fs::create_dir(&report_dir).unwrap();
    fs::write(
        report_dir.join("pre-fix.json"),
        "{\n\
          \"summary\": {\"packages_checked\": 1, \"vulnerabilities\": 1, \"vulnerable_packages\": 1},\n\
          \"vulnerabilities\": [{\n\
            \"package\": \"bad|pkg</code>\",\n\
            \"version\": \"1.0.0`\",\n\
            \"ecosystem\": \"npm\",\n\
            \"id\": \"GHSA-aaaa-bbbb-cccc\",\n\
            \"aliases\": [\"CVE-2026-99999\"],\n\
            \"severity\": \"High\",\n\
            \"summary\": \"<script>alert(1)</script> *urgent* [click](https://evil.invalid) \u{202e}spoof\"\n\
          }],\n\
          \"fixes\": [{\n\
            \"package\": \"bad|pkg</code>\", \"from_version\": \"1.0.0`\",\n\
            \"to_version\": \"1.0.1\", \"path\": \"Cargo.toml|oops\", \"status\": \"applied\"\n\
          }]\n\
        }",
    )
    .unwrap();
    fs::write(
        report_dir.join("post-fix.json"),
        r#"{"summary":{"vulnerabilities":0},"vulnerabilities":[]}"#,
    )
    .unwrap();

    let output = Command::new("bash")
        .arg("-c")
        .arg(workflow_script("Classify the validated result"))
        .current_dir(&fixture.checkout)
        .env("ALLOWED_PATHS", "Cargo.toml")
        .env("AUTO_MERGE", "false")
        .env("GITHUB_OUTPUT", &fixture.output)
        .env("GITHUB_STEP_SUMMARY", &fixture.summary)
        .env("REPORT_DIR", &report_dir)
        .output()
        .expect("classifier script starts");
    assert!(
        output.status.success(),
        "classifier failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let body = fs::read_to_string(report_dir.join("pull-request.md")).unwrap();
    assert!(!body.contains("<script>"));
    assert!(!body.contains("*urgent*"));
    assert!(!body.contains("[click](https://evil.invalid)"));
    assert!(!body.contains('\u{202e}'));
    assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(body.contains("&#42;urgent&#42;"));
    assert!(body.contains("bad&#124;pkg&lt;/code&gt;"));

    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report_dir.join("metadata.json")).unwrap())
            .unwrap();
    assert_eq!(
        metadata["pull_request_title"],
        "fix(security): resolve CVE-2026-99999"
    );
}

#[test]
fn presentation_describes_partial_results_without_overclaiming() {
    let fixture = Fixture::new();
    fixture.stage_change("1.0.1");
    let report_dir = fixture.state.join("partial-reports");
    fs::create_dir(&report_dir).unwrap();
    let vulnerability = r#"{
      "package":"example", "version":"1.0.0", "ecosystem":"npm",
      "id":"GHSA-aaaa-bbbb-cccc", "aliases":["CVE-2026-12345"],
      "severity":"Critical", "summary":"A critical test vulnerability"
    }"#;
    fs::write(
        report_dir.join("pre-fix.json"),
        format!(
            r#"{{"summary":{{"packages_checked":1,"vulnerabilities":1,"vulnerable_packages":1}},"vulnerabilities":[{vulnerability}],"fixes":[{{"package":"example","from_version":"1.0.0","to_version":"1.0.1","path":"Cargo.toml","status":"applied"}}]}}"#
        ),
    )
    .unwrap();
    fs::write(
        report_dir.join("post-fix.json"),
        format!(r#"{{"summary":{{"vulnerabilities":1}},"vulnerabilities":[{vulnerability}]}}"#),
    )
    .unwrap();

    let output = Command::new("bash")
        .arg("-c")
        .arg(workflow_script("Classify the validated result"))
        .current_dir(&fixture.checkout)
        .env("ALLOWED_PATHS", "Cargo.toml")
        .env("AUTO_MERGE", "true")
        .env("GITHUB_OUTPUT", &fixture.output)
        .env("GITHUB_STEP_SUMMARY", &fixture.summary)
        .env("REPORT_DIR", &report_dir)
        .output()
        .expect("classifier script starts");
    assert!(output.status.success());

    let body = fs::read_to_string(report_dir.join("pull-request.md")).unwrap();
    assert!(body.contains("**Partial fix ready for review.**"));
    assert!(body.contains("This is not a complete remediation."));
    assert!(body.contains("### Still needs attention"));
    assert!(body.contains("Auto-merge off"));
    assert!(!body.contains("fresh audit clean"));
    assert!(!body.contains("found no remaining advisory records"));
}

#[test]
fn configuration_rejects_an_unknown_merge_method() {
    let fixture = Fixture::new();
    let output = Command::new("bash")
        .arg("-c")
        .arg(workflow_script("Validate configuration"))
        .current_dir(&fixture.checkout)
        .env("ALLOWED_PATHS", "Cargo.toml")
        .env("BASE_BRANCH", "main")
        .env("BRANCH", BRANCH)
        .env("MERGE_METHOD", "octopus")
        .env("VALIDATION_COMMAND", "cargo test")
        .output()
        .expect("configuration script starts");
    assert_eq!(output.status.code(), Some(4));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("merge-method must be squash, merge, or rebase")
    );
}

#[test]
fn presentation_keeps_no_change_states_truthful() {
    for (name, pre, post, expected_disposition, expected_copy) in [
        (
            "clean",
            r#"{"summary":{"packages_checked":9,"vulnerabilities":0,"vulnerable_packages":0},"vulnerabilities":[],"fixes":[]}"#,
            r#"{"summary":{"vulnerabilities":0},"vulnerabilities":[]}"#,
            "clean_no_change",
            "**No remediation needed.**",
        ),
        (
            "residual",
            r#"{"summary":{"packages_checked":9,"vulnerabilities":1,"vulnerable_packages":1},"vulnerabilities":[{"package":"example","version":"1.0.0","ecosystem":"npm","id":"GHSA-aaaa-bbbb-cccc","severity":"High","summary":"Test vulnerability"}],"fixes":[]}"#,
            r#"{"summary":{"vulnerabilities":1},"vulnerabilities":[{"package":"example","version":"1.0.0","ecosystem":"npm","id":"GHSA-aaaa-bbbb-cccc","severity":"High","summary":"Test vulnerability"}]}"#,
            "residual_no_change",
            "**No safe automated fix available.**",
        ),
    ] {
        let fixture = Fixture::new();
        let report_dir = fixture.state.join(format!("{name}-no-change-reports"));
        fs::create_dir(&report_dir).unwrap();
        fs::write(report_dir.join("pre-fix.json"), pre).unwrap();
        fs::write(report_dir.join("post-fix.json"), post).unwrap();

        let output = Command::new("bash")
            .arg("-c")
            .arg(workflow_script("Classify the validated result"))
            .current_dir(&fixture.checkout)
            .env("ALLOWED_PATHS", "Cargo.toml")
            .env("AUTO_MERGE", "false")
            .env("GITHUB_OUTPUT", &fixture.output)
            .env("GITHUB_STEP_SUMMARY", &fixture.summary)
            .env("REPORT_DIR", &report_dir)
            .output()
            .expect("classifier script starts");
        assert!(
            output.status.success(),
            "classifier failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let presentation: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(report_dir.join("presentation.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(presentation["disposition"], expected_disposition);
        let body = fs::read_to_string(report_dir.join("pull-request.md")).unwrap();
        assert!(body.contains(expected_copy), "body:\n{body}");
        assert!(!body.contains("**Ready for review.**"), "body:\n{body}");
        assert!(!body.contains("Complete remediation; fresh audit clean") || name == "clean");
    }
}

#[test]
fn publisher_creates_one_commit_and_is_idempotent() {
    let fixture = Fixture::new();
    let base = fixture.base_sha();
    fixture.stage_change("1.0.1");
    let first = fixture.run_publish("clean_changed", &base);
    assert!(
        first.status.success(),
        "first publish failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        fixture.branch_file().as_deref(),
        Some("version = \"1.0.1\"\n")
    );
    assert_eq!(fixture.branch_commit_count(), 1);

    fixture.reset_to_main();
    fixture.stage_change("1.0.1");
    let second = fixture.run_publish("clean_changed", &base);
    assert!(
        second.status.success(),
        "second publish failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(fixture.branch_commit_count(), 1);
    assert!(fixture.log().contains("pr create"));
    assert!(fixture.log().contains("pr edit 7"));
}

#[test]
fn clean_no_change_closes_only_its_rolling_pull_request() {
    let fixture = Fixture::new();
    let base = fixture.base_sha();
    fixture.stage_change("1.0.1");
    assert!(fixture.run_publish("clean_changed", &base).status.success());

    fixture.reset_to_main();
    assert!(
        fixture
            .run_publish("clean_no_change", &base)
            .status
            .success()
    );
    assert_eq!(fixture.branch_file(), None);
    assert!(fixture.log().contains("pr close 7"));
}

#[test]
fn publisher_refuses_stale_bases_and_duplicate_pull_requests() {
    let fixture = Fixture::new();
    fixture.stage_change("1.0.1");
    let stale = fixture.run_publish("clean_changed", &"0".repeat(40));
    assert_eq!(stale.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&stale.stderr).contains("refusing a stale publication"));

    fs::write(fixture.state.join("pr-state"), "duplicate\n").unwrap();
    let duplicate = fixture.run_publish("clean_changed", &fixture.base_sha());
    assert_eq!(duplicate.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&duplicate.stderr)
            .contains("More than one open remediation pull request")
    );
}
