//! End-to-end tests for what a run reports about a `.mise.toml`, driven
//! through the built binary.
//!
//! A mise entry names its backend (`cargo:cargo-zigbuild`) or leaves it to
//! mise's own registry (`node`). upd answers for some of those backends and
//! not others, and an entry it cannot answer for used to be dropped during
//! parsing: the file closed green with "all dependencies up to date" over pins
//! nobody had looked at. These tests pin the reporting, not the lookups, so
//! every fixture here is one upd resolves without a network call.

use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

/// Arguments for a run that must not consult the network. Every entry in
/// `UNCHECKABLE_TOOLS` stops before any lookup.
const OFFLINE: &[&str] = &["--dry-run", "--no-cache", "--output", "text"];

/// One group per reason upd cannot check a tool: a backend it has no registry
/// for, a bare name outside its core table, and a version mise resolves at
/// install time.
///
/// The groups are deliberately different sizes. With one entry each, a summary
/// that paired every count with the wrong reason would print the same three
/// lines as a correct one, and no assertion below could tell them apart.
const UNCHECKABLE_TOOLS: &str = "\
[tools]
\"asdf:private-tool\" = \"1.0.0\"
\"asdf:another-tool\" = \"2.0.0\"
\"vfox:third-tool\" = \"3.0.0\"
actionlint = \"1.7.12\"
lefthook = \"1.8.0\"
node = \"latest\"
";

/// The control that proves the assertions below can observe the green line
/// when it is warranted: a file whose only section declares no tools at all.
const NO_TOOLS: &str = "\
[settings]
experimental = true
";

fn fixture(body: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join(".mise.toml"), body).expect("fixture written");
    dir
}

/// Run the binary over the fixture, isolated from the host so no user-level
/// `.updrc.toml` or credential can change the outcome.
fn run(dir: &TempDir, args: &[&str]) -> std::process::Output {
    let home = dir.path().join("home");
    let xdg_config = dir.path().join("xdg-config");
    std::fs::create_dir_all(&home).expect("fixture HOME created");
    std::fs::create_dir_all(&xdg_config).expect("fixture XDG config created");

    Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .arg(".mise.toml")
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

/// The headline defect: a file whose every tool went unchecked is not up to
/// date, because nothing in it was ever compared against a registry.
#[test]
fn text_summary_does_not_claim_up_to_date_for_tools_it_never_checked() {
    let dir = fixture(UNCHECKABLE_TOOLS);
    let output = run(&dir, OFFLINE);
    let stdout = stdout_of(&output);

    assert!(
        !stdout.contains("all dependencies up to date"),
        "not one of the six tools was looked at:\n{stdout}"
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "an unchecked tool is a reporting fact, not a failure:\n{stdout}"
    );
}

/// The negative control for the assertion above.
#[test]
fn text_summary_keeps_the_tick_when_there_is_nothing_to_check() {
    let dir = fixture(NO_TOOLS);
    let stdout = stdout_of(&run(&dir, OFFLINE));

    assert!(
        stdout.contains("all dependencies up to date"),
        "a file that declares no tools has genuinely nothing outstanding:\n{stdout}"
    );
}

/// The summary counts unchecked dependencies by the reason they went
/// unchecked. Reporting a mise tool in the wording written for GitHub Actions
/// would name a cause that is not this one.
#[test]
fn the_summary_names_the_reason_each_tool_went_unchecked() {
    let dir = fixture(UNCHECKABLE_TOOLS);
    let stdout = stdout_of(&run(&dir, OFFLINE));

    assert!(
        !stdout.contains("SHA-pinned action"),
        "no action is involved in this run:\n{stdout}"
    );
    for phrase in [
        "3 tool(s) on a backend upd cannot query",
        "2 tool(s) upd knows no registry for",
        "1 tool version(s) mise resolves at install time",
    ] {
        assert!(
            stdout.contains(phrase),
            "the summary must account for every unchecked tool, missing {phrase:?}:\n{stdout}"
        );
    }
}

/// One line per unchecked tool would be noise on every run in a repo full of
/// `latest` entries, so the names are behind --verbose while the count is not.
#[test]
fn verbose_names_the_unchecked_tools() {
    let dir = fixture(UNCHECKABLE_TOOLS);
    let quiet = stdout_of(&run(&dir, OFFLINE));
    // `-o text` matters: without it the run emits JSON, which names every tool
    // whatever --verbose says, so the assertions below would hold either way.
    let verbose = stdout_of(&run(
        &dir,
        &["--verbose", "--dry-run", "--no-cache", "--output", "text"],
    ));

    assert!(
        !quiet.contains("asdf:private-tool"),
        "the default run names no unchecked tool:\n{quiet}"
    );
    for (tool, line) in [
        ("asdf:private-tool", ".mise.toml:2:"),
        ("vfox:third-tool", ".mise.toml:4:"),
        ("actionlint", ".mise.toml:5:"),
        ("node", ".mise.toml:7:"),
    ] {
        assert!(
            verbose.contains(tool),
            "--verbose must name {tool}:\n{verbose}"
        );
        assert!(
            verbose.contains(line),
            "--verbose must point at {tool} on {line}\n{verbose}"
        );
    }
}

/// The machine-readable report carries every unchecked tool whether or not the
/// text output names it, keyed by a stable reason token.
#[test]
fn the_json_report_carries_every_unchecked_tool_with_its_reason() {
    let dir = fixture(UNCHECKABLE_TOOLS);
    let report = json_of(&run(&dir, &["--dry-run", "--no-cache", "--output", "json"]));

    let entries = report["files"][0]["skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("report has no skipped entries: {report:#}"));

    let reported: Vec<(&str, &str, u64)> = entries
        .iter()
        .map(|entry| {
            (
                entry["package"].as_str().expect("package"),
                entry["reason"].as_str().expect("reason"),
                entry["line"].as_u64().expect("line"),
            )
        })
        .collect();
    assert_eq!(
        reported,
        vec![
            ("asdf:private-tool", "unsupported-backend", 2),
            ("asdf:another-tool", "unsupported-backend", 3),
            ("vfox:third-tool", "unsupported-backend", 4),
            ("actionlint", "unknown-tool", 5),
            ("lefthook", "unknown-tool", 6),
            ("node", "symbolic-version", 7),
        ],
        "report: {report:#}"
    );
    assert!(
        entries
            .iter()
            .all(|entry| entry["status"] == "not-examined"),
        "a tool upd never looked at was not blocked by a safety check: {report:#}"
    );
}
