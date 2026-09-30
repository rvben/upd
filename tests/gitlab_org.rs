//! End-to-end tests for `upd gitlab org run` and its CI template: a mocked
//! GitLab API in front of real bare repositories, one per project, and a fake
//! updater that records how it was invoked.

mod isolated;

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
const MAJOR_BRANCH: &str = "automation/upd-dependencies-major";
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
if [ "${1:-}" = audit ]; then
  printf '%s\n' "$*" >> "$FAKE_LOG/$name.audit.args"
  cat <<JSON
{"command":"audit","errors":[],"fixes":[],"status":"complete","summary":{"errors":0,"packages_checked":1,"vulnerabilities":0,"vulnerable_packages":0},"vulnerabilities":[]}
JSON
  exit 0
fi
case " $* " in
  *" --only-bump major "*)
    printf '%s\n' "$*" > "$FAKE_LOG/$name.major.args"
    if [ -e fail-major ]; then
      echo "$name major updater failure" >&2
      exit 1
    fi
    printf 'major\n' > major.txt
    cat <<JSON
{"command":"update","mode":"applied","files":[{"path":"major.txt","file_type":"test","lang":"test","updates":[{"package":"breaking","current":"1.4.0","latest":"2.0.0","bump":"major"}],"pinned":[],"ignored":[],"errors":[],"warnings":[]}],"summary":{"files_scanned":1,"files_with_changes":1,"updates_total":1,"updates_major":1,"updates_minor":0,"updates_patch":0,"pinned":0,"ignored":0,"errors":0,"warnings":0}}
JSON
    exit 0
    ;;
