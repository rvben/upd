//! Regression test: a manifest named as a bare file (`upd --lock Cargo.toml`)
//! must run lockfile tools in the current directory.
//!
//! `Path::parent` returns `Some("")` for a bare file name, and an empty path
//! handed to `Command::current_dir` makes the spawn fail with the same
//! "No such file or directory" error a missing binary produces. The regen
//! step then reports `Failed to run `cargo`: No such file or directory`
//! even though `cargo` is on PATH, and the run exits non-zero after the
//! manifest edits were already applied.
//!
//! This file must hold exactly one test: it changes the process working
//! directory, which is shared between threads, so it cannot run next to
//! sibling tests in the same binary.

use std::path::Path;

use tempfile::TempDir;
use upd::lockfile::regenerate_lockfiles;

/// A dependency-free crate keeps `cargo update` offline: with nothing to
/// resolve, cargo touches no registry, so the only way this fails is the
/// spawn itself.
fn write_fixture_crate(dir: &Path) {
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("lib.rs"), "").unwrap();
    std::fs::write(
        dir.join("Cargo.lock"),
        "version = 3\n\n[[package]]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
}

#[test]
fn bare_manifest_name_regenerates_in_current_directory() {
    let tmp = TempDir::new().unwrap();
    write_fixture_crate(tmp.path());
    std::env::set_current_dir(tmp.path()).unwrap();

    let result = regenerate_lockfiles(Path::new("Cargo.toml"), &[], false);

    assert!(
        !result.no_lockfiles,
        "Cargo.lock sits next to the manifest and must be detected"
    );
    let errors = result.error_messages();
    assert!(
        errors.is_empty(),
        "regeneration must run cargo in the manifest's directory; got: {errors:?}"
    );
}
