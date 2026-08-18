//! End-to-end tests for how a run treats GitHub Actions pinned to a commit SHA,
//! driven through the built binary.
//!
//! Two defects these guard. A pin left unchecked used to be dropped without a
//! trace, so a workflow whose every action is SHA-pinned closed with a green
//! "all dependencies up to date" even though not one of them had been looked
//! at; a pin nobody examined is not an up-to-date dependency. And checking them
//! used to be off by default, which left the repositories that took GitHub's
//! own pinning advice on stale action code.
//!
//! `--no-update-action-shas` is therefore explicit wherever these tests want the
//! unchecked state, rather than relied on as the default.

use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

/// Arguments for a run that must not consult the network: opting out stops at
/// the pin before any lookup.
const OFFLINE_OPT_OUT: &[&str] = &[
    "--dry-run",
    "--no-cache",
    "--no-update-action-shas",
    "--output",
    "text",
];

/// A real, full-length commit SHA carrying the concrete version comment that
/// makes the pin updateable.
const PINNED_WORKFLOW: &str = "\
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2
";

/// The same workflow with no actions at all, so a run over it genuinely has
/// nothing left to check. This is the control that proves the assertions below
/// can observe the green line when it is warranted.
const EMPTY_WORKFLOW: &str = "\
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo hello
";

/// A pin too short to verify. Whether SHA updating is on decides which of the
/// two statuses this gets, and the difference is visible without a lookup:
/// off leaves it not-examined, on examines it and blocks on the short SHA.
const SHORT_SHA_WORKFLOW: &str = "\
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@11bd719 # v4.2.2
";

/// Write `body` to `.github/workflows/ci.yml` under a fresh fixture directory.
fn fixture(body: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let workflows = dir.path().join(".github").join("workflows");
    std::fs::create_dir_all(&workflows).expect("fixture workflows dir created");
    std::fs::write(workflows.join("ci.yml"), body).expect("workflow written");
    dir
}

/// Run the binary over the fixture workflow, isolated from the host so no
/// user-level `.updrc.toml` or credential can change the outcome.
fn run(dir: &TempDir, args: &[&str]) -> std::process::Output {
    let home = dir.path().join("home");
    let xdg_config = dir.path().join("xdg-config");
    std::fs::create_dir_all(&home).expect("fixture HOME created");
    std::fs::create_dir_all(&xdg_config).expect("fixture XDG config created");

    Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .arg(".github/workflows/ci.yml")
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", xdg_config)
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .current_dir(dir.path())
        .output()
        .expect("upd ran")
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn json_of(output: &std::process::Output) -> Value {
    let stdout = stdout_of(output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}):\n{stdout}\nstderr:\n{stderr}"))
}

/// The headline defect: the closing line must not call a workflow up to date
/// when its only action was never examined.
#[test]
fn text_summary_does_not_claim_up_to_date_for_an_unchecked_sha_pin() {
    let dir = fixture(PINNED_WORKFLOW);
    let output = run(&dir, OFFLINE_OPT_OUT);
    let stdout = stdout_of(&output);

    assert!(
        !stdout.contains("all dependencies up to date"),
        "the only action was never looked at:\n{stdout}"
    );
    assert!(
        stdout.contains("1 SHA-pinned action(s), not checked while SHA updates are off"),
        "the summary must account for the unchecked pin:\n{stdout}"
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "an unchecked pin is a reporting fact, not a failure:\n{stdout}"
    );
}

/// The negative control for the assertion above: with nothing to check at all,
/// the green line is still correct and still printed.
#[test]
fn text_summary_keeps_the_tick_when_there_is_nothing_to_check() {
    let dir = fixture(EMPTY_WORKFLOW);
    let output = run(&dir, OFFLINE_OPT_OUT);
    let stdout = stdout_of(&output);

    assert!(
        stdout.contains("all dependencies up to date"),
        "a workflow with no actions has genuinely nothing outstanding:\n{stdout}"
    );
}

