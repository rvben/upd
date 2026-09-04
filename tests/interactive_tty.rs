//! End-to-end tests for `--interactive`: the TTY guard, and the exit code an
//! interactive session reports.
//!
//! These tests spawn the real `upd` binary with stdin redirected from
//! `/dev/null` (non-TTY) and verify that the binary exits with code 2
//! and prints a clear error message without mutating any files.

mod common;

#[cfg(unix)]
use common::run_on_a_terminal;
use common::upd_bin;
use std::fs;
use std::process::Command;

#[test]
fn interactive_without_tty_exits_with_error() {
    let tmp = tempfile::tempdir().unwrap();

    // Write a real requirements.txt so that, if the guard were absent,
    // the binary would silently apply all updates.
    let req_path = tmp.path().join("requirements.txt");
    let original_contents = "requests==2.0.0\n";
    fs::write(&req_path, original_contents).unwrap();

    let stdin_null = fs::File::open(if cfg!(target_os = "windows") {
        "NUL"
    } else {
        "/dev/null"
    })
    .expect("could not open null device");

    let dir_str = tmp.path().to_str().expect("non-UTF-8 path");
    let output = Command::new(upd_bin())
        .args(["--interactive", dir_str])
        .current_dir(tmp.path())
        .stdin(stdin_null)
        .output()
        .expect("failed to spawn upd");

    let exit_code = output.status.code().unwrap_or(-1);
    let stderr = String::from_utf8(output.stderr).expect("stderr not UTF-8");

    assert_eq!(
        exit_code, 2,
        "expected exit code 2 when stdin is not a TTY, got {exit_code}; stderr: {stderr}"
    );

    assert!(
        stderr.contains("--interactive requires a terminal"),
        "expected error message about TTY requirement, got: {stderr}"
    );

    // The fixture must not have been modified.
    let contents_after = fs::read_to_string(&req_path).expect("could not read fixture");
    assert_eq!(
        contents_after, original_contents,
        "fixture file was mutated even though --interactive was rejected"
    );
}

/// A dependency `upd` could not resolve is a failed run however the session
/// ends. Interactive mode has its own way out of the program, so it has to say
/// so itself: without this the same manifest that exits 2 under a plain run
/// exits 0 here, and a script driving `upd` reads the run as clean.
#[cfg(unix)]
#[test]
fn an_interactive_session_reports_a_scan_error() {
    let tmp = tempfile::tempdir().unwrap();
    // `<3` names no version to raise, so the pin cannot be written. The error is
    // raised before any release lookup, which keeps the test off the network.
    fs::write(
        tmp.path().join("package.json"),
        "{\n  \"name\": \"p\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": {\n    \"chalk\": \"<3\"\n  }\n}\n",
    )
    .unwrap();
    fs::write(tmp.path().join(".updrc.toml"), "[pin]\nchalk = \"5.0.0\"\n").unwrap();

    let dir = tmp.path().to_str().expect("non-UTF-8 path");
    let (code, output) = run_on_a_terminal(&["--interactive", dir], tmp.path(), &[], "");

    assert!(
        output.contains("cannot pin 'chalk'"),
        "expected the pin failure to be reported, got: {output}"
    );
    assert_eq!(code, 2, "expected exit 2 for a scan error, got {code}");
}

/// The negative control for the test above: the same session with nothing wrong
/// in it exits 0. An exit code that is always 2 would satisfy that test while
/// telling a caller every run had failed.
#[cfg(unix)]
#[test]
fn an_interactive_session_with_nothing_to_report_exits_clean() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("package.json"),
        "{\n  \"name\": \"p\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": {}\n}\n",
    )
    .unwrap();

    let dir = tmp.path().to_str().expect("non-UTF-8 path");
    let (code, output) = run_on_a_terminal(&["--interactive", dir], tmp.path(), &[], "");

    assert_eq!(
        code, 0,
        "expected exit 0 for a clean session, got {code}: {output}"
    );
}
