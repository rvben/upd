use super::*;
use crate::config::UpdConfig;
use crate::cooldown::CooldownPolicy;
use crate::registry::MockRegistry;
use crate::updater::{BumpFilter, BumpKind, FileType};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OLD: &str = "00455b0a3690d3f5dc61e9aef4277dc86235b73f";
const NEW: &str = "4975466d324710c576dc11ad614684e6bd8cad8e";
const OTHER: &str = "11b3f4e8c1d2a0b9e8f7a6b5c4d3e2f1a0b9c8d7";
const OTHER_NEW: &str = "22c4a5f9d2e3b1cafe98b7c6d5e4f3a2b1c0d9e8";

/// Seconds since the epoch of 2026-09-01T00:00:00Z.
const LOCKED_AT: i64 = 1_788_220_800;

fn node(kind: &str, owner: &str, repo: &str, reference: Option<&str>, rev: &str) -> Value {
    let mut original = serde_json::json!({ "type": kind, "owner": owner, "repo": repo });
    if let Some(reference) = reference {
        original["ref"] = reference.into();
    }
    serde_json::json!({
        "locked": {
            "lastModified": LOCKED_AT,
            "narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "owner": owner,
            "repo": repo,
            "rev": rev,
            "type": kind,
        },
        "original": original,
    })
}

fn lock(nodes: Vec<(&str, Value)>, root_inputs: Value) -> String {
    let mut all = serde_json::Map::new();
    for (name, value) in nodes {
        all.insert(name.to_string(), value);
    }
    all.insert(
        "root".to_string(),
        serde_json::json!({ "inputs": root_inputs }),
    );
    serde_json::to_string_pretty(&serde_json::json!({
        "nodes": all,
        "root": "root",
        "version": 7,
    }))
    .unwrap()
}

fn nixpkgs_lock(rev: &str) -> String {
    lock(
        vec![(
            "nixpkgs",
            node("github", "NixOS", "nixpkgs", Some("nixos-unstable"), rev),
        )],
        serde_json::json!({ "nixpkgs": "nixpkgs" }),
    )
}

struct Flake {
    dir: TempDir,
}

impl Flake {
    fn new(lock: &str) -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("flake.lock"), lock).unwrap();
        Self { dir }
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.path().join("flake.lock")
    }

    fn lock(&self) -> String {
        std::fs::read_to_string(self.lock_path()).unwrap()
    }

    /// A stand-in for `nix` that records its arguments and replaces the lock
    /// with `replacement`, as `nix flake update` rewrites it in place.
    #[cfg(unix)]
    fn fake_nix(&self, replacement: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let replacement_path = self.dir.path().join("replacement.lock");
        std::fs::write(&replacement_path, replacement).unwrap();
        let script = self.dir.path().join("nix");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{args}'\ncp '{replacement}' flake.lock\n",
                args = self.dir.path().join("args").display(),
                replacement = replacement_path.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn nix_args(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("args"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

async fn github_head(server: &MockServer, route: &str, rev: &str) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_string(rev))
        .expect(1)
        .mount(server)
        .await;
}

fn updater(server: &MockServer) -> FlakeLockUpdater {
    let mut updater =
        FlakeLockUpdater::with_endpoints(server.uri(), Some(format!("{}/gitlab", server.uri())))
            // Any attempt to run Nix in a dry-run test fails loudly.
            .with_nix_program("/nonexistent/nix");
    // Independent of a GITLAB_TOKEN in the environment running the tests.
    updater.gitlab_token = None;
    updater
}

fn registry() -> MockRegistry {
    MockRegistry::new("unused")
}

fn dry_run() -> UpdateOptions {
    UpdateOptions::new(true, false)
}

fn config(content: &str) -> Arc<UpdConfig> {
    Arc::new(
        UpdConfig::parse_with_warnings(content, "test.toml")
            .unwrap()
            .0,
    )
}

fn cooldown(days: i64, now: DateTime<Utc>) -> UpdateOptions {
    let policy = CooldownPolicy {
        default: Duration::days(days),
        per_ecosystem: HashMap::new(),
        force_override: None,
    };
    UpdateOptions::new(true, false).with_cooldown_policy(policy, now)
}

#[test]
fn flake_lock_is_detected_as_a_nix_file() {
    let detected = FileType::detect(Path::new("repo/flake.lock"));
    assert_eq!(detected, Some(FileType::FlakeLock));
    assert_eq!(FileType::FlakeLock.lang(), Lang::Nix);
    assert_eq!(FileType::detect(Path::new("repo/flake.nix")), None);
}

#[test]
fn only_direct_inputs_with_their_own_node_are_read() {
    let content = lock(
        vec![
            (
                "nixpkgs",
                node("github", "NixOS", "nixpkgs", Some("nixos-unstable"), OLD),
            ),
            (
                "utils",
                node("github", "numtide", "flake-utils", None, OTHER),
            ),
            // A transitive input: locked in the file, but not the root's to move.
            (
                "systems",
                node("github", "nix-systems", "default", None, OTHER),
            ),
        ],
        serde_json::json!({
            "nixpkgs": "nixpkgs",
            "utils": "utils",
            "pinned-pkgs": ["nixpkgs"],
        }),
    );
    let inputs = parse_inputs(&content).unwrap();
    let names: Vec<_> = inputs.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["nixpkgs", "utils"]);
    assert_eq!(
        inputs[0].source,
        Source::GitHub {
            owner: "NixOS".into(),
            repo: "nixpkgs".into(),
            reference: Some("nixos-unstable".into()),
        }
    );
    assert_eq!(inputs[0].locked_rev.as_deref(), Some(OLD));
    assert_eq!(
        inputs[0].last_modified,
        DateTime::from_timestamp(LOCKED_AT, 0)
    );
}