esac
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
    let output = isolated::command("git")
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
        Self::with_updater(FAKE_UPDATER).await
    }

    /// An organization whose updater is the bash `script`.
    async fn with_updater(script: &str) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let updater = temp.path().join("fake-upd");
        fs::write(&updater, script).expect("fake updater");
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
        Mock::given(method("GET"))
            .and(path_regex(r"^/api/v4/projects/\d+/merge_requests/\d+$"))
            .respond_with(MergeRequestHeads(Arc::new(self.remotes.clone())))
            .mount(&self.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/groups/acme"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"full_path": "acme"})))
            .mount(&self.server)
            .await;
        for listed in &self.listing {
            Mock::given(method("GET"))
                .and(path(format!("/api/v4/projects/{}", listed["id"]).as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_json(listed))
                .mount(&self.server)
                .await;
        }
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

    /// Every audit invocation for the project at `path`, one per line, if
    /// any ran.
    fn audit_args(&self, path: &str) -> Option<String> {
        fs::read_to_string(
            self.log
                .join(format!("{}.audit.args", path.replace('/', "-"))),
        )
        .ok()
    }

    /// The major lane's updater arguments for the project at `path`, if it
    /// ran.
    fn major_updater_args(&self, path: &str) -> Option<String> {
        fs::read_to_string(
            self.log
                .join(format!("{}.major.args", path.replace('/', "-"))),
        )
        .ok()
    }

    /// The automation branch's copy of `dependency.txt` in project `id`.
    fn branch_file(&self, id: u64) -> Option<String> {
        self.file_on(id, BRANCH, "dependency.txt")
    }

    /// `file` on `branch` in project `id`'s remote, if it is there.
    fn file_on(&self, id: u64, branch: &str, file: &str) -> Option<String> {
        let output = isolated::command("git")
            .arg(format!("--git-dir={}", self.remotes[&id].display()))
            .args(["show", &format!("refs/heads/{branch}:{file}")])
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
        let output = isolated::command("git")
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

/// Answers a merge request read as GitLab does once it has processed a push:
/// the only merge request upd reads back is the one it arms, which heads the
/// rolling branch at the commit the project's remote holds, with
/// mergeability checked.
struct MergeRequestHeads(Arc<HashMap<u64, PathBuf>>);

impl Respond for MergeRequestHeads {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let segments: Vec<&str> = request.url.path().split('/').collect();
        let id: u64 = segments[4].parse().unwrap();
        let Some(remote) = self.0.get(&id) else {
            return ResponseTemplate::new(404);
        };
        let output = isolated::command("git")
            .arg(format!("--git-dir={}", remote.display()))
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{BRANCH}"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{BRANCH} was never pushed");
        ResponseTemplate::new(200).set_body_json(json!({
            "iid": id,
            "web_url": format!("https://gitlab.example.test/{id}/-/merge_requests/{id}"),
            "sha": String::from_utf8(output.stdout).unwrap().trim(),
            "detailed_merge_status": "mergeable",
        }))
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

const REMEDIATION_OPTED_IN: &str =
    "[automation]\ndependency_updates = true\nsecurity_remediation = true\n";

#[tokio::test]
async fn security_fixes_need_both_the_group_and_the_project_to_allow_them() {
    let mut org = Org::new().await;
    org.project(
        31,
        "acme/both",
        &[Entry::File(".updrc.toml", REMEDIATION_OPTED_IN)],
    );
    org.project(32, "acme/absent", &[Entry::File(".updrc.toml", OPTED_IN)]);
    org.project(
        33,
        "acme/declined",
        &[Entry::File(
            ".updrc.toml",
            "[automation]\ndependency_updates = true\nsecurity_remediation = false\n",
        )],
    );
    org.serve().await;

    // The group allows remediation unless it turns it off.
    let (code, report, stderr) = org.run(&[], &[]);

    assert_eq!(code, 0, "{report:#}\n{stderr}");
    let both = project(&report, "acme/both");
    assert_eq!(both["state"], "processed", "{report:#}");
    assert_eq!(
        both["security_remediation"],
        json!({"enabled": true}),
        "{report:#}"
    );
    assert_eq!(
        org.audit_args("acme/both").as_deref(),
        Some(
            "audit --fix-audit --apply --full-precision --format json --no-lock --config .updrc.toml --min-age-floor 7d --exclude-lang nix .\n"
        ),
        "without lock consent the fixes leave lockfiles alone"
    );
    assert_eq!(org.branch_file(31).as_deref(), Some("new\n"));
    for path in ["acme/absent", "acme/declined"] {
        let declined = project(&report, path);
        assert_eq!(declined["state"], "processed", "{report:#}");
        assert_eq!(
            declined["security_remediation"],
            json!({
                "enabled": false,
                "reason": ".updrc.toml does not set security_remediation = true in [automation]",
            }),
            "{report:#}"
        );
        assert!(declined.get("security").is_none(), "{report:#}");
        assert_eq!(org.audit_args(path), None, "{path} was audited");
        assert!(org.updater_args(path).is_some(), "{path} was not updated");
    }
    assert!(
        stderr.contains(
            "acme/absent: security fixes off (.updrc.toml does not set security_remediation = true in [automation])"
        ),
        "{stderr}"
    );

    let mut org = Org::new().await;
    org.project(
        34,
        "acme/project-only",
        &[Entry::File(".updrc.toml", REMEDIATION_OPTED_IN)],
    );
    org.serve().await;
    let (code, report, stderr) = org.run(&[("UPD_SECURITY_REMEDIATION", "false")], &[]);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    let project_only = project(&report, "acme/project-only");
    assert_eq!(project_only["state"], "processed", "{report:#}");
    assert_eq!(
        project_only["security_remediation"],
        json!({
            "enabled": false,
            "reason": "the organization run turned security remediation off",
        }),
        "{report:#}"
    );
    assert_eq!(org.audit_args("acme/project-only"), None);
    assert_eq!(org.branch_file(34).as_deref(), Some("new\n"));
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
async fn a_linked_configuration_is_reported_as_a_link_not_as_bad_toml() {
    let mut org = Org::new().await;
    org.project(
        33,
        "acme/linked",
        &[
            Entry::File("real.toml", OPTED_IN),
            Entry::Link(".updrc.toml", "real.toml"),
        ],
    );
    org.serve().await;

    // The file API answers with the link's target path, which does not parse
    // as TOML; the report still names the actual problem.
    let (code, report, stderr) = org.run(&[], &[]);

    assert_eq!(code, 2, "{report:#}\n{stderr}");
    let linked = project(&report, "acme/linked");
    assert_eq!(linked["state"], "config_invalid", "{report:#}");
    assert_eq!(
        linked["message"], ".updrc.toml must be a regular file, not a symbolic link",
        "{linked}"
    );
    assert_eq!(org.updater_args("acme/linked"), None);
    assert_eq!(org.branch_file(33), None);
    assert!(org.merge_request_calls(33).await.is_empty());
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

/// A git hook in a linked worktree exports `GIT_DIR`, `GIT_WORK_TREE` and
/// `GIT_INDEX_FILE`. upd started from one must still work in its own
/// checkouts, and must not re-initialize the caller's repository with them.
#[tokio::test]
async fn an_inherited_git_hook_environment_leaves_the_callers_repository_alone() {
    let mut org = Org::new().await;
    org.project(CENTRAL, "acme/central", &[]);
    org.project(11, "acme/app", &[Entry::File(".updrc.toml", OPTED_IN)]);
    org.serve().await;

    let caller = tempfile::tempdir().expect("tempdir");
    let repo = caller.path().join("repo");
    let worktree = caller.path().join("worktree");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--quiet"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "x",
        ],
    );
    git(
        &repo,
        &["worktree", "add", "--quiet", worktree.to_str().unwrap()],
    );
    let config = repo.join(".git/config");
    let before = fs::read_to_string(&config).unwrap();
    let admin = repo.join(".git/worktrees/worktree");
    let hook_env = [
        ("GIT_DIR", admin.display().to_string()),
        ("GIT_WORK_TREE", worktree.display().to_string()),
        ("GIT_INDEX_FILE", admin.join("index").display().to_string()),
    ];
    let vars: Vec<(&str, &str)> = hook_env
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();

    let (code, report, stderr) = org.run(&vars, &[]);

    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        before,
        "the caller's shared config changed\n{stderr}"
    );
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(
        project(&report, "acme/app")["state"],
        "processed",
        "{report:#}"
    );
    assert_eq!(org.branch_file(11).as_deref(), Some("new\n"));
}

const MAJOR_OPTED_IN: &str = "[automation]\ndependency_updates = true\nmajor_mr = true\n";

#[tokio::test]
async fn the_major_lane_needs_both_the_group_and_the_project_to_ask_for_it() {
    let mut org = Org::new().await;
    org.project(
        61,
        "acme/both",
        &[Entry::File(".updrc.toml", MAJOR_OPTED_IN)],
    );
    org.project(
        62,
        "acme/group-only",
        &[Entry::File(".updrc.toml", OPTED_IN)],
    );
    org.serve().await;

    let (code, report, stderr) = org.run(&[("UPD_MAJOR_MR", "true")], &[]);

    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["major_branch"], MAJOR_BRANCH, "{report:#}");
    assert_eq!(report["counts"]["major_failed"], 0, "{report:#}");
    let both = project(&report, "acme/both");
    assert_eq!(both["state"], "processed", "{report:#}");
    assert_eq!(both["major"]["outcome"], "published", "{report:#}");
    assert_eq!(both["major"]["branch"], MAJOR_BRANCH, "{report:#}");
    assert_eq!(
        org.file_on(61, MAJOR_BRANCH, "major.txt").as_deref(),
        Some("major\n")
    );
    assert_eq!(org.file_on(61, BRANCH, "major.txt"), None);
    assert_eq!(org.branch_file(61).as_deref(), Some("new\n"));
    let args = org.major_updater_args("acme/both").expect("major lane ran");
    assert!(args.contains("--only-bump major --strict-bump"), "{args}");
    assert!(!args.contains("--max-bump"), "{args}");

    let group_only = project(&report, "acme/group-only");
    assert_eq!(group_only["state"], "processed", "{report:#}");
    assert!(group_only.get("major").is_none(), "{report:#}");
    assert_eq!(org.major_updater_args("acme/group-only"), None);
    assert_eq!(org.file_on(62, MAJOR_BRANCH, "major.txt"), None);
    assert_eq!(org.branch_file(62).as_deref(), Some("new\n"));

    let mut org = Org::new().await;
    org.project(
        63,
        "acme/project-only",
        &[Entry::File(".updrc.toml", MAJOR_OPTED_IN)],
    );
    org.serve().await;
    let (code, report, stderr) = org.run(&[], &[]);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert!(report.get("major_branch").is_none(), "{report:#}");
    assert!(report["counts"].get("major_failed").is_none(), "{report:#}");
    assert!(project(&report, "acme/project-only").get("major").is_none());
    assert_eq!(org.major_updater_args("acme/project-only"), None);
    assert_eq!(org.file_on(63, MAJOR_BRANCH, "major.txt"), None);
}

#[tokio::test]
async fn a_failed_major_lane_fails_the_run_but_not_the_ordinary_lane() {
    let mut org = Org::new().await;
    org.project(
        71,
        "acme/major-fails",
        &[
            Entry::File(".updrc.toml", MAJOR_OPTED_IN),
            Entry::File("fail-major", ""),
        ],
    );
    org.project(
        72,
        "acme/fine",
        &[Entry::File(".updrc.toml", MAJOR_OPTED_IN)],
    );
    org.serve().await;

    let (code, report, stderr) = org.run(&[("UPD_MAJOR_MR", "true")], &[]);

    assert_eq!(code, 2, "{report:#}\n{stderr}");
    assert_eq!(report["counts"]["processed"], 2, "{report:#}");
    assert_eq!(report["counts"]["failed"], 0, "{report:#}");
    assert_eq!(report["counts"]["major_failed"], 1, "{report:#}");
    let failing = project(&report, "acme/major-fails");
    assert_eq!(failing["major"]["outcome"], "failed", "{report:#}");
    assert!(failing["major"].get("command").is_none(), "{report:#}");
    assert_eq!(org.branch_file(71).as_deref(), Some("new\n"));
    assert_eq!(org.file_on(71, MAJOR_BRANCH, "major.txt"), None);
    assert_eq!(
        org.file_on(72, MAJOR_BRANCH, "major.txt").as_deref(),
        Some("major\n")
    );
    assert!(
        stderr.contains("acme/major-fails: major lane: Failed on"),
        "{stderr}"
    );
    assert!(stderr.contains("1 major lane(s) failed"), "{stderr}");
}

#[tokio::test]
async fn an_invalid_major_lane_is_refused_before_any_project_is_listed() {
    for vars in [
        &[("UPD_MAJOR_MR", "true"), ("UPD_MAX_BUMP", "major")][..],
        &[("UPD_MAJOR_MR", "true"), ("UPD_MAJOR_BRANCH", BRANCH)][..],
        &[("UPD_MAJOR_MR", "true"), ("UPD_MAJOR_BRANCH", "bad..name")][..],
    ] {
        let org = Org::new().await;
        org.serve().await;
        let output = org
            .command(env!("CARGO_BIN_EXE_upd"), vars)
            .args(["gitlab", "org", "run", "--output", "json"])
            .output()
            .expect("upd starts");
        assert_eq!(
            output.status.code(),
            Some(4),
            "{vars:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            org.server.received_requests().await.unwrap().is_empty(),
            "{vars:?}"
        );
    }
}

// Lock mode: a project that consents to relocking has each lane split into
// three jobs. `prepare` holds the token and edits manifests only, the lock
// job holds no token and runs the relock, and `publish` holds the token
// again and admits only what its checks allow.

const PIPELINE: &str = "981";
const LOCK_OPTED_IN: &str = "[automation]\ndependency_updates = true\nlock = true\n";

const PYPROJECT: &str =
    "[project]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"example==1.0.0\"]\n";

const UV_LOCK: &str = r#"version = 1
requires-python = ">=3.12"

[[package]]
name = "example"
version = "1.0.0"
source = { registry = "https://pypi.example.test/simple" }
sdist = { url = "https://files.example.test/example-1.0.0.tar.gz", hash = "sha256:00" }
"#;

const CARGO_LOCK: &str = r#"version = 4

[[package]]
name = "vulnerable"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "00"
"#;

/// An updater that edits manifests always and lockfiles only when asked to
/// relock, as `upd` does. `scenario.txt` in the repository picks what it
/// changes; every invocation is logged with the `UV_NO_BUILD` it saw.
const LOCK_UPDATER: &str = r##"#!/usr/bin/env bash
set -euo pipefail
if [ -n "${UPD_GITLAB_TOKEN+set}" ]; then
  echo "fake upd received the GitLab token" >&2
  exit 9
fi
name="$(cat project.txt)"
scenario="$(cat scenario.txt)"
printf '%s | UV_NO_BUILD=%s\n' "$*" "${UV_NO_BUILD:-}" >> "$FAKE_LOG/$name.calls"
case " $* " in
  *" --lock "*) locking=1 ;;
  *" --fix-audit "*" --no-lock "*) locking=0 ;;
  *" --fix-audit "*) locking=1 ;;
  *) locking=0 ;;
