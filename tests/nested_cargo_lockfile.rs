//! Integration coverage for the `update --apply --lock` wiring that syncs a
//! nested Cargo workspace (the cargo-fuzz layout) after a root Cargo
//! lockfile refresh. `src/nested_lock.rs` has its own unit tests proving the
//! sync logic repairs a real `cargo check --locked` failure; this file
//! proves the CLI actually calls that logic and reports the result in its
//! JSON output, which is the part a unit test on the module alone cannot
//! see. `cargo` itself is faked here (matching `tests/lock_cooldown.rs` and
//! `tests/fix_audit_floors.rs`) since this file is checking the wiring, not
//! cargo's own dependency resolution, and a real registry bump would need
//! network access. The fake is a shell script, so this file runs on Unix
//! only.
#![cfg(unix)]

use std::fs;
use std::process::Command;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

fn run_with_env(
    args: &[&str],
    cwd: &std::path::Path,
    env: &[(&str, &str)],
) -> (String, String, i32) {
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

fn write_fake_tool(bin_dir: &std::path::Path, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = bin_dir.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn path_with(bin_dir: &std::path::Path) -> String {
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Mounts the crates.io API endpoint `CARGO_REGISTRIES_CRATES_IO_INDEX`
/// resolves to, matching `tests/update_package_floor.rs::mount_crates_latest`.
async fn mount_crates_latest(server: &wiremock::MockServer, name: &str, version: &str) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/api/v1/crates/{name}")))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "crate": { "max_stable_version": version },
                "versions": [{
                    "num": version, "yanked": false,
                    "created_at": "2024-01-01T00:00:00Z"
                }]
            })),
        )
        .mount(server)
        .await;
}

/// Both the root refresh and the nested-workspace sync run `cargo update -p
/// dupcrate` (in their own directory); this fake answers that call the same
/// way everywhere it runs and records every invocation so the test can
/// assert the nested one happened.
const FAKE_CARGO: &str = r#"#!/bin/sh
case "$1" in
  --version) echo 'cargo 1.96.0 (00 2026-01-01)'; exit 0 ;;
esac
echo "$PWD $*" >> "$FAKE_LOG"
case "$*" in
  'update -p dupcrate')
    cat > Cargo.lock <<'LOCK'
version = 4

[[package]]
name = "t"
version = "0.1.0"
dependencies = [
 "dupcrate",
]

[[package]]
name = "dupcrate"
version = "2.0.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
LOCK
    ;;
esac
exit 0
"#;

const ROOT_CARGO_TOML: &str = "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndupcrate = \"1.2.3\"\n";
const ROOT_CARGO_LOCK: &str = "version = 4\n\n[[package]]\nname = \"t\"\nversion = \"0.1.0\"\ndependencies = [\n \"dupcrate\",\n]\n\n[[package]]\nname = \"dupcrate\"\nversion = \"1.2.3\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
const FUZZ_CARGO_TOML: &str = "[package]\nname = \"t-fuzz\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[workspace]\n\n[dependencies.t]\npath = \"..\"\n";
const FUZZ_CARGO_LOCK: &str = "version = 4\n\n[[package]]\nname = \"t-fuzz\"\nversion = \"0.0.0\"\ndependencies = [\n \"t\",\n]\n\n[[package]]\nname = \"t\"\nversion = \"0.1.0\"\ndependencies = [\n \"dupcrate\",\n]\n\n[[package]]\nname = \"dupcrate\"\nversion = \"1.2.3\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";

/// A root manifest bump that upd applies and relocks reaches a nested
/// cargo-fuzz-style workspace too: `nested_lockfiles` in the JSON report
/// names `fuzz/Cargo.toml` as synced, and the fake `cargo` log proves
/// `cargo update -p dupcrate` actually ran a second time inside `fuzz/`.
#[tokio::test]
async fn a_root_bump_syncs_the_nested_fuzz_workspace_and_reports_it() {
    let server = wiremock::MockServer::start().await;
    mount_crates_latest(&server, "dupcrate", "2.0.1").await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("Cargo.toml"), ROOT_CARGO_TOML).unwrap();
    fs::write(tmp.path().join("Cargo.lock"), ROOT_CARGO_LOCK).unwrap();
    fs::create_dir_all(tmp.path().join("fuzz")).unwrap();
    fs::write(tmp.path().join("fuzz/Cargo.toml"), FUZZ_CARGO_TOML).unwrap();
    fs::write(tmp.path().join("fuzz/Cargo.lock"), FUZZ_CARGO_LOCK).unwrap();

    let bin_dir = tmp.path().join("fakebin");
    fs::create_dir(&bin_dir).unwrap();
    let log = tmp.path().join("cargo-invocations.log");
    write_fake_tool(&bin_dir, "cargo", FAKE_CARGO);

    let (stdout, stderr, code) = run_with_env(
        &[
            "update",
            "--package",
            "dupcrate",
            "--apply",
            "--lock",
            "--format",
            "json",
            "--no-cache",
            ".",
        ],
        tmp.path(),
        &[
            ("CARGO_REGISTRIES_CRATES_IO_INDEX", &server.uri()),
            ("PATH", &path_with(&bin_dir)),
            ("FAKE_LOG", &log.display().to_string()),
        ],
    );

    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}\nstdout: {stdout}"));
    let nested = json["nested_lockfiles"]
        .as_array()
        .unwrap_or_else(|| panic!("no nested_lockfiles array: {json}"));
    assert_eq!(nested.len(), 1, "{nested:?}");
    assert!(
        nested[0]["manifest"]
            .as_str()
            .unwrap_or_default()
            .ends_with("fuzz/Cargo.toml"),
        "{nested:?}"
    );
    assert!(
        nested[0]["lockfile"]
            .as_str()
            .unwrap_or_default()
            .ends_with("fuzz/Cargo.lock"),
        "{nested:?}"
    );
    assert_eq!(nested[0]["status"], "synced", "{nested:?}");
    assert!(nested[0]["reason"].is_null(), "{nested:?}");

    let invocations = fs::read_to_string(&log).unwrap();
    assert_eq!(
        invocations.matches("update -p dupcrate").count(),
        2,
        "expected one `cargo update -p dupcrate` for the root lockfile and one for the \
         nested fuzz workspace: {invocations}"
    );
    assert!(
        invocations.contains("/fuzz "),
        "the second invocation must have run inside fuzz/: {invocations}"
    );
}
