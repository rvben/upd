//! Integration tests for unknown positional argument handling.
//!
//! When a user passes a positional argument that is neither an existing path
//! nor a known subcommand, `upd` must exit 2 with a descriptive error on
//! stderr rather than silently printing "No dependency files found."

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

/// `upd foobar` must exit 2 with a clear error when "foobar" is neither a
/// known subcommand nor an existing path.
#[test]
fn nonsense_positional_exits_two_with_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (_stdout, stderr, code) = run(&["foobar"], tmp.path());
    assert_eq!(
        code, 2,
        "'foobar' is not a path or subcommand — must exit 2, got {code}; stderr: {stderr}"
    );
    assert!(
        stderr.contains("'foobar' is not a known subcommand or existing path"),
        "stderr must contain the error message; got:\n{stderr}"
    );
}

/// `upd /does/not/exist` must also exit 2 when the absolute path does not
/// exist on disk.
#[test]
fn nonexistent_absolute_path_exits_two_with_error() {
    let tmp = tempfile::tempdir().unwrap();
    let path_arg = "/does/not/exist/ever";
    let (_stdout, stderr, code) = run(&[path_arg], tmp.path());
    assert_eq!(
        code, 2,
        "non-existent absolute path must exit 2, got {code}; stderr: {stderr}"
    );
    assert!(
        stderr.contains("is not a known subcommand or existing path"),
        "stderr must contain the error message for a bad absolute path; got:\n{stderr}"
    );
}

/// `upd ./tmpdir` — an existing directory — must work normally (exit 0 on
/// empty workspace with no dependency files found).
#[test]
fn existing_directory_works_normally() {
    let tmp = tempfile::tempdir().unwrap();
    let subdir = tmp.path().join("workspace");
    fs::create_dir(&subdir).unwrap();

    // Pass the subdir as a relative path from the tempdir.
    let rel = subdir
        .strip_prefix(tmp.path())
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let rel_arg = format!("./{rel}");

    let (_stdout, _stderr, code) = run(&[&rel_arg], tmp.path());
    assert_eq!(
        code, 0,
        "existing directory arg must not trigger the error path, got {code}"
    );
}

/// `upd --check ./tmpdir` must still work when the path exists.
#[test]
fn check_flag_with_existing_directory_works() {
    let tmp = tempfile::tempdir().unwrap();
    let subdir = tmp.path().join("workspace");
    fs::create_dir(&subdir).unwrap();

    let rel = subdir
        .strip_prefix(tmp.path())
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let rel_arg = format!("./{rel}");

    let (_stdout, _stderr, code) = run(&["--check", &rel_arg], tmp.path());
    assert_eq!(
        code, 0,
        "--check with an existing directory must not error, got {code}"
    );
}

/// `upd audit <tmpdir>` — real audit subcommand with an existing directory —
/// must not be affected by the new path validation.
#[test]
fn audit_subcommand_with_existing_directory_works() {
    let tmp = tempfile::tempdir().unwrap();
    let (_stdout, _stderr, code) = run(&["audit", tmp.path().to_str().unwrap()], tmp.path());
    assert_eq!(
        code, 0,
        "audit subcommand with an existing dir must succeed, got {code}"
    );
}

/// `upd --help` must still exit 0.
#[test]
fn help_flag_still_works() {
    let tmp = tempfile::tempdir().unwrap();
    // clap exits 0 for --help
    let (_stdout, _stderr, code) = run(&["--help"], tmp.path());
    assert_eq!(code, 0, "--help must exit 0, got {code}");
}

/// `upd aling` — a misspelling of `align` — must exit 2 since it is not an
/// existing path and is not a known subcommand.
#[test]
fn typo_subcommand_exits_two_with_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (_stdout, stderr, code) = run(&["aling"], tmp.path());
    assert_eq!(
        code, 2,
        "'aling' typo must exit 2, got {code}; stderr: {stderr}"
    );
    assert!(
        stderr.contains("'aling' is not a known subcommand or existing path"),
        "stderr must name the offending argument; got:\n{stderr}"
    );
}
