//! `upd --apply --lock` is a transaction per manifest group: when the
//! lockfile refresh fails, the manifest and every lockfile it owns are
//! restored byte for byte, stderr names what was rolled back, and the JSON
//! report marks each entry `rolled_back` instead of counting it as an
//! applied update. A failed refresh used to leave the manifest rewritten and
//! the lockfile stale while `updates_total` claimed the update had landed.
//!
//! The same fake-tool harness covers the lock commands whose shape depends
//! on the installed tool: Poetry 2 removed `poetry lock --no-update`, and
//! bun writes the text `bun.lock` and has a `--lockfile-only` form.
//!
//! The fake tools are POSIX shell scripts, so this file is unix-only.
#![cfg(unix)]

mod common;

use common::{run_on_a_terminal, upd_bin};
use std::fs;
use std::path::Path;
use std::process::Command;

fn run_with_env(args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> (String, String, i32) {
    let mut cmd = Command::new(upd_bin());
    cmd.args(args).current_dir(cwd).env("NO_COLOR", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to run upd");
    (
        String::from_utf8(output.stdout).expect("stdout not UTF-8"),
        String::from_utf8(output.stderr).expect("stderr not UTF-8"),
        output.status.code().unwrap_or(-1),
    )
}

fn write_fake_tool(bin_dir: &Path, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = bin_dir.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn path_with(bin_dir: &Path) -> String {
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

const PYPROJECT: &str =
    "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = [\"requests==2.31.0\"]\n";
const PYPROJECT_UPDATED: &str =
    "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = [\"requests==2.32.0\"]\n";

/// A `uv.lock` holding exactly the given `(package, version)` entries.
fn uv_lock_with(packages: &[(&str, &str)]) -> String {
    let mut lock = String::from("version = 1\n");
    for (package, version) in packages {
        lock.push_str(&format!(
            "\n[[package]]\nname = \"{package}\"\nversion = \"{version}\"\nsource = {{ registry = \"https://pypi.org/simple\" }}\n"
        ));
    }
    lock
}

fn uv_lock_at(package: &str, version: &str) -> String {
    uv_lock_with(&[(package, version)])
}

/// Mounts `/simple/{name}/` -> 404 (forces the legacy JSON API fallback) and
/// `/pypi/{name}/json` -> a single-release `releases` body.
async fn mount_pypi_latest(server: &wiremock::MockServer, name: &str, version: &str) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/simple/{name}/")))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/pypi/{name}/json")))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "releases": {
                    version: [{"yanked": false, "upload_time_iso_8601": "2024-01-01T00:00:00Z"}]
                }
            })),
        )
        .mount(server)
        .await;
}

/// Mounts `GET {registry}/{name}` with an abbreviated npm metadata document.
async fn mount_npm_latest(server: &wiremock::MockServer, name: &str, version: &str) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/{name}")))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": name,
                "dist-tags": { "latest": version },
                "versions": { version: { "name": name, "version": version } }
            })),
        )
        .mount(server)
        .await;
}

/// A `uv` whose `lock` fails the way a resolver does: a message on stderr
/// and a non-zero exit. `--version` answers so the tool probe finds it.
const UV_FAILING: &str = "#!/bin/sh
case \"$1\" in
  --version) echo 'uv 0.9.0'; exit 0 ;;
  lock) echo 'error: resolver blew up' >&2; exit 1 ;;
esac
exit 0
";

/// A `uv` whose `lock` succeeds and leaves a recognisable lockfile behind.
const UV_WRITING: &str = "#!/bin/sh
case \"$1\" in
  --version) echo 'uv 0.9.0'; exit 0 ;;
  lock) printf 'regenerated\\n' > uv.lock; exit 0 ;;
esac
exit 0
";

/// A `uv` that succeeds everywhere except in a directory named `b`.
const UV_FAILING_IN_B: &str = "#!/bin/sh
case \"$1\" in
  --version) echo 'uv 0.9.0'; exit 0 ;;
  lock)
    case \"$(pwd -P)\" in
      */b) echo 'error: b is broken' >&2; exit 1 ;;
    esac
    printf 'regenerated\\n' > uv.lock; exit 0 ;;
esac
exit 0
";