esac
if [ "$1" = audit ]; then
  if [ "$scenario" = cargo-transitive ] && [ "$2" = --fix-audit ]; then
    status=skipped
    if [ "$locking" = 1 ]; then
      status=applied
      cat > Cargo.lock <<'LOCK'
version = 4

[[package]]
name = "vulnerable"
version = "1.0.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "01"
LOCK
    fi
    cat <<JSON
{"command":"audit","errors":[],"status":"complete","summary":{"errors":0,"packages_checked":1,"vulnerabilities":1,"vulnerable_packages":1},"vulnerabilities":[{"package":"vulnerable","version":"1.0.0","ecosystem":"crates.io","id":"RUSTSEC-2026-0001","summary":"advisory for vulnerable","severity":"High","source":"RUSTSEC"}],"fixes":[{"package":"vulnerable","ecosystem":"crates.io","from_version":"1.0.0","to_version":"1.0.1","method":"lockfile","path":"Cargo.lock","status":"$status"}]}
JSON
  else
    echo '{"command":"audit","errors":[],"fixes":[],"status":"complete","summary":{"errors":0,"packages_checked":1,"vulnerabilities":0,"vulnerable_packages":0},"vulnerabilities":[]}'
  fi
  exit 0
fi
version=1.1.0 bump=minor majors=0 minors=1
case " $* " in
  *" --only-bump major "*) version=2.0.0 bump=major majors=1 minors=0 ;;
