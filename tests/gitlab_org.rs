//! End-to-end tests for `upd gitlab org run` and its CI template: a mocked
//! GitLab API in front of real bare repositories, one per project, and a fake
//! updater that records how it was invoked.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TEMPLATE: &str = include_str!("../ci/gitlab-organization-update.yml");
const BRANCH: &str = "automation/upd-dependencies";
const CENTRAL: u64 = 10;
const OPTED_IN: &str = "[automation]\ndependency_updates = true\n";

const FAKE_UPDATER: &str = r#"#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = gitlab ]; then
  exec "$REAL_UPD" "$@"
fi
if [ -n "${UPD_GITLAB_TOKEN+set}" ]; then
  echo "fake upd received the GitLab token" >&2
  exit 9
fi
name="$(cat project.txt)"
printf '%s\n' "$*" > "$FAKE_LOG/$name.args"
for step in 1 2 3; do
  echo "$name progress $step" >&2
  sleep 0.05
done
if [ -e fail-updater ]; then
  echo "$name updater failure" >&2
  exit 1
fi
printf 'new\n' > dependency.txt
cat <<JSON
{"command":"update","mode":"applied","files":[{"path":"dependency.txt","file_type":"test","lang":"test","updates":[{"package":"example","current":"1.0.0","latest":"1.1.0","bump":"minor"}],"pinned":[],"ignored":[],"errors":[],"warnings":[]}],"summary":{"files_scanned":1,"files_with_changes":1,"updates_total":1,"updates_major":0,"updates_minor":1,"updates_patch":0,"pinned":0,"ignored":0,"errors":0,"warnings":0}}
JSON
"#;

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("git starts");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A file in a fixture project: regular content or a symbolic link.
enum Entry {
    File(&'static str, &'static str),
    Link(&'static str, &'static str),
}

struct Org {
    temp: TempDir,
    server: MockServer,
    updater: PathBuf,
    log: PathBuf,
    listing: Vec<Value>,
    remotes: HashMap<u64, PathBuf>,
}

