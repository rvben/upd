//! `upd align` and annotated files.
//!
//! v1 excludes annotated occurrences from alignment outright: `find_alignments`
//! skips any group whose `Lang` is `Lang::Annotated` (`src/align.rs:142`). The
//! reason is that a package name on an annotated line is only meaningful
//! together with its source. Two lines naming `foo` can point at PyPI and npm,
//! and one key per name across all seven sources is not a grouping align can act
//! on safely.
//!
//! Align never reaches the network - it compares versions already present in the
//! scanned files - so this file mocks no registry, matching `tests/align_config.rs`.
//! If a test here ever needs a `MockServer`, align has started making requests
//! and that is the defect.

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
        // Align is network-free. Do not let inherited TLS-bundle overrides make
        // registry construction fail before the alignment code can run.
        .env_remove("UPD_CA_BUNDLE")
        .env_remove("REQUESTS_CA_BUNDLE")
        .env_remove("CURL_CA_BUNDLE")
        .env_remove("SSL_CERT_FILE")
        .output()
        .expect("failed to run upd");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

/// The misaligned package names `align --format json` reports, in the order the
/// report lists them. `emit_align_json` (`src/main.rs:2505-2513`) has already
/// filtered `packages[]` to the misaligned, config-visible set, so presence in
/// this list is the assertion.
fn misaligned_names(stdout: &str) -> Vec<String> {
    let report: serde_json::Value = serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("align must emit JSON: {e}\n{stdout}"));
    let mut names: Vec<String> = report["packages"]
        .as_array()
        .expect("packages[] is an array")
        .iter()
        .filter_map(|p| p["package"].as_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

fn files_scanned(stdout: &str) -> u64 {
    let report: serde_json::Value = serde_json::from_str(stdout).expect("align must emit JSON");
    report["summary"]["files_scanned"]
        .as_u64()
        .unwrap_or_else(|| panic!("summary.files_scanned missing:\n{stdout}"))
}

/// A cross-source name collision, which is the case the exclusion exists for.
/// Two Makefiles pin the same NAME at different versions from different
/// registries. If annotated lines participated in alignment they would form one
/// group, `find_highest_version` would call `3.4.0` the highest, and `--apply`
/// would write npm's release into a PyPI pin.
///
/// The control lives in the SAME fixture and the SAME invocation: `pyproject.toml`
/// and `requirements.txt` disagree about `rich`, and that misalignment must
/// still be found and still be written by `--apply`. A control in a separate run
/// would only prove align works somewhere, not that it kept working here.
#[test]
fn annotated_pins_never_align_even_when_the_name_collides() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("a")).unwrap();
    std::fs::create_dir(tmp.path().join("b")).unwrap();

    let makefile_a = tmp.path().join("a/Makefile");
    let makefile_b = tmp.path().join("b/Makefile");
    let a_original = "FOO ?= 1.2.0  # upd: pypi foo\n";
    let b_original = "FOO ?= 3.4.0  # upd: npm foo\n";
    std::fs::write(&makefile_a, a_original).unwrap();
    std::fs::write(&makefile_b, b_original).unwrap();
    std::fs::write(
        tmp.path().join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0.1.0\"\ndependencies = [\"rich==13.0.0\"]\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("requirements.txt"), "rich==14.0.0\n").unwrap();

    let (stdout, stderr, code) = run(&["align", "--format", "json", "."], tmp.path());
    assert_eq!(code, 1, "the rich misalignment must still exit 1: {stderr}");
    assert_eq!(
        misaligned_names(&stdout),
        vec!["rich".to_string()],
        "only the two Python manifests may align:\n{stdout}"
    );

    let (_stdout, stderr, code) = run(&["align", "--apply", "--output", "text", "."], tmp.path());
    assert_eq!(code, 0, "--apply exits 0: {stderr}");

    assert_eq!(
        std::fs::read_to_string(&makefile_a).unwrap(),
        a_original,
        "the PyPI pin must be byte-identical after align --apply"
    );
    assert_eq!(
        std::fs::read_to_string(&makefile_b).unwrap(),
        b_original,
        "the npm pin must be byte-identical after align --apply"
    );
    assert!(
        std::fs::read_to_string(tmp.path().join("pyproject.toml"))
            .unwrap()
            .contains("rich==14.0.0"),
        "control: align --apply must still raise the pyproject pin to 14.0.0"
    );
}

