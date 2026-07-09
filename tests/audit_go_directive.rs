//! go.mod files predating go 1.17 do not list the full transitive module
//! set; audit must say so (status incomplete + warning) without failing.

use std::fs;
use std::process::Command;

fn upd_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_upd"))
}

#[test]
fn old_go_directive_warns_and_stays_exit_zero() {
    let tmp = tempfile::tempdir().unwrap();
    // No require block: zero packages to audit, so --offline cannot error.
    fs::write(
        tmp.path().join("go.mod"),
        "module example.com/legacy\n\ngo 1.16\n",
    )
    .unwrap();

    let out = upd_bin()
        .args(["audit", "--offline", "--format", "json"])
        .arg(tmp.path())
        .env("UPD_CACHE_DIR", tmp.path().join("cache"))
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "warnings must not change the exit code"
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["status"], "incomplete");
    let warnings = json["warnings"].as_array().expect("warnings array");
    assert!(
        warnings[0].as_str().unwrap().contains("go 1.17"),
        "warning names the go 1.17 pruning boundary: {warnings:?}"
    );
}

#[test]
fn modern_go_directive_produces_no_warning() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("go.mod"),
        "module example.com/modern\n\ngo\t1.22\n",
    )
    .unwrap();
    // Tab-separated directive: a valid go.mod spelling that a naive
    // `strip_prefix("go ")` parser would misread as missing.

    let out = upd_bin()
        .args(["audit", "--offline", "--format", "json"])
        .arg(tmp.path())
        .env("UPD_CACHE_DIR", tmp.path().join("cache"))
        .output()
        .unwrap();

    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["status"], "complete");
    assert!(
        json.get("warnings").is_none(),
        "no warnings field when empty"
    );
}

#[test]
fn missing_go_directive_is_treated_as_pre_117() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("go.mod"),
        "module example.com/nodirective\n",
    )
    .unwrap();

    let out = upd_bin()
        .args(["audit", "--offline", "--format", "json"])
        .arg(tmp.path())
        .env("UPD_CACHE_DIR", tmp.path().join("cache"))
        .output()
        .unwrap();

    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["status"], "incomplete");
}