impl Org {
    async fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let updater = temp.path().join("fake-upd");
        fs::write(&updater, FAKE_UPDATER).expect("fake updater");
        fs::set_permissions(&updater, fs::Permissions::from_mode(0o755)).unwrap();
        let log = temp.path().join("updater-log");
        fs::create_dir(&log).unwrap();
        Self {
            temp,
            server: MockServer::start().await,
            updater,
            log,
            listing: Vec::new(),
            remotes: HashMap::new(),
        }
    }

    /// Adds project `id` at `path` to the group, with `entries` committed
    /// beside `dependency.txt` on its `main` branch.
    fn project(&mut self, id: u64, path: &str, entries: &[Entry]) -> &mut Value {
        let name = path.replace('/', "-");
        let work = self.temp.path().join("work").join(&name);
        fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "--quiet", "--initial-branch=main"]);
        git(&work, &["config", "user.name", "Test User"]);
        git(&work, &["config", "user.email", "test@example.com"]);
        git(&work, &["config", "commit.gpgsign", "false"]);
        fs::write(work.join("project.txt"), &name).unwrap();
        fs::write(work.join("dependency.txt"), "old\n").unwrap();
        for entry in entries {
            match entry {
                Entry::File(file, content) => fs::write(work.join(file), content).unwrap(),
                Entry::Link(file, target) => {
                    std::os::unix::fs::symlink(target, work.join(file)).unwrap()
                }
            }
        }
        git(&work, &["add", "--all"]);
        git(&work, &["commit", "--quiet", "-m", "test: initial"]);
        let remote = self.temp.path().join(format!("{path}.git"));
        fs::create_dir_all(remote.parent().unwrap()).unwrap();
        git(
            self.temp.path(),
            &["init", "--quiet", "--bare", remote.to_str().unwrap()],
        );
        git(
            &work,
            &["push", "--quiet", remote.to_str().unwrap(), "main"],
        );
        self.remotes.insert(id, remote);
        self.listing.push(json!({
            "id": id,
            "path_with_namespace": path,
            "default_branch": "main",
            "archived": false,
            "empty_repo": false,
            "repository_access_level": "enabled",
            "marked_for_deletion_at": null,
        }));
        self.listing.last_mut().unwrap()
    }

    /// Mounts the group listing, the file API backed by the bare remotes, and
    /// merge request endpoints for every project.
    async fn serve(&self) {
        Mock::given(method("GET"))
            .and(path("/api/v4/groups/acme/projects"))
            .and(query_param("include_subgroups", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&self.listing))
            .mount(&self.server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(
                r"^/api/v4/projects/\d+/repository/files/[^/]+/raw$",
            ))
            .respond_with(RawFiles(Arc::new(self.remotes.clone())))
            .mount(&self.server)
            .await;
        for id in self.remotes.keys() {
            let merge_requests = format!("/api/v4/projects/{id}/merge_requests");
            Mock::given(method("GET"))
                .and(path(merge_requests.as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                .mount(&self.server)
                .await;
            Mock::given(method("POST"))
                .and(path(merge_requests.as_str()))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                    "iid": id,
                    "web_url": format!("https://gitlab.example.test/{id}/-/merge_requests/{id}"),
                    "merge_when_pipeline_succeeds": false,
                })))
                .mount(&self.server)
                .await;
            Mock::given(method("PUT"))
                .and(path(format!("{merge_requests}/{id}/merge").as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "iid": id,
                    "web_url": format!("https://gitlab.example.test/{id}/-/merge_requests/{id}"),
                    "merge_when_pipeline_succeeds": true,
                })))
                .mount(&self.server)
                .await;
        }
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>, vars: &[(&str, &str)]) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(self.temp.path())
            .env("UPD_GITLAB_TOKEN", "test-token")
            .env(
                "CI_SERVER_URL",
                format!("file://{}", self.temp.path().display()),
            )
            .env("CI_API_V4_URL", format!("{}/api/v4", self.server.uri()))
            .env("CI_PROJECT_ID", CENTRAL.to_string())
            .env("UPD_GROUP", "acme")
            .env("UPD_MIN_AGE", "7d")
            .env("UPD_MAX_BUMP", "minor")
            .env("UPD_GIT_NAME", "upd test")
            .env("UPD_GIT_EMAIL", "upd-test@example.com")
            .env("UPD_EXECUTABLE", &self.updater)
            .env("REAL_UPD", env!("CARGO_BIN_EXE_upd"))
            .env("FAKE_LOG", &self.log);
        for (name, value) in vars {
            command.env(name, value);
        }
        command
    }

    /// Runs `upd gitlab org run --output json` and returns its exit code,
    /// parsed report and stderr.
    fn run(&self, vars: &[(&str, &str)], args: &[&str]) -> (i32, Value, String) {
        let output = self
            .command(env!("CARGO_BIN_EXE_upd"), vars)
            .args(["gitlab", "org", "run", "--output", "json"])
            .args(args)
            .output()
            .expect("upd starts");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "no JSON report ({error})\nstdout:\n{}\nstderr:\n{stderr}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        (output.status.code().expect("exit code"), report, stderr)
    }

    /// The updater's arguments for the project at `path`, if it ran.
    fn updater_args(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.log.join(format!("{}.args", path.replace('/', "-")))).ok()
    }

    /// The automation branch's copy of `dependency.txt` in project `id`.
    fn branch_file(&self, id: u64) -> Option<String> {
        let output = Command::new("git")
            .arg(format!("--git-dir={}", self.remotes[&id].display()))
            .args(["show", &format!("refs/heads/{BRANCH}:dependency.txt")])
            .output()
            .expect("git show starts");
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap())
    }

    /// `METHOD path` for every request that touched project `id`'s merge
    /// requests.
    async fn merge_request_calls(&self, id: u64) -> Vec<String> {
        let prefix = format!("/api/v4/projects/{id}/merge_requests");
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path().starts_with(&prefix))
            .map(|request| format!("{} {}", request.method, request.url.path()))
            .collect()
    }
}

/// Serves `GET /projects/:id/repository/files/:name/raw` from the project's
/// bare repository, as GitLab would.
struct RawFiles(Arc<HashMap<u64, PathBuf>>);

