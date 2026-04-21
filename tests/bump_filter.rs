//! Integration tests for `--only-bump` and `--max-bump` filter flags.
//!
//! `--only-bump <LEVEL>[,<LEVEL>...]` restricts updates to those whose bump level
//! exactly matches one of the listed levels (e.g. `--only-bump minor,patch` skips major).
//!
//! `--max-bump <LEVEL>` applies a ceiling: only updates at or below that level are
//! included (e.g. `--max-bump minor` allows patch and minor, but not major).
//!
//! The two flags are mutually exclusive.

use std::fs;
use std::path::Path;
use std::process::Command;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

fn run_with_env(args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> (String, String, i32) {
    let mut cmd = Command::new(upd_bin());
    cmd.args(args)
        .current_dir(cwd)
        .env("UPD_CACHE_DIR", cwd.join(".cache").to_str().unwrap());
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

// ── CLI parsing (unit-level, no binary I/O) ────────────────────────────────

#[test]
fn cli_only_bump_accepts_single_level() {
    use clap::Parser;
    use upd::cli::{BumpLevel, Cli};
    let cli = Cli::try_parse_from(["upd", "--only-bump", "minor"]).unwrap();
    assert_eq!(cli.only_bump, vec![BumpLevel::Minor]);
    assert!(cli.max_bump.is_none());
}

#[test]
fn cli_only_bump_accepts_comma_separated() {
    use clap::Parser;
    use upd::cli::{BumpLevel, Cli};
    let cli = Cli::try_parse_from(["upd", "--only-bump", "minor,patch"]).unwrap();
    assert_eq!(cli.only_bump, vec![BumpLevel::Minor, BumpLevel::Patch]);
}

#[test]
fn cli_only_bump_accepts_repeated() {
    use clap::Parser;
    use upd::cli::{BumpLevel, Cli};
    let cli = Cli::try_parse_from(["upd", "--only-bump", "major", "--only-bump", "patch"]).unwrap();
    assert_eq!(cli.only_bump, vec![BumpLevel::Major, BumpLevel::Patch]);
}

#[test]
fn cli_max_bump_major_parses() {
    use clap::Parser;
    use upd::cli::{BumpLevel, Cli};
    let cli = Cli::try_parse_from(["upd", "--max-bump", "major"]).unwrap();
    assert_eq!(cli.max_bump, Some(BumpLevel::Major));
    assert!(cli.only_bump.is_empty());
}

#[test]
fn cli_max_bump_minor_parses() {
    use clap::Parser;
    use upd::cli::{BumpLevel, Cli};
    let cli = Cli::try_parse_from(["upd", "--max-bump", "minor"]).unwrap();
    assert_eq!(cli.max_bump, Some(BumpLevel::Minor));
}

#[test]
fn cli_max_bump_patch_parses() {
    use clap::Parser;
    use upd::cli::{BumpLevel, Cli};
    let cli = Cli::try_parse_from(["upd", "--max-bump", "patch"]).unwrap();
    assert_eq!(cli.max_bump, Some(BumpLevel::Patch));
}

#[test]
fn cli_only_bump_and_max_bump_conflict() {
    use clap::Parser;
    use upd::cli::Cli;
    let result = Cli::try_parse_from(["upd", "--only-bump", "minor", "--max-bump", "minor"]);
    assert!(
        result.is_err(),
        "--only-bump and --max-bump must be mutually exclusive; got Ok"
    );
}

#[test]
fn cli_old_bump_flag_rejected() {
    use clap::Parser;
    use upd::cli::Cli;
    let result = Cli::try_parse_from(["upd", "--bump", "minor"]);
    assert!(
        result.is_err(),
        "--bump must not exist; it was renamed to --only-bump"
    );
}

// ── Filtering behaviour tests (via subprocess with mock registry) ──────────

/// `--max-bump minor` must skip a major update and treat the workspace as clean
/// (exit 0 under --check), because the only available update is major.
#[tokio::test]
async fn max_bump_minor_skips_major_update() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Advertise version 2.0.0 — a major bump from 1.0.0.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-2.0.0.tar.gz">requests-2.0.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    // --max-bump minor: the major bump from 1.0.0→2.0.0 must be excluded, so
    // --check sees no pending updates and must exit 0.
    let (_stdout, stderr, code) = run_with_env(
        &["--check", "--no-cache", "--max-bump", "minor", &path_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 0,
        "--max-bump minor must skip a major bump; --check should exit 0; stderr: {stderr}"
    );
}

/// `--max-bump minor` must allow a minor update (exit 1 under --check because
/// a pending minor update exists and is within the ceiling).
#[tokio::test]
async fn max_bump_minor_allows_minor_update() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Advertise version 1.1.0 — a minor bump from 1.0.0.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-1.1.0.tar.gz">requests-1.1.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    // --max-bump minor: a minor bump is within the ceiling, so --check must exit 1.
    let (_stdout, stderr, code) = run_with_env(
        &["--check", "--no-cache", "--max-bump", "minor", &path_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 1,
        "--max-bump minor must include a minor bump; --check should exit 1; stderr: {stderr}"
    );
}

/// `--only-bump minor` skips a major bump so the workspace looks clean.
#[tokio::test]
async fn only_bump_minor_skips_major_update() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Advertise version 2.0.0 — a major bump from 1.0.0.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-2.0.0.tar.gz">requests-2.0.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    // --only-bump minor: only include exact minor bumps; the available update is
    // major so --check must see no pending updates and exit 0.
    let (_stdout, stderr, code) = run_with_env(
        &["--check", "--no-cache", "--only-bump", "minor", &path_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 0,
        "--only-bump minor must skip a major bump; --check should exit 0; stderr: {stderr}"
    );
}

/// `--max-bump patch` must skip both a minor and a major update.
#[tokio::test]
async fn max_bump_patch_skips_minor_and_major_updates() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;

    // Advertise version 1.1.0 — a minor bump from 1.0.0.
    let html = r#"<!DOCTYPE html><html><body>
<a href="requests-1.1.0.tar.gz">requests-1.1.0.tar.gz</a>
</body></html>"#;

    Mock::given(method("GET"))
        .and(path_regex(r"^/simple/requests/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes(), "text/html"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("requirements.txt"), "requests==1.0.0\n").unwrap();
    let path_str = tmp.path().to_str().unwrap().to_string();

    // --max-bump patch: a minor bump is above the ceiling; --check exits 0.
    let (_stdout, stderr, code) = run_with_env(
        &["--check", "--no-cache", "--max-bump", "patch", &path_str],
        tmp.path(),
        &[("UV_INDEX_URL", &server.uri())],
    );

    assert_eq!(
        code, 0,
        "--max-bump patch must skip a minor bump; --check should exit 0; stderr: {stderr}"
    );
}