esac
file=pyproject.toml
case "$scenario" in
  cargo-transitive)
    echo '{"command":"update","mode":"applied","files":[],"summary":{"files_scanned":1,"files_with_changes":0,"updates_total":0,"updates_major":0,"updates_minor":0,"updates_patch":0,"pinned":0,"ignored":0,"errors":0,"warnings":0}}'
    exit 0
    ;;
  go)
    file=go.mod
    printf 'module example.test/app\n\nrequire example.test/example v%s\n' "$version" > go.mod
    ;;
  *)
    manifest="$version"
    if [ "$scenario" = uv-drift ] && [ "$locking" = 1 ]; then
      manifest=1.2.0
    fi
    printf '[project]\nname = "app"\nversion = "0.1.0"\ndependencies = ["example==%s"]\n' "$manifest" > pyproject.toml
    if [ "$locking" = 1 ]; then
      cat > uv.lock <<LOCK
version = 1
requires-python = ">=3.12"

[[package]]
name = "example"
version = "$version"
source = { registry = "https://pypi.example.test/simple" }
sdist = { url = "https://files.example.test/example-$version.tar.gz", hash = "sha256:00" }
LOCK
      case "$scenario" in
        uv-git)
          cat >> uv.lock <<'LOCK'

[[package]]
name = "helper"
version = "0.1.0"
source = { git = "https://git.example.test/helper.git#0123456789abcdef" }
LOCK
          ;;
        uv-extra)
          printf 'changed by the lock job\n' > dependency.txt
          ;;
        uv-new-lock)
          mkdir -p vendor
          cp uv.lock vendor/uv.lock
          ;;
      esac
    fi
    ;;
esac
cat <<JSON
{"command":"update","mode":"applied","files":[{"path":"$file","file_type":"test","lang":"test","updates":[{"package":"example","current":"1.0.0","latest":"$version","bump":"$bump"}],"pinned":[],"ignored":[],"errors":[],"warnings":[]}],"summary":{"files_scanned":1,"files_with_changes":1,"updates_total":1,"updates_major":$majors,"updates_minor":$minors,"updates_patch":0,"pinned":0,"ignored":0,"errors":0,"warnings":0}}
JSON
"##;

/// The first `program` on the test's own `PATH`.
fn on_path(program: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set"))
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{program} is not on PATH"))
}

/// Runs `command` and returns its exit code, the JSON on stdout (`Null`
/// when there is none) and stderr.
fn finish(mut command: Command) -> (i32, Value, String) {
    let output = command.output().expect("upd starts");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let report = if output.stdout.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout is not JSON ({error})\nstdout:\n{}\nstderr:\n{stderr}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    };
    (output.status.code().expect("exit code"), report, stderr)
}

/// A lock job's view of the machine: the programs a job image carries, and
/// optionally the lock tools.
struct Toolbox {
    /// `git`, and the shell tools the fake updater uses.
    base: PathBuf,
    /// `uv` and `cargo`, printing a version and doing nothing else.
    lock_tools: PathBuf,
}

