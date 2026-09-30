//! End-to-end tests for `upd gitlab org run` and its CI template: a mocked
//! GitLab API in front of real bare repositories, one per project, and a fake
//! updater that records how it was invoked.

mod isolated;

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

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
    /// The CI jobs that ran, as GitLab's job API serves them.
    jobs: CiJobs,
    /// What each child pipeline job, by name, uploads besides its declared
    /// artifacts in [`run_lock_pipeline`].
    uploads: HashMap<String, Upload>,
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
            jobs: CiJobs::default(),
            uploads: HashMap::new(),
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
                Entry::File(file, content) => {
                    fs::create_dir_all(work.join(file).parent().unwrap()).unwrap();
                    fs::write(work.join(file), content).unwrap()
                }
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
            .and(path_regex(
                r"^/api/v4/(projects/\d+/pipelines/\d+/jobs|jobs/\d+/artifacts)$",
            ))
            .respond_with(JobApi(self.jobs.clone()))
            .mount(&self.server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v4/projects/{CENTRAL}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": CENTRAL,
                "path_with_namespace": "acme/central",
                "ci_push_repository_for_job_token_allowed": false,
            })))
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
            .env("CI_JOB_TOKEN", "job-token")
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

/// A CI job that ran, and what it left for GitLab to serve.
#[derive(Clone)]
struct CiJob {
    id: u64,
    pipeline: String,
    name: String,
    status: &'static str,
    /// The directory the job ran in, which its artifact paths are relative to.
    root: PathBuf,
    /// The artifact paths the job declared.
    paths: Vec<String>,
    /// What the job uploaded besides.
    upload: Upload,
}

/// What a job's own code uploads with its job token besides the artifacts
/// it declares, which GitLab accepts from any job.
#[derive(Clone, Default)]
struct Upload {
    /// Files added to the job's artifacts archive, by archive name.
    files: Vec<(String, Vec<u8>)>,
    /// The variables of a dotenv report, which GitLab loads into every job
    /// taking the job's artifacts.
    dotenv: Vec<(String, String)>,
}

impl CiJob {
    /// The job's artifacts archive, as the runner would have uploaded it at
    /// the end of the job (built when read, so a test can alter the files
    /// first).
    fn archive(&self) -> Vec<u8> {
        zip_paths(&self.root, &self.paths, &self.upload.files)
    }
}

/// The CI jobs that ran, shared with the mock job API.
#[derive(Clone, Default)]
struct CiJobs(Arc<Mutex<Vec<CiJob>>>);

impl CiJobs {
    /// Records `job` with the next free id, replacing an earlier run of the
    /// same job, as a retry does. Returns the id.
    fn record(&self, mut job: CiJob) -> u64 {
        let mut jobs = self.0.lock().unwrap();
        job.id = 5000 + jobs.iter().map(|job| job.id - 5000 + 1).max().unwrap_or(0);
        jobs.retain(|other| (&other.pipeline, &other.name) != (&job.pipeline, &job.name));
        jobs.push(job);
        jobs.last().unwrap().id
    }

    fn named(&self, pipeline: &str, name: &str) -> CiJob {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|job| job.pipeline == pipeline && job.name == name)
            .unwrap_or_else(|| panic!("no job {name} ran in pipeline {pipeline}"))
            .clone()
    }

    /// Changes the recorded job `name` of `pipeline` as `change` says.
    fn alter(&self, pipeline: &str, name: &str, change: impl FnOnce(&mut CiJob)) {
        let mut jobs = self.0.lock().unwrap();
        let job = jobs
            .iter_mut()
            .find(|job| job.pipeline == pipeline && job.name == name)
            .unwrap_or_else(|| panic!("no job {name} ran in pipeline {pipeline}"));
        change(job);
    }
}

/// Answers the reads publish makes about the lock job, as GitLab does. The
/// jobs of a pipeline in the central project are listed for the group token
/// only: GitLab's job token permissions do not include that endpoint. A
/// job's artifacts archive is served for a job token, and here for nothing
/// else, so the group token never goes there.
struct JobApi(CiJobs);

impl Respond for JobApi {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let header = |name: &str| {
            request
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let segments: Vec<&str> = request.url.path().split('/').collect();
        let allowed = match segments[3] {
            "projects" => header("PRIVATE-TOKEN").as_deref() == Some("test-token"),
            _ => {
                header("PRIVATE-TOKEN").is_none()
                    && header("JOB-TOKEN").is_some_and(|token| token.starts_with("job-token"))
            }
        };
        if !allowed {
            return ResponseTemplate::new(403);
        }
        let jobs = self.0.0.lock().unwrap();
        match segments[3] {
            "projects" if segments[4] == CENTRAL.to_string() => {
                let listed: Vec<Value> = jobs
                    .iter()
                    .filter(|job| job.pipeline == segments[6])
                    .map(|job| json!({"id": job.id, "name": job.name, "status": job.status}))
                    .collect();
                ResponseTemplate::new(200)
                    .set_body_json(listed)
                    .insert_header("X-Next-Page", "")
            }
            "jobs" => match jobs.iter().find(|job| job.id.to_string() == segments[4]) {
                Some(job) => ResponseTemplate::new(200).set_body_bytes(job.archive()),
                None => ResponseTemplate::new(404),
            },
            _ => ResponseTemplate::new(404),
        }
    }
}