#[test]
fn a_revision_named_in_flake_nix_and_registry_inputs_are_not_followed() {
    let mut pinned = node("github", "NixOS", "nixpkgs", None, OLD);
    pinned["original"]["rev"] = OLD.into();
    let mut indirect = node("github", "NixOS", "nixpkgs", None, OLD);
    indirect["original"] = serde_json::json!({ "type": "indirect", "id": "nixpkgs" });
    let mut tarball = node("github", "x", "y", None, OLD);
    tarball["original"] =
        serde_json::json!({ "type": "tarball", "url": "https://example.org/x.tar.gz" });
    tarball["locked"]["type"] = "tarball".into();
    let content = lock(
        vec![
            ("pinned", pinned),
            ("indirect", indirect),
            ("tarball", tarball),
        ],
        serde_json::json!({ "pinned": "pinned", "indirect": "indirect", "tarball": "tarball" }),
    );
    let inputs = parse_inputs(&content).unwrap();
    let source = |name: &str| &inputs.iter().find(|i| i.name == name).unwrap().source;
    assert_eq!(source("pinned"), &Source::PinnedRev);
    assert!(matches!(source("indirect"), Source::Unsupported(_)));
    assert!(matches!(source("tarball"), Source::Unsupported(_)));
}

#[tokio::test]
async fn a_moved_branch_is_reported_as_a_revision_update() {
    let server = MockServer::start().await;
    github_head(&server, "/repos/NixOS/nixpkgs/commits/nixos-unstable", NEW).await;
    let flake = Flake::new(&nixpkgs_lock(OLD));

    let result = updater(&server)
        .update(&flake.lock_path(), &registry(), dry_run())
        .await
        .unwrap();

    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(
        result.updated,
        vec![("nixpkgs".into(), OLD[..12].into(), NEW[..12].into(), None)]
    );
    assert_eq!(result.update_bump(0), BumpKind::Revision);
    assert_eq!(flake.lock(), nixpkgs_lock(OLD), "a dry run writes nothing");
}

#[tokio::test]
async fn an_input_at_its_upstream_head_is_up_to_date() {
    let server = MockServer::start().await;
    github_head(&server, "/repos/NixOS/nixpkgs/commits/nixos-unstable", OLD).await;
    let flake = Flake::new(&nixpkgs_lock(OLD));

    let result = updater(&server)
        .update(&flake.lock_path(), &registry(), dry_run())
        .await
        .unwrap();

    assert!(result.updated.is_empty());
    assert_eq!(result.unchanged, 1);
}