impl Toolbox {
    fn new(root: &Path) -> Self {
        let base = root.join("tools");
        fs::create_dir(&base).unwrap();
        for program in ["git", "bash", "cat", "mkdir", "cp"] {
            std::os::unix::fs::symlink(on_path(program), base.join(program)).unwrap();
        }
        let lock_tools = root.join("lock-tools");
        fs::create_dir(&lock_tools).unwrap();
        for (program, version) in [("uv", "uv 0.9.0 (fake)"), ("cargo", "cargo 1.90.0 (fake)")] {
            let script = lock_tools.join(program);
            fs::write(&script, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { base, lock_tools }
    }
}

impl Org {
    /// An organization whose projects' updater relocks as `upd` does.
    async fn locking() -> (Self, Toolbox) {
        let org = Self::with_updater(LOCK_UPDATER).await;
        let toolbox = Toolbox::new(org.temp.path());
        (org, toolbox)
    }

    /// The directory the jobs of project `id`'s `lane` share, as the
    /// pipeline's artifacts carry it from job to job.
    fn job_dir(&self, id: u64, lane: &str) -> PathBuf {
        self.temp.path().join("jobs").join(format!("{id}-{lane}"))
    }

    /// Runs `upd gitlab org <job>` (prepare or publish) for project `id`'s
    /// `lane` in pipeline [`PIPELINE`] of a group that allows lock mode.
    fn job(&self, job: &str, id: u64, lane: &str, vars: &[(&str, &str)]) -> (i32, Value, String) {
        let mut command = self.command(
            env!("CARGO_BIN_EXE_upd"),
            &[("UPD_LOCK", "true"), ("CI_PIPELINE_ID", PIPELINE)],
        );
        for (name, value) in vars {
            command.env(name, value);
        }
        command
            .args(["gitlab", "org", job, "--project", &id.to_string()])
            .args(["--lane", lane, "--dir"])
            .arg(self.job_dir(id, lane))
            .args(["--output", "json"]);
        finish(command)
    }

    fn prepare(&self, id: u64, lane: &str) -> (i32, Value, String) {
        self.job("prepare", id, lane, &[])
    }

    fn publish(&self, id: u64, lane: &str) -> (i32, Value, String) {
        self.job("publish", id, lane, &[])
    }

    /// Runs the lock job as the template does: without the token, with only
    /// `path` on `PATH`.
    fn lock_worker(&self, id: u64, lane: &str, path: &[&Path]) -> (i32, Value, String) {
        let mut command = self.command(env!("CARGO_BIN_EXE_upd"), &[]);
        command
            .env_remove("UPD_GITLAB_TOKEN")
            .env("PATH", std::env::join_paths(path).unwrap())
            .args(["gitlab", "org", "lock-worker", "--dir"])
            .arg(self.job_dir(id, lane))
            .args(["--output", "json"]);
        finish(command)
    }

    /// Every updater invocation for the project at `path`, one per line.
    fn calls(&self, path: &str) -> Vec<String> {
        fs::read_to_string(self.log.join(format!("{}.calls", path.replace('/', "-"))))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The body of the merge request created for project `id`, if one was.
    async fn created_merge_request(&self, id: u64) -> Option<Value> {
        let merge_requests = format!("/api/v4/projects/{id}/merge_requests");
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .find(|request| {
                request.method.as_str() == "POST" && request.url.path() == merge_requests
            })
            .map(|request| serde_json::from_slice(&request.body).unwrap())
    }

    /// The work tree `project` pushed project `path`'s `main` from.
    fn work_tree(&self, path: &str) -> PathBuf {
        self.temp.path().join("work").join(path.replace('/', "-"))
    }
}

/// A Python project with a uv lockfile that consents with `config`, whose
/// updater plays `scenario`.
fn uv_project(org: &mut Org, id: u64, path: &str, config: &'static str, scenario: &'static str) {
    org.project(
        id,
        path,
        &[
            Entry::File(".updrc.toml", config),
            Entry::File("scenario.txt", scenario),
            Entry::File("pyproject.toml", PYPROJECT),
            Entry::File("uv.lock", UV_LOCK),
        ],
    );
}

/// Runs prepare and the lock job for project `id`'s `lane`, asserting both
/// hand the lane on.
fn prepare_and_lock(org: &Org, toolbox: &Toolbox, id: u64, lane: &str) {
    let (code, report, stderr) = org.prepare(id, lane);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["step"], "locking", "{report:#}");
    let (code, report, stderr) = org.lock_worker(id, lane, &[&toolbox.lock_tools, &toolbox.base]);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["step"], "locked", "{report:#}");
}

#[tokio::test]
async fn a_lock_project_is_relocked_without_the_token_and_published_with_it() {
    let (mut org, toolbox) = Org::locking().await;
    uv_project(&mut org, 81, "acme/python", LOCK_OPTED_IN, "uv");
    uv_project(
        &mut org,
        82,
        "acme/builds",
        "[automation]\ndependency_updates = true\nlock = true\nlock_build = true\n",
        "uv",
    );
    org.serve().await;

    let (code, report, stderr) = org.prepare(81, "ordinary");
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["command"], "gitlab org prepare", "{report:#}");
    assert_eq!(report["path"], "acme/python", "{report:#}");
    assert_eq!(report["branch"], BRANCH, "{report:#}");
    assert_eq!(report["step"], "locking", "{report:#}");
    assert_eq!(report["lockfiles"], json!(["uv.lock"]), "{report:#}");
    assert_eq!(
        org.file_on(81, BRANCH, "pyproject.toml"),
        None,
        "prepare pushed"
    );
    assert!(org.created_merge_request(81).await.is_none());
    let calls = org.calls("acme/python");
    assert_eq!(calls.len(), 1, "{calls:#?}");
    assert!(
        calls[0].starts_with("update --apply --format json --config .updrc.toml"),
        "{calls:#?}"
    );
    assert!(!calls[0].contains("--lock"), "prepare relocked: {calls:#?}");

    let (code, report, stderr) =
        org.lock_worker(81, "ordinary", &[&toolbox.lock_tools, &toolbox.base]);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["command"], "gitlab org lock-worker", "{report:#}");
    assert_eq!(report["step"], "locked", "{report:#}");
    assert_eq!(
        report["tools"],
        json!({"uv": "uv 0.9.0 (fake)"}),
        "{report:#}"
    );
    let calls = org.calls("acme/python");
    assert_eq!(calls.len(), 2, "{calls:#?}");
    assert!(
        calls[1].starts_with("update --apply --format json --lock --config .updrc.toml"),
        "{calls:#?}"
    );
    assert!(calls[1].ends_with(" | UV_NO_BUILD=1"), "{calls:#?}");
    assert_eq!(
        org.file_on(81, BRANCH, "pyproject.toml"),
        None,
        "the lock job pushed"
    );

    let (code, report, stderr) = org.publish(81, "ordinary");
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["command"], "gitlab org publish", "{report:#}");
    assert_eq!(report["step"], "finished", "{report:#}");
    assert_eq!(report["result"]["outcome"], "published", "{report:#}");
    assert_eq!(org.calls("acme/python").len(), 2, "publish ran the updater");
    assert!(
        org.file_on(81, BRANCH, "pyproject.toml")
            .unwrap()
            .contains("example==1.1.0")
    );
    let lock = org.file_on(81, BRANCH, "uv.lock").unwrap();
    assert!(lock.contains("version = \"1.1.0\""), "{lock}");
    let merge_request = org.created_merge_request(81).await.expect("merge request");
    assert_eq!(merge_request["source_branch"], BRANCH, "{merge_request:#}");
    let description = merge_request["description"].as_str().unwrap();
    assert!(description.contains("example"), "{description}");
    assert!(!description.contains("Built from"), "{description}");

    // A project that lets its lock jobs build sdists gets no UV_NO_BUILD.
    prepare_and_lock(&org, &toolbox, 82, "ordinary");
    let calls = org.calls("acme/builds");
    assert!(calls[1].contains("--lock"), "{calls:#?}");
    assert!(calls[1].ends_with(" | UV_NO_BUILD="), "{calls:#?}");
    let (code, report, stderr) = org.publish(82, "ordinary");
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["result"]["outcome"], "published", "{report:#}");
}