/// Lays out `<root>/bin/<tool>` and `<root>/project/{pyproject.toml,uv.lock}`
/// and returns `(bin, project, original lockfile bytes)`.
fn python_project(
    root: &Path,
    uv_script: &str,
) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    write_fake_tool(&bin, "uv", uv_script);
    let project = root.join("project");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("pyproject.toml"), PYPROJECT).unwrap();
    let lock = uv_lock_at("requests", "2.31.0");
    fs::write(project.join("uv.lock"), &lock).unwrap();
    (bin, project, lock)
}

#[tokio::test]
async fn a_failed_refresh_restores_the_manifest_and_lockfile_bytes() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "requests", "2.32.0").await;
    let tmp = tempfile::tempdir().unwrap();
    let (bin, project, lock) = python_project(tmp.path(), UV_FAILING);

    let (stdout, stderr, code) = run_with_env(
        &["--apply", "--lock", "--no-cache", "--format", "text", "."],
        &project,
        &[("UV_INDEX_URL", &server.uri()), ("PATH", &path_with(&bin))],
    );

    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        fs::read_to_string(project.join("pyproject.toml")).unwrap(),
        PYPROJECT,
        "the manifest must be restored after a failed refresh\nstderr: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(project.join("uv.lock")).unwrap(),
        lock,
        "the lockfile must be restored after a failed refresh"
    );
    assert!(
        stderr.contains("Failed to regenerate uv.lock: error: resolver blew up"),
        "the tool's own stderr must be reported: {stderr}"
    );
    assert!(
        stderr.contains("rolled back pyproject.toml and uv.lock"),
        "the rollback must be named: {stderr}"
    );
    assert!(
        !stdout.contains("Updated 1 package"),
        "the summary must not count a rolled-back update: {stdout}"
    );
}

/// Positive control for the harness: the same fixture with a `uv` that
/// succeeds keeps the manifest edit and the refreshed lockfile.
#[tokio::test]
async fn a_successful_refresh_keeps_the_update() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "requests", "2.32.0").await;
    let tmp = tempfile::tempdir().unwrap();
    let (bin, project, _) = python_project(tmp.path(), UV_WRITING);

    let (stdout, stderr, code) = run_with_env(
        &["--apply", "--lock", "--no-cache", "--format", "text", "."],
        &project,
        &[("UV_INDEX_URL", &server.uri()), ("PATH", &path_with(&bin))],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        fs::read_to_string(project.join("pyproject.toml")).unwrap(),
        PYPROJECT_UPDATED
    );
    assert_eq!(
        fs::read_to_string(project.join("uv.lock")).unwrap(),
        "regenerated\n"
    );
    assert!(stdout.contains("Regenerated uv.lock"), "{stdout}");
}

/// An interactive session writes the approved update itself and refreshes
/// the lockfile under `--lock`, so the same transaction holds there: a failed
/// refresh puts the manifest and lockfile back, the session says so, and the
/// run exits 2 rather than counting the update as applied.
#[tokio::test]
async fn an_interactive_session_rolls_back_a_failed_refresh() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "requests", "2.32.0").await;
    let tmp = tempfile::tempdir().unwrap();
    let (bin, project, lock) = python_project(tmp.path(), UV_FAILING);

    let (code, output) = run_on_a_terminal(
        &["--interactive", "--lock", "--no-cache", "."],
        &project,
        &[
            ("UV_INDEX_URL", &server.uri()),
            ("PATH", &path_with(&bin)),
            ("NO_COLOR", "1"),
        ],
        "y\n",
    );

    assert_eq!(code, 2, "{output}");
    assert_eq!(
        fs::read_to_string(project.join("pyproject.toml")).unwrap(),
        PYPROJECT,
        "the manifest must be restored after a failed refresh\n{output}"
    );
    assert_eq!(
        fs::read_to_string(project.join("uv.lock")).unwrap(),
        lock,
        "the lockfile must be restored after a failed refresh"
    );
    assert!(
        output.contains("rolled back pyproject.toml and uv.lock"),
        "the rollback must be named: {output}"
    );
    assert!(
        !output.contains("Updated 1 package"),
        "the summary must not count a rolled-back update: {output}"
    );
}

