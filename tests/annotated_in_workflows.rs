//! End-to-end tests for an `upd:` annotation inside a GitHub Actions workflow,
//! driven through the built binary.
//!
//! A workflow is a file `upd` already recognizes, and a recognized file never
//! reaches the annotated updater on its own: `FileType::detect_with_annotated`
//! gives a real detected type precedence, which is the rule that keeps `main.tf`
//! Terraform. The consequence was a blind spot. A tool version passed to an
//! action through a `with:` input is a real pin that nothing in the actions
//! grammar can see, and there was no way to opt in. Both updaters now run over
//! the file in one invocation.
//!
//! These are the CLI half, and they cover the dispatch that in-process tests
//! cannot reach. `github-releases` has no base-URL override, so the fixtures
//! reach their answers without a lookup: a `[pin]` resolves an annotated line
//! before the registry, and `--no-update-action-shas` stops the actions pass at
//! a SHA-pinned `uses:` ref. Neither run contacts the network at all.

use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

/// A workflow carrying both kinds of version: a `uses:` ref the actions updater
/// owns, and a tool version passed through a `with:` input that only the
/// annotation beside it makes visible.
const WORKFLOW: &str = "\
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2
      - uses: jdx/mise-action@11bd71901bbe5b1630ceea73d27597364c9af683 # v3.4.0
        with:
          version: 2025.8.18 # upd: github-releases jdx/mise
";

/// The same workflow with plain version refs in place of SHA pins, paired with
/// [`PINS`]. A `[pin]` resolves a `uses:` ref without a lookup, so whether the
/// actions pass ran is legible in the file itself rather than resting on the
/// network being unreachable.
const WORKFLOW_WITH_VERSION_REFS: &str = "\
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: jdx/mise-action@v2
        with:
          version: 2025.8.18 # upd: github-releases jdx/mise
";

/// A pin for every name in [`WORKFLOW_WITH_VERSION_REFS`], so each pass has
/// something it would visibly write if it ran, and no run needs the network.
const PINS: &str = "\
[pin]
\"jdx/mise\" = \"2026.8.14\"
\"actions/checkout\" = \"v9.9.9\"
\"jdx/mise-action\" = \"v9.9.9\"
";

/// Write `body` to `.github/workflows/ci.yml`, with `updrc` as `.updrc.toml`
/// beside it.
fn fixture(body: &str, updrc: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let workflows = dir.path().join(".github").join("workflows");
    std::fs::create_dir_all(&workflows).expect("fixture workflows dir created");
    std::fs::write(workflows.join("ci.yml"), body).expect("workflow written");
    std::fs::write(dir.path().join(".updrc.toml"), updrc).expect("config written");
    dir
}

/// Run the binary over the fixture workflow, isolated from the host so no
/// user-level config or credential can change the outcome.
fn run(dir: &TempDir, args: &[&str]) -> std::process::Output {
    run_at(dir, ".github/workflows/ci.yml", args)
}

/// The same, over an explicit `target`. A directory goes through the walk and a
/// file through the explicit-file branch, and `--lang` is applied separately in
/// each, so a discovery test has to say which it means.
fn run_at(dir: &TempDir, target: &str, args: &[&str]) -> std::process::Output {
    let home = dir.path().join("home");
    let xdg_config = dir.path().join("xdg-config");
    std::fs::create_dir_all(&home).expect("fixture HOME created");
    std::fs::create_dir_all(&xdg_config).expect("fixture XDG config created");

    Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .arg(target)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", xdg_config)
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .current_dir(dir.path())
        .output()
        .expect("upd ran")
}

fn json_of(output: &std::process::Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}):\n{stdout}\nstderr:\n{stderr}"))
}

fn workflow_text(dir: &TempDir) -> String {
    std::fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap()
}

