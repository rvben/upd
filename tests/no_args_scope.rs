//! Integration tests for VCS-root scoping and --apply gate (C1).
//!
//! Behaviour contract:
//!   - no args, inside git repo   → dry-run scoped to VCS root, exit 1 if pending, "Run with --apply"
//!   - no args, outside git repo  → exit 2, stderr "not inside a git repository"
//!   - --apply inside git repo    → actually mutates
//!   - --check outside git repo   → exit 2, no VCS root to scope to
//!   - explicit path, no --apply  → dry-run, exit 1 if pending, "Run with --apply"
//!   - explicit path + --apply    → mutates
//!   - -i explicit path, no TTY   → exit 2 with TTY error, NOT "not inside a git repository"

use std::fs;
use std::path::Path;
use std::process::Command;

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

fn run_with_null_stdin(args: &[&str], cwd: &Path) -> (String, String, i32) {
    let stdin_null = fs::File::open(if cfg!(target_os = "windows") {
        "NUL"
    } else {
        "/dev/null"
    })
    .expect("could not open null device");

    let output = Command::new(upd_bin())
        .args(args)
        .current_dir(cwd)
        .stdin(stdin_null)
        .output()
        .expect("failed to run upd");
    (
        String::from_utf8(output.stdout).expect("stdout not UTF-8"),
        String::from_utf8(output.stderr).expect("stderr not UTF-8"),
        output.status.code().unwrap_or(-1),
    )
}

/// Initialise a bare git repo in the given directory.
fn git_init(dir: &Path) {
    let status = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(dir)
        .output()
        .expect("failed to run git init");
    assert!(
        status.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

/// Spin up a wiremock server advertising version 99.0.0 for `requests`.
async fn setup_fake_pypi() -> wiremock::MockServer {
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

    server
}

// ── test 1: no args inside git repo → implicit dry-run, "Run with --apply" ───

/// `upd` with no args inside a git repo:
///   - treats repo root as discovery scope
///   - runs as dry-run (no file mutations)
///   - prints "Run with --apply" when updates are pending
///   - exits 1
#[tokio::test]
async fn no_args_inside_git_repo_is_dry_run_with_hint() {
    let server = setup_fake_pypi().await;

    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());

    let req_path = tmp.path().join("requirements.txt");
    fs::write(&req_path, "requests==1.0.0\n").unwrap();

    let (stdout, stderr, code) = run_with_env(
        &["--no-cache"],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    let combined = format!("{stdout}{stderr}");

    assert_eq!(
        code, 1,
        "no-args inside git repo with pending updates must exit 1; combined:\n{combined}"
    );

    // Must not have mutated the file
    let contents_after = fs::read_to_string(&req_path).expect("could not read fixture");
    assert_eq!(
        contents_after, "requests==1.0.0\n",
        "file must not be mutated in implicit dry-run mode"
    );

    // Must print the --apply hint
    assert!(
        combined.contains("--apply"),
        "output must mention --apply; combined:\n{combined}"
    );
}

// ── test 2: no args outside git repo → exit 2 ────────────────────────────────

/// `upd` with no args in a tempdir that is NOT a git repo must exit 2
/// with a clear error about not being inside a git repository.
#[test]
fn no_args_outside_git_repo_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    // Deliberately NOT calling git_init — we need a plain directory.

    let (_stdout, stderr, code) = run(&[], tmp.path());

    assert_eq!(
        code, 2,
        "no args outside git repo must exit 2; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("not inside a git repository"),
        "stderr must mention 'not inside a git repository'; got:\n{stderr}"
    );
}

// ── test 3: --apply inside git repo → actually mutates ───────────────────────

/// `upd --apply` inside a git repo with pending updates must write the file.
#[tokio::test]
async fn apply_flag_inside_git_repo_mutates_files() {
    let server = setup_fake_pypi().await;

    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());

    let req_path = tmp.path().join("requirements.txt");
    fs::write(&req_path, "requests==1.0.0\n").unwrap();

    let (_stdout, _stderr, code) = run_with_env(
        &["--apply", "--no-cache"],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 0,
        "upd --apply inside git repo must exit 0 when update succeeds"
    );

    let contents_after = fs::read_to_string(&req_path).expect("could not read fixture");
    assert!(
        contents_after.contains("99.0.0"),
        "file must be updated to 99.0.0 after --apply; got:\n{contents_after}"
    );
}

// ── test 4: --check outside git repo → exit 2 ────────────────────────────────

/// `upd --check` outside a git repo must exit 2 (no scope to discover).
#[test]
fn check_outside_git_repo_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    // No git init.

    let (_stdout, stderr, code) = run(&["--check"], tmp.path());

    assert_eq!(
        code, 2,
        "--check outside git repo must exit 2; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("not inside a git repository"),
        "stderr must mention 'not inside a git repository'; got:\n{stderr}"
    );
}