/// A zip archive of `paths` under `root`, named relative to `root`, as the
/// runner uploads a job's declared artifacts, followed by `extra` files.
fn zip_paths(root: &Path, paths: &[String], extra: &[(String, Vec<u8>)]) -> Vec<u8> {
    use std::io::Write;
    fn add(writer: &mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>, root: &Path, path: &Path) {
        let name = path
            .strip_prefix(root)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let options = zip::write::SimpleFileOptions::default();
        if path.is_dir() {
            writer.add_directory(format!("{name}/"), options).unwrap();
            let mut entries: Vec<PathBuf> = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            for entry in entries {
                add(writer, root, &entry);
            }
        } else {
            writer.start_file(name, options).unwrap();
            writer.write_all(&fs::read(path).unwrap()).unwrap();
        }
    }
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for artifact in paths {
        let path = root.join(artifact.trim_end_matches('/'));
        if path.exists() {
            add(&mut writer, root, &path);
        }
    }
    for (name, content) in extra {
        writer
            .start_file(name.as_str(), zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(content).unwrap();
    }
    writer.finish().unwrap().into_inner()
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
        json!({"projects": 7, "skipped": 2, "not_opted_in": 2, "config_invalid": 1, "processed": 2, "deferred": 0, "failed": 0}),
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
        .take_while(|line| line.is_empty() || line.starts_with("      "))
        .map(|line| line.strip_prefix("      ").unwrap_or(line).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The top-level YAML block of `key` in the organization template, up to
/// the next top-level line.
fn template_block(key: &str) -> String {
    let start = TEMPLATE
        .find(&format!("\n{key}:\n"))
        .unwrap_or_else(|| panic!("template has no {key}"));
    TEMPLATE[start + 1..]
        .lines()
        .enumerate()
        .take_while(|(index, line)| *index == 0 || line.is_empty() || line.starts_with(' '))
        .map(|(_, line)| format!("{line}\n"))
        .collect()
}

/// The variables `.upd-organization-update` sets, with each input
/// interpolated from `inputs` or else from its declared default.
fn template_variables(inputs: &[(&str, &str)]) -> Vec<(String, String)> {
    let (spec, _) = TEMPLATE.split_once("\n---\n").expect("spec header");
    let mut defaults = HashMap::new();
    let mut input = "";
    for line in spec.lines() {
        if let Some(name) = line
            .strip_prefix("    ")
            .and_then(|line| line.strip_suffix(':'))
        {
            if !name.starts_with(' ') {
                input = name;
            }
        } else if let Some(value) = line.strip_prefix("      default: ") {
            defaults.insert(input, value.trim_matches('"').to_string());
        }
    }
    let block = template_block(".upd-organization-update");
    let variables = block
        .split_once("\n  variables:\n")
        .expect("the job sets variables")
        .1;
    variables
        .lines()
        .take_while(|line| line.starts_with("    "))
        .filter(|line| !line.trim_start().starts_with('#'))
        .map(|line| {
            let (name, value) = line.trim().split_once(": ").expect("NAME: value");
            let mut value = value.trim_matches('"').to_string();
            while let Some(start) = value.find("$[[ inputs.") {
                let end = start + value[start..].find(" ]]").unwrap() + 3;
                let name = &value[start + "$[[ inputs.".len()..end - 3];
                let with = inputs
                    .iter()
                    .find(|(input, _)| *input == name)
                    .map(|(_, value)| value.to_string())
                    .or_else(|| defaults.get(name).cloned())
                    .unwrap_or_else(|| panic!("input {name} has no value"));
                value.replace_range(start..end, &with);
            }
            (name.to_string(), value)
        })
        .collect()
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
                &[
                    ("UPD_DRY_RUN", dry_run),
                    ("UPD_LOCK", "false"),
                    ("UPD_VERSION", "v0.0.0"),
                ],
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

/// Lock mode on an upd release without it stops before planning, naming
/// the inputs that choose the release. Such a release reads `plan` as a
/// project path and answers `--help` with the help of `gitlab org`, exit 0.
#[tokio::test]
async fn lock_mode_on_a_release_without_it_names_the_inputs_to_change() {
    let org = Org::new().await;
    let old = org.temp.path().join("old-upd");
    fs::write(
        &old,
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >> \"$FAKE_LOG/old-upd.args\"\n\
         echo 'Usage: upd gitlab org [OPTIONS] [PATHS]... <COMMAND>'\n\
         echo 'Commands:'\n\
         echo '  run   Run `gitlab run` for every group project that opts in'\n",
    )
    .unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).unwrap();
    let output = org
        .command(
            "bash",
            &[
                ("UPD_DRY_RUN", "false"),
                ("UPD_LOCK", "true"),
                ("UPD_VERSION", "v0.0.0"),
                ("UPD_EXECUTABLE", old.to_str().unwrap()),
            ],
        )
        .current_dir(org.temp.path())
        .args(["-c", &embedded_script()])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(4), "{stderr}");
    assert!(
        stderr.contains("upd v0.0.0 has no lock mode")
            && stderr.contains("upd_version")
            && stderr.contains("upd_sha256"),
        "{stderr}"
    );
    let calls = fs::read_to_string(org.log.join("old-upd.args")).unwrap();
    assert_eq!(calls, "gitlab org plan --help\n");
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
if [ "${1:-}" = gitlab ]; then
  exec "$REAL_UPD" "$@"
fi
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
        for program in ["git", "bash", "cat", "mkdir", "cp", "chmod"] {
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
        self.temp.path().join(job_dir_arg(id, lane))
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
            .args(["--lane", lane, "--dir", &job_dir_arg(id, lane)])
            .args(["--output", "json"]);
        if job == "publish" {
            command.args(["--lock-job", &lock_job_name(id, lane)]);
        }
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
            .args([
                "gitlab",
                "org",
                "lock-worker",
                "--dir",
                &job_dir_arg(id, lane),
            ])
            .args(["--output", "json"]);
        let outcome = finish(command);
        self.jobs.record(CiJob {
            id: 0,
            pipeline: PIPELINE.to_string(),
            name: lock_job_name(id, lane),
            status: if outcome.0 == 0 { "success" } else { "failed" },
            root: self.temp.path().to_path_buf(),
            paths: vec![format!("{}/result/", job_dir_arg(id, lane))],
            upload: Upload::default(),
        });
        outcome
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

    /// Each read of the job API: its path and the job token it carried, or
    /// the group token as `PRIVATE-TOKEN <token>`.
    async fn job_api_reads(&self) -> Vec<(String, String)> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| {
                let path = request.url.path();
                path.starts_with("/api/v4/jobs/") || path.contains("/pipelines/")
            })
            .map(|request| {
                let header = |name: &str| {
                    request
                        .headers
                        .get(name)
                        .map(|value| value.to_str().unwrap().to_string())
                };
                let token = match header("PRIVATE-TOKEN") {
                    Some(token) => format!("PRIVATE-TOKEN {token}"),
                    None => header("JOB-TOKEN").unwrap_or_default(),
                };
                (request.url.path().to_string(), token)
            })
            .collect()
    }

    /// The work tree `project` pushed project `path`'s `main` from.
    fn work_tree(&self, path: &str) -> PathBuf {
        self.temp.path().join("work").join(path.replace('/', "-"))
    }
}

/// The `--dir` of project `id`'s `lane` jobs, relative to the directory they
/// run in, as the plan writes it.
fn job_dir_arg(id: u64, lane: &str) -> String {
    format!("jobs/{id}-{lane}")
}

/// The lock job of project `id`'s `lane`, named as the plan names it.
fn lock_job_name(id: u64, lane: &str) -> String {
    match lane {
        "major" => format!("lock-{id}-major"),
        _ => format!("lock-{id}"),
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
async fn publish_reads_the_result_of_the_one_lock_job_that_succeeded() {
    let (mut org, toolbox) = Org::locking().await;
    uv_project(&mut org, 87, "acme/sealed", LOCK_OPTED_IN, "uv");
    org.serve().await;
    let (code, report, stderr) = org.prepare(87, "ordinary");
    assert_eq!(code, 0, "{report:#}\n{stderr}");

    let (code, _, stderr) = org.publish(87, "ordinary");
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("Pipeline 981 has 0 jobs named lock-87, not one"),
        "{stderr}"
    );

    let (code, report, stderr) =
        org.lock_worker(87, "ordinary", &[&toolbox.lock_tools, &toolbox.base]);
    assert_eq!(code, 0, "{report:#}\n{stderr}");

    // A lock job that did not succeed is not read, whatever its archive.
    org.jobs
        .alter(PIPELINE, "lock-87", |job| job.status = "failed");
    let (code, _, stderr) = org.publish(87, "ordinary");
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("The lock job lock-87 has not succeeded: its status is failed"),
        "{stderr}"
    );
    org.jobs
        .alter(PIPELINE, "lock-87", |job| job.status = "success");

    let twin = org.jobs.named(PIPELINE, "lock-87");
    org.jobs.0.lock().unwrap().push(CiJob { id: 9999, ..twin });
    let (code, _, stderr) = org.publish(87, "ordinary");
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("Pipeline 981 has 2 jobs named lock-87, not one"),
        "{stderr}"
    );
    org.jobs.0.lock().unwrap().retain(|job| job.id != 9999);

    // A lock job of another pipeline is not this pipeline's.
    org.jobs
        .alter(PIPELINE, "lock-87", |job| job.pipeline = "980".to_string());
    let (code, _, stderr) = org.publish(87, "ordinary");
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("has 0 jobs named lock-87"), "{stderr}");
    org.jobs
        .alter("980", "lock-87", |job| job.pipeline = PIPELINE.to_string());

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

