//! `upd --apply --lock` end to end, with fake package managers on PATH.
//! The lock command a refresh runs depends on the installed tool, and the
//! only way to see the command line is to record what the tool received:
//! Poetry 2 removed `poetry lock --no-update`, so the flag upd passed
//! Poetry 1 now fails the refresh outright, and bun writes the text
//! `bun.lock` and has a `--lockfile-only` form.
//!
//! The fake tools are POSIX shell scripts, so this file is unix-only.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

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