#[tokio::test]
async fn a_rolled_back_update_is_reported_as_rolled_back_in_json() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "requests", "2.32.0").await;
    let tmp = tempfile::tempdir().unwrap();
    let (bin, project, lock) = python_project(tmp.path(), UV_FAILING);

    let (stdout, stderr, code) = run_with_env(
        &["--apply", "--lock", "--no-cache", "--format", "json", "."],
        &project,
        &[("UV_INDEX_URL", &server.uri()), ("PATH", &path_with(&bin))],
    );

    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let file = &json["files"][0];
    let update = &file["updates"][0];
    assert_eq!(update["package"], "requests", "{file}");
    assert_eq!(update["latest"], "2.32.0", "{file}");
    assert_eq!(update["status"], "rolled_back", "{file}");
    assert!(
        update["error"]
            .as_str()
            .is_some_and(|e| e.contains("Failed to regenerate uv.lock: error: resolver blew up")),
        "{file}"
    );
    let error = &file["errors"][0];
    assert_eq!(error["kind"], "lockfile", "{file}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("rolled back pyproject.toml and uv.lock")),
        "{file}"
    );
    let summary = &json["summary"];
    assert_eq!(summary["updates_total"], 0, "{summary}");
    assert_eq!(summary["files_with_changes"], 0, "{summary}");
    assert_eq!(summary["errors"], 1, "{summary}");

    assert_eq!(
        fs::read_to_string(project.join("pyproject.toml")).unwrap(),
        PYPROJECT
    );
    assert_eq!(fs::read_to_string(project.join("uv.lock")).unwrap(), lock);
}

/// A `uv` whose `lock` makes the manifest read-only before failing, the way
/// a tool that chmods files or a filesystem going read-only mid-run would.
/// The manifest then cannot be restored.
const UV_FAILING_AFTER_LOCKING_THE_MANIFEST: &str = "#!/bin/sh
case \"$1\" in
  --version) echo 'uv 0.9.0'; exit 0 ;;
  lock) chmod 444 pyproject.toml; echo 'error: resolver blew up' >&2; exit 1 ;;
esac
exit 0
";

/// The disk state after a refresh failed and the manifest could not be put
/// back: the manifest carries the update and the lockfile is the pre-run one,
/// the inconsistency the transaction exists to prevent.
fn assert_manifest_ahead_of_its_lockfile(project: &Path, lock: &str) {
    assert_eq!(
        fs::read_to_string(project.join("pyproject.toml")).unwrap(),
        PYPROJECT_UPDATED,
        "the read-only manifest cannot be restored, so it keeps the update"
    );
    assert_eq!(
        fs::read_to_string(project.join("uv.lock")).unwrap(),
        lock,
        "the lockfile was never rewritten"
    );
}

/// When a file in the directory cannot be put back, the directory is neither
/// at its pre-run bytes nor consistent, so no write in it is reported as
/// applied or as rolled back: every entry is `failed`, the error names the
/// file that was not restored, and the totals exclude the directory.
#[tokio::test]
async fn a_file_that_cannot_be_restored_fails_every_write_in_its_directory() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "requests", "2.32.0").await;
    let tmp = tempfile::tempdir().unwrap();
    let (bin, project, lock) = python_project(tmp.path(), UV_FAILING_AFTER_LOCKING_THE_MANIFEST);

    let (stdout, stderr, code) = run_with_env(
        &["--apply", "--lock", "--no-cache", "--format", "json", "."],
        &project,
        &[("UV_INDEX_URL", &server.uri()), ("PATH", &path_with(&bin))],
    );

    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert_manifest_ahead_of_its_lockfile(&project, &lock);
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let file = &json["files"][0];
    let update = &file["updates"][0];
    assert_eq!(update["package"], "requests", "{file}");
    assert_eq!(
        update["status"], "failed",
        "the manifest is ahead of its lockfile, which is neither applied nor rolled back: {file}"
    );
    let names_the_unrestored_file = |text: &str| text.contains("pyproject.toml was not restored");
    assert!(
        update["error"]
            .as_str()
            .is_some_and(names_the_unrestored_file),
        "{file}"
    );
    let error = &file["errors"][0];
    assert_eq!(error["kind"], "lockfile", "{file}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(names_the_unrestored_file),
        "{file}"
    );
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("Failed to regenerate uv.lock: error: resolver blew up")),
        "{file}"
    );
    let summary = &json["summary"];
    assert_eq!(summary["updates_total"], 0, "{summary}");
    assert_eq!(summary["files_with_changes"], 0, "{summary}");
    assert_eq!(summary["errors"], 1, "{summary}");
    assert!(
        stderr.contains("pyproject.toml was not restored"),
        "the file that could not be put back must be named: {stderr}"
    );
}