/// Adds project `id` at `path` with `config` as its `.updrc.toml` and a uv
/// lockfile beside its manifest.
fn locked_project(org: &mut Org, id: u64, path: &str, config: &'static str) {
    org.project(
        id,
        path,
        &[
            Entry::File(".updrc.toml", config),
            Entry::File("pyproject.toml", PYPROJECT),
            Entry::File("sub/uv.lock", UV_LOCK),
        ],
    );
}

#[tokio::test]
async fn a_project_whose_lockfiles_stay_stale_is_told_why_and_how_to_fix_it() {
    let mut org = Org::new().await;
    locked_project(&mut org, 71, "acme/python", OPTED_IN);
    locked_project(&mut org, 72, "acme/consents", LOCK_OPTED_IN);
    org.project(73, "acme/plain", &[Entry::File(".updrc.toml", OPTED_IN)]);
    org.serve().await;

    let (code, report, stderr) = org.run(&[], &[]);

    assert_eq!(code, 0, "{report:#}\n{stderr}");
    for path in ["acme/python", "acme/consents"] {
        let project = project(&report, path);
        assert_eq!(project["state"], "processed", "{report:#}");
        assert_eq!(project["lockfiles"]["regenerated"], false, "{report:#}");
        let reason = project["lockfiles"]["reason"].as_str().unwrap();
        assert!(
            reason.starts_with("the organization run does not regenerate lockfiles; turn its lock input on and set lock = true in .updrc.toml"),
            "{reason}"
        );
        assert!(
            reason.ends_with(
                "or run ci/gitlab-dependency-update.yml with lock: true in this project"
            ),
            "{reason}"
        );
        assert!(
            stderr.contains(&format!(
                "{path}: lockfiles not regenerated (the organization run"
            )),
            "{stderr}"
        );
    }
    let plain = project(&report, "acme/plain");
    assert_eq!(plain["state"], "processed", "{report:#}");
    assert!(plain.get("lockfiles").is_none(), "{report:#}");
    assert!(!stderr.contains("acme/plain: lockfiles"), "{stderr}");
}

#[tokio::test]
async fn a_lock_run_leaves_handed_off_and_newly_consenting_projects_to_lock_jobs() {
    let mut org = Org::new().await;
    locked_project(&mut org, 74, "acme/handed", LOCK_OPTED_IN);
    locked_project(&mut org, 75, "acme/newly", LOCK_OPTED_IN);
    locked_project(&mut org, 76, "acme/unlocked", OPTED_IN);
    org.project(77, "acme/plain", &[Entry::File(".updrc.toml", OPTED_IN)]);
    org.serve().await;
    let vars = [("UPD_LOCK", "true"), ("UPD_LOCK_HANDED_OFF", "74")];

    let (code, report, stderr) = org.run(&vars, &[]);

    assert_eq!(
        code, 0,
        "a deferred project is not a failure\n{report:#}\n{stderr}"
    );
    let handed = project(&report, "acme/handed");
    assert_eq!(handed["state"], "skipped", "{report:#}");
    assert_eq!(handed["reason"], "handed_off", "{report:#}");
    let newly = project(&report, "acme/newly");
    assert_eq!(newly["state"], "deferred", "{report:#}");
    assert!(
        newly["reason"].as_str().unwrap().starts_with(
            ".updrc.toml consented to lockfile regeneration after this run was planned"
        ),
        "{report:#}"
    );
    for (id, path) in [(74, "acme/handed"), (75, "acme/newly")] {
        assert_eq!(org.updater_args(path), None, "{path} was updated");
        assert_eq!(org.branch_file(id), None, "{path} got a branch");
        assert!(org.merge_request_calls(id).await.is_empty(), "{path}");
    }
    let unlocked = project(&report, "acme/unlocked");
    assert_eq!(unlocked["state"], "processed", "{report:#}");
    assert_eq!(
        unlocked["lockfiles"],
        json!({"regenerated": false, "reason": ".updrc.toml does not set lock = true in [automation]; set it, or run ci/gitlab-dependency-update.yml with lock: true in this project"}),
        "{report:#}"
    );
    assert_eq!(org.branch_file(76).as_deref(), Some("new\n"));
    assert_eq!(project(&report, "acme/plain")["state"], "processed");
    assert_eq!(report["counts"]["deferred"], 1, "{report:#}");
    assert!(
        stderr.contains("4 projects in acme: 2 processed, 1 deferred to the next run's lock jobs,"),
        "{stderr}"
    );

    // A dry run hands nothing off, so it previews every consenting project.
    let (code, report, stderr) = org.run(&[("UPD_LOCK", "true")], &["--dry-run"]);
    assert_eq!(code, 0, "{report:#}\n{stderr}");
    let newly = project(&report, "acme/newly");
    assert_eq!(newly["state"], "processed", "{report:#}");
    assert_eq!(
        newly["lockfiles"],
        json!({"regenerated": true}),
        "{report:#}"
    );
}