/// The exclusion is not conditional on a source disagreement. Two annotated
/// files, same source, same package, different versions: still nothing to
/// align. This is the case V2.1 flips - once an annotated dependency carries its
/// own ecosystem `Lang`, this fixture becomes a genuine `Lang::Python`
/// misalignment and this test must be rewritten, not deleted. The
/// `requirements.txt` pair is the control that align is still working.
#[test]
fn two_annotated_pins_of_the_same_source_are_still_not_aligned() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("a")).unwrap();
    std::fs::create_dir(tmp.path().join("b")).unwrap();
    std::fs::write(
        tmp.path().join("a/Makefile"),
        "RUFF ?= 0.13.0  # upd: pypi ruff\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("b/Makefile"),
        "RUFF ?= 0.12.0  # upd: pypi ruff\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("a/requirements.txt"), "rich==13.0.0\n").unwrap();
    std::fs::write(tmp.path().join("b/requirements.txt"), "rich==14.0.0\n").unwrap();

    let (stdout, stderr, code) = run(&["align", "--format", "json", "."], tmp.path());
    assert_eq!(code, 1, "the rich misalignment must still exit 1: {stderr}");
    assert_eq!(
        misaligned_names(&stdout),
        vec!["rich".to_string()],
        "ruff is annotated in both files and must not align:\n{stdout}"
    );
}

/// The v1 behavior is asymmetric: `upd align` reports no annotated occurrence
/// under any `--lang`, including `--lang annotated`. The update half
/// of the same asymmetry - `--lang python` DOES select an annotated PyPI pin -
/// is `lang_python_selects_the_pypi_pin_and_skips_the_github_pin` in
/// `tests/annotated_pins.rs`, which needs a registry mock and so cannot live
/// here.
///
/// Each run carries its own control. The first two report `rich`, which proves
/// align ran and found something. The third discovers no Python manifest at all,
/// so its control is `files_scanned == 2`: the two Makefiles WERE opened and
/// were excluded, rather than never being discovered.
#[test]
fn align_reports_no_annotated_occurrence_under_any_lang() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("a")).unwrap();
    std::fs::create_dir(tmp.path().join("b")).unwrap();
    std::fs::write(
        tmp.path().join("a/Makefile"),
        "RUFF ?= 0.13.0  # upd: pypi ruff\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("b/Makefile"),
        "RUFF ?= 0.12.0  # upd: pypi ruff\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("pyproject.toml"),
        "[project]\nname = \"t\"\nversion = \"0.1.0\"\ndependencies = [\"rich==13.0.0\"]\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("requirements.txt"), "rich==14.0.0\n").unwrap();

    // No --lang: the annotated group is built and then skipped by find_alignments.
    let (stdout, stderr, code) = run(&["align", "--format", "json", "."], tmp.path());
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        misaligned_names(&stdout),
        vec!["rich".to_string()],
        "{stdout}"
    );

    // --lang python: the Makefiles are still admitted by discovery,
    // and scan_packages' langs filter drops their occurrences.
    let (stdout, stderr, code) = run(
        &["align", "--lang", "python", "--format", "json", "."],
        tmp.path(),
    );
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        misaligned_names(&stdout),
        vec!["rich".to_string()],
        "--lang python must not pull annotated pins into alignment:\n{stdout}"
    );
    assert_eq!(
        files_scanned(&stdout),
        4,
        "--lang python must still admit both Makefiles:\n{stdout}"
    );

    // --lang annotated: only the Makefiles are discovered, and align still has
    // nothing to say about them.
    let (stdout, stderr, code) = run(
        &["align", "--lang", "annotated", "--format", "json", "."],
        tmp.path(),
    );
    assert_eq!(code, 0, "nothing is misaligned, so align exits 0: {stderr}");
    assert!(
        misaligned_names(&stdout).is_empty(),
        "--lang annotated must report no alignment:\n{stdout}"
    );
    assert_eq!(
        files_scanned(&stdout),
        2,
        "control: both Makefiles were opened and excluded, not skipped by discovery:\n{stdout}"
    );
}