/// The headline: the annotated `with:` input is resolved, in a run over a file
/// the actions updater also claims. The report still calls the file a GitHub
/// Actions workflow, because that is what it is; the annotation pass rides
/// alongside rather than taking the file over.
#[test]
fn an_annotated_input_in_a_workflow_is_resolved_through_the_cli() {
    let dir = fixture(WORKFLOW, "[pin]\n\"jdx/mise\" = \"2026.8.14\"\n");

    let output = run(
        &dir,
        &[
            "--apply",
            "--no-cache",
            "--no-update-action-shas",
            "--output",
            "json",
        ],
    );
    let report = json_of(&output);
    assert!(
        output.status.success(),
        "{}\n{report:#}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        workflow_text(&dir).contains("version: 2026.8.14 # upd: github-releases jdx/mise"),
        "the annotated input was not resolved:\n{}",
        workflow_text(&dir)
    );

    let file = &report["files"][0];
    assert_eq!(
        file["file_type"], "github_actions",
        "the annotation pass must not reclassify the file: {report:#}"
    );
    assert_eq!(file["pinned"][0]["package"], "jdx/mise", "{report:#}");
    assert_eq!(file["pinned"][0]["source"], "github-releases", "{report:#}");

    // The actions pass ran over the same file in the same invocation: both of
    // its `uses:` refs are accounted for. Without this the test passes against
    // an implementation that simply reclassified the workflow as annotated.
    let skipped = file["skipped"].as_array().expect("skipped[] is an array");
    assert_eq!(skipped.len(), 2, "{report:#}");
    assert!(
        skipped
            .iter()
            .all(|s| s["reason"] == "action-sha-updates-off"),
        "{report:#}"
    );
    assert_eq!(report["summary"]["errors"], 0, "{report:#}");
}