impl Respond for RawFiles {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let segments: Vec<&str> = request.url.path().split('/').collect();
        let id: u64 = segments[4].parse().unwrap();
        let name = segments[7];
        let reference = request
            .url
            .query_pairs()
            .find(|(key, _)| key == "ref")
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default();
        let Some(remote) = self.0.get(&id) else {
            return ResponseTemplate::new(404);
        };
        let output = Command::new("git")
            .arg(format!("--git-dir={}", remote.display()))
            .args(["show", &format!("{reference}:{name}")])
            .output()
            .unwrap();
        if output.status.success() {
            ResponseTemplate::new(200).set_body_bytes(output.stdout)
        } else {
            ResponseTemplate::new(404)
        }
    }
}

fn project<'a>(report: &'a Value, path: &str) -> &'a Value {
    report["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["path"] == path)
        .unwrap_or_else(|| panic!("{path} missing from {report:#}"))
}

#[tokio::test]
async fn only_projects_that_opted_in_are_updated() {
    let mut org = Org::new().await;
    org.project(CENTRAL, "acme/central", &[]);
    org.project(11, "acme/app", &[Entry::File(".updrc.toml", OPTED_IN)]);
    org.project(12, "acme/lib", &[]);
    org.project(
        13,
        "acme/off",
        &[Entry::File(
            "upd.toml",
            "[automation]\ndependency_updates = false\n",
        )],
    );
    org.project(
        14,
        "acme/broken",
        &[Entry::File(
            ".updrc.toml",
            "[automation]\ndependency_update = true\n",
        )],
    );
    org.project(15, "acme/old", &[Entry::File(".updrc.toml", OPTED_IN)])["archived"] = json!(true);
    org.project(16, "acme/sub/tool", &[Entry::File(".updrc", OPTED_IN)]);
    org.serve().await;

    let (code, report, stderr) = org.run(&[], &[]);

    assert_eq!(code, 2, "invalid configuration fails the run\n{stderr}");
    assert_eq!(
        report["counts"],
        json!({"projects": 7, "skipped": 2, "not_opted_in": 2, "config_invalid": 1, "processed": 2, "failed": 0}),
        "{report:#}\n{stderr}"
    );
    assert_eq!(
        project(&report, "acme/central")["reason"],
        "central_project"
    );
    assert_eq!(project(&report, "acme/old")["reason"], "archived");
    assert_eq!(project(&report, "acme/lib")["state"], "not_opted_in");
    assert_eq!(project(&report, "acme/off")["state"], "not_opted_in");
    assert_eq!(project(&report, "acme/broken")["state"], "config_invalid");
    for (id, path, config) in [
        (11, "acme/app", ".updrc.toml"),
        (16, "acme/sub/tool", ".updrc"),
    ] {
        assert_eq!(project(&report, path)["state"], "processed", "{report:#}");
        assert_eq!(org.branch_file(id).as_deref(), Some("new\n"), "{path}");
        assert_eq!(
            org.updater_args(path).unwrap().trim(),
            format!(
                "update --apply --format json --config {config} --min-age-floor 7d --max-bump minor --exclude-lang nix ."
            ),
            "{path}"
        );
    }
    for (id, path) in [
        (CENTRAL, "acme/central"),
        (12, "acme/lib"),
        (13, "acme/off"),
        (14, "acme/broken"),
        (15, "acme/old"),
    ] {
        assert_eq!(org.updater_args(path), None, "{path} was updated");
        assert_eq!(org.branch_file(id), None, "{path} got a branch");
        assert!(
            org.merge_request_calls(id).await.is_empty(),
            "{path} saw merge request calls"
        );
    }
    assert!(
        stderr.contains("7 projects in acme: 2 processed"),
        "{stderr}"
    );
}