#[tokio::test]
async fn an_input_without_a_ref_follows_the_default_branch() {
    let server = MockServer::start().await;
    github_head(&server, "/repos/numtide/flake-utils/commits/HEAD", NEW).await;
    let flake = Flake::new(&lock(
        vec![("utils", node("github", "numtide", "flake-utils", None, OLD))],
        serde_json::json!({ "utils": "utils" }),
    ));

    let result = updater(&server)
        .update(&flake.lock_path(), &registry(), dry_run())
        .await
        .unwrap();

    assert_eq!(result.updated.len(), 1, "{:?}", result.errors);
}

#[tokio::test]
async fn a_branch_name_with_a_slash_keeps_its_path() {
    let server = MockServer::start().await;
    github_head(&server, "/repos/o/r/commits/release/24.05", NEW).await;
    let flake = Flake::new(&lock(
        vec![("dep", node("github", "o", "r", Some("release/24.05"), OLD))],
        serde_json::json!({ "dep": "dep" }),
    ));

    let result = updater(&server)
        .update(&flake.lock_path(), &registry(), dry_run())
        .await
        .unwrap();

    assert_eq!(result.updated.len(), 1, "{:?}", result.errors);
}

#[tokio::test]
async fn a_gitlab_subgroup_project_is_addressed_as_one_encoded_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/gitlab/projects/group%2Fsub%2Frepo/repository/commits/main",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": NEW })))
        .expect(1)
        .mount(&server)
        .await;
    let flake = Flake::new(&lock(
        vec![(
            "dep",
            node("gitlab", "group%2Fsub", "repo", Some("main"), OLD),
        )],
        serde_json::json!({ "dep": "dep" }),
    ));

    let result = updater(&server)
        .update(&flake.lock_path(), &registry(), dry_run())
        .await
        .unwrap();

    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.updated[0].2, NEW[..12]);
}

#[tokio::test]
async fn a_failed_lookup_is_an_error_not_an_up_to_date_input() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let flake = Flake::new(&nixpkgs_lock(OLD));

    let result = updater(&server)
        .update(&flake.lock_path(), &registry(), dry_run())
        .await
        .unwrap();

    assert_eq!(result.unchanged, 0);
    assert!(result.updated.is_empty());
    assert_eq!(result.errors.len(), 1);
    assert!(
        result.errors[0].starts_with("nixpkgs: GitHub returned 404"),
        "{:?}",
        result.errors
    );
}

#[tokio::test]
async fn a_forge_answer_that_is_not_a_commit_hash_is_refused() {
    let server = MockServer::start().await;
    github_head(
        &server,
        "/repos/NixOS/nixpkgs/commits/nixos-unstable",
        "<html>",
    )
    .await;
    let flake = Flake::new(&nixpkgs_lock(OLD));

    let result = updater(&server)
        .update(&flake.lock_path(), &registry(), dry_run())
        .await
        .unwrap();

    assert!(result.updated.is_empty());
    assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
}

#[tokio::test]
async fn the_bump_ceiling_does_not_hold_back_a_revision() {
    let server = MockServer::start().await;
    github_head(&server, "/repos/NixOS/nixpkgs/commits/nixos-unstable", NEW).await;
    let flake = Flake::new(&nixpkgs_lock(OLD));
    let patch_only = BumpFilter {
        major: false,
        minor: false,
        patch: true,
    };

    let result = updater(&server)
        .update(
            &flake.lock_path(),
            &registry(),
            dry_run().with_bump_filter(patch_only),
        )
        .await
        .unwrap();

    assert_eq!(result.updated.len(), 1);
    assert!(result.capped.is_empty());
    assert!(
        dry_run()
            .with_bump_filter(patch_only)
            .allows_bump_for(Lang::Nix, OLD, NEW)
    );
}

