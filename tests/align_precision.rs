//! `align` must report exactly what it would write.
//!
//! A declared version is raised by writing a new string into the file, and the
//! string a writer produces keeps the declaration's own precision and `v`
//! prefix. Two declarations that differ only in a detail the writer cannot
//! express separately (`1.0` against `1.0.0`) therefore already agree: there is
//! no edit that would change one into the other, so neither `--check` nor
//! `--apply` may claim otherwise.
//!
//! These run the real binary across every ecosystem whose writer reaches the
//! alignment path. `align` never hits the network (it compares versions already
//! present in the files), so no registry stubbing is required.

use std::fs;
use std::path::Path;
use std::process::Command;

fn upd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_upd")
}

fn run(args: &[&str], cwd: &Path) -> (String, i32) {
    let output = Command::new(upd_bin())
        .args(args)
        .current_dir(cwd)
        .env("UPD_CACHE_DIR", cwd.join("upd-cache"))
        .output()
        .expect("failed to run upd");
    (
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

/// Two sibling projects each declaring `package` once, in the file `name`.
fn workspace(name: &str, a_contents: &str, b_contents: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    for (dir, contents) in [("proj_a", a_contents), ("proj_b", b_contents)] {
        let path = tmp.path().join(dir).join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
    }
    tmp
}

fn read(tmp: &tempfile::TempDir, dir: &str, name: &str) -> String {
    fs::read_to_string(tmp.path().join(dir).join(name)).unwrap()
}

/// `--check` and `--apply` must agree on whether there is anything to do, and a
/// pair that cannot be written apart must leave both files exactly as found.
///
/// Asserted in both directions. The dependency walk is unordered, and two
/// versions naming one release compare Equal, so a single direction would leave
/// the outcome resting on whichever file the filesystem handed back last. Such a
/// case passes on a creation-ordered filesystem and fails on a hash-ordered one,
/// which is a defect that reaches CI rather than the machine it was written on.
fn assert_already_aligned(name: &str, a: &str, b: &str) {
    assert_already_aligned_in_order(name, a, b);
    assert_already_aligned_in_order(name, b, a);
}

fn assert_already_aligned_in_order(name: &str, a: &str, b: &str) {
    let tmp = workspace(name, a, b);

    let (check_err, check_code) = run(&["align", "--check", "."], tmp.path());
    assert_eq!(
        check_code, 0,
        "{name}: --check must exit 0 when no edit would change a file; stderr: {check_err}"
    );

    let (apply_err, apply_code) = run(&["align", "--apply", "."], tmp.path());
    assert_eq!(
        apply_code, 0,
        "{name}: --apply must exit 0 when there is nothing to write; stderr: {apply_err}"
    );

    assert_eq!(
        read(&tmp, "proj_a", name),
        a,
        "{name}: proj_a was rewritten"
    );
    assert_eq!(
        read(&tmp, "proj_b", name),
        b,
        "{name}: proj_b was rewritten"
    );
}

#[test]
fn cargo_precision_only_difference_is_already_aligned() {
    assert_already_aligned(
        "Cargo.toml",
        "[dependencies]\nserde = \"1.0\"\n",
        "[dependencies]\nserde = \"1.0.0\"\n",
    );
}

#[test]
fn requirements_precision_only_difference_is_already_aligned() {
    assert_already_aligned("requirements.txt", "requests==1.0\n", "requests==1.0.0\n");
}

#[test]
fn dockerfile_precision_only_difference_is_already_aligned() {
    assert_already_aligned("Dockerfile", "FROM nginx:1.25\n", "FROM nginx:1.25.3\n");
}

#[test]
fn workflow_precision_only_difference_is_already_aligned() {
    assert_already_aligned(
        ".github/workflows/ci.yml",
        "jobs:\n  x:\n    steps:\n      - uses: actions/checkout@v4\n",
        "jobs:\n  x:\n    steps:\n      - uses: actions/checkout@v4.1.1\n",
    );
}

#[test]
fn pre_commit_precision_only_difference_is_already_aligned() {
    assert_already_aligned(
        ".pre-commit-config.yaml",
        "repos:\n  - repo: https://github.com/psf/black\n    rev: v1.0\n    hooks: []\n",
        "repos:\n  - repo: https://github.com/psf/black\n    rev: v1.0.0\n    hooks: []\n",
    );
}

/// `v4` and `v4.0.0` name one release: release segments compare with implicit
/// trailing zeros, so neither is higher. Which of them a scan happens to reach
/// first therefore cannot decide anything, and in particular cannot make the
/// more precise declaration the one that gets rewritten. Raising a declaration
/// never shortens it.
///
/// Only one direction fails on a defective build, since whichever declaration
/// wins the tie is left alone. The helper asserts both.
#[test]
fn a_workflow_pair_naming_one_release_is_aligned_in_either_order() {
    let file = ".github/workflows/ci.yml";
    let short = "jobs:\n  x:\n    steps:\n      - uses: actions/checkout@v4\n";
    let long = "jobs:\n  x:\n    steps:\n      - uses: actions/checkout@v4.0.0\n";

    assert_already_aligned(file, short, long);
}

/// The same tie, through pre-commit's own writer.
#[test]
fn a_pre_commit_pair_naming_one_release_is_aligned_in_either_order() {
    let file = ".pre-commit-config.yaml";
    let short = "repos:\n  - repo: https://github.com/psf/black\n    rev: v1.0\n    hooks: []\n";
    let long = "repos:\n  - repo: https://github.com/psf/black\n    rev: v1.0.0\n    hooks: []\n";

    assert_already_aligned(file, short, long);
}

/// A ref names a git tag, so the `v` belongs to the declaration and survives the
/// rewrite. Writing the bare number produces a ref the repository does not
/// publish.
#[test]
fn aligning_a_workflow_keeps_the_v_prefix() {
    let file = ".github/workflows/ci.yml";
    let older = "jobs:\n  x:\n    steps:\n      - uses: actions/checkout@v4\n";
    let newer = "jobs:\n  x:\n    steps:\n      - uses: actions/checkout@v5.0.0\n";
    let tmp = workspace(file, older, newer);

    let (stderr, code) = run(&["align", "--apply", "."], tmp.path());
    assert_eq!(code, 0, "align --apply failed: {stderr}");

    let written = read(&tmp, "proj_a", file);
    assert!(
        written.contains("actions/checkout@v5"),
        "the raised ref must keep its v prefix, got: {written}"
    );
    assert!(
        !written.contains("actions/checkout@5"),
        "the v prefix was stripped, leaving a ref the repository does not publish: {written}"
    );
}

/// The same rev, raised through pre-commit's own writer.
#[test]
fn aligning_a_pre_commit_rev_keeps_the_v_prefix() {
    let file = ".pre-commit-config.yaml";
    let older = "repos:\n  - repo: https://github.com/psf/black\n    rev: v1.0\n    hooks: []\n";
    let newer = "repos:\n  - repo: https://github.com/psf/black\n    rev: v2.0.0\n    hooks: []\n";
    let tmp = workspace(file, older, newer);

    let (stderr, code) = run(&["align", "--apply", "."], tmp.path());
    assert_eq!(code, 0, "align --apply failed: {stderr}");

    let written = read(&tmp, "proj_a", file);
    assert!(
        written.contains("rev: v2"),
        "the raised rev must keep its v prefix, got: {written}"
    );
    assert!(
        !written.contains("rev: 2"),
        "the v prefix was stripped: {written}"
    );
}

/// `--full-precision` writes the target whole, so the same pair it leaves alone
/// by default is a real misalignment here and is raised.
#[test]
fn full_precision_still_raises_a_precision_only_difference() {
    let tmp = workspace(
        "Cargo.toml",
        "[dependencies]\nserde = \"1.0\"\n",
        "[dependencies]\nserde = \"1.0.0\"\n",
    );

    let (check_err, check_code) = run(&["align", "--check", "--full-precision", "."], tmp.path());
    assert_eq!(
        check_code, 1,
        "--full-precision --check must report the pending rewrite; stderr: {check_err}"
    );

    let (apply_err, apply_code) = run(&["align", "--apply", "--full-precision", "."], tmp.path());
    assert_eq!(apply_code, 0, "align --apply failed: {apply_err}");
    assert_eq!(
        read(&tmp, "proj_a", "Cargo.toml"),
        "[dependencies]\nserde = \"1.0.0\"\n"
    );
}

/// A genuine misalignment is still raised, written at the declaration's own
/// precision rather than the target's.
#[test]
fn a_real_misalignment_is_still_raised_at_the_declared_precision() {
    let tmp = workspace("requirements.txt", "requests==0.9\n", "requests==1.0.0\n");

    let (check_err, check_code) = run(&["align", "--check", "."], tmp.path());
    assert_eq!(check_code, 1, "--check must report it; stderr: {check_err}");

    let (apply_err, apply_code) = run(&["align", "--apply", "."], tmp.path());
    assert_eq!(apply_code, 0, "align --apply failed: {apply_err}");
    assert_eq!(read(&tmp, "proj_a", "requirements.txt"), "requests==1.0\n");
    assert_eq!(
        read(&tmp, "proj_b", "requirements.txt"),
        "requests==1.0.0\n"
    );
}

/// The Cargo writer drops build metadata, so a declaration carrying it already
/// names the highest release and only the older sibling is raised.
#[test]
fn cargo_build_metadata_is_not_a_misalignment() {
    let tmp = workspace(
        "Cargo.toml",
        "[dependencies]\nfoo = \"1.0.0+build.1\"\n",
        "[dependencies]\nfoo = \"0.9.0\"\n",
    );

    let (apply_err, apply_code) = run(&["align", "--apply", "."], tmp.path());
    assert_eq!(apply_code, 0, "align --apply failed: {apply_err}");
    assert_eq!(
        read(&tmp, "proj_a", "Cargo.toml"),
        "[dependencies]\nfoo = \"1.0.0+build.1\"\n",
        "the build-metadata declaration must be left alone"
    );
    assert_eq!(
        read(&tmp, "proj_b", "Cargo.toml"),
        "[dependencies]\nfoo = \"1.0.0\"\n"
    );
}
