//! Cross-file contract for release-derived integration defaults.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

const MANIFEST: &str = include_str!("../release-pins.json");
const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");
const MISE_CONFIG: &str = include_str!("../.mise.toml");
const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
const RUST_TOOLCHAIN: &str = include_str!("../rust-toolchain.toml");
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
            (1, &[("x86_64-unknown-linux-gnu", 1)][..]),
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
fn every_repository_workflow_pins_external_actions_to_full_commits() {
    let directory = repo_root().join(".github/workflows");
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if !matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("yml" | "yaml")
        ) {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy();
        let workflow = std::fs::read_to_string(&path).unwrap();
        for line in workflow.lines() {
            let trimmed = line.trim();
            let Some(reference) = trimmed
                .strip_prefix("uses: ")
                .or_else(|| trimmed.strip_prefix("- uses: "))
            else {
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
    let staged = SYNC_WORKFLOW
        .split_once("          git add \\\n")
        .unwrap()
        .1
        .split_once("          git commit")
        .unwrap()
        .0;
    assert!(!staged.contains(".github/workflows/"));
    assert!(!SYNC_WORKFLOW.contains("git push --force "));
    assert!(!VERSHIP_CONFIG.contains("post-push"));
}

#[test]
fn ci_and_release_jobs_install_only_the_tools_they_use() {
    for (name, workflow) in [("CI", CI_WORKFLOW), ("release", RELEASE_WORKFLOW)] {
        assert!(
            !workflow.contains("install: true"),
            "{name} must not ask mise-action to install every configured tool"
        );
        assert!(
            !workflow.contains("mise install --yes"),
            "{name} must not install unrelated tools as a single concurrent batch"
        );
    }
    assert_eq!(CI_WORKFLOW.matches("run: mise install rust").count(), 2);
    assert!(RELEASE_WORKFLOW.contains("run: mise install cargo-binstall"));
    assert!(!RELEASE_WORKFLOW.contains("mise install cargo:cargo-binstall"));
    assert!(RELEASE_WORKFLOW.contains("run: mise install cargo:maturin cargo:cargo-zigbuild"));

    let mise: toml::Value =
        toml::from_str(include_str!("../.mise.toml")).expect(".mise.toml should be valid TOML");
    assert!(
        mise["tools"]["cargo-binstall"]
            .as_str()
            .is_some_and(|version| !version.is_empty()),
        "cargo-binstall must use mise's prebuilt registry backend",
    );
    assert!(
        mise["tools"].get("cargo:cargo-binstall").is_none(),
        "cargo-binstall must not bootstrap itself from source",
    );
    assert_eq!(
        mise["settings"]["cargo"]["binstall"].as_bool(),
        Some(true),
        "Cargo tools must prefer cargo-binstall's prebuilt artifacts",
    );
    assert_eq!(
        mise["env"]["BINSTALL_DISABLE_TELEMETRY"].as_str(),
        Some("true"),
    );
    let setup = mise["tasks"]["setup"]["run"]
        .as_str()
        .expect("setup task should be a script");
    assert!(!setup.lines().any(|line| line.trim() == "mise install"));
    assert!(setup.contains("mise install cargo-binstall"));
}

#[test]
fn mise_and_rustup_select_the_same_rust_toolchain() {
    let mise: toml::Value = toml::from_str(MISE_CONFIG).unwrap();
    let rustup: toml::Value = toml::from_str(RUST_TOOLCHAIN).unwrap();
    assert_eq!(mise["tools"]["rust"], rustup["toolchain"]["channel"]);
}