#[tokio::test]
async fn cooldown_holds_an_input_whose_locked_commit_is_younger_than_the_window() {
    let server = MockServer::start().await;
    github_head(&server, "/repos/NixOS/nixpkgs/commits/nixos-unstable", NEW).await;
    let flake = Flake::new(&nixpkgs_lock(OLD));
    let three_days_later = DateTime::from_timestamp(LOCKED_AT, 0).unwrap() + Duration::days(3);

    let result = updater(&server)
        .update(
            &flake.lock_path(),
            &registry(),
            cooldown(7, three_days_later),
        )
        .await
        .unwrap();

    assert!(result.updated.is_empty());
    assert_eq!(
        result.skipped_by_cooldown,
        vec![("nixpkgs".into(), OLD[..12].into(), NEW[..12].into(), None)]
    );
}

#[tokio::test]
async fn cooldown_moves_an_input_to_the_newest_commit_once_the_window_has_passed() {
    let server = MockServer::start().await;
    github_head(&server, "/repos/NixOS/nixpkgs/commits/nixos-unstable", NEW).await;
    let flake = Flake::new(&nixpkgs_lock(OLD));
    let eight_days_later = DateTime::from_timestamp(LOCKED_AT, 0).unwrap() + Duration::days(8);

    let result = updater(&server)
        .update(
            &flake.lock_path(),
            &registry(),
            cooldown(7, eight_days_later),
        )
        .await
        .unwrap();

    assert!(result.skipped_by_cooldown.is_empty());
    assert_eq!(result.updated[0].2, NEW[..12]);
}

#[tokio::test]
async fn cooldown_blocks_an_input_whose_age_the_lock_does_not_record() {
    let server = MockServer::start().await;
    github_head(&server, "/repos/NixOS/nixpkgs/commits/nixos-unstable", NEW).await;
    let mut value: Value = serde_json::from_str(&nixpkgs_lock(OLD)).unwrap();
    value["nodes"]["nixpkgs"]["locked"]
        .as_object_mut()
        .unwrap()
        .remove("lastModified");
    let flake = Flake::new(&serde_json::to_string(&value).unwrap());

    let result = updater(&server)
        .update(&flake.lock_path(), &registry(), cooldown(7, Utc::now()))
        .await
        .unwrap();

    assert!(result.updated.is_empty());
    assert_eq!(result.skipped.len(), 1);
    assert_eq!(result.skipped[0].status, SkipStatus::Blocked);
    assert_eq!(result.skipped[0].reason, "cooldown-age-unknown");
}

#[tokio::test]
async fn ignored_inputs_are_not_looked_up_and_pins_are_refused() {
    // No mock is mounted: any lookup would fail the test with an error entry.
    let server = MockServer::start().await;
    let flake = Flake::new(&lock(
        vec![
            ("nixpkgs", node("github", "NixOS", "nixpkgs", None, OLD)),
            (
                "utils",
                node("github", "numtide", "flake-utils", None, OTHER),
            ),
        ],
        serde_json::json!({ "nixpkgs": "nixpkgs", "utils": "utils" }),
    ));
    let options =
        dry_run().with_config(config("ignore = [\"nixpkgs\"]\n[pin]\nutils = \"1.0.0\"\n"));

    let result = updater(&server)
        .update(&flake.lock_path(), &registry(), options)
        .await
        .unwrap();

    assert_eq!(
        result.ignored,
        vec![("nixpkgs".into(), OLD[..12].into(), None)]
    );
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors[0].starts_with("utils: a flake input cannot be pinned"));
}

#[tokio::test]
async fn unsupported_inputs_are_reported_as_not_examined() {
    let server = MockServer::start().await;
    let mut indirect = node("github", "NixOS", "nixpkgs", None, OLD);
    indirect["original"] = serde_json::json!({ "type": "indirect", "id": "nixpkgs" });
    let flake = Flake::new(&lock(
        vec![("nixpkgs", indirect)],
        serde_json::json!({ "nixpkgs": "nixpkgs" }),
    ));

    let result = updater(&server)
        .update(&flake.lock_path(), &registry(), dry_run())
        .await
        .unwrap();

    assert_eq!(result.skipped.len(), 1);
    assert_eq!(result.skipped[0].status, SkipStatus::NotExamined);
    assert_eq!(result.unchanged, 0, "an unexamined input is not up to date");
}

