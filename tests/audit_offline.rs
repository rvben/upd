//! Integration tests for `upd audit --offline`.
//!
//! Each test gets a dedicated `UPD_CACHE_DIR` via a temp directory so that
//! tests are fully isolated from the real user cache and from each other.

use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

fn run_offline(args: &[&str], cwd: &Path, cache_dir: &Path) -> (String, String, i32) {
    let mut cmd = Command::new(upd_bin());
    cmd.args(args)
        .current_dir(cwd)
        .env("UPD_CACHE_DIR", cache_dir)
        // Ensure that if OSV were contacted it would fail immediately —
        // this makes accidental network calls obvious.
        .env("OSV_API_URL", "http://127.0.0.1:0");
    let output = cmd.output().expect("failed to run upd");
    (
        String::from_utf8(output.stdout).expect("stdout not UTF-8"),
        String::from_utf8(output.stderr).expect("stderr not UTF-8"),
        output.status.code().unwrap_or(-1),
    )
}

/// Create an isolated temp directory to act as `UPD_CACHE_DIR`.
fn isolated_cache() -> TempDir {
    tempfile::tempdir().expect("failed to create temp dir")
}

/// Write a pre-built `audit.json` into `cache_dir` that records one safe
/// package and one vulnerable package (both for PyPI).
fn write_pre_populated_cache(cache_dir: &Path) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let json = serde_json::json!({
        "entries": {
            "PyPI::requests::2.31.0": {
                "vulnerabilities": [],
                "fetched_at": now
            },
            "PyPI::django::3.2.0": {
                "vulnerabilities": [
                    {
                        "id": "GHSA-offline-test",
                        "summary": "Offline test vulnerability",
                        "severity": "High",
                        "url": "https://example.com/vuln",
                        "fixed_version": "3.2.1"
                    }
                ],
                "fetched_at": now
            }
        }
    });

    std::fs::create_dir_all(cache_dir).unwrap();
    std::fs::write(
        cache_dir.join("audit.json"),
        serde_json::to_string_pretty(&json).unwrap(),
    )
    .unwrap();
}

/// Write a minimal `requirements.txt` into `dir`.
fn write_requirements(dir: &Path, content: &str) {
    std::fs::write(dir.join("requirements.txt"), content).unwrap();
}

// ── Test 1: --offline reads from a pre-populated cache ───────────────────────

#[test]
fn audit_offline_reads_from_cache_and_does_not_contact_osv() {
    let cache_tmp = isolated_cache();
    write_pre_populated_cache(cache_tmp.path());

    let work_tmp = isolated_cache();
    // Both packages are in the cache; django 3.2.0 has a known vulnerability.
    write_requirements(work_tmp.path(), "requests==2.31.0\ndjango==3.2.0\n");

    let (stdout, stderr, code) =
        run_offline(&["audit", "--offline"], work_tmp.path(), cache_tmp.path());

    // The run must complete without a "Connection refused" error, proving that
    // OSV was not contacted.
    assert!(
        !stderr.contains("Connection refused") && !stderr.contains("connection refused"),
        "OSV must not be contacted in --offline mode; stderr={stderr:?}"
    );

    // The vulnerable django package must be reported.
    assert!(
        stdout.contains("GHSA-offline-test") || code != 0,
        "expected vulnerability from cache to be reported or non-zero exit; \
         stdout={stdout:?} stderr={stderr:?} code={code}"
    );
}

// ── Test 2: --offline with empty cache exits 2 with cache-miss message ───────

#[test]
fn audit_offline_empty_cache_exits_2_with_cache_miss_message() {
    let cache_tmp = isolated_cache();
    // Leave the cache directory empty — no audit.json.

    let work_tmp = isolated_cache();
    write_requirements(work_tmp.path(), "requests==2.31.0\n");

    let (stdout, stderr, code) =
        run_offline(&["audit", "--offline"], work_tmp.path(), cache_tmp.path());

    assert_eq!(
        code, 2,
        "exit code must be 2 when offline and cache is empty; \
         stdout={stdout:?} stderr={stderr:?}"
    );

    assert!(
        stderr.contains("cache miss"),
        "stderr should contain 'cache miss'; stderr={stderr:?}"
    );

    assert!(
        stderr.contains("PyPI/requests 2.31.0"),
        "stderr should include ecosystem/name version for diagnostics; \
         stderr={stderr:?}"
    );
}