/// The interactive session applies the same rule: a directory it could not
/// put back is not counted as updated.
#[tokio::test]
async fn an_interactive_session_does_not_count_a_directory_it_could_not_restore() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "requests", "2.32.0").await;
    let tmp = tempfile::tempdir().unwrap();
    let (bin, project, lock) = python_project(tmp.path(), UV_FAILING_AFTER_LOCKING_THE_MANIFEST);

    let (code, output) = run_on_a_terminal(
        &["--interactive", "--lock", "--no-cache", "."],
        &project,
        &[
            ("UV_INDEX_URL", &server.uri()),
            ("PATH", &path_with(&bin)),
            ("NO_COLOR", "1"),
        ],
        "y\n",
    );

    assert_eq!(code, 2, "{output}");
    assert_manifest_ahead_of_its_lockfile(&project, &lock);
    assert!(
        output.contains("pyproject.toml was not restored"),
        "the file that could not be put back must be named: {output}"
    );
    assert!(
        !output.contains("Updated 1 package"),
        "a manifest ahead of its lockfile is not an applied update: {output}"
    );
}

/// Two projects in one run: a refresh that fails in `b` rolls `b` back and
/// leaves `a`, whose refresh succeeded, applied.
#[tokio::test]
async fn only_the_group_whose_refresh_failed_is_rolled_back() {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "requests", "2.32.0").await;
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    write_fake_tool(&bin, "uv", UV_FAILING_IN_B);
    let root = tmp.path().join("root");
    let lock = uv_lock_at("requests", "2.31.0");
    for name in ["a", "b"] {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pyproject.toml"), PYPROJECT).unwrap();
        fs::write(dir.join("uv.lock"), &lock).unwrap();
    }

    let (stdout, stderr, code) = run_with_env(
        &["--apply", "--lock", "--no-cache", "--format", "json", "."],
        &root,
        &[("UV_INDEX_URL", &server.uri()), ("PATH", &path_with(&bin))],
    );

    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        fs::read_to_string(root.join("a/pyproject.toml")).unwrap(),
        PYPROJECT_UPDATED,
        "a's refresh succeeded, so its update stays applied"
    );
    assert_eq!(
        fs::read_to_string(root.join("a/uv.lock")).unwrap(),
        "regenerated\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("b/pyproject.toml")).unwrap(),
        PYPROJECT,
        "b's refresh failed, so its manifest is restored"
    );
    assert_eq!(fs::read_to_string(root.join("b/uv.lock")).unwrap(), lock);

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let files = json["files"].as_array().unwrap();
    let status_of = |suffix: &str| {
        files
            .iter()
            .find(|f| f["path"].as_str().is_some_and(|p| p.ends_with(suffix)))
            .map(|f| f["updates"][0]["status"].clone())
            .unwrap_or_else(|| panic!("no report for {suffix}: {files:?}"))
    };
    assert!(status_of("a/pyproject.toml").is_null(), "{files:?}");
    assert_eq!(status_of("b/pyproject.toml"), "rolled_back", "{files:?}");
    assert_eq!(json["summary"]["updates_total"], 1, "{}", json["summary"]);
    assert_eq!(
        json["summary"]["files_with_changes"], 1,
        "{}",
        json["summary"]
    );
    assert_eq!(json["summary"]["errors"], 1, "{}", json["summary"]);
}

/// A `uv` whose `lock` counts its calls in `count_file` and fails on call
/// number `failing_call`; every other call leaves `lock` behind and exits 0.
fn uv_failing_on_call(failing_call: u32, count_file: &Path, lock: &str) -> String {
    format!(
        "#!/bin/sh
case \"$1\" in
  --version) echo 'uv 0.9.0'; exit 0 ;;
  lock)
    N=$(( $(cat '{count}' 2>/dev/null || echo 0) + 1 ))
    echo $N > '{count}'
    if [ \"$N\" = \"{failing_call}\" ]; then
      echo 'error: relock {failing_call} blew up' >&2
      exit 1
    fi
    cat > uv.lock <<'EOF'
{lock}EOF
    exit 0
    ;;
esac
exit 0
",
        count = count_file.display(),
    )
}

/// What `upd --package requests,lockonly --apply --lock` leaves behind when
/// one project needs both an ordinary update (`requests`, declared in the
/// manifest) and a version floor (`lockonly`, present only in `uv.lock`).
struct TwoRelocks {
    code: i32,
    stdout: String,
    stderr: String,
    json: serde_json::Value,
    pyproject: String,
    lock: String,
    /// The lockfile a successful `uv lock` writes.
    relocked: String,
    /// How many times `uv lock` ran.
    relocks: u32,
}