/// One line per unchecked pin would be noise on every run in a repo that pins
/// every action, so the names are behind --verbose while the count is not.
#[test]
fn verbose_names_the_unchecked_pin() {
    let dir = fixture(PINNED_WORKFLOW);

    let quiet = stdout_of(&run(&dir, OFFLINE_OPT_OUT));
    assert!(
        !quiet.contains("actions/checkout"),
        "the summary reports the count, not the names:\n{quiet}"
    );

    let mut verbose_args = OFFLINE_OPT_OUT.to_vec();
    verbose_args.push("--verbose");
    let verbose = stdout_of(&run(&dir, &verbose_args));
    assert!(
        verbose.contains("actions/checkout"),
        "--verbose must name the pin so it can be found:\n{verbose}"
    );
    assert!(
        verbose.contains("Not checked"),
        "the line must say the pin was not checked, not that it was blocked:\n{verbose}"
    );
}

/// Machine-readable consumers must be able to tell "never examined" from both
/// "up to date" and "blocked by a safety check".
#[test]
fn json_report_separates_unchecked_pins_from_up_to_date_dependencies() {
    let dir = fixture(PINNED_WORKFLOW);
    let report = json_of(&run(
        &dir,
        &[
            "--dry-run",
            "--no-cache",
            "--no-update-action-shas",
            "--output",
            "json",
        ],
    ));

    let summary = &report["summary"];
    assert_eq!(summary["not_examined"], 1, "report: {report:#}");
    assert!(
        summary["skipped"].is_null(),
        "a pin nobody examined was not blocked by a safety check: {report:#}"
    );
    assert_eq!(summary["updates_total"], 0, "report: {report:#}");
    assert_eq!(summary["errors"], 0, "report: {report:#}");

    let skipped = &report["files"][0]["skipped"][0];
    assert_eq!(skipped["status"], "not-examined", "report: {report:#}");
    assert_eq!(
        skipped["reason"], "action-sha-updates-off",
        "report: {report:#}"
    );
    assert_eq!(skipped["package"], "actions/checkout", "report: {report:#}");
    assert_eq!(skipped["line"], 7, "report: {report:#}");
}

/// Read the single skipped entry out of a report over one file.
fn only_skipped(report: &Value) -> &Value {
    let entries = report["files"][0]["skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("report has no skipped entries: {report:#}"));
    assert_eq!(entries.len(), 1, "report: {report:#}");
    &entries[0]
}

/// Checking SHA pins is the default, so a plain run examines one. Visible
/// without a network lookup: the short SHA is only reached once the pin is
/// examined, and reporting it as not-examined would mean the default was off.
#[test]
fn sha_pins_are_checked_by_default() {
    let dir = fixture(SHORT_SHA_WORKFLOW);
    let report = json_of(&run(&dir, &["--dry-run", "--no-cache", "--output", "json"]));

    let entry = only_skipped(&report);
    assert_eq!(entry["status"], "blocked", "report: {report:#}");
    assert_eq!(entry["reason"], "short-sha", "report: {report:#}");
}

/// `update_action_shas = false` in `.updrc.toml` turns the default off for the
/// repository, and the opt-in flag still wins over it.
#[test]
fn config_key_turns_sha_checking_off_and_the_flag_turns_it_back_on() {
    let dir = fixture(SHORT_SHA_WORKFLOW);
    std::fs::write(
        dir.path().join(".updrc.toml"),
        "update_action_shas = false\n",
    )
    .expect("config written");

    let from_config = json_of(&run(&dir, &["--dry-run", "--no-cache", "--output", "json"]));
    let entry = only_skipped(&from_config);
    assert_eq!(
        entry["status"], "not-examined",
        "the config key must reach the updater: {from_config:#}"
    );
    assert_eq!(
        entry["reason"], "action-sha-updates-off",
        "report: {from_config:#}"
    );

    let overridden = json_of(&run(
        &dir,
        &[
            "--dry-run",
            "--no-cache",
            "--output",
            "json",
            "--update-action-shas",
        ],
    ));
    let entry = only_skipped(&overridden);
    assert_eq!(
        entry["status"], "blocked",
        "the flag must override the config key: {overridden:#}"
    );
    assert_eq!(entry["reason"], "short-sha", "report: {overridden:#}");
}
