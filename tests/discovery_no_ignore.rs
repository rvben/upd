//! Integration tests for `.gitignore`-aware discovery and the
//! `--no-ignore` escape hatch.
//!
//! These run the real binary against a temp workspace so the full path
//! (CLI parsing → DiscoverOptions → walker → output) is covered.

use std::fs;
use std::path::Path;
use std::process::Command;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

fn run(args: &[&str], cwd: &Path) -> (String, String, i32) {
    let output = Command::new(upd_bin())
        .args(args)
        .env("UPD_CACHE_DIR", cwd.join("upd-cache"))
        .output()
        .expect("failed to run upd");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

/// Build a workspace where one of two `package.json` files is gitignored.
/// Returns the temp dir, kept-file path, and ignored-file path.
fn workspace_with_gitignored_package_json()
-> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    fs::write(root.join(".gitignore"), "vendor/\n").unwrap();

    let kept = root.join("package.json");
    fs::write(&kept, "{\"dependencies\":{}}").unwrap();

    let vendor = root.join("vendor");
    fs::create_dir_all(&vendor).unwrap();
    let ignored = vendor.join("package.json");
    fs::write(&ignored, "{\"dependencies\":{}}").unwrap();

    (tmp, kept, ignored)
}

/// Default behavior must skip the gitignored file: only one file is scanned.
#[test]
fn discovery_skips_gitignored_files_by_default() {
    let (tmp, _kept, _ignored) = workspace_with_gitignored_package_json();
    let path = tmp.path().to_str().unwrap();

    let (stdout, stderr, _code) = run(&["--check", "--no-cache", path], tmp.path());

    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("Scanned 1 file"),
        "default discovery must skip the gitignored file (expect 1 scanned); got:\n{combined}"
    );
}

/// `--no-ignore` must include the gitignored file: two files scanned.
#[test]
fn discovery_no_ignore_flag_includes_gitignored_files() {
    let (tmp, _kept, _ignored) = workspace_with_gitignored_package_json();
    let path = tmp.path().to_str().unwrap();

    let (stdout, stderr, _code) = run(&["--check", "--no-cache", "--no-ignore", path], tmp.path());

    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("Scanned 2 file"),
        "with --no-ignore both files must be scanned; got:\n{combined}"
    );
}

/// `--verbose` must surface a `skipping ... gitignored` line on stderr so
/// users can see why a file they expected to be processed is being silent.
#[test]
fn discovery_verbose_logs_gitignored_skip() {
    let (tmp, _kept, ignored) = workspace_with_gitignored_package_json();
    let path = tmp.path().to_str().unwrap();

    let (_stdout, stderr, _code) = run(&["--check", "--no-cache", "--verbose", path], tmp.path());

    let ignored_str = ignored.to_str().unwrap();
    assert!(
        stderr.contains("skipping") && stderr.contains("gitignored"),
        "verbose mode must emit a 'skipping ... gitignored' line; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(ignored_str) || stderr.contains("vendor/package.json"),
        "verbose skip log must mention the ignored path; stderr:\n{stderr}"
    );
}

/// `--no-ignore` with `--verbose` should NOT emit skip lines (nothing was
/// skipped — gitignore is disabled). Pinning this avoids spurious noise.
#[test]
fn discovery_no_ignore_with_verbose_emits_no_skip_lines() {
    let (tmp, _kept, _ignored) = workspace_with_gitignored_package_json();
    let path = tmp.path().to_str().unwrap();

    let (_stdout, stderr, _code) = run(
        &["--check", "--no-cache", "--no-ignore", "--verbose", path],
        tmp.path(),
    );

    assert!(
        !stderr.contains("gitignored"),
        "with --no-ignore there is nothing to skip; stderr should not mention gitignored:\n{stderr}"
    );
}