#[tokio::test]
async fn auto_merge_needs_both_the_group_and_the_project_to_allow_it() {
    let both = "[automation]\ndependency_updates = true\nauto_merge = true\n";
    let mut org = Org::new().await;
    org.project(21, "acme/both", &[Entry::File(".updrc.toml", both)]);
    org.project(
        22,
        "acme/group-only",
        &[Entry::File(".updrc.toml", OPTED_IN)],
    );
    org.serve().await;
    let (code, report, stderr) = org.run(&[("UPD_AUTO_MERGE", "true")], &[]);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert!(
        org.merge_request_calls(21)
            .await
            .contains(&"PUT /api/v4/projects/21/merge_requests/21/merge".to_string())
    );
    assert!(
        !org.merge_request_calls(22)
            .await
            .iter()
            .any(|call| call.starts_with("PUT")),
        "a project without auto_merge = true must not be auto-merged"
    );

    let mut org = Org::new().await;
    org.project(23, "acme/project-only", &[Entry::File(".updrc.toml", both)]);
    org.serve().await;
    let (code, report, stderr) = org.run(&[("UPD_AUTO_MERGE", "false")], &[]);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(project(&report, "acme/project-only")["state"], "processed");
    assert!(
        !org.merge_request_calls(23)
            .await
            .iter()
            .any(|call| call.starts_with("PUT")),
        "the group did not allow auto-merge"
    );
}

#[tokio::test]
async fn the_fetched_commit_decides_consent_not_the_file_api() {
    let mut org = Org::new().await;
    org.project(31, "acme/withdrawn", &[]);
    org.project(
        32,
        "acme/linked",
        &[
            Entry::File("real.toml", OPTED_IN),
            Entry::Link(".updrc.toml", "real.toml"),
        ],
    );
    // GitLab's file API resolves nothing here; this stands in for a stale
    // answer, or one that followed the link.
    for id in [31, 32] {
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/v4/projects/{id}/repository/files/.updrc.toml/raw"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string(OPTED_IN))
            .with_priority(1)
            .mount(&org.server)
            .await;
    }
    org.serve().await;

    let (code, report, stderr) = org.run(&[], &[]);

    assert_eq!(code, 2, "{report:#}\n{stderr}");
    assert_eq!(project(&report, "acme/withdrawn")["state"], "not_opted_in");
    let linked = project(&report, "acme/linked");
    assert_eq!(linked["state"], "config_invalid", "{report:#}");
    assert!(
        linked["message"]
            .as_str()
            .unwrap()
            .contains("must be a regular file"),
        "{linked}"
    );
    for (id, path) in [(31, "acme/withdrawn"), (32, "acme/linked")] {
        assert_eq!(org.updater_args(path), None, "{path} was updated");
        assert_eq!(org.branch_file(id), None, "{path} got a branch");
        assert!(org.merge_request_calls(id).await.is_empty(), "{path}");
    }
}

#[tokio::test]
async fn a_dry_run_reports_without_pushing_or_writing_to_gitlab() {
    let mut org = Org::new().await;
    org.project(41, "acme/app", &[Entry::File(".updrc.toml", OPTED_IN)]);
    org.serve().await;

    let (code, report, stderr) = org.run(&[], &["--dry-run"]);

    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["dry_run"], true);
    let app = project(&report, "acme/app");
    assert_eq!(app["state"], "processed", "{report:#}");
    assert_eq!(app["outcome"], "would_publish", "{report:#}");
    assert!(
        org.updater_args("acme/app").is_some(),
        "the update still runs"
    );
    assert_eq!(org.branch_file(41), None);
    let writes: Vec<String> = org
        .server
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
async fn a_failing_project_neither_stops_nor_garbles_the_others() {
    let mut org = Org::new().await;
    let paths = ["acme/one", "acme/two", "acme/three", "acme/four"];
    for (id, path) in (51..).zip(paths) {
        org.project(id, path, &[Entry::File(".updrc.toml", OPTED_IN)]);
    }
    org.project(
        55,
        "acme/failing",
        &[
            Entry::File(".updrc.toml", OPTED_IN),
            Entry::File("fail-updater", ""),
        ],
    );
    org.serve().await;

    let (code, report, stderr) = org.run(&[("UPD_CONCURRENCY", "5")], &[]);

    assert_eq!(code, 2, "{report:#}\n{stderr}");
    assert_eq!(report["counts"]["processed"], 4, "{report:#}");
    assert_eq!(report["counts"]["failed"], 1, "{report:#}");
    assert_eq!(project(&report, "acme/failing")["state"], "failed");
    for (id, path) in (51..).zip(paths) {
        assert_eq!(org.branch_file(id).as_deref(), Some("new\n"), "{path}");
    }

    // Projects ran side by side, yet each one's output is a single block
    // under its own heading.
    for path in paths.iter().chain(&["acme/failing"]) {
        let name = path.replace('/', "-");
        let heading = stderr
            .find(&format!("==> {path}\n"))
            .unwrap_or_else(|| panic!("no block for {path}\n{stderr}"));
        let block = &stderr[heading..];
        let block = &block[..block[4..].find("==> ").map_or(block.len(), |end| end + 4)];
        for step in 1..=3 {
            assert!(
                block.contains(&format!("{name} progress {step}\n")),
                "{path} block is missing step {step}:\n{block}\nfull stderr:\n{stderr}"
            );
        }
        assert_eq!(
            stderr.matches(&format!("{name} progress")).count(),
            3,
            "{stderr}"
        );
    }
    assert!(stderr.contains("acme-failing updater failure"), "{stderr}");
}

