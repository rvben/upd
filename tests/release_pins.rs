//! Cross-file contract for release-derived integration defaults.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

const MANIFEST: &str = include_str!("../release-pins.json");
const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
const SYNC_WORKFLOW: &str = include_str!("../.github/workflows/sync-release-pins.yml");
const VERSHIP_CONFIG: &str = include_str!("../vership.toml");

fn manifest() -> serde_json::Value {
    serde_json::from_str(MANIFEST).expect("release-pins.json is valid JSON")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn every_release_pin_consumer_matches_the_manifest() {
    let manifest = manifest();
    assert_eq!(manifest["schema"], 1);
    let version = manifest["version"].as_str().unwrap();
    let assets = manifest["assets"].as_object().unwrap();
    assert_eq!(assets.len(), 3);

    let consumers: BTreeMap<&str, (usize, &[(&str, usize)])> = BTreeMap::from([
        (
            ".github/workflows/dependency-health.yml",
            (
                3,
                &[
                    ("aarch64-unknown-linux-gnu", 1),
                    ("x86_64-unknown-linux-gnu", 1),
                    ("x86_64-unknown-linux-musl", 1),
                ][..],
            ),
        ),
        (
            ".github/workflows/dependencies.yml",
            (1, &[("x86_64-unknown-linux-gnu", 1)][..]),
        ),
        (".github/workflows/upd.yml", (1, &[][..])),
        (
            "ci/gitlab-dependency-update.yml",
            (
                2,
                &[
                    ("aarch64-unknown-linux-gnu", 1),
                    ("x86_64-unknown-linux-gnu", 1),
                    ("x86_64-unknown-linux-musl", 1),
                ][..],
            ),
        ),
        (
            "docs/github-actions.md",
            (3, &[("x86_64-unknown-linux-gnu", 1)][..]),
        ),
        (
            "docs/gitlab.md",
            (2, &[("x86_64-unknown-linux-gnu", 1)][..]),
        ),
    ]);

    for (relative, (expected_versions, expected_hashes)) in consumers {
        let content = std::fs::read_to_string(repo_root().join(relative)).unwrap();
        assert_eq!(
            content.matches(version).count(),
            expected_versions,
            "unexpected release-version count in {relative}"
        );
        for (target, expected_count) in expected_hashes {
            let checksum = assets[*target]["sha256"].as_str().unwrap();
            assert_eq!(
                content.matches(checksum).count(),
                *expected_count,
                "unexpected {target} checksum count in {relative}"
            );
        }
    }
}

#[test]
fn release_pin_synchronizer_accepts_the_checked_in_state() {
    let output = Command::new("python3")
        .arg(repo_root().join("scripts/sync-release-pins.py"))
        .arg("--check")
        .current_dir(repo_root())
        .output()
        .expect("python3 starts");
    assert!(
        output.status.success(),
        "synchronizer failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("release pins are consistent"));
}

#[test]
fn release_workflows_pin_every_external_action_to_a_full_commit() {
    for (name, workflow) in [
        ("release", RELEASE_WORKFLOW),
        ("release-pin sync", SYNC_WORKFLOW),
    ] {
        for line in workflow.lines() {
            let Some(reference) = line.trim().strip_prefix("uses: ") else {
                continue;
            };
            if reference.starts_with("./") {
                continue;
            }
            let revision = reference
                .split_once('@')
                .unwrap_or_else(|| panic!("{name} workflow action lacks a revision: {reference}"))
                .1
                .split_whitespace()
                .next()
                .unwrap();
            assert!(
                revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "{name} workflow action is not commit-pinned: {reference}"
            );
        }
    }
}

#[test]
fn release_pin_publication_keeps_its_recovery_and_integrity_guards() {
    assert!(RELEASE_WORKFLOW.contains("fail_on_unmatched_files: true"));
    assert!(RELEASE_WORKFLOW.contains("actions/attest@"));
    assert!(RELEASE_WORKFLOW.contains("uses: ./.github/workflows/sync-release-pins.yml"));
    assert!(SYNC_WORKFLOW.contains("git merge-base --is-ancestor"));
    assert!(SYNC_WORKFLOW.contains("gh attestation verify"));
    assert!(SYNC_WORKFLOW.contains("--force-with-lease="));
    assert!(SYNC_WORKFLOW.contains("--match-head-commit"));
    assert!(!SYNC_WORKFLOW.contains("git push --force "));
    assert!(!VERSHIP_CONFIG.contains("post-push"));
}