#[tokio::test]
async fn a_lock_job_change_beyond_the_rules_is_not_published() {
    let (mut org, toolbox) = Org::locking().await;
    let cases = [
        (
            83,
            "acme/git-source",
            "uv-git",
            "the regenerated uv.lock takes code from https://git.example.test/helper.git, which the original never does",
        ),
        (
            84,
            "acme/extra-file",
            "uv-extra",
            "dependency.txt: is neither a planned edit nor a lockfile",
        ),
        (
            85,
            "acme/drift",
            "uv-drift",
            "pyproject.toml: differs from the planned edit",
        ),
        (
            86,
            "acme/new-lock",
            "uv-new-lock",
            "vendor/uv.lock: creates a file",
        ),
    ];
    for (id, path, scenario, _) in cases {
        uv_project(&mut org, id, path, LOCK_OPTED_IN, scenario);
    }
    org.serve().await;

    for (id, path, _, problem) in cases {
        prepare_and_lock(&org, &toolbox, id, "ordinary");
        let (code, report, stderr) = org.publish(id, "ordinary");
        assert_eq!(code, 2, "{path}: {report:#}\n{stderr}");
        assert!(
            stderr.contains("The lock job changed what lock mode does not publish"),
            "{path}: {stderr}"
        );
        assert!(stderr.contains(problem), "{path}: {stderr}");
        assert_eq!(
            org.file_on(id, BRANCH, "pyproject.toml"),
            None,
            "{path} was pushed"
        );
        assert!(org.created_merge_request(id).await.is_none(), "{path}");
    }
}

#[tokio::test]
async fn publish_refuses_work_it_did_not_seal_or_a_result_that_does_not_match() {
    let (mut org, toolbox) = Org::locking().await;
    uv_project(&mut org, 87, "acme/sealed", LOCK_OPTED_IN, "uv");
    org.serve().await;
    prepare_and_lock(&org, &toolbox, 87, "ordinary");
    let dir = org.job_dir(87, "ordinary");

    let (code, _, stderr) = org.job("publish", 87, "ordinary", &[("CI_PIPELINE_ID", "982")]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("does not carry this pipeline's seal"),
        "{stderr}"
    );

    let (code, _, stderr) = org.job("publish", 87, "ordinary", &[("UPD_GITLAB_TOKEN", "other")]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("does not carry this pipeline's seal"),
        "{stderr}"
    );

    // The work is sealed to its project and lane, not just its pipeline.
    let (code, _, stderr) = org.job("publish", 87, "major", &[]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("cannot read"), "{stderr}");
    fs::create_dir_all(org.job_dir(87, "major")).unwrap();
    for file in ["work.json", "work.seal"] {
        fs::copy(dir.join(file), org.job_dir(87, "major").join(file)).unwrap();
    }
    let (code, _, stderr) = org.job("publish", 87, "major", &[]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("was prepared for pipeline 981, project 87, the ordinary lane"),
        "{stderr}"
    );

    let tampered = |file: &str, from: &str, to: &str| {
        let original = fs::read(dir.join(file)).unwrap();
        let text = String::from_utf8(original.clone()).unwrap();
        assert!(text.contains(from), "{file} lacks {from:?}:\n{text}");
        fs::write(dir.join(file), text.replacen(from, to, 1)).unwrap();
        let outcome = org.publish(87, "ordinary");
        fs::write(dir.join(file), original).unwrap();
        outcome
    };
    let (code, _, stderr) = tampered("work.json", "\"lock_build\": false", "\"lock_build\": true");
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("does not carry this pipeline's seal"),
        "{stderr}"
    );
    let (code, _, stderr) = tampered("planned.patch", "example==1.1.0", "example==6.6.6");
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("planned.patch is not the patch prepare sealed"),
        "{stderr}"
    );
    let (code, _, stderr) = tampered("result/result.patch", "1.1.0", "6.6.6");
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("result.patch is not the patch the lock job's result describes"),
        "{stderr}"
    );
    assert_eq!(org.file_on(87, BRANCH, "pyproject.toml"), None);

    let (code, report, stderr) = org.publish(87, "ordinary");
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["result"]["outcome"], "published", "{report:#}");
}