/// Runs the two-relock scenario with a `uv` that fails on `failing_call`.
/// The ordinary update and the floor each relock once, so one of the two
/// relocks succeeds and the other fails whichever order they run in.
async fn two_relocks_one_failing(failing_call: u32) -> TwoRelocks {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "requests", "2.32.0").await;
    mount_pypi_latest(&server, "lockonly", "0.49.1").await;
    let tmp = tempfile::tempdir().unwrap();
    let count_file = tmp.path().join("uv-lock-calls");
    // The resolver's answer for the rewritten manifest: it re-resolves
    // `requests` and moves the transitive `lockonly` one patch, so the version
    // the floor is planned from tells which lockfile the floor branch read.
    let relocked = uv_lock_with(&[("requests", "2.32.0"), ("lockonly", "0.41.0")]);
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    write_fake_tool(
        &bin,
        "uv",
        &uv_failing_on_call(failing_call, &count_file, &relocked),
    );
    let project = tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("pyproject.toml"), PYPROJECT).unwrap();
    fs::write(
        project.join("uv.lock"),
        uv_lock_with(&[("requests", "2.31.0"), ("lockonly", "0.40.0")]),
    )
    .unwrap();

    let (stdout, stderr, code) = run_with_env(
        &[
            "--package",
            "requests,lockonly",
            "--apply",
            "--lock",
            "--no-cache",
            "--format",
            "json",
            ".",
        ],
        &project,
        &[("UV_INDEX_URL", &server.uri()), ("PATH", &path_with(&bin))],
    );
    let json = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{stdout}\nstderr: {stderr}"));
    let relocks = fs::read_to_string(&count_file)
        .map(|n| n.trim().parse().unwrap())
        .unwrap_or(0);
    TwoRelocks {
        code,
        stdout,
        stderr,
        json,
        pyproject: fs::read_to_string(project.join("pyproject.toml")).unwrap(),
        lock: fs::read_to_string(project.join("uv.lock")).unwrap(),
        relocked,
        relocks,
    }
}

/// The report entry for `package`, wherever the report files it.
fn report_entry<'a>(json: &'a serde_json::Value, package: &str) -> &'a serde_json::Value {
    json["files"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|f| f["updates"].as_array().into_iter().flatten())
        .find(|u| u["package"] == package)
        .unwrap_or_else(|| panic!("no report entry for {package}: {json}"))
}

/// Whether an entry says its write is on disk: an ordinary update reports
/// no status when applied, a floor reports `applied`.
fn reported_applied(entry: &serde_json::Value) -> bool {
    match entry["status"].as_str() {
        None | Some("applied") => true,
        Some("rolled_back") => false,
        Some(other) => panic!("unexpected status {other:?}: {entry}"),
    }
}

/// Every entry the report calls applied is on disk and every entry it calls
/// rolled back is not, whichever of the two relocks failed. The ordinary
/// update and the floor each run their own snapshot-and-restore; a rollback
/// by one must never undo a write the other has already reported as applied.
fn assert_report_matches_disk(run: &TwoRelocks) {
    let TwoRelocks {
        code,
        stdout,
        stderr,
        json,
        pyproject,
        lock,
        relocked,
        relocks,
    } = run;
    assert_eq!(
        *relocks, 2,
        "both the update and the floor must relock\nstderr: {stderr}"
    );
    assert_eq!(
        *code, 2,
        "one relock failed\nstdout: {stdout}\nstderr: {stderr}"
    );

    let requests = report_entry(json, "requests");
    let lockonly = report_entry(json, "lockonly");
    assert_eq!(lockonly["method"], "uv-constraint", "{lockonly}");
    assert_ne!(
        reported_applied(requests),
        reported_applied(lockonly),
        "one relock succeeded and one failed, so exactly one entry is applied: {json}"
    );

    let constraint = r#"constraint-dependencies = ["lockonly>=0.49.1"]"#;
    if reported_applied(requests) {
        assert!(
            pyproject.contains("requests==2.32.0"),
            "requests is reported applied but the manifest on disk reads:\n{pyproject}"
        );
    } else {
        assert!(
            pyproject.contains("requests==2.31.0"),
            "requests is reported rolled back but the manifest on disk reads:\n{pyproject}"
        );
    }
    if reported_applied(lockonly) {
        assert!(
            pyproject.contains(constraint),
            "lockonly is reported applied but the manifest on disk has no floor:\n{pyproject}"
        );
    } else {
        assert!(
            !pyproject.contains("constraint-dependencies"),
            "lockonly is reported rolled back but its floor is still on disk:\n{pyproject}"
        );
    }
    assert_eq!(
        lock, relocked,
        "the lockfile on disk must be the one the successful relock wrote"
    );

    // The floor is planned from the lockfile as the update's relock left it:
    // the relocked one when that relock succeeded, the pre-run one when it
    // was rolled back. A floor planned from a lockfile the run has since
    // rewritten would report a version that is no longer on disk.
    let lockonly_on_disk_when_planned = if reported_applied(requests) {
        "0.41.0"
    } else {
        "0.40.0"
    };
    assert_eq!(
        lockonly["current"], lockonly_on_disk_when_planned,
        "the floor must be planned from the lockfile the update's relock left behind: {lockonly}"
    );

    let summary = &json["summary"];
    assert_eq!(summary["updates_total"], 1, "{summary}");
    assert_eq!(summary["files_with_changes"], 1, "{summary}");
}

