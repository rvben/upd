//! End-to-end tests for `--interactive` TTY guard.
//!
//! These tests spawn the real `upd` binary with stdin redirected from
//! `/dev/null` (non-TTY) and verify that the binary exits with code 2
//! and prints a clear error message without mutating any files.

use std::fs;
use std::process::Command;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

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

    let output = Command::new(upd_bin())
        .args(["--interactive"])
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