#[tokio::test]
async fn publish_keeps_the_lease_and_the_base_prepare_saw() {
    let (mut org, toolbox) = Org::locking().await;
    uv_project(&mut org, 88, "acme/raced", LOCK_OPTED_IN, "uv");
    uv_project(&mut org, 89, "acme/moved", LOCK_OPTED_IN, "uv");
    uv_project(&mut org, 90, "acme/rewritten", LOCK_OPTED_IN, "uv");
    org.serve().await;
    for id in [88, 89, 90] {
        prepare_and_lock(&org, &toolbox, id, "ordinary");
    }

    // Someone pushed the automation branch after prepare looked at it.
    let raced = org.work_tree("acme/raced");
    let remote = org.remotes[&88].to_str().unwrap().to_string();
    git(
        &raced,
        &[
            "push",
            "--quiet",
            &remote,
            &format!("main:refs/heads/{BRANCH}"),
        ],
    );
    let (code, _, stderr) = org.publish(88, "ordinary");
    assert_eq!(code, 5, "{stderr}");
    assert!(
        stderr.contains("changed on the remote while this run was working"),
        "{stderr}"
    );
    assert_eq!(
        org.file_on(88, BRANCH, "pyproject.toml").as_deref(),
        Some(PYPROJECT),
        "the racing push was overwritten"
    );

    // The default branch moved on: the proposal stays on the commit it was
    // built from, and says so.
    let moved = org.work_tree("acme/moved");
    fs::write(moved.join("later.txt"), "later\n").unwrap();
    git(&moved, &["add", "later.txt"]);
    git(&moved, &["commit", "--quiet", "-m", "test: later"]);
    git(
        &moved,
        &[
            "push",
            "--quiet",
            org.remotes[&89].to_str().unwrap(),
            "main",
        ],
    );
    let (code, report, stderr) = org.publish(89, "ordinary");
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["result"]["outcome"], "published", "{report:#}");
    assert_eq!(org.file_on(89, BRANCH, "later.txt"), None);
    let merge_request = org.created_merge_request(89).await.expect("merge request");
    let description = merge_request["description"].as_str().unwrap();
    assert!(description.contains("Built from"), "{description}");

    // The default branch was rewritten without the commit prepare saw.
    let rewritten = org.work_tree("acme/rewritten");
    git(
        &rewritten,
        &["commit", "--quiet", "--amend", "-m", "test: rewritten"],
    );
    git(
        &rewritten,
        &[
            "push",
            "--quiet",
            "--force",
            org.remotes[&90].to_str().unwrap(),
            "main",
        ],
    );
    let (code, _, stderr) = org.publish(90, "ordinary");
    assert_eq!(code, 5, "{stderr}");
    assert!(stderr.contains("main no longer contains"), "{stderr}");
    assert_eq!(org.file_on(90, BRANCH, "pyproject.toml"), None);
}

#[tokio::test]
async fn the_lock_job_refuses_the_token_and_needs_its_tools() {
    let (mut org, toolbox) = Org::locking().await;
    uv_project(&mut org, 91, "acme/tools", LOCK_OPTED_IN, "uv");
    org.serve().await;
    let (code, report, stderr) = org.prepare(91, "ordinary");
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    let result = org.job_dir(91, "ordinary").join("result");

    let mut command = org.command(env!("CARGO_BIN_EXE_upd"), &[]);
    command
        .env(
            "PATH",
            std::env::join_paths([&toolbox.lock_tools, &toolbox.base]).unwrap(),
        )
        .args(["gitlab", "org", "lock-worker", "--dir"])
        .arg(org.job_dir(91, "ordinary"));
    let (code, _, stderr) = finish(command);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("must not hold the token"), "{stderr}");
    assert!(!result.exists());

    let (code, _, stderr) = org.lock_worker(91, "ordinary", &[&toolbox.base]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("lock tool missing: uv is not on PATH, and regenerating uv.lock needs it"),
        "{stderr}"
    );
    assert!(!result.exists());
    assert_eq!(
        org.calls("acme/tools").len(),
        1,
        "the updater ran without its tools"
    );

    let (code, report, stderr) =
        org.lock_worker(91, "ordinary", &[&toolbox.lock_tools, &toolbox.base]);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert!(result.join("result.patch").exists());
    let (code, _, stderr) = org.lock_worker(91, "ordinary", &[&toolbox.lock_tools, &toolbox.base]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
}

#[tokio::test]
async fn prepare_refuses_a_lockfile_lock_mode_cannot_check() {
    let (mut org, _toolbox) = Org::locking().await;
    org.project(
        92,
        "acme/go",
        &[
            Entry::File(".updrc.toml", LOCK_OPTED_IN),
            Entry::File("scenario.txt", "go"),
            Entry::File(
                "go.mod",
                "module example.test/app\n\nrequire example.test/example v1.0.0\n",
            ),
            Entry::File("go.sum", "example.test/example v1.0.0 h1:AAAA=\n"),
        ],
    );
    org.serve().await;

    let (code, _, stderr) = org.prepare(92, "ordinary");

    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("lock mode does not support go.sum yet; nothing was published"),
        "{stderr}"
    );
    assert!(!org.job_dir(92, "ordinary").join("work.json").exists());
    assert_eq!(org.file_on(92, BRANCH, "go.mod"), None);
    assert!(org.created_merge_request(92).await.is_none());
}

#[tokio::test]
async fn a_security_fix_that_needs_a_relock_is_relocked_by_the_lock_job() {
    let (mut org, toolbox) = Org::locking().await;
    org.project(
        93,
        "acme/crate",
        &[
            Entry::File(
                ".updrc.toml",
                "[automation]\ndependency_updates = true\nsecurity_remediation = true\nlock = true\n",
            ),
            Entry::File("scenario.txt", "cargo-transitive"),
            Entry::File("Cargo.lock", CARGO_LOCK),
        ],
    );
    org.project(
        99,
        "acme/crate-unlocked",
        &[
            Entry::File(
                ".updrc.toml",
                "[automation]\ndependency_updates = true\nsecurity_remediation = true\n",
            ),
            Entry::File("scenario.txt", "cargo-transitive"),
            Entry::File("Cargo.lock", CARGO_LOCK),
        ],
    );
    org.serve().await;

    let (code, report, stderr) = org.prepare(93, "ordinary");
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["lockfiles"], json!(["Cargo.lock"]), "{report:#}");
    let calls = org.calls("acme/crate");
    assert!(calls[0].starts_with("audit --fix-audit"), "{calls:#?}");
    assert!(calls[0].contains(" --no-lock "), "{calls:#?}");

    let (code, report, stderr) =
        org.lock_worker(93, "ordinary", &[&toolbox.lock_tools, &toolbox.base]);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(
        report["tools"],
        json!({"cargo": "cargo 1.90.0 (fake)"}),
        "{report:#}"
    );
    let calls = org.calls("acme/crate");
    let fix = calls
        .iter()
        .skip(2)
        .find(|call| call.starts_with("audit --fix-audit"))
        .unwrap_or_else(|| panic!("the lock job applied no fixes: {calls:#?}"));
    assert!(!fix.contains("--no-lock"), "{calls:#?}");

    let (code, report, stderr) = org.publish(93, "ordinary");
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["result"]["outcome"], "published", "{report:#}");
    assert_eq!(report["result"]["security"]["fixes"], 1, "{report:#}");
    assert_eq!(report["result"]["security"]["skipped"], 0, "{report:#}");
    assert_eq!(report["result"]["security"]["advisories"], 1, "{report:#}");
    let lock = org.file_on(93, BRANCH, "Cargo.lock").unwrap();
    assert!(lock.contains("version = \"1.0.1\""), "{lock}");
    let merge_request = org.created_merge_request(93).await.expect("merge request");
    let title = merge_request["title"].as_str().unwrap();
    assert!(title.starts_with("fix(security): "), "{title}");

    // Without lock consent prepare finishes the lane, and the fix that
    // needs a relock is reported skipped.
    let (code, report, stderr) = org.prepare(99, "ordinary");
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["step"], "finished", "{report:#}");
    assert_eq!(report["result"]["security"]["fixes"], 0, "{report:#}");
    assert_eq!(report["result"]["security"]["skipped"], 1, "{report:#}");
    let calls = org.calls("acme/crate-unlocked");
    assert!(calls[0].contains(" --no-lock "), "{calls:#?}");
}

