//! Integration tests for `.updrc.toml` honoring in `upd align`:
//!   * the `ignore` list suppresses named packages (text + JSON, --check + --apply)
//!   * case-insensitive / PEP 503 ignore matching
//!   * the `exclude` path globs drop whole files from discovery
//!   * explicit file-path arguments bypass `exclude`
//!   * `--verbose` surfaces both skip reasons on stderr
//!
//! These run the real binary so the full path (CLI → config resolution →
//! DiscoverOptions → walker → align → output) is covered. `align` never hits
//! the network (it compares versions already present in the files), so no
//! cache or registry stubbing is required.

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
        .env("UPD_CACHE_DIR", cwd.join("upd-cache"))
        .output()
        .expect("failed to run upd");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

/// Two projects whose `numpy` and `flask` pins disagree (misaligned), plus a
/// `requests` pin that already agrees (aligned). `config` is written verbatim
/// to `.updrc.toml` at the workspace root.
fn workspace(config: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    fs::write(root.join(".updrc.toml"), config).unwrap();

    let a = root.join("proj_a");
    let b = root.join("proj_b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();

    fs::write(
        a.join("requirements.txt"),
        "numpy==1.26.4\nflask==1.0.0\nrequests==1.0.0\n",
    )
    .unwrap();
    fs::write(
        b.join("requirements.txt"),
        "numpy==2.2.6\nflask==2.0.0\nrequests==1.0.0\n",
    )
    .unwrap();

    tmp
}

/// Sanity anchor: with no ignore/exclude, both numpy and flask are flagged and
/// `--check` exits 1. If this regresses, the suppression tests below would pass
/// vacuously.
#[test]
fn align_check_flags_all_misalignments_without_config() {
    let tmp = workspace("");
    let (stdout, _stderr, code) = run(&["align", "--check", "--output", "text", "."], tmp.path());

    assert_eq!(code, 1, "misalignments present must exit 1; got:\n{stdout}");
    assert!(
        stdout.contains("numpy"),
        "numpy must be flagged; got:\n{stdout}"
    );
    assert!(
        stdout.contains("flask"),
        "flask must be flagged; got:\n{stdout}"
    );
}

/// Plain `align` (dry-run, no `--check`, no `--apply`) must exit 1 when
/// misalignments exist - the same "changes available" signal `update` uses -
/// and must hint at `--apply` so the next step is discoverable.
#[test]
fn align_dry_run_exits_1_and_hints_apply() {
    let tmp = workspace("");
    let (stdout, _stderr, code) = run(&["align", "--output", "text", "."], tmp.path());

    assert_eq!(
        code, 1,
        "misalignments in a plain dry-run must exit 1; got:\n{stdout}"
    );
    assert!(
        stdout.to_lowercase().contains("--apply"),
        "align dry-run must hint at --apply to write changes; got:\n{stdout}"
    );
}

/// `ignore = ["numpy"]` drops numpy from the report but leaves flask flagged,
/// so `--check` still exits 1.
#[test]
fn align_check_ignore_suppresses_named_package() {
    let tmp = workspace("ignore = [\"numpy\"]\n");
    let (stdout, _stderr, code) = run(&["align", "--check", "--output", "text", "."], tmp.path());

    assert_eq!(
        code, 1,
        "flask still misaligned must exit 1; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("numpy"),
        "ignored numpy must not appear in report; got:\n{stdout}"
    );
    assert!(
        stdout.contains("flask"),
        "non-ignored flask must still be flagged; got:\n{stdout}"
    );
}

/// Ignoring every misaligned package leaves the workspace fully aligned, so
/// `--check` exits 0 with the all-aligned message.
#[test]
fn align_check_ignore_all_misaligned_exits_zero() {
    let tmp = workspace("ignore = [\"numpy\", \"flask\"]\n");
    let (stdout, _stderr, code) = run(&["align", "--check", "--output", "text", "."], tmp.path());

    assert_eq!(
        code, 0,
        "all misalignments ignored must exit 0; got:\n{stdout}"
    );
    assert!(
        stdout.contains("all packages are aligned"),
        "expected all-aligned message; got:\n{stdout}"
    );
}

/// The ignore filter must apply to JSON output too, so text and JSON agree.
#[test]
fn align_ignore_filters_json_output() {
    let tmp = workspace("ignore = [\"numpy\"]\n");
    let (stdout, _stderr, _code) = run(&["align", "--format", "json", "."], tmp.path());

    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let names: Vec<&str> = json["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["package"].as_str().unwrap())
        .collect();

    assert!(
        !names.contains(&"numpy"),
        "ignored numpy must be absent from JSON packages; got: {names:?}"
    );
    assert!(
        names.contains(&"flask"),
        "non-ignored flask must remain in JSON packages; got: {names:?}"
    );
}

/// Ignore matching is case-insensitive (PEP 503): `NumPy` suppresses `numpy`.
#[test]
fn align_ignore_is_case_insensitive() {
    let tmp = workspace("ignore = [\"NumPy\"]\n");
    let (stdout, _stderr, _code) = run(&["align", "--check", "--output", "text", "."], tmp.path());

    assert!(
        !stdout.contains("numpy"),
        "case-insensitive ignore must suppress numpy; got:\n{stdout}"
    );
}

/// `--apply` must not rewrite an ignored package. After aligning with
/// `ignore = ["numpy"]`, flask converges to the highest version but numpy keeps
/// its original, divergent pins.
#[test]
fn align_apply_does_not_touch_ignored_package() {
    let tmp = workspace("ignore = [\"numpy\"]\n");
    let (_stdout, _stderr, _code) = run(&["align", "--apply", "--output", "text", "."], tmp.path());

    let a = fs::read_to_string(tmp.path().join("proj_a/requirements.txt")).unwrap();
    let b = fs::read_to_string(tmp.path().join("proj_b/requirements.txt")).unwrap();

    // numpy left split by design.
    assert!(
        a.contains("numpy==1.26.4") && b.contains("numpy==2.2.6"),
        "ignored numpy must keep its original pins;\nproj_a:\n{a}\nproj_b:\n{b}"
    );
    // flask aligned up to the highest version everywhere.
    assert!(
        a.contains("flask==2.0.0") && b.contains("flask==2.0.0"),
        "flask must align to the highest version;\nproj_a:\n{a}\nproj_b:\n{b}"
    );
}

/// `--verbose` emits `skipping <package>: ignored by config` on stderr.
#[test]
fn align_verbose_logs_ignored_skip() {
    let tmp = workspace("ignore = [\"numpy\"]\n");
    let (_stdout, stderr, _code) = run(
        &["align", "--check", "--verbose", "--output", "text", "."],
        tmp.path(),
    );

    assert!(
        stderr.contains("skipping numpy: ignored by config"),
        "verbose must explain the numpy skip; stderr:\n{stderr}"
    );
}

// ==================== exclude (path globs) ====================

/// A workspace where the only second occurrence of `numpy` lives under
/// `archive/`. Excluding that tree leaves numpy with a single occurrence
/// (aligned); without the exclude it is misaligned.
fn workspace_with_archive(config: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    fs::write(root.join(".updrc.toml"), config).unwrap();
    fs::write(root.join("requirements.txt"), "numpy==1.26.4\n").unwrap();

    let archive = root.join("archive");
    fs::create_dir_all(&archive).unwrap();
    fs::write(archive.join("requirements.txt"), "numpy==2.2.6\n").unwrap();

    tmp
}

/// Without `exclude`, the archived file is scanned and numpy is misaligned.
#[test]
fn align_without_exclude_scans_archive() {
    let tmp = workspace_with_archive("");
    let (stdout, _stderr, code) = run(&["align", "--check", "--output", "text", "."], tmp.path());

    assert_eq!(
        code, 1,
        "archive copy makes numpy misaligned; got:\n{stdout}"
    );
    assert!(
        stdout.contains("numpy"),
        "numpy must be flagged; got:\n{stdout}"
    );
}

/// `exclude = ["**/archive/**"]` drops the archived file from discovery, so
/// numpy has a single occurrence and the workspace is aligned.
#[test]
fn align_exclude_drops_file_from_discovery() {
    let tmp = workspace_with_archive("exclude = [\"**/archive/**\"]\n");
    let (stdout, _stderr, code) = run(&["align", "--check", "--output", "text", "."], tmp.path());

    assert_eq!(
        code, 0,
        "excluding archive must align numpy; got:\n{stdout}"
    );
    assert!(
        stdout.contains("all packages are aligned"),
        "expected all-aligned message; got:\n{stdout}"
    );
}

/// `--verbose` emits `skipping <path>: excluded by config` for excluded files.
#[test]
fn align_verbose_logs_excluded_skip() {
    let tmp = workspace_with_archive("exclude = [\"**/archive/**\"]\n");
    let (_stdout, stderr, _code) = run(
        &["align", "--check", "--verbose", "--output", "text", "."],
        tmp.path(),
    );

    assert!(
        stderr.contains("excluded by config") && stderr.contains("archive"),
        "verbose must explain the excluded path; stderr:\n{stderr}"
    );
}

/// An explicit file-path argument bypasses `exclude`: passing both
/// requirements files directly scans the archived one even though the glob
/// would drop it in a directory walk.
#[test]
fn align_explicit_path_bypasses_exclude() {
    let tmp = workspace_with_archive("exclude = [\"**/archive/**\"]\n");
    let (stdout, _stderr, code) = run(
        &[
            "align",
            "--check",
            "--output",
            "text",
            "requirements.txt",
            "archive/requirements.txt",
        ],
        tmp.path(),
    );

    assert_eq!(
        code, 1,
        "explicit archive path must bypass exclude and re-expose the misalignment; got:\n{stdout}"
    );
    assert!(
        stdout.contains("numpy"),
        "numpy must be flagged; got:\n{stdout}"
    );
}

/// `exclude` is honored via `--config` precedence as well as auto-discovery.
#[test]
fn align_exclude_honored_via_explicit_config_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("requirements.txt"), "numpy==1.26.4\n").unwrap();
    let archive = root.join("archive");
    fs::create_dir_all(&archive).unwrap();
    fs::write(archive.join("requirements.txt"), "numpy==2.2.6\n").unwrap();

    // Config lives outside the scanned tree and is passed explicitly.
    let cfg = root.join("custom-config.toml");
    fs::write(&cfg, "exclude = [\"**/archive/**\"]\n").unwrap();

    let (stdout, _stderr, code) = run(
        &[
            "align",
            "--check",
            "--config",
            cfg.to_str().unwrap(),
            "--output",
            "text",
            ".",
        ],
        tmp.path(),
    );

    assert_eq!(
        code, 0,
        "exclude via --config must align numpy; got:\n{stdout}"
    );
}