#[tokio::test]
async fn a_group_that_cannot_be_listed_fails_the_run() {
    let org = Org::new().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/groups/acme/projects"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"message": "404 Group Not Found"})),
        )
        .mount(&org.server)
        .await;
    let output = org
        .command(env!("CARGO_BIN_EXE_upd"), &[])
        .args(["gitlab", "org", "run", "--output", "json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(
        output
            .stderr
            .trim_ascii_end()
            .rsplit(|b| *b == b'\n')
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(error["error"]["exit_code"], 2, "{error}");
}

fn embedded_script() -> String {
    let marker = "  script:\n    - |\n";
    TEMPLATE
        .split_once(marker)
        .expect("template has one literal script block")
        .1
        .lines()
        .map(|line| line.strip_prefix("      ").unwrap_or(line).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_template_keeps_the_report_and_the_exit_status() {
    let mut org = Org::new().await;
    org.project(61, "acme/app", &[Entry::File(".updrc.toml", OPTED_IN)]);
    org.project(
        62,
        "acme/broken",
        &[Entry::File(".updrc.toml", "[automation]\nunknown = 1\n")],
    );
    org.serve().await;

    for (dry_run, expected_branch) in [("true", None), ("false", Some("new\n"))] {
        let output: Output = org
            .command(
                "bash",
                &[("UPD_DRY_RUN", dry_run), ("UPD_VERSION", "v0.0.0")],
            )
            .args(["-c", &embedded_script()])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        let report: Value = serde_json::from_str(
            &fs::read_to_string(org.temp.path().join(".upd-ci/upd-org-report.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(report["dry_run"], dry_run == "true", "{report:#}");
        assert_eq!(report["counts"]["config_invalid"], 1, "{report:#}");
        assert_eq!(org.branch_file(61).as_deref(), expected_branch);
        assert!(stderr.contains("2 projects in acme"), "{stderr}");
    }
}

/// Every input a template declares is used, and every interpolation names a
/// declared input: GitLab rejects the second only when the file is included.
#[test]
fn template_inputs_are_declared_exactly_where_they_are_used() {
    for (name, template) in [
        ("organization", TEMPLATE),
        (
            "single-project",
            include_str!("../ci/gitlab-dependency-update.yml"),
        ),
    ] {
        let (spec, job) = template.split_once("\n---\n").expect("spec header");
        let declared: std::collections::BTreeSet<&str> = spec
            .lines()
            .filter_map(|line| line.strip_prefix("    ")?.strip_suffix(':'))
            .filter(|key| !key.starts_with(' '))
            .collect();
        let used: std::collections::BTreeSet<&str> = job
            .split("$[[ inputs.")
            .skip(1)
            .map(|rest| rest.split_once(" ]]").expect("closed interpolation").0)
            .collect();
        assert!(!declared.is_empty(), "{name}: no inputs found");
        assert_eq!(declared, used, "{name}");
    }
}

#[test]
fn template_defaults_are_pinned_and_protect_the_report() {
    assert!(TEMPLATE.contains("debian:bookworm-slim@sha256:"));
    assert!(TEMPLATE.contains("UPD_VERSION: \"$[[ inputs.upd_version ]]\""));
    assert!(TEMPLATE.contains("args=(gitlab org run --output json)"));
    assert!(TEMPLATE.contains("    access: developer\n"));
    assert!(TEMPLATE.contains("  resource_group: upd-organization-update\n"));
    assert!(!TEMPLATE.contains("UPD_VERSION: \"latest\""));
}
