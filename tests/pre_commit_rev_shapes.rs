//! End-to-end tests for what a run does with a pre-commit `rev` that is not a
//! version tag, driven through the built binary.
//!
//! The defect these guard is data destruction. upd read every `rev` as a
//! version and rewrote it to the repository's latest release truncated to the
//! current value's component count, so a 40-character commit pin became
//! `rev: 6` and `rev: 1.x` became `rev: 6.0`, both silently and both exit 0.
//! Which unreadable revs were destroyed and which merely went unreported was
//! decided by a plain string comparison against the target version, so `main`
//! and one commit SHA happened to survive while another SHA did not.
//!
//! Every test here runs offline. The guard is reached before any registry
//! lookup, and the cases that need a target version use a configured `[pin]`,
//! which upd resolves without the network. That is also the point of the pin
//! cases: the pin path never ran the downgrade comparison, so it destroyed
//! every unreadable rev rather than only the unlucky ones.

use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

/// The upstream repository the fixtures name. Real, so a run that reaches the
/// network resolves rather than erroring, which keeps a failure here about the
/// rev and not about connectivity.
const REPO: &str = "pre-commit/pre-commit-hooks";

/// A real 40-character commit pin. On the released binary this one is rewritten
/// to `rev: 6`.
const DESTROYED_SHA: &str = "2c9f875913ee60ca25ce70243dc24d5b6415598c";

/// A real 40-character commit pin from the same repository that sorts above the
/// target version as a plain string, so the downgrade comparison left it alone.
/// The bytes survived; nothing said the pin had never been checked.
const LUCKY_SHA: &str = "c4a0b883114b00d8d76b479c820ce7950211c99b";

fn yaml(rev: &str) -> String {
    format!(
        "repos:\n  - repo: https://github.com/{REPO}\n    rev: {rev}\n    hooks:\n      - id: trailing-whitespace\n"
    )
}

fn toml(rev: &str) -> String {
    format!(
        "[[repos]]\nrepo = \"https://github.com/{REPO}\"\nrev = \"{rev}\"\nhooks = [{{ id = \"trailing-whitespace\" }}]\n"
    )
}

/// Write `body` as `name` under a fresh fixture directory.
fn fixture(name: &str, body: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join(name), body).expect("config written");
    dir
}

/// Pin the fixture's repository, which gives the run a target version without a
/// registry lookup.
fn pin(dir: &TempDir, version: &str) {
    std::fs::write(
        dir.path().join(".updrc.toml"),
        format!("[pin]\n\"{REPO}\" = \"{version}\"\n"),
    )
    .expect("config written");
}