#[tokio::test]
async fn handed_off_projects_need_lock_mode_and_numeric_ids() {
    let mut org = Org::new().await;
    org.project(78, "acme/plain", &[Entry::File(".updrc.toml", OPTED_IN)]);
    org.serve().await;

    for vars in [
        &[("UPD_LOCK_HANDED_OFF", "78")][..],
        &[
            ("UPD_LOCK", "true"),
            ("UPD_LOCK_HANDED_OFF", "78,acme/plain"),
        ],
    ] {
        let output = org
            .command(env!("CARGO_BIN_EXE_upd"), vars)
            .args(["gitlab", "org", "run", "--output", "json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4), "{vars:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("UPD_LOCK_HANDED_OFF"),
            "{vars:?}"
        );
    }
    assert_eq!(org.updater_args("acme/plain"), None);
}

#[tokio::test]
async fn shards_split_the_group_by_project_id() {
    let mut org = Org::new().await;
    for (id, path) in [
        (60, "acme/a"),
        (61, "acme/b"),
        (62, "acme/c"),
        (63, "acme/d"),
        (64, "acme/e"),
    ] {
        org.project(id, path, &[Entry::File(".updrc.toml", OPTED_IN)]);
    }
    org.serve().await;

    let mut seen = Vec::new();
    for shard in ["1/3", "2/3", "3/3"] {
        let (code, report, stderr) = org.run(&[("UPD_SHARD", shard)], &["--dry-run"]);
        assert_eq!(code, 0, "{shard}: {report:#}\n{stderr}");
        assert_eq!(report["shard"], shard, "{report:#}");
        assert!(
            stderr.contains(&format!("in acme (shard {shard}):")),
            "{stderr}"
        );
        let ids: Vec<u64> = report["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|project| project["id"].as_u64().unwrap())
            .collect();
        let index: u64 = shard[..1].parse().unwrap();
        assert!(ids.iter().all(|id| id % 3 == index - 1), "{shard}: {ids:?}");
        seen.extend(ids);
    }
    seen.sort_unstable();
    assert_eq!(
        seen,
        [60, 61, 62, 63, 64],
        "every project in exactly one shard"
    );

    let (_, whole, _) = org.run(&[], &["--dry-run"]);
    assert!(whole.get("shard").is_none(), "{whole:#}");
    assert_eq!(whole["counts"]["projects"], 5, "{whole:#}");

    for shard in ["0/3", "4/3", "1/0", "3", "1/3/5", "a/3", "+1/3", " 1/3"] {
        let output = org
            .command(env!("CARGO_BIN_EXE_upd"), &[("UPD_SHARD", shard)])
            .args(["gitlab", "org", "run", "--dry-run", "--output", "json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4), "{shard:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("UPD_SHARD must be k/n"),
            "{shard:?}"
        );
    }
}

/// The ID GitLab gives the child pipeline in [`run_lock_pipeline`].
const CHILD_PIPELINE: &str = "982";

/// GitLab's default `max_artifacts_content_include_size`: the largest
/// artifact archive `trigger: include: artifact` reads a pipeline from.
const MAX_INCLUDED_ARCHIVE: usize = 5 * 1024 * 1024;

/// The child pipeline as the template's trigger job includes it: from the
/// artifact archive of the job its `include` names, which GitLab refuses
/// past [`MAX_INCLUDED_ARCHIVE`] whatever the included file's own size. A
/// job between the organization job and the trigger runs as the template
/// defines it, in a fresh directory holding the organization job's
/// artifacts.
fn included_pipeline(org: &Org, parent: &Path) -> Value {
    let trigger = template_block(".upd-organization-lock");
    let (_, include) = trigger
        .split_once("      - artifact: .upd-ci/upd-lock-pipeline.yml\n        job: \"$[[ inputs.")
        .expect("the trigger includes the pipeline file from a job's artifacts");
    let input = include.split_once(" ]]\"\n").unwrap().0;
    let (root, block) = match input {
        "organization_job" => (
            parent.to_path_buf(),
            template_block(".upd-organization-update"),
        ),
        "lock_plan_job" => {
            let block = template_block(".upd-organization-lock-plan");
            assert!(
                block.contains("  needs:\n    - job: \"$[[ inputs.organization_job ]]\"\n      artifacts: true\n"),
                "{block}"
            );
            let dir = org.temp.path().join("lock-plan");
            fs::create_dir(&dir).unwrap();
            copy_tree(&parent.join(".upd-ci"), &dir.join(".upd-ci"));
            let script = lock_plan_script();
            let output = org
                .command("bash", &[])
                .current_dir(&dir)
                .env("CI_JOB_NAME", "upd-organization-lock-plan")
                .env("UPD_LOCK_PLAN_JOB", "upd-organization-lock-plan")
                .args(["-c", &script])
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(0),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            (dir, block)
        }
        other => panic!("the trigger includes from an unknown input {other}"),
    };
    let paths: Vec<String> = block
        .split_once("    paths:\n")
        .expect("the included job keeps artifacts")
        .1
        .lines()
        .map_while(|line| line.strip_prefix("      - "))
        .map(str::to_string)
        .collect();
    let archive = zip_paths(&root, &paths, &[]);
    assert!(
        archive.len() <= MAX_INCLUDED_ARCHIVE,
        "Artifacts archive for job {input} is too large: {} bytes exceeds maximum of {MAX_INCLUDED_ARCHIVE}",
        archive.len()
    );
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(archive)).unwrap();
    let mut file = archive.by_name(".upd-ci/upd-lock-pipeline.yml").unwrap();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut file, &mut text).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The script of the template's `.upd-organization-lock-plan` job.
fn lock_plan_script() -> String {
    template_block(".upd-organization-lock-plan")
        .split_once("  script:\n    - |\n")
        .expect("the lock plan job has one literal script")
        .1
        .lines()
        .take_while(|line| line.is_empty() || line.starts_with("      "))
        .map(|line| line.strip_prefix("      ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_lock_plan_job_refuses_a_name_the_trigger_does_not_include_from() {
    let dir = tempfile::tempdir().unwrap();
    let run = |job: &str| {
        isolated::command("bash")
            .current_dir(dir.path())
            .env("CI_JOB_NAME", job)
            .env("UPD_LOCK_PLAN_JOB", "upd-organization-lock-plan")
            .args(["-c", &lock_plan_script()])
            .output()
            .unwrap()
    };
    let output = run("lock-plan");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "The lock_plan_job input names upd-organization-lock-plan, but this job is lock-plan"
        ),
        "{output:?}"
    );
    let output = run("upd-organization-lock-plan");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("The organization job wrote no lock pipeline"),
        "{output:?}"
    );
    fs::create_dir(dir.path().join(".upd-ci")).unwrap();
    fs::write(dir.path().join(".upd-ci/upd-lock-pipeline.yml"), "{}").unwrap();
    assert_eq!(run("upd-organization-lock-plan").status.code(), Some(0));
}

/// What one job of the simulated lock pipeline did.
struct ChildJob {
    code: i32,
    held_token: bool,
    stdout: String,
    stderr: String,
}

/// Runs an organization job in lock mode as the template does, then every
/// job of the child pipeline it writes, as GitLab would: in `needs` order,
/// each in a fresh directory holding only the artifacts it needs, with the
/// child's variables and its own, and the token only in jobs that declare
/// the environment it is scoped to. Returns the plan report, the child
/// pipeline and each job's outcome.
///
/// Variables follow GitLab's precedence: `project` variables (the central
/// project's CI/CD settings) reach every job and outrank YAML variables;
/// `pipeline` variables (a schedule's or a manual run's) outrank them in the
/// organization job and do not reach the child, whose trigger forwards
/// nothing.
fn run_lock_pipeline(
    org: &Org,
    toolbox: &Toolbox,
    inputs: &[(&str, &str)],
    project: &[(&str, &str)],
    pipeline_variables: &[(&str, &str)],
) -> (Value, Value, HashMap<String, ChildJob>) {
    let parent = org.temp.path().join("parent");
    fs::create_dir(&parent).unwrap();
    let mut command = org.command("bash", &[]);
    for name in [
        "UPD_GROUP",
        "UPD_MIN_AGE",
        "UPD_MAX_BUMP",
        "UPD_GIT_NAME",
        "UPD_GIT_EMAIL",
    ] {
        command.env_remove(name);
    }
    for (name, value) in template_variables(inputs) {
        if name != "UPD_EXECUTABLE" {
            command.env(name, value);
        }
    }
    for (name, value) in project.iter().chain(pipeline_variables) {
        command.env(name, value);
    }
    let output = command
        .current_dir(&parent)
        .env("CI_PIPELINE_ID", PIPELINE)
        .env("CI_JOB_NAME", "upd-organization-update")
        .env("UPD_VERSION", "v0.0.0")
        .args(["-c", &embedded_script()])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let plan: Value = serde_json::from_str(
        &fs::read_to_string(parent.join(".upd-ci/upd-org-plan.json")).unwrap(),
    )
    .unwrap();
    // The organization job keeps .upd-ci/ as its artifact.
    assert!(template_block(".upd-organization-update").contains("    paths:\n      - .upd-ci/\n"));
    let pipeline = included_pipeline(org, &parent);

    let jobs: Vec<(&String, &Value)> = pipeline
        .as_object()
        .unwrap()
        .iter()
        .filter(|(name, _)| *name != "variables")
        .collect();
    let mut done: HashMap<String, ChildJob> = HashMap::new();
    while done.len() < jobs.len() {
        let ready = jobs
            .iter()
            .find(|(name, job)| {
                !done.contains_key(*name)
                    && job["needs"].as_array().unwrap().iter().all(|need| {
                        need.get("pipeline").is_some()
                            || done.contains_key(need["job"].as_str().unwrap())
                    })
            })
            .unwrap_or_else(|| panic!("no job can start; done: {:?}", done.keys()));
        let (name, job) = (ready.0.clone(), ready.1);
        let dir = org.temp.path().join("child").join(&name);
        fs::create_dir_all(&dir).unwrap();
        let mut dotenv: Vec<(String, String)> = Vec::new();
        for need in job["needs"].as_array().unwrap() {
            if need.get("pipeline").is_some() {
                assert_eq!(need["pipeline"], PIPELINE, "{name}");
                assert_eq!(need["job"], "upd-organization-update", "{name}");
                copy_tree(&parent.join(".upd-ci"), &dir.join(".upd-ci"));
                continue;
            }
            let needed = need["job"].as_str().unwrap();
            // A failed job's artifacts cannot be fetched.
            let failed = &done[needed];
            assert_eq!(
                failed.code, 0,
                "{name} needs {needed}, which failed:\n{}\n{}",
                failed.stdout, failed.stderr
            );
            // The job waits for the jobs it needs either way; it takes
            // their artifacts, unpacked over its directory, and the
            // variables of their dotenv reports unless `artifacts` is false.
            if need.get("artifacts") == Some(&Value::Bool(false)) {
                continue;
            }
            let needed = org.jobs.named(CHILD_PIPELINE, needed);
            zip::ZipArchive::new(std::io::Cursor::new(needed.archive()))
                .unwrap()
                .extract(&dir)
                .unwrap();
            dotenv.extend(needed.upload.dotenv);
        }
        let held_token = job.get("environment").is_some();
        let mut command = org.command(on_path("bash"), &[]);
        command.current_dir(&dir);
        for name in [
            "UPD_GITLAB_TOKEN",
            "UPD_GROUP",
            "UPD_MIN_AGE",
            "UPD_MAX_BUMP",
            "UPD_GIT_NAME",
            "UPD_GIT_EMAIL",
        ] {
            command.env_remove(name);
        }
        if held_token {
            assert_eq!(job["environment"]["name"], "upd-organization", "{name}");
            command.env("UPD_GITLAB_TOKEN", "test-token");
        } else {
            command.env(
                "PATH",
                std::env::join_paths([&toolbox.lock_tools, &toolbox.base]).unwrap(),
            );
        }
        let variables = pipeline["variables"].as_object().unwrap().iter();
        for (variable, value) in variables.chain(job["variables"].as_object().into_iter().flatten())
        {
            let value = match value {
                Value::String(value) => value.clone(),
                value => {
                    assert_eq!(value["expand"], false, "{name}: {variable}");
                    value["value"].as_str().unwrap().to_string()
                }
            };
            command.env(variable, value);
        }
        let id = org.jobs.record(CiJob {
            id: 0,
            pipeline: CHILD_PIPELINE.to_string(),
            name: name.clone(),
            status: "running",
            root: dir.clone(),
            paths: Vec::new(),
            upload: Upload::default(),
        });
        // Predefined variables, then dotenv reports, which outrank them, then
        // the project's CI/CD variables, which outrank both.
        command
            .env("CI_PIPELINE_ID", CHILD_PIPELINE)
            .env("CI_JOB_NAME", &name)
            .env("CI_JOB_ID", id.to_string())
            .env("CI_JOB_TOKEN", format!("job-token-{id}"))
            .env("CI_PROJECT_DIR", &dir);
        for (variable, value) in dotenv {
            command.env(variable, value);
        }
        for (variable, value) in project {
            command.env(variable, value);
        }
        let mut script: Vec<String> = vec!["set -eo pipefail".to_string()];
        for part in ["before_script", "script"] {
            for line in job[part].as_array().unwrap() {
                script.push(line.as_str().unwrap().to_string());
            }
        }
        let output = command.args(["-c", &script.join("\n")]).output().unwrap();
        let code = output.status.code().unwrap();
        org.jobs.alter(CHILD_PIPELINE, &name, |ci_job| {
            ci_job.status = if code == 0 { "success" } else { "failed" };
            ci_job.paths = job["artifacts"]["paths"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|path| path.as_str().unwrap().to_string())
                .collect();
            ci_job.upload = org.uploads.get(&name).cloned().unwrap_or_default();
        });
        done.insert(
            name,
            ChildJob {
                code,
                held_token,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            },
        );
    }
    (plan, pipeline, done)
}

fn copy_tree(from: &Path, to: &Path) {
    if from.is_dir() {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            copy_tree(&entry.path(), &to.join(entry.file_name()));
        }
    } else {
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(from, to).unwrap();
    }
}

#[tokio::test]
async fn the_template_regenerates_lockfiles_in_a_child_pipeline_without_the_token() {
    let (mut org, toolbox) = Org::locking().await;
    uv_project(&mut org, 81, "acme/python", LOCK_OPTED_IN, "uv");
    uv_project(&mut org, 82, "acme/unlocked", OPTED_IN, "uv");
    org.serve().await;

    let (plan, pipeline, jobs) = run_lock_pipeline(
        &org,
        &toolbox,
        &[
            ("group", "acme"),
            ("lock", "true"),
            ("lock_runner_tags", "upd-lock"),
        ],
        &[],
        &[],
    );

    assert_eq!(plan["command"], "gitlab org plan", "{plan:#}");
    assert_eq!(
        plan["handed_off"],
        json!([{"id": 81, "path": "acme/python", "lanes": ["ordinary"]}]),
        "{plan:#}"
    );
    assert_eq!(plan["counts"]["jobs"], 4, "{plan:#}");
    let mut names: Vec<&String> = jobs.keys().collect();
    names.sort();
    assert_eq!(
        names,
        ["lock-81", "organization-run", "prepare-81", "publish-81"]
    );
    for (name, job) in &jobs {
        assert_eq!(job.code, 0, "{name}\n{}\n{}", job.stdout, job.stderr);
        assert_eq!(job.held_token, name != "lock-81", "{name}");
    }
    assert_eq!(pipeline["lock-81"]["tags"], json!(["upd-lock"]));
    // organization-run does what the organization job does without lock
    // mode, so it gets the same time.
    let timeout = pipeline["organization-run"]["timeout"]
        .as_str()
        .unwrap_or("none");
    assert!(
        template_block(".upd-organization-update").contains(&format!("\n  timeout: {timeout}\n")),
        "organization-run timeout {timeout}"
    );
    // The child runs with the settings the organization job read.
    let settings = pipeline["prepare-81"]["script"][0].as_str().unwrap();
    assert!(settings.contains("export UPD_GROUP='acme'\n"), "{settings}");
    assert!(
        settings.contains("export UPD_MAX_BUMP='minor'\n"),
        "{settings}"
    );

    let lock: Value = serde_json::from_str(&jobs["lock-81"].stdout).unwrap();
    assert_eq!(lock["step"], "locked", "{lock:#}");
    let lock = org
        .file_on(81, BRANCH, "uv.lock")
        .expect("the lock job's lockfile was published");
    assert!(lock.contains("version = \"1.1.0\""), "{lock}");
    assert!(org.created_merge_request(81).await.is_some());

    let report: Value = serde_json::from_str(
        &fs::read_to_string(
            org.temp
                .path()
                .join("child/organization-run/.upd-ci/upd-org-report.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        project(&report, "acme/python")["reason"],
        "handed_off",
        "{report:#}"
    );
    let unlocked = project(&report, "acme/unlocked");
    assert_eq!(unlocked["state"], "processed", "{report:#}");
    assert_eq!(unlocked["lockfiles"]["regenerated"], false, "{report:#}");
    assert!(org.file_on(82, BRANCH, "pyproject.toml").is_some());

    // Publish found the lock job with the group token, which GitLab's job
    // token permissions require, and read its archive with its own job
    // token; the group token never went to the artifacts.
    let reads = org.job_api_reads().await;
    assert!(
        reads
            .iter()
            .any(|(path, token)| path.ends_with("/jobs") && token == "PRIVATE-TOKEN test-token"),
        "{reads:?}"
    );
    assert!(
        reads
            .iter()
            .any(|(path, token)| path.ends_with("/artifacts") && token.starts_with("job-token-")),
        "{reads:?}"
    );
    assert!(
        reads
            .iter()
            .filter(|(path, _)| path.ends_with("/artifacts"))
            .all(|(_, token)| token.starts_with("job-token-")),
        "{reads:?}"
    );
}

#[tokio::test]
async fn publish_takes_nothing_the_lock_job_uploads_but_its_result() {
    let hostile = |org: &Org| {
        let marker = org.temp.path().join("hostile-binary-ran");
        [
            Upload {
                files: vec![(
                    ".upd-ci/bin/upd".to_string(),
                    format!("#!/bin/sh\ntouch {}\n", marker.display()).into_bytes(),
                )],
                dotenv: Vec::new(),
            },
            Upload {
                files: Vec::new(),
                dotenv: vec![(
                    "CI_API_V4_URL".to_string(),
                    format!("{}/hostile/api/v4", org.server.uri()),
                )],
            },
        ]
    };
    for case in 0..2 {
        let (mut org, toolbox) = Org::locking().await;
        uv_project(&mut org, 81, "acme/python", LOCK_OPTED_IN, "uv");
        org.serve().await;
        let upload = hostile(&org)[case].clone();
        org.uploads.insert("lock-81".to_string(), upload);

        let (_, _, jobs) = run_lock_pipeline(
            &org,
            &toolbox,
            &[
                ("group", "acme"),
                ("lock", "true"),
                ("lock_runner_tags", "upd-lock"),
            ],
            &[],
            &[],
        );

        let publish = &jobs["publish-81"];
        assert_eq!(
            publish.code, 0,
            "case {case}\n{}\n{}",
            publish.stdout, publish.stderr
        );
        assert!(!org.temp.path().join("hostile-binary-ran").exists());
        let requests = org.server.received_requests().await.unwrap();
        let hostile: Vec<&str> = requests
            .iter()
            .map(|request| request.url.path())
            .filter(|path| path.starts_with("/hostile"))
            .collect();
        assert_eq!(hostile, Vec::<&str>::new(), "case {case}");
        let lock = org
            .file_on(81, BRANCH, "uv.lock")
            .expect("the lock job's lockfile was published");
        assert!(lock.contains("version = \"1.1.0\""), "{lock}");
    }
}

#[tokio::test]
async fn a_lock_mode_dry_run_previews_every_project_in_one_job() {
    let (mut org, toolbox) = Org::locking().await;
    uv_project(&mut org, 81, "acme/python", LOCK_OPTED_IN, "uv");
    org.serve().await;

    let (plan, _, jobs) = run_lock_pipeline(
        &org,
        &toolbox,
        &[
            ("group", "acme"),
            ("lock", "true"),
            ("lock_runner_tags", "upd-lock"),
            ("dry_run", "true"),
        ],
        &[],
        &[],
    );

    assert_eq!(plan["dry_run"], true, "{plan:#}");
    assert_eq!(plan["handed_off"], json!([]), "{plan:#}");
    assert_eq!(jobs.keys().collect::<Vec<_>>(), ["organization-run"]);
    let run = &jobs["organization-run"];
    assert_eq!(run.code, 0, "{}", run.stderr);
    let report: Value = serde_json::from_str(
        &fs::read_to_string(
            org.temp
                .path()
                .join("child/organization-run/.upd-ci/upd-org-report.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(report["dry_run"], true, "{report:#}");
    assert_eq!(
        project(&report, "acme/python")["lockfiles"]["regenerated"],
        true,
        "{report:#}"
    );
    assert_eq!(
        org.file_on(81, BRANCH, "pyproject.toml"),
        None,
        "a dry run pushed"
    );
}

#[test]
fn the_lock_trigger_serializes_runs_and_forwards_nothing() {
    let trigger = template_block(".upd-organization-lock");
    for expected in [
        "  resource_group: upd-organization-update\n",
        "    strategy: mirror\n",
        "      - artifact: .upd-ci/upd-lock-pipeline.yml\n        job: \"$[[ inputs.lock_plan_job ]]\"\n",
        "    - job: \"$[[ inputs.lock_plan_job ]]\"\n      artifacts: true\n",
        "    forward:\n      yaml_variables: false\n      pipeline_variables: false\n",
        "    - if: '$UPD_LOCK != \"true\"'\n      when: never\n",
    ] {
        assert!(trigger.contains(expected), "{expected}\n{trigger}");
    }
    let update = template_block(".upd-organization-update");
    assert!(update.contains("  resource_group: upd-organization-update\n"));
    assert!(
        update.contains(
            "  environment:\n    name: \"$[[ inputs.environment ]]\"\n    action: access\n"
        )
    );
    // The trigger runs in lock mode whenever the organization job runs.
    let rules = |block: &str| -> String {
        block
            .split_once("  rules:\n")
            .unwrap()
            .1
            .lines()
            .take_while(|line| line.starts_with("    "))
            .map(|line| format!("{line}\n"))
            .collect()
    };
    assert_eq!(
        rules(&trigger),
        format!(
            "    - if: '$UPD_LOCK != \"true\"'\n      when: never\n{}",
            rules(&update)
        )
    );
    // The job the trigger includes from runs whenever the trigger does,
    // holds no token, and keeps nothing but the pipeline file.
    let plan = template_block(".upd-organization-lock-plan");
    assert_eq!(rules(&plan), rules(&trigger));
    assert!(!plan.contains("environment:"), "{plan}");
    let paths: Vec<&str> = plan
        .split_once("    paths:\n")
        .unwrap()
        .1
        .lines()
        .map_while(|line| line.strip_prefix("      - "))
        .collect();
    assert_eq!(paths, [".upd-ci/upd-lock-pipeline.yml"], "{plan}");
}

impl Org {
    /// Runs `upd gitlab org plan --output json` as the organization job
    /// named `upd-organization-update` of pipeline [`PIPELINE`] would.
    fn plan(&self, vars: &[(&str, &str)]) -> (i32, Value, String) {
        finish(self.plan_command(vars))
    }

    /// The command [`Org::plan`] runs, for a test to change first.
    fn plan_command(&self, vars: &[(&str, &str)]) -> Command {
        let mut command = self.command(
            env!("CARGO_BIN_EXE_upd"),
            &[
                ("UPD_LOCK", "true"),
                ("CI_PIPELINE_ID", PIPELINE),
                ("CI_JOB_NAME", "upd-organization-update"),
                ("UPD_ORGANIZATION_JOB", "upd-organization-update"),
                ("UPD_IMAGE", "debian:bookworm-slim"),
                ("UPD_LOCK_RUNNER_TAGS", "upd-lock"),
            ],
        );
        for (name, value) in vars {
            command.env(name, value);
        }
        command.args(["gitlab", "org", "plan", "--output", "json"]);
        command
    }
}

/// Lock jobs run repository code with the central project's job token,
/// which GitLab lets push to the central project when the project allows
/// it: such a push could rewrite the pipeline that holds the group token.
/// Planning therefore needs to see that the central project refuses job
/// token pushes, and a dry run, which hands nothing off, does not.
#[tokio::test]
async fn lock_mode_needs_a_central_project_that_refuses_job_token_pushes() {
    /// An organization with one lock project, whose central project GitLab
    /// describes as `central`, or as the default organization does.
    async fn org(central: Option<&Value>) -> Org {
        let mut org = Org::new().await;
        locked_project(&mut org, 96, "acme/locks", LOCK_OPTED_IN);
        org.serve().await;
        if let Some(central) = central {
            Mock::given(method("GET"))
                .and(path(format!("/api/v4/projects/{CENTRAL}").as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_json(central))
                .with_priority(1)
                .mount(&org.server)
                .await;
        }
        org
    }

    let refusing = org(None).await;
    let (code, plan, stderr) = refusing.plan(&[]);
    assert_eq!(code, 0, "{plan:#}\n{stderr}");
    assert_eq!(plan["handed_off"].as_array().unwrap().len(), 1, "{plan:#}");

    for (central, expected) in [
        (
            json!({"id": CENTRAL, "ci_push_repository_for_job_token_allowed": true}),
            "Allow Git push requests to the repository",
        ),
        (json!({"id": CENTRAL}), "Maintainer"),
        (
            json!({"id": CENTRAL, "ci_push_repository_for_job_token_allowed": "false"}),
            "Maintainer",
        ),
    ] {
        let org = org(Some(&central)).await;
        let (code, report, stderr) = org.plan(&[]);
        assert_eq!(code, 2, "{central}\n{report:#}\n{stderr}");
        assert!(stderr.contains(expected), "{central}\n{stderr}");
        assert!(report.get("handed_off").is_none(), "{report:#}");
        let mut dry_run = org.plan_command(&[]);
        dry_run.arg("--dry-run");
        let (code, preview, stderr) = finish(dry_run);
        assert_eq!(
            code, 0,
            "a dry run hands nothing off\n{preview:#}\n{stderr}"
        );
    }

    let mut command = refusing.plan_command(&[]);
    command.env_remove("CI_PROJECT_ID");
    let (code, report, stderr) = finish(command);
    assert_eq!(code, 4, "{report:#}\n{stderr}");
    assert!(stderr.contains("CI_PROJECT_ID"), "{stderr}");
    assert!(report.get("handed_off").is_none(), "{report:#}");
}

#[tokio::test]
async fn plan_gives_each_lock_consenting_lane_its_own_jobs() {
    let mut org = Org::new().await;
    let lock_and_major = "[automation]\ndependency_updates = true\nlock = true\nmajor_mr = true\n";
    locked_project(&mut org, 91, "acme/both", lock_and_major);
    locked_project(&mut org, 92, "acme/locks", LOCK_OPTED_IN);
    locked_project(&mut org, 93, "acme/unlocked", OPTED_IN);
    locked_project(&mut org, 94, "acme/unreadable", LOCK_OPTED_IN);
    org.project(
        95,
        "acme/archived",
        &[Entry::File(".updrc.toml", LOCK_OPTED_IN)],
    )["archived"] = json!(true);
    org.serve().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v4/projects/94/repository/files/"))
        .respond_with(ResponseTemplate::new(403))
        .with_priority(1)
        .mount(&org.server)
        .await;

    let (code, plan, stderr) = org.plan(&[("UPD_MAJOR_MR", "true")]);

    assert_eq!(
        code, 0,
        "an unreadable opt-in is not a planning failure\n{plan:#}\n{stderr}"
    );
    assert_eq!(
        plan["handed_off"],
        json!([
            {"id": 91, "path": "acme/both", "lanes": ["ordinary", "major"]},
            {"id": 92, "path": "acme/locks", "lanes": ["ordinary"]},
        ]),
        "{plan:#}"
    );
    assert_eq!(plan["unplanned"][0]["id"], 94, "{plan:#}");
    assert_eq!(
        plan["unplanned"][0]["error"]["kind"], "api_error",
        "{plan:#}"
    );
    assert_eq!(
        plan["counts"],
        json!({"projects": 5, "handed_off": 2, "lanes": 3, "jobs": 10, "unplanned": 1}),
        "{plan:#}"
    );
    assert!(
        stderr.contains("acme/unreadable: opt-in unreadable, left to organization-run"),
        "{stderr}"
    );
    let pipeline: Value = serde_json::from_str(
        &fs::read_to_string(org.temp.path().join(".upd-ci/upd-lock-pipeline.yml")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        pipeline.as_object().unwrap().len(),
        11,
        "the jobs and the variables"
    );
    assert_eq!(
        pipeline["organization-run"]["script"][1],
        "export UPD_LOCK_HANDED_OFF='91,92'"
    );
    let copied = org.temp.path().join(".upd-ci/bin/upd");
    let version = Command::new(&copied).arg("--version").output().unwrap();
    assert!(version.status.success(), "the copied binary does not run");
    // Planning only reads GitLab.
    for request in org.server.received_requests().await.unwrap() {
        assert_eq!(request.method.as_str(), "GET", "{}", request.url);
    }
    for id in [91, 92, 93] {
        assert_eq!(org.branch_file(id), None, "planning pushed to {id}");
    }

    // The major lane needs the organization's consent too.
    let (code, plan, stderr) = org.plan(&[]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        plan["handed_off"][0]["lanes"],
        json!(["ordinary"]),
        "{plan:#}"
    );
    assert_eq!(plan["counts"]["jobs"], 7, "{plan:#}");

    // A shard plans only its own projects.
    let (code, plan, stderr) = org.plan(&[("UPD_SHARD", "1/2")]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(plan["shard"], "1/2", "{plan:#}");
    assert_eq!(plan["counts"]["projects"], 2, "{plan:#}");
    assert_eq!(
        plan["handed_off"],
        json!([{"id": 92, "path": "acme/locks", "lanes": ["ordinary"]}])
    );
    assert_eq!(plan["unplanned"][0]["id"], 94, "{plan:#}");

    // Without lock mode there is nothing to plan.
    let (code, _, stderr) = org.plan(&[("UPD_LOCK", "false")]);
    assert_eq!(code, 4, "{stderr}");
    assert!(
        stderr.contains("gitlab org plan plans lock mode"),
        "{stderr}"
    );
}

#[tokio::test]
async fn the_child_runs_with_the_planned_settings_whatever_the_project_variables_say() {
    let (mut org, toolbox) = Org::locking().await;
    uv_project(&mut org, 81, "acme/python", LOCK_OPTED_IN, "uv");
    uv_project(&mut org, 82, "acme/unlocked", OPTED_IN, "uv");
    org.serve().await;

    // The central project's own variables would hold every project back and
    // exclude them all; this run's schedule overrides both, and the plan
    // reads the schedule's values. The child, which the schedule's variables
    // do not reach, must run with the planned ones: a set value replaced,
    // and an unset one removed.
    let (plan, _, jobs) = run_lock_pipeline(
        &org,
        &toolbox,
        &[
            ("group", "acme"),
            ("lock", "true"),
            ("lock_runner_tags", "upd-lock"),
        ],
        &[("UPD_MAX_BUMP", "patch"), ("UPD_EXCLUDE", "acme/*")],
        &[("UPD_MAX_BUMP", "minor"), ("UPD_EXCLUDE", "")],
    );

    assert_eq!(
        plan["handed_off"],
        json!([{"id": 81, "path": "acme/python", "lanes": ["ordinary"]}]),
        "{plan:#}"
    );
    for (name, job) in &jobs {
        assert_eq!(job.code, 0, "{name}\n{}\n{}", job.stdout, job.stderr);
    }
    let lock = org
        .file_on(81, BRANCH, "uv.lock")
        .expect("the lock job's lockfile was published");
    assert!(lock.contains("version = \"1.1.0\""), "{lock}");
    let report: Value = serde_json::from_str(
        &fs::read_to_string(
            org.temp
                .path()
                .join("child/organization-run/.upd-ci/upd-org-report.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        project(&report, "acme/unlocked")["state"],
        "processed",
        "{report:#}"
    );
    assert!(org.file_on(82, BRANCH, "pyproject.toml").is_some());
}
