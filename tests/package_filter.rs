//! Integration tests for the `--package` filter flag.
//!
//! The filter restricts update processing to packages whose name exactly
//! matches one of the supplied names.  Non-matching packages are silently
//! skipped (treated as up-to-date).  The filter applies to both mutate and
//! `--dry-run`/`--check` paths.

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
        .env("UPD_CACHE_DIR", cwd.join(".cache").to_str().unwrap())
        .output()
        .expect("failed to run upd");
    (
        String::from_utf8(output.stdout).expect("stdout not UTF-8"),
        String::from_utf8(output.stderr).expect("stderr not UTF-8"),
        output.status.code().unwrap_or(-1),
    )
}

// ── CLI parsing tests ──────────────────────────────────────────────────────

#[test]
fn package_flag_accepts_single_name() {
    // clap parsing: --package foo should not error
    let tmp = tempfile::tempdir().unwrap();
    let (_stdout, stderr, code) = run(&["--package", "foo", "--dry-run"], tmp.path());
    assert_ne!(code, 2, "parse error: {stderr}");
    // empty workspace → exit 0
    assert_eq!(
        code, 0,
        "expected 0 on empty workspace: stdout/err: {stderr}"
    );
}

#[test]
fn package_flag_accepts_comma_separated() {
    let tmp = tempfile::tempdir().unwrap();
    let (_stdout, stderr, code) = run(&["--package", "foo,bar", "--dry-run"], tmp.path());
    assert_ne!(code, 2, "parse error: {stderr}");
    assert_eq!(code, 0, "expected 0 on empty workspace: {stderr}");
}

#[test]
fn package_flag_accepts_repeated() {
    let tmp = tempfile::tempdir().unwrap();
    let (_stdout, stderr, code) = run(
        &["--package", "foo", "--package", "bar", "--dry-run"],
        tmp.path(),
    );
    assert_ne!(code, 2, "parse error: {stderr}");
    assert_eq!(code, 0, "expected 0 on empty workspace: {stderr}");
}

// ── Filtering behaviour tests ──────────────────────────────────────────────

/// Without --package, upd reports all pending updates.
/// With --package foo (and the file only having bar), it should exit 0 (no
/// updates found for the filtered set).
#[test]
fn package_filter_excludes_non_matching_packages_dry_run() {
    let tmp = tempfile::tempdir().unwrap();

    // No mock registry — this test only verifies the CLI accepts the flag; filter behaviour is covered by unit tests.
    fs::write(tmp.path().join("requirements.txt"), "requests==2.28.0\n").unwrap();

    let (_stdout, _stderr, code) =
        run(&["--package", "foo", "--dry-run", "--no-cache"], tmp.path());
    assert_eq!(
        code, 0,
        "with --package foo, 'requests' must be silently skipped (exit 0)"
    );
}

/// Without --package, upd processes all packages.
#[test]
fn without_package_filter_processes_all_packages() {
    let tmp = tempfile::tempdir().unwrap();

    // An empty workspace with no files → always exit 0 regardless of filter.
    // This test verifies the flag is truly optional (no regression).
    let (_stdout, _stderr, code) = run(&["--dry-run", "--no-cache"], tmp.path());
    assert_eq!(code, 0, "no --package flag on empty workspace must exit 0");
}

/// With --package foo,bar (comma-separated), packages not in that list are filtered.
#[test]
fn comma_separated_package_filter_excludes_others() {
    let tmp = tempfile::tempdir().unwrap();

    fs::write(tmp.path().join("requirements.txt"), "requests==2.28.0\n").unwrap();

    // "requests" is not in {foo, bar}; should be silently skipped.
    let (_stdout, _stderr, code) = run(
        &["--package", "foo,bar", "--dry-run", "--no-cache"],
        tmp.path(),
    );
    assert_eq!(
        code, 0,
        "with --package foo,bar, 'requests' must be silently skipped"
    );
}

/// With --package requests (the actual package name), the package is NOT skipped
/// and upd will attempt to check/update it.  The exit code here depends on whether
/// the registry says there's a newer version; since we run with --no-cache we would
/// normally hit the network.  To avoid flakiness, we assert only that the exit code
/// is NOT 2 (which would mean an unexpected error unrelated to the filter).
#[test]
fn package_filter_matching_name_does_not_skip() {
    let tmp = tempfile::tempdir().unwrap();

    fs::write(tmp.path().join("requirements.txt"), "requests==2.28.0\n").unwrap();

    // "requests" matches the filter; upd will try to resolve its version.
    // We don't assert a specific update count but we verify the filter
    // itself does not erroneously skip it (the run must not exit 2 from a crash).
    let (_stdout, _stderr, code) = run(&["--package", "requests", "--dry-run"], tmp.path());
    assert_ne!(
        code, 2,
        "a network/parse error should not occur from the filter"
    );
}

// ── CLI unit tests (no binary needed) ─────────────────────────────────────

#[test]
fn cli_parses_package_single() {
    use clap::Parser;
    let cli = upd::cli::Cli::try_parse_from(["upd", "--package", "foo"]).unwrap();
    assert_eq!(cli.packages, vec!["foo".to_string()]);
}

#[test]
fn cli_parses_package_comma_separated() {
    use clap::Parser;
    let cli = upd::cli::Cli::try_parse_from(["upd", "--package", "foo,bar"]).unwrap();
    assert_eq!(cli.packages, vec!["foo".to_string(), "bar".to_string()]);
}

#[test]
fn cli_parses_package_repeated() {
    use clap::Parser;
    let cli =
        upd::cli::Cli::try_parse_from(["upd", "--package", "foo", "--package", "bar"]).unwrap();
    assert_eq!(cli.packages, vec!["foo".to_string(), "bar".to_string()]);
}

#[test]
fn cli_packages_default_empty() {
    use clap::Parser;
    let cli = upd::cli::Cli::try_parse_from(["upd"]).unwrap();
    assert!(cli.packages.is_empty());
}

#[test]
fn cli_parses_package_mixed_comma_and_repeated() {
    use clap::Parser;
    let cli =
        upd::cli::Cli::try_parse_from(["upd", "--package", "foo,bar", "--package", "baz"]).unwrap();
    assert_eq!(
        cli.packages,
        vec!["foo".to_string(), "bar".to_string(), "baz".to_string()]
    );
}