#[tokio::test]
async fn the_major_lane_is_split_like_the_ordinary_lane() {
    let (mut org, toolbox) = Org::locking().await;
    uv_project(
        &mut org,
        94,
        "acme/major",
        "[automation]\ndependency_updates = true\nmajor_mr = true\nlock = true\n",
        "uv",
    );
    uv_project(&mut org, 95, "acme/no-major", LOCK_OPTED_IN, "uv");
    org.serve().await;
    let major = [("UPD_MAJOR_MR", "true")];

    let (code, report, stderr) = org.job("prepare", 94, "major", &major);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["branch"], MAJOR_BRANCH, "{report:#}");
    assert_eq!(report["step"], "locking", "{report:#}");
    let (code, report, stderr) =
        org.lock_worker(94, "major", &[&toolbox.lock_tools, &toolbox.base]);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    let calls = org.calls("acme/major");
    assert!(
        calls[1].contains("--lock --only-bump major --strict-bump"),
        "{calls:#?}"
    );
    let (code, report, stderr) = org.job("publish", 94, "major", &major);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["result"]["outcome"], "published", "{report:#}");
    let lock = org.file_on(94, MAJOR_BRANCH, "uv.lock").unwrap();
    assert!(lock.contains("version = \"2.0.0\""), "{lock}");
    assert_eq!(org.file_on(94, BRANCH, "uv.lock"), None);

    let (code, report, stderr) = org.job("prepare", 95, "major", &major);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["step"], "lane_off", "{report:#}");
    let (code, report, stderr) =
        org.lock_worker(95, "major", &[&toolbox.lock_tools, &toolbox.base]);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["step"], "nothing_to_do", "{report:#}");
    let (code, report, stderr) = org.job("publish", 95, "major", &major);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    assert_eq!(report["step"], "nothing_to_do", "{report:#}");
    assert!(org.calls("acme/no-major").is_empty());
    assert_eq!(org.file_on(95, MAJOR_BRANCH, "uv.lock"), None);
}

#[tokio::test]
async fn without_consent_on_both_sides_prepare_finishes_the_lane_itself() {
    let (mut org, toolbox) = Org::locking().await;
    uv_project(&mut org, 96, "acme/declined", OPTED_IN, "uv");
    uv_project(&mut org, 97, "acme/group-off", LOCK_OPTED_IN, "uv");
    org.serve().await;

    for (id, path, vars) in [
        (96, "acme/declined", &[][..]),
        (97, "acme/group-off", &[("UPD_LOCK", "false")][..]),
    ] {
        let (code, report, stderr) = org.job("prepare", id, "ordinary", vars);
        assert_eq!(code, 0, "{path}: {report:#}\n{stderr}");
        assert_eq!(report["step"], "finished", "{path}: {report:#}");
        assert_eq!(
            report["result"]["outcome"], "published",
            "{path}: {report:#}"
        );
        assert!(
            org.file_on(id, BRANCH, "pyproject.toml")
                .unwrap()
                .contains("example==1.1.0")
        );
        assert_eq!(org.file_on(id, BRANCH, "uv.lock").as_deref(), Some(UV_LOCK));
        let calls = org.calls(path);
        assert!(
            calls.iter().all(|call| !call.contains("--lock")),
            "{calls:#?}"
        );

        let (code, report, stderr) =
            org.lock_worker(id, "ordinary", &[&toolbox.lock_tools, &toolbox.base]);
        assert_eq!(code, 0, "{path}: {report:#}\n{stderr}");
        assert_eq!(report["step"], "nothing_to_do", "{path}: {report:#}");
        let (code, report, stderr) = org.job("publish", id, "ordinary", vars);
        assert_eq!(code, 0, "{path}: {report:#}\n{stderr}");
        assert_eq!(report["step"], "nothing_to_do", "{path}: {report:#}");
        assert_eq!(org.calls(path).len(), calls.len(), "{path}");
    }
}

#[tokio::test]
async fn prepare_refuses_a_project_outside_the_group() {
    let (mut org, _toolbox) = Org::locking().await;
    uv_project(&mut org, 98, "elsewhere/app", LOCK_OPTED_IN, "uv");
    org.serve().await;

    let (code, _, stderr) = org.prepare(98, "ordinary");

    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("(elsewhere/app) is not in acme"),
        "{stderr}"
    );
    assert!(org.calls("elsewhere/app").is_empty());
    assert!(!org.job_dir(98, "ordinary").join("work.json").exists());
}