#[tokio::test]
async fn a_floor_reported_as_applied_survives_the_other_relock_failing() {
    assert_report_matches_disk(&two_relocks_one_failing(2).await);
}

/// Mirror image: the first relock fails and the second succeeds.
#[tokio::test]
async fn an_update_reported_as_applied_survives_the_other_relock_failing() {
    assert_report_matches_disk(&two_relocks_one_failing(1).await);
}

/// A `poetry` that runs the shell snippet `version_probe` for `--version`
/// and appends every other invocation's arguments to `$FAKE_LOG`.
fn poetry_script(version_probe: &str) -> String {
    format!(
        "#!/bin/sh
case \"$1\" in
  --version) {version_probe} ;;
esac
echo \"$*\" >> \"$FAKE_LOG\"
exit 0
"
    )
}

/// Runs `upd --apply --lock` against a Poetry project whose `poetry` answers
/// `--version` with the shell snippet `version_probe`, and returns the lock
/// invocations the fake recorded.
async fn poetry_lock_invocations(version_probe: &str) -> String {
    let server = wiremock::MockServer::start().await;
    mount_pypi_latest(&server, "requests", "2.32.0").await;
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    write_fake_tool(&bin, "poetry", &poetry_script(version_probe));
    let project = tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("pyproject.toml"), PYPROJECT).unwrap();
    fs::write(project.join("poetry.lock"), "# poetry\n").unwrap();
    let log = tmp.path().join("poetry.log");

    let (stdout, stderr, code) = run_with_env(
        &["--apply", "--lock", "--no-cache", "--format", "text", "."],
        &project,
        &[
            ("UV_INDEX_URL", &server.uri()),
            ("PATH", &path_with(&bin)),
            ("FAKE_LOG", log.to_str().unwrap()),
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    fs::read_to_string(&log).unwrap_or_else(|e| panic!("poetry was never invoked: {e}"))
}

#[tokio::test]
async fn poetry_2_locks_without_the_removed_no_update_flag() {
    assert_eq!(
        poetry_lock_invocations("echo 'Poetry (version 2.2.1)'; exit 0").await,
        "lock\n"
    );
}

/// A `poetry --version` that fails leaves the major unknown, and the flagged
/// form is used: on Poetry 2 it fails loudly, where a bare `poetry lock` on
/// Poetry 1 would silently refresh every package.
#[tokio::test]
async fn a_poetry_whose_version_cannot_be_read_gets_no_update() {
    assert_eq!(
        poetry_lock_invocations("exit 1").await,
        "lock --no-update\n"
    );
}

#[tokio::test]
async fn poetry_1_locks_with_no_update() {
    assert_eq!(
        poetry_lock_invocations("echo 'Poetry (version 1.8.5)'; exit 0").await,
        "lock --no-update\n"
    );
}

/// Runs `upd --apply --lock` against a bun project whose lockfile is named
/// `lockfile`, with a `bun` that records every invocation. Returns the run's
/// stderr and the invocations recorded.
async fn bun_refresh_of(lockfile: &str) -> (String, String) {
    let server = wiremock::MockServer::start().await;
    mount_npm_latest(&server, "left-pad", "1.3.0").await;
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    write_fake_tool(
        &bin,
        "bun",
        "#!/bin/sh
case \"$1\" in
  --version) echo '1.3.14'; exit 0 ;;
esac
echo \"$*\" >> \"$FAKE_LOG\"
exit 0
",
    );
    let project = tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::write(
        project.join("package.json"),
        "{\n  \"dependencies\": {\n    \"left-pad\": \"1.0.0\"\n  }\n}\n",
    )
    .unwrap();
    fs::write(project.join(lockfile), "lockfile bytes\n").unwrap();
    let log = tmp.path().join("bun.log");

    let (stdout, stderr, code) = run_with_env(
        &["--apply", "--lock", "--no-cache", "--format", "text", "."],
        &project,
        &[
            ("NPM_REGISTRY", &server.uri()),
            ("PATH", &path_with(&bin)),
            ("FAKE_LOG", log.to_str().unwrap()),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let invocations =
        fs::read_to_string(&log).unwrap_or_else(|e| panic!("bun was never invoked: {e}"));
    (stderr, invocations)
}

#[tokio::test]
async fn bun_refreshes_the_text_lockfile_without_installing() {
    let (stderr, invocations) = bun_refresh_of("bun.lock").await;

    assert!(
        !stderr.contains("no lockfile found"),
        "bun.lock must be detected as bun's lockfile: {stderr}"
    );
    assert_eq!(invocations, "install --lockfile-only\n");
}

/// The binary lockfile from before bun 1.2 is refreshed the same way; bun
/// keeps whichever format the project already has.
#[tokio::test]
async fn bun_refreshes_the_binary_lockfile_without_installing() {
    let (stderr, invocations) = bun_refresh_of("bun.lockb").await;

    assert!(
        !stderr.contains("no lockfile found"),
        "bun.lockb must be detected as bun's lockfile: {stderr}"
    );
    assert_eq!(invocations, "install --lockfile-only\n");
}

/// An `npm` whose refresh succeeds and leaves a recognisable lockfile behind.
const NPM_WRITING: &str = "#!/bin/sh
case \"$1\" in
  --version) echo '11.0.0'; exit 0 ;;
esac
printf 'regenerated\\n' > package-lock.json
exit 0
";

/// A `yarn` whose refresh fails the way a resolver does.
const YARN_FAILING: &str = "#!/bin/sh
case \"$1\" in
  --version) echo '4.0.0'; exit 0 ;;