#[cfg(unix)]
#[tokio::test]
async fn apply_moves_only_the_outdated_inputs_through_nix() {
    let server = MockServer::start().await;
    github_head(&server, "/repos/NixOS/nixpkgs/commits/nixos-unstable", NEW).await;
    github_head(&server, "/repos/numtide/flake-utils/commits/HEAD", OTHER).await;
    let before = lock(
        vec![
            (
                "nixpkgs",
                node("github", "NixOS", "nixpkgs", Some("nixos-unstable"), OLD),
            ),
            (
                "utils",
                node("github", "numtide", "flake-utils", None, OTHER),
            ),
        ],
        serde_json::json!({ "nixpkgs": "nixpkgs", "utils": "utils" }),
    );
    let after = lock(
        vec![
            (
                "nixpkgs",
                node("github", "NixOS", "nixpkgs", Some("nixos-unstable"), NEW),
            ),
            (
                "utils",
                node("github", "numtide", "flake-utils", None, OTHER),
            ),
        ],
        serde_json::json!({ "nixpkgs": "nixpkgs", "utils": "utils" }),
    );
    let flake = Flake::new(&before);
    let nix = flake.fake_nix(&after);

    let result = updater(&server)
        .with_nix_program(nix)
        .update(
            &flake.lock_path(),
            &registry(),
            UpdateOptions::new(false, false),
        )
        .await
        .unwrap();

    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.updated.len(), 1);
    assert_eq!(flake.lock(), after);
    assert_eq!(
        flake.nix_args(),
        [
            "--extra-experimental-features",
            "nix-command flakes",
            "flake",
            "update",
            "nixpkgs"
        ]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn apply_restores_the_lock_when_nix_locks_a_different_commit() {
    let server = MockServer::start().await;
    github_head(&server, "/repos/NixOS/nixpkgs/commits/nixos-unstable", NEW).await;
    let before = nixpkgs_lock(OLD);
    let flake = Flake::new(&before);
    // The branch moved again between the lookup and the update.
    let nix = flake.fake_nix(&nixpkgs_lock(OTHER_NEW));

    let result = updater(&server)
        .with_nix_program(nix)
        .update(
            &flake.lock_path(),
            &registry(),
            UpdateOptions::new(false, false),
        )
        .await
        .unwrap();

    assert!(
        result.updated.is_empty(),
        "a refused write is not an update"
    );
    assert_eq!(result.errors.len(), 1);
    assert!(
        result.errors[0].contains("expected 4975466d"),
        "{:?}",
        result.errors
    );
    assert_eq!(flake.lock(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn apply_restores_the_lock_when_nix_moves_an_input_it_was_not_asked_to() {
    let before = lock(
        vec![
            ("nixpkgs", node("github", "NixOS", "nixpkgs", None, OLD)),
            (
                "utils",
                node("github", "numtide", "flake-utils", None, OTHER),
            ),
        ],
        serde_json::json!({ "nixpkgs": "nixpkgs", "utils": "utils" }),
    );
    let after = lock(
        vec![
            ("nixpkgs", node("github", "NixOS", "nixpkgs", None, NEW)),
            (
                "utils",
                node("github", "numtide", "flake-utils", None, OTHER_NEW),
            ),
        ],
        serde_json::json!({ "nixpkgs": "nixpkgs", "utils": "utils" }),
    );
    let flake = Flake::new(&before);
    let nix = flake.fake_nix(&after);
    let expected = BTreeMap::from([("nixpkgs".to_string(), NEW[..12].to_string())]);

    let error = FlakeLockUpdater::new()
        .with_nix_program(nix)
        .refresh_inputs(&flake.lock_path(), &expected)
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "input `utils` changed after nix flake update, though `utils` was not updated"
    );
    assert_eq!(flake.lock(), before);
}

/// nixpkgs and utils each carry their own `systems` input; only nixpkgs is
/// asked to move. Node names for the transitive inputs are given, since Nix
/// renames them freely.
fn nested_lock(
    pkgs_rev: &str,
    pkgs_systems: &str,
    utils_systems: &str,
    utils_node: &str,
) -> String {
    let mut nixpkgs = node("github", "NixOS", "nixpkgs", None, pkgs_rev);
    nixpkgs["inputs"] = serde_json::json!({ "systems": "systems" });
    let mut utils = node("github", "numtide", "flake-utils", None, OTHER);
    utils["inputs"] = serde_json::json!({ "systems": utils_node });
    lock(
        vec![
            ("nixpkgs", nixpkgs),
            ("utils", utils),
            (
                "systems",
                node("github", "nix-systems", "default", None, pkgs_systems),
            ),
            (
                utils_node,
                node("github", "nix-systems", "default", None, utils_systems),
            ),
        ],
        serde_json::json!({ "nixpkgs": "nixpkgs", "utils": "utils" }),
    )
}

#[cfg(unix)]
#[tokio::test]
async fn apply_accepts_transitive_inputs_moving_with_the_input_that_moved() {
    let flake = Flake::new(&nested_lock(OLD, OLD, OLD, "systems_2"));
    let after = nested_lock(NEW, OTHER_NEW, OLD, "systems_3");
    let nix = flake.fake_nix(&after);
    let expected = BTreeMap::from([("nixpkgs".to_string(), NEW.to_string())]);

    FlakeLockUpdater::new()
        .with_nix_program(nix)
        .refresh_inputs(&flake.lock_path(), &expected)
        .await
        .unwrap();

    assert_eq!(flake.lock(), after);
}

#[cfg(unix)]
#[tokio::test]
async fn apply_restores_the_lock_when_nix_moves_a_transitive_input_of_another_input() {
    let before = nested_lock(OLD, OLD, OLD, "systems_2");
    let flake = Flake::new(&before);
    let nix = flake.fake_nix(&nested_lock(NEW, OLD, OTHER_NEW, "systems_2"));
    let expected = BTreeMap::from([("nixpkgs".to_string(), NEW.to_string())]);

    let error = FlakeLockUpdater::new()
        .with_nix_program(nix)
        .refresh_inputs(&flake.lock_path(), &expected)
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "input `utils/systems` changed after nix flake update, though `utils` was not updated"
    );
    assert_eq!(flake.lock(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn apply_restores_the_lock_when_an_untouched_input_is_refetched_at_the_same_commit() {
    let before = nested_lock(OLD, OLD, OLD, "systems_2");
    let mut refetched: Value = serde_json::from_str(&before).unwrap();
    refetched["nodes"]["utils"]["locked"]["narHash"] =
        "sha256-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=".into();
    refetched["nodes"]["nixpkgs"]["locked"]["rev"] = NEW.into();
    let flake = Flake::new(&before);
    let nix = flake.fake_nix(&serde_json::to_string_pretty(&refetched).unwrap());
    let expected = BTreeMap::from([("nixpkgs".to_string(), NEW.to_string())]);

    let error = FlakeLockUpdater::new()
        .with_nix_program(nix)
        .refresh_inputs(&flake.lock_path(), &expected)
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "input `utils` changed after nix flake update, though `utils` was not updated"
    );
    assert_eq!(flake.lock(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn apply_restores_the_lock_when_an_untouched_input_now_follows_another_branch() {
    let before = nested_lock(OLD, OLD, OLD, "systems_2");
    let mut retargeted: Value = serde_json::from_str(&before).unwrap();
    retargeted["nodes"]["utils"]["original"]["ref"] = "develop".into();
    retargeted["nodes"]["nixpkgs"]["locked"]["rev"] = NEW.into();
    let flake = Flake::new(&before);
    let nix = flake.fake_nix(&serde_json::to_string_pretty(&retargeted).unwrap());
    let expected = BTreeMap::from([("nixpkgs".to_string(), NEW.to_string())]);

    let error = FlakeLockUpdater::new()
        .with_nix_program(nix)
        .refresh_inputs(&flake.lock_path(), &expected)
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "input `utils` changed after nix flake update, though `utils` was not updated"
    );
    assert_eq!(flake.lock(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn apply_restores_the_lock_when_nix_also_locks_a_new_input() {
    let before = nixpkgs_lock(OLD);
    let after = lock(
        vec![
            (
                "nixpkgs",
                node("github", "NixOS", "nixpkgs", Some("nixos-unstable"), NEW),
            ),
            (
                "utils",
                node("github", "numtide", "flake-utils", None, OTHER),
            ),
        ],
        serde_json::json!({ "nixpkgs": "nixpkgs", "utils": "utils" }),
    );
    let flake = Flake::new(&before);
    let nix = flake.fake_nix(&after);
    let expected = BTreeMap::from([("nixpkgs".to_string(), NEW.to_string())]);

    let error = FlakeLockUpdater::new()
        .with_nix_program(nix)
        .refresh_inputs(&flake.lock_path(), &expected)
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "nix flake update also locked the new input `utils`; lock it with `nix flake lock` first"
    );
    assert_eq!(flake.lock(), before);
}

#[test]
fn a_gitlab_token_belongs_to_one_host() {
    assert_eq!(
        gitlab_credentials(None, Some("gitlab.example.org".into())),
        None
    );
    assert_eq!(gitlab_credentials(Some(" ".into()), None), None);
    assert_eq!(
        gitlab_credentials(Some("glpat-x".into()), None),
        Some(("gitlab.com".into(), "glpat-x".into()))
    );
    assert_eq!(
        gitlab_credentials(
            Some("glpat-x".into()),
            Some("https://gitlab.example.org/".into())
        ),
        Some(("gitlab.example.org".into(), "glpat-x".into()))
    );
}

async fn gitlab_lookup(host: &str, token_host: &str) -> (UpdateResult, Vec<wiremock::Request>) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/gitlab/projects/group%2Frepo/repository/commits/main",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": NEW })))
        .mount(&server)
        .await;
    let mut dep = node("gitlab", "group", "repo", Some("main"), OLD);
    dep["original"]["host"] = host.into();
    dep["locked"]["host"] = host.into();
    let flake = Flake::new(&lock(
        vec![("dep", dep)],
        serde_json::json!({ "dep": "dep" }),
    ));

    let result = updater(&server)
        .with_gitlab_token(token_host, "glpat-secret")
        .update(&flake.lock_path(), &registry(), dry_run())
        .await
        .unwrap();
    (result, server.received_requests().await.unwrap())
}

#[tokio::test]
async fn a_gitlab_token_is_sent_to_its_own_host() {
    let (result, requests) = gitlab_lookup("gitlab.example.org", "GitLab.example.org").await;

    assert_eq!(result.updated.len(), 1, "{:?}", result.errors);
    assert_eq!(
        requests[0].headers.get("PRIVATE-TOKEN").unwrap(),
        "glpat-secret"
    );
}

#[tokio::test]
async fn a_gitlab_token_is_never_sent_to_another_host() {
    let (result, requests) = gitlab_lookup("gitlab.example.org", "gitlab.com").await;

    assert_eq!(result.updated.len(), 1, "{:?}", result.errors);
    assert_eq!(requests.len(), 1);
    assert!(requests[0].headers.get("PRIVATE-TOKEN").is_none());
}

#[tokio::test]
async fn a_private_gitlab_project_without_a_token_says_how_to_reach_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let flake = Flake::new(&lock(
        vec![("dep", node("gitlab", "group", "repo", None, OLD))],
        serde_json::json!({ "dep": "dep" }),
    ));

    let result = updater(&server)
        .update(&flake.lock_path(), &registry(), dry_run())
        .await
        .unwrap();

    assert_eq!(
        result.errors,
        [
            "dep: GitLab returned 404 Not Found for group/repo; for a private project set GITLAB_TOKEN, and GITLAB_HOST when it is not on gitlab.com"
        ]
    );
}

#[tokio::test]
async fn apply_without_nix_is_an_error_and_leaves_the_lock_alone() {
    let server = MockServer::start().await;
    github_head(&server, "/repos/NixOS/nixpkgs/commits/nixos-unstable", NEW).await;
    let before = nixpkgs_lock(OLD);
    let flake = Flake::new(&before);

    let result = updater(&server)
        .update(
            &flake.lock_path(),
            &registry(),
            UpdateOptions::new(false, false),
        )
        .await
        .unwrap();

    assert!(result.updated.is_empty());
    assert_eq!(result.errors.len(), 1);
    assert!(
        result.errors[0].contains("requires Nix"),
        "{:?}",
        result.errors
    );
    assert_eq!(flake.lock(), before);
}
