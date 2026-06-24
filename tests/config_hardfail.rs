//! A malformed config must hard-fail rather than be silently ignored.
//!
//! Auto-discovered `.updrc.toml` previously parsed-failed silently and upd ran
//! with defaults, dropping the user's `ignore`/`pin` rules (which can let
//! unwanted updates through). It must instead abort with the same `parse_error`
//! exit code (4) that an explicit `--config` with a bad file already produces.

use std::fs;
use std::process::Command;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

#[test]
fn malformed_autodiscovered_config_hard_fails() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    // Unclosed array: invalid TOML.
    fs::write(tmp.path().join(".updrc.toml"), "ignore = [ broken\n").unwrap();

    let out = Command::new(upd_bin())
        .args(["--no-cache", tmp.path().to_str().unwrap()])
        .output()
        .expect("run upd");
    let code = out.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        code, 4,
        "a malformed auto-discovered config must hard-fail with exit 4 (parse_error), \
         not run with defaults; stderr: {stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("parse") || stderr.contains("Invalid TOML"),
        "the error must name the config parse failure; stderr: {stderr}"
    );
}

#[test]
fn explicit_bad_config_still_exits_4() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    let cfg = tmp.path().join("bad.toml");
    fs::write(&cfg, "ignore = [ broken\n").unwrap();

    let out = Command::new(upd_bin())
        .args([
            "--no-cache",
            "--config",
            cfg.to_str().unwrap(),
            tmp.path().join("requirements.txt").to_str().unwrap(),
        ])
        .output()
        .expect("run upd");
    assert_eq!(
        out.status.code().unwrap_or(-1),
        4,
        "explicit --config with a malformed file must exit 4; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