esac
echo 'error: yarn is broken' >&2
exit 1
";

/// Two lockfiles beside one manifest form one group. When the second refresh
/// fails, the first lockfile is put back along with everything else, so the
/// run must not announce it as regenerated.
#[tokio::test]
async fn a_lockfile_regenerated_before_the_rollback_is_not_announced() {
    let server = wiremock::MockServer::start().await;
    mount_npm_latest(&server, "left-pad", "1.3.0").await;
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    write_fake_tool(&bin, "npm", NPM_WRITING);
    write_fake_tool(&bin, "yarn", YARN_FAILING);
    let project = tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    let manifest = "{\n  \"dependencies\": {\n    \"left-pad\": \"1.0.0\"\n  }\n}\n";
    let package_lock = "{\n  \"name\": \"t\",\n  \"lockfileVersion\": 3,\n  \"packages\": {}\n}\n";
    fs::write(project.join("package.json"), manifest).unwrap();
    fs::write(project.join("package-lock.json"), package_lock).unwrap();
    fs::write(project.join("yarn.lock"), "# yarn lockfile v1\n").unwrap();

    let (stdout, stderr, code) = run_with_env(
        &["--apply", "--lock", "--no-cache", "--format", "text", "."],
        &project,
        &[("NPM_REGISTRY", &server.uri()), ("PATH", &path_with(&bin))],
    );

    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        fs::read_to_string(project.join("package.json")).unwrap(),
        manifest,
        "the manifest must be restored after a failed refresh\nstderr: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(project.join("package-lock.json")).unwrap(),
        package_lock,
        "a lockfile regenerated before the failure must be put back with its group"
    );
    assert!(
        stderr.contains("rolled back package.json, package-lock.json and yarn.lock"),
        "the rollback must name every file it put back: {stderr}"
    );
    assert!(
        !stdout.contains("Regenerated package-lock.json"),
        "a lockfile that was put back must not be announced as regenerated: {stdout}"
    );
}