/// The negative control for the test above. The identical run over the identical
/// workflow, with the annotation removed, leaves the same line untouched. Without
/// it an implementation that rewrote every `version:` it found would pass.
#[test]
fn an_unannotated_input_in_a_workflow_is_left_alone() {
    let dir = fixture(
        &WORKFLOW.replace(" # upd: github-releases jdx/mise", ""),
        "[pin]\n\"jdx/mise\" = \"2026.8.14\"\n",
    );
    let before = workflow_text(&dir);

    let output = run(
        &dir,
        &[
            "--apply",
            "--no-cache",
            "--no-update-action-shas",
            "--output",
            "json",
        ],
    );
    let report = json_of(&output);
    assert!(
        output.status.success(),
        "{}\n{report:#}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(workflow_text(&dir), before);
    assert!(
        report["files"][0]["pinned"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "nothing declares a source for that line: {report:#}"
    );
}

/// A workflow with no annotation at all must behave exactly as it did before the
/// two passes were composed, including its diagnostics. This is the regression
/// guard for every workflow in every repository, which is nearly all of them.
#[test]
fn a_workflow_without_annotations_reports_exactly_what_it_always_did() {
    let dir = fixture(
        "\
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2
",
        "",
    );

    let output = run(
        &dir,
        &[
            "--dry-run",
            "--no-cache",
            "--no-update-action-shas",
            "--output",
            "json",
        ],
    );
    let report = json_of(&output);
    assert!(
        output.status.success(),
        "{}\n{report:#}",
        String::from_utf8_lossy(&output.stderr)
    );

    let file = &report["files"][0];
    assert_eq!(file["file_type"], "github_actions", "{report:#}");
    assert_eq!(
        file["warnings"].as_array().unwrap().len(),
        0,
        "the annotation pass found nothing and must say nothing: {report:#}"
    );
    assert_eq!(report["summary"]["errors"], 0, "{report:#}");
    // The positive control: the actions pass still examined the ref, so
    // "no warnings" is not two passes that both did nothing.
    assert_eq!(report["summary"]["not_examined"], 1, "{report:#}");
}

/// `--lang` is answered in two places, and only the inner one used to know
/// about annotations. A workflow was admitted for `actions` and nothing else,
/// so `upd -l annotated` over a tree of workflows reported "No dependency files
/// found." and exited 0 - the shape of answer that reads as "nothing to do".
///
/// Discovery is the half these cover, which the in-process tests cannot reach.
#[test]
fn a_directory_walk_finds_a_workflow_for_a_selection_of_annotations_alone() {
    let dir = fixture(WORKFLOW_WITH_VERSION_REFS, PINS);

    let output = run_at(
        &dir,
        ".",
        &[
            "--apply",
            "--no-cache",
            "-l",
            "annotated",
            "--output",
            "json",
        ],
    );
    let report = json_of(&output);
    assert!(
        output.status.success(),
        "{}\n{report:#}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        workflow_text(&dir).contains("version: 2026.8.14 # upd: github-releases jdx/mise"),
        "`-l annotated` did not reach the annotation:\n{}",
        workflow_text(&dir)
    );
    assert_eq!(
        report["files"][0]["file_type"], "github_actions",
        "{report:#}"
    );
    // The file's own dependencies are not what was asked for. Both refs are
    // pinned in the config, so had the actions pass run they would read `v9`
    // here: this is an assertion about the run, not about the network.
    assert!(
        workflow_text(&dir).contains("actions/checkout@v4"),
        "a `uses:` ref moved under `-l annotated`:\n{}",
        workflow_text(&dir)
    );
    assert!(
        workflow_text(&dir).contains("jdx/mise-action@v2"),
        "a `uses:` ref moved under `-l annotated`:\n{}",
        workflow_text(&dir)
    );
    assert_eq!(report["summary"]["errors"], 0, "{report:#}");
}

/// The same through the explicit-file branch, which applies `--lang` separately
/// from the walk, and with a source's own lang rather than `annotated`. This is
/// the spelling a user reaches for first: the annotation says
/// `github-releases`, so that is what they select.
#[test]
fn an_explicit_workflow_is_reached_by_the_annotations_own_source_lang() {
    let dir = fixture(WORKFLOW_WITH_VERSION_REFS, PINS);

    let output = run(
        &dir,
        &[
            "--apply",
            "--no-cache",
            "-l",
            "github-releases",
            "--output",
            "json",
        ],
    );
    let report = json_of(&output);
    assert!(
        output.status.success(),
        "{}\n{report:#}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        workflow_text(&dir).contains("version: 2026.8.14 # upd: github-releases jdx/mise"),
        "`-l github-releases` did not reach the annotation:\n{}",
        workflow_text(&dir)
    );
    // Pinned in the config, so this stays `v2` only because the actions pass
    // did not run.
    assert!(
        workflow_text(&dir).contains("jdx/mise-action@v2"),
        "a `uses:` ref moved under `-l github-releases`:\n{}",
        workflow_text(&dir)
    );
}

/// The negative control for both tests above. A selection naming neither the
/// file's own ecosystem nor anything an annotation can carry still turns the
/// workflow away - otherwise "admitted for its annotations" would just be
/// "admitted", and `--lang` would have stopped meaning anything here.
#[test]
fn a_selection_reaching_neither_the_workflow_nor_its_annotations_finds_nothing() {
    let dir = fixture(WORKFLOW, "[pin]\n\"jdx/mise\" = \"2026.8.14\"\n");
    let before = workflow_text(&dir);

    let output = run_at(
        &dir,
        ".",
        &[
            "--apply",
            "--no-cache",
            "-l",
            "terraform",
            "--output",
            "json",
        ],
    );
    let report = json_of(&output);
    assert!(
        output.status.success(),
        "{}\n{report:#}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        report["files"].as_array().is_none_or(Vec::is_empty),
        "{report:#}"
    );
    assert_eq!(workflow_text(&dir), before);
}

/// `-l actions` keeps meaning what it always did: the file's own dependencies
/// and not the annotations beside them. Without this, widening the file gate
/// would have quietly widened what `-l actions` writes.
#[test]
fn the_files_own_lang_still_leaves_the_annotation_alone() {
    let dir = fixture(WORKFLOW, "[pin]\n\"jdx/mise\" = \"2026.8.14\"\n");

    let output = run(
        &dir,
        &[
            "--apply",
            "--no-cache",
            "--no-update-action-shas",
            "-l",
            "actions",
            "--output",
            "json",
        ],
    );
    let report = json_of(&output);
    assert!(
        output.status.success(),
        "{}\n{report:#}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        workflow_text(&dir).contains("version: 2025.8.18 # upd: github-releases jdx/mise"),
        "the annotation was resolved under `-l actions`:\n{}",
        workflow_text(&dir)
    );
    // The positive control: the actions pass really did run over this file, so
    // the untouched annotation is not a run that examined nothing.
    let skipped = report["files"][0]["skipped"]
        .as_array()
        .expect("skipped[] is an array");
    assert_eq!(skipped.len(), 2, "{report:#}");
}
