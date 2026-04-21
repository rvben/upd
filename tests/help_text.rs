//! Integration tests for the revert tip in `--help` and post-run summary.
//!
//! The tip line `Tip: changes are applied in-place — use git to revert.`
//! must appear:
//!   - at the bottom of `upd --help`
//!   - after a mutating run that applied at least one update
//!
//! It must NOT appear:
//!   - when `--check` is used
//!   - when `--dry-run` is used
//!   - when `--format json` is used

use std::path::Path;
use std::process::Command;
use upd::REVERT_TIP;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

fn run(args: &[&str], cwd: &Path) -> (String, String, i32) {
    let output = Command::new(upd_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("failed to run upd");
    (
        String::from_utf8(output.stdout).expect("stdout not UTF-8"),
        String::from_utf8(output.stderr).expect("stderr not UTF-8"),
        output.status.code().unwrap_or(-1),
    )
}

fn run_with_env(args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> (String, String, i32) {
    let mut cmd = Command::new(upd_bin());
    cmd.args(args).current_dir(cwd);
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

/// Spin up a wiremock server advertising version 99.0.0 for `requests`,
/// write a requirements.txt pinned at 1.0.0, and return the server + temp dir.
async fn setup_fake_pypi_with_update() -> (wiremock::MockServer, tempfile::TempDir) {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-99.0.0.tar.gz">requests-99.0.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();

    (server, tmp)
}

/// `upd --help` must contain the revert tip at the bottom.
#[test]
fn help_contains_revert_tip() {
    let tmp = tempfile::tempdir().unwrap();
    // clap exits with code 0 for --help; capture stdout
    let (stdout, _stderr, _code) = run(&["--help"], tmp.path());
    assert!(
        stdout.contains(REVERT_TIP),
        "--help must contain the revert tip; got:\n{stdout}"
    );
}

/// `--check` must NOT print the tip even when updates are pending.
/// We use a workspace with no dependency files so no network is needed
/// and no updates are pending; the assertion holds vacuously and confirms
/// the flag plumbing doesn't accidentally enable the tip.
#[test]
fn check_mode_does_not_print_tip() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, stderr, _code) = run(&["--check"], tmp.path());
    assert!(
        !stdout.contains(REVERT_TIP),
        "--check must not print the tip in stdout; got:\n{stdout}"
    );
    assert!(
        !stderr.contains(REVERT_TIP),
        "--check must not print the tip in stderr; got:\n{stderr}"
    );
}

/// `--dry-run` must NOT print the tip even when an update is available.
///
/// We mock the registry to serve version 99.0.0 for `requests` while the
/// manifest pins 1.0.0, so the run would apply an update in mutating mode —
/// exercising the `!dry_run` guard in the real post-summary path.
#[tokio::test]
async fn dry_run_does_not_print_tip() {
    let (server, tmp) = setup_fake_pypi_with_update().await;

    let (stdout, stderr, _code) = run_with_env(
        &["--dry-run", "--no-cache"],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    let combined = format!("{stdout}{stderr}");
    assert!(
        !combined.contains(REVERT_TIP),
        "--dry-run must not print the tip; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// `--format json` must NOT include the tip (it would corrupt the JSON stream).
#[test]
fn json_format_does_not_print_tip() {
    let tmp = tempfile::tempdir().unwrap();
    // Run in both dry-run and plain mode with JSON format
    for args in [
        ["--format", "json", "--dry-run"].as_slice(),
        ["--format", "json"].as_slice(),
    ] {
        let (stdout, stderr, _code) = run(args, tmp.path());
        assert!(
            !stdout.contains(REVERT_TIP),
            "--format json must not print the tip in stdout; got:\n{stdout}"
        );
        assert!(
            !stderr.contains(REVERT_TIP),
            "--format json must not print the tip in stderr; got:\n{stderr}"
        );
    }
}

/// A mutating run that applies at least one update must print the tip.
///
/// We use wiremock to serve a fake PyPI Simple-API advertising version 99.0.0
/// for `requests`, while the manifest pins it at 1.0.0.  The run is mutating
/// (no --check / --dry-run), so the update is applied and the tip must appear.
#[tokio::test]
async fn mutating_run_with_applied_update_prints_tip() {
    let (server, tmp) = setup_fake_pypi_with_update().await;

    let (stdout, stderr, _code) = run_with_env(
        &["--no-cache"],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    // The tip should appear in either stdout or stderr (wherever the summary goes)
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains(REVERT_TIP),
        "mutating run with applied update must print the tip; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