/// Run the binary over the fixture, isolated from the host so no user-level
/// `.updrc.toml` or credential can change the outcome.
fn run(dir: &TempDir, name: &str, args: &[&str]) -> std::process::Output {
    let home = dir.path().join("home");
    let xdg_config = dir.path().join("xdg-config");
    std::fs::create_dir_all(&home).expect("fixture HOME created");
    std::fs::create_dir_all(&xdg_config).expect("fixture XDG config created");

    Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .arg(name)
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

/// Read the single skipped entry out of a report over one file.
fn only_skipped(report: &Value) -> &Value {
    let entries = report["files"][0]["skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("report has no skipped entries: {report:#}"));
    assert_eq!(entries.len(), 1, "report: {report:#}");
    &entries[0]
}

/// Apply a run over one config and return the file as it stands afterwards,
/// together with the JSON report.
fn apply(dir: &TempDir, name: &str, extra: &[&str]) -> (String, Value) {
    let mut args = vec!["--apply", "--no-cache", "--output", "json"];
    args.extend_from_slice(extra);
    let report = json_of(&run(dir, name, &args));
    let written = std::fs::read_to_string(dir.path().join(name)).expect("config still readable");
    (written, report)
}

/// The headline defect: a commit pin must come out of `--apply` byte for byte.
#[test]
fn a_commit_pinned_rev_survives_apply_and_is_reported() {
    let original = yaml(DESTROYED_SHA);
    let dir = fixture(".pre-commit-config.yaml", &original);
    let (written, report) = apply(&dir, ".pre-commit-config.yaml", &[]);

    assert_eq!(written, original, "the commit pin was rewritten");

    let entry = only_skipped(&report);
    assert_eq!(entry["status"], "blocked", "report: {report:#}");
    assert_eq!(entry["reason"], "sha-pinned-rev", "report: {report:#}");
    assert_eq!(entry["package"], REPO, "report: {report:#}");
    assert_eq!(entry["current"], DESTROYED_SHA, "report: {report:#}");
    assert_eq!(entry["line"], 3, "report: {report:#}");
    assert_eq!(report["summary"]["updates_total"], 0, "report: {report:#}");
    assert_eq!(report["summary"]["errors"], 0, "report: {report:#}");
}

/// The same shape on the other side of the string comparison. These bytes
/// already survived, so this test is about the report: a pin upd never checked
/// was counted as an up-to-date dependency, which is the same wrong claim the
/// SHA-pinned actions work removed from the actions updater.
#[test]
fn a_commit_pinned_rev_that_survived_by_luck_is_still_reported() {
    let original = yaml(LUCKY_SHA);
    let dir = fixture(".pre-commit-config.yaml", &original);
    let (written, report) = apply(&dir, ".pre-commit-config.yaml", &[]);

    assert_eq!(written, original, "the commit pin was rewritten");
    let entry = only_skipped(&report);
    assert_eq!(entry["status"], "blocked", "report: {report:#}");
    assert_eq!(entry["reason"], "sha-pinned-rev", "report: {report:#}");
}

/// A rev upd cannot read as a version, whatever it is. The reason separates
/// these from a full commit pin because they are differently actionable: a
/// commit pin is a deliberate choice, an abbreviated SHA or a moving pointer is
/// usually one the maintainer would want to hear about.
#[test]
fn revs_that_are_not_version_tags_are_reported_rather_than_rewritten() {
    for rev in ["2c9f875", "1.x", "main", "stable", "release-4"] {
        let original = yaml(rev);
        let dir = fixture(".pre-commit-config.yaml", &original);
        let (written, report) = apply(&dir, ".pre-commit-config.yaml", &[]);

        assert_eq!(written, original, "rev {rev} was rewritten");
        let entry = only_skipped(&report);
        assert_eq!(entry["status"], "blocked", "rev {rev}: {report:#}");
        assert_eq!(entry["reason"], "unrecognized-rev", "rev {rev}: {report:#}");
        assert_eq!(entry["current"], rev, "rev {rev}: {report:#}");
    }
}

/// The pin path skipped the downgrade comparison entirely, so it destroyed
/// every unreadable rev rather than only the unlucky ones: this fixture came
/// out of the released binary as `rev: 5`.
#[test]
fn a_configured_pin_does_not_overwrite_a_commit_pinned_rev() {
    let original = yaml(DESTROYED_SHA);
    let dir = fixture(".pre-commit-config.yaml", &original);
    pin(&dir, "v5.0.0");
    let (written, report) = apply(&dir, ".pre-commit-config.yaml", &[]);

    assert_eq!(written, original, "the pin overwrote the commit pin");
    assert_eq!(
        only_skipped(&report)["reason"],
        "sha-pinned-rev",
        "report: {report:#}"
    );
}

/// `--full-precision` writes the target version whole instead of truncating it,
/// so it destroyed the pin without producing an obviously broken value: this
/// fixture came out of the released binary as `rev: 6.0.0`. The guard cannot be
/// a property of the truncation.
#[test]
fn full_precision_does_not_overwrite_a_commit_pinned_rev() {
    let original = yaml(DESTROYED_SHA);
    let dir = fixture(".pre-commit-config.yaml", &original);
    pin(&dir, "v6.0.0");
    let (written, report) = apply(&dir, ".pre-commit-config.yaml", &["--full-precision"]);

    assert_eq!(written, original, "the pin overwrote the commit pin");
    assert_eq!(
        only_skipped(&report)["reason"],
        "sha-pinned-rev",
        "report: {report:#}"
    );
}

/// `prek.toml` and `.pre-commit-config.yaml` are documented as equivalent, so
/// the guard cannot live in one reader.
#[test]
fn prek_toml_guards_the_same_rev_shapes() {
    let original = toml(DESTROYED_SHA);
    let dir = fixture("prek.toml", &original);
    let (written, report) = apply(&dir, "prek.toml", &[]);

    assert_eq!(written, original, "the commit pin was rewritten");
    let entry = only_skipped(&report);
    assert_eq!(entry["status"], "blocked", "report: {report:#}");
    assert_eq!(entry["reason"], "sha-pinned-rev", "report: {report:#}");
}

/// The positive control. A rev upd can read is still rewritten, and reports
/// nothing skipped, which is what makes every assertion above meaningful.
#[test]
fn a_readable_rev_is_still_rewritten() {
    let dir = fixture(".pre-commit-config.yaml", &yaml("v4.5.0"));
    pin(&dir, "v5.0.0");
    let (written, report) = apply(&dir, ".pre-commit-config.yaml", &[]);

    assert_eq!(written, yaml("v5.0.0"), "report: {report:#}");
    assert!(
        report["files"][0]["skipped"]
            .as_array()
            .is_none_or(|entries| entries.is_empty()),
        "a readable rev is not skipped: {report:#}"
    );
}

/// A config whose only rev was never checked must not close on the green tick,
/// and the line naming it has to say what upd refused to do.
#[test]
fn the_text_summary_does_not_claim_up_to_date_for_an_unchecked_rev() {
    let dir = fixture(".pre-commit-config.yaml", &yaml(DESTROYED_SHA));
    let stdout = stdout_of(&run(
        &dir,
        ".pre-commit-config.yaml",
        &["--dry-run", "--no-cache", "--output", "text"],
    ));

    assert!(
        !stdout.contains("all dependencies up to date"),
        "the only rev was never looked at:\n{stdout}"
    );
    assert!(
        stdout.contains("Blocked") && stdout.contains("sha-pinned-rev"),
        "the line must name the refusal and its reason:\n{stdout}"
    );
    assert!(
        stdout.contains(DESTROYED_SHA),
        "the line must name the rev it left alone:\n{stdout}"
    );
}

/// The negative control for the assertion above: a config upd fully checked
/// still earns the tick.
#[test]
fn the_text_summary_keeps_the_tick_when_every_rev_was_checked() {
    let dir = fixture(".pre-commit-config.yaml", &yaml("v5.0.0"));
    pin(&dir, "v5.0.0");
    let stdout = stdout_of(&run(
        &dir,
        ".pre-commit-config.yaml",
        &["--dry-run", "--no-cache", "--output", "text"],
    ));

    assert!(
        stdout.contains("all dependencies up to date"),
        "a rev already at its pinned version has nothing outstanding:\n{stdout}"
    );
}