// ── test 5: explicit path, no --apply → dry-run, exit 1, hint ────────────────

/// `upd ./dir` with pending updates but no `--apply`:
///   - must not mutate files
///   - must exit 1
///   - must print "Run with --apply"
#[tokio::test]
async fn explicit_path_without_apply_is_dry_run() {
    let server = setup_fake_pypi().await;

    let tmp = tempfile::tempdir().unwrap();
    // No git init — explicit path bypasses VCS check.

    let req_path = tmp.path().join("requirements.txt");
    fs::write(&req_path, "requests==1.0.0\n").unwrap();

    let dir_str = tmp.path().to_str().expect("non-UTF-8 path");

    let (stdout, stderr, code) = run_with_env(
        &["--no-cache", dir_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    let combined = format!("{stdout}{stderr}");

    assert_eq!(
        code, 1,
        "explicit path without --apply with pending updates must exit 1; combined:\n{combined}"
    );

    let contents_after = fs::read_to_string(&req_path).expect("could not read fixture");
    assert_eq!(
        contents_after, "requests==1.0.0\n",
        "file must not be mutated without --apply"
    );

    assert!(
        combined.contains("--apply"),
        "output must mention --apply; combined:\n{combined}"
    );
}

// ── test 6: explicit path + --apply → mutates ────────────────────────────────

/// `upd ./dir --apply` with pending updates must write the file.
#[tokio::test]
async fn explicit_path_with_apply_mutates_files() {
    let server = setup_fake_pypi().await;

    let tmp = tempfile::tempdir().unwrap();
    // No git init — explicit path bypasses VCS check.

    let req_path = tmp.path().join("requirements.txt");
    fs::write(&req_path, "requests==1.0.0\n").unwrap();

    let dir_str = tmp.path().to_str().expect("non-UTF-8 path");

    let (_stdout, _stderr, code) = run_with_env(
        &["--apply", "--no-cache", dir_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 0,
        "explicit path with --apply must exit 0 when update succeeds"
    );

    let contents_after = fs::read_to_string(&req_path).expect("could not read fixture");
    assert!(
        contents_after.contains("99.0.0"),
        "file must be updated to 99.0.0 after --apply; got:\n{contents_after}"
    );
}

// ── test 7: -i explicit path, no TTY → TTY error, not VCS error ──────────────

/// `upd -i ./path` with no TTY must fail with the TTY error, NOT with
/// "not inside a git repository" — the explicit path bypasses the VCS check
/// before the TTY guard fires.
#[test]
fn interactive_explicit_path_no_tty_gives_tty_error_not_vcs_error() {
    let tmp = tempfile::tempdir().unwrap();
    // No git init.

    let req_path = tmp.path().join("requirements.txt");
    fs::write(&req_path, "requests==2.0.0\n").unwrap();

    let dir_str = tmp.path().to_str().expect("non-UTF-8 path");

    let (_stdout, stderr, code) = run_with_null_stdin(&["-i", dir_str], tmp.path());

    assert_eq!(
        code, 2,
        "--interactive with no TTY must exit 2; stderr:\n{stderr}"
    );

    assert!(
        stderr.contains("--interactive requires a terminal"),
        "must report TTY error, not VCS error; stderr:\n{stderr}"
    );

    assert!(
        !stderr.contains("not inside a git repository"),
        "must NOT report VCS error when explicit path bypasses VCS check; stderr:\n{stderr}"
    );
}

// ── test 8: no args, empty git repo → exit 0 (nothing to update) ─────────────

/// `upd` with no args inside a git repo with no dependency files:
///   - exits 0 (nothing to update)
///   - no "Run with --apply" hint needed
#[test]
fn no_args_inside_git_repo_empty_workspace_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    git_init(tmp.path());

    let (_stdout, _stderr, code) = run(&[], tmp.path());

    assert_eq!(
        code, 0,
        "no-args on empty git workspace must exit 0; got {code}"
    );
}

// ── test 9: --apply flag is parsed ───────────────────────────────────────────

/// `--apply` is recognised as a valid CLI flag (no parse error).
#[test]
fn apply_flag_is_valid_cli_arg() {
    use clap::Parser;
    use upd::cli::Cli;

    let cli = Cli::try_parse_from(["upd", "--apply"]).unwrap();
    assert!(cli.apply, "--apply must set the apply field to true");
}

// ── test 10: --apply default is false ────────────────────────────────────────

/// Without `--apply`, the field defaults to false.
#[test]
fn apply_flag_defaults_to_false() {
    use clap::Parser;
    use upd::cli::Cli;

    let cli = Cli::try_parse_from(["upd"]).unwrap();
    assert!(!cli.apply, "--apply must default to false");
}
