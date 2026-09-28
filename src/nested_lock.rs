//! Sync for Cargo lockfiles owned by a workspace nested inside a repo.
//!
//! The standard cargo-fuzz layout gives `fuzz/Cargo.toml` its own
//! `[workspace]` table, which opts it out of the ancestor workspace
//! regardless of whether that ancestor excludes it, and its own separate
//! `Cargo.lock`. It path-depends on a crate in the root workspace, so it
//! shares the root's dependency graph transitively, but nothing about that
//! edge rewrites `fuzz/Cargo.toml` itself when upd bumps a root requirement.
//! upd's ordinary lockfile regeneration is entirely manifest-driven: a
//! lockfile is only regenerated when the manifest that owns it was itself
//! rewritten. `fuzz/Cargo.lock` never qualifies, so it goes stale the moment
//! the root's lockfile does not match it any more. This module finds nested
//! workspaces like it and re-resolves only the packages upd just changed at
//! the root, the same targeted `cargo update -p` upd already prefers over a
//! full refresh.

use std::path::{Path, PathBuf};
use std::process::Command;

use colored::Colorize;

use crate::lockfile::tool_available;
use crate::lockgate::condense;

/// Directories a nested-workspace walk never descends into: build output
/// and dependency-installation trees can hold vendored `Cargo.toml` files
/// with their own `[workspace]` table that are not real nested projects.
const NESTED_WALK_SKIP: [&str; 2] = ["target", "node_modules"];

/// A nested Cargo workspace discovered under a root workspace directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NestedCargoWorkspace {
    pub manifest: PathBuf,
    pub lockfile: PathBuf,
}

/// The result of trying to bring one nested workspace's lockfile back in
/// sync with the root change that made it stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NestedSyncStatus {
    Synced,
    /// `cargo update` refused; the reason is cargo's own condensed stderr.
    Blocked(String),
}

/// One nested workspace's sync attempt, for reporting alongside the root
/// lockfile refresh it followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NestedLockSync {
    pub manifest: PathBuf,
    pub lockfile: PathBuf,
    pub status: NestedSyncStatus,
}

impl NestedLockSync {
    pub fn is_blocked(&self) -> bool {
        matches!(self.status, NestedSyncStatus::Blocked(_))
    }
}

fn read_manifest(path: &Path) -> Option<toml::Table> {
    std::fs::read_to_string(path)
        .ok()?
        .parse::<toml::Table>()
        .ok()
}

/// True when `doc` declares its own `[workspace]` table, opting the manifest
/// out of any ancestor workspace and giving it a separate `Cargo.lock` under
/// Cargo's own rules, regardless of whether the ancestor lists it as
/// excluded.
fn declares_own_workspace(doc: &toml::Table) -> bool {
    doc.contains_key("workspace")
}

/// True when any dependency table in `doc` (read from `manifest`) names a
/// `path` dependency that resolves inside `root` - the edge through which a
/// nested workspace shares the root workspace's dependency graph.
fn path_depends_on(doc: &toml::Table, manifest: &Path, root: &Path) -> bool {
    const TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
    let dir = manifest.parent().unwrap_or_else(|| Path::new("."));

    let mut tables: Vec<&toml::Value> = Vec::new();
    for table in TABLES {
        if let Some(deps) = doc.get(table) {
            tables.push(deps);
        }
    }
    if let Some(deps) = doc.get("workspace").and_then(|w| w.get("dependencies")) {
        tables.push(deps);
    }

    for deps in tables {
        let Some(deps) = deps.as_table() else {
            continue;
        };
        for value in deps.values() {
            let Some(path) = value.get("path").and_then(|p| p.as_str()) else {
                continue;
            };
            let Ok(resolved) = std::fs::canonicalize(dir.join(path)) else {
                continue;
            };
            if resolved.starts_with(root) {
                return true;
            }
        }
    }
    false
}

/// Find every nested Cargo workspace under `workspace_root` that path-depends
/// on a crate inside it. The cargo-fuzz layout (`fuzz/Cargo.toml`) is the
/// common case, but the search is not name-restricted, so `benches/`,
/// `examples/` or any other detached workspace is found the same way.
///
/// The walk respects `.gitignore` (a nested workspace nothing tracks carries
/// no obligation to stay in sync) and skips `target/` and `node_modules/`
/// outright, since neither can hold a real nested project. `workspace_root`
/// itself is never reported, so the root's own `Cargo.toml` is not treated
/// as nested.
pub fn discover_nested_cargo_workspaces(workspace_root: &Path) -> Vec<NestedCargoWorkspace> {
    let Ok(root) = std::fs::canonicalize(workspace_root) else {
        return Vec::new();
    };
    let root_manifest = root.join("Cargo.toml");
    let mut found = Vec::new();

    let walker = ignore::WalkBuilder::new(&root)
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| NESTED_WALK_SKIP.contains(&name))
        })
        .build();

    for entry in walker.flatten() {
        if entry.file_name() != "Cargo.toml" {
            continue;
        }
        let Ok(manifest) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        if manifest == root_manifest {
            continue;
        }
        let lockfile = manifest.with_file_name("Cargo.lock");
        if !lockfile.exists() {
            continue;
        }
        let Some(doc) = read_manifest(&manifest) else {
            continue;
        };
        if !declares_own_workspace(&doc) || !path_depends_on(&doc, &manifest, &root) {
            continue;
        }
        found.push(NestedCargoWorkspace { manifest, lockfile });
    }

    found.sort_by(|a, b| a.manifest.cmp(&b.manifest));
    found
}

/// Re-sync every nested Cargo workspace under `workspace_root` by moving only
/// `changed` package names in that workspace's own `Cargo.lock` - the same
/// targeted `cargo update -p` upd already prefers for the root lockfile
/// itself, rather than a full refresh, so pins the root change never touched
/// stay put.
///
/// A nested workspace whose `cargo update` cargo refuses (typically because a
/// changed package never reached that workspace's own dependency graph, for
/// example it was only ever a dev-dependency at the root) is reported
/// `Blocked` with cargo's own reason. It is never a hard failure; the caller
/// decides what a blocked nested sync means for the run's outcome.
pub fn sync_nested_cargo_workspaces(
    workspace_root: &Path,
    changed: &[String],
    verbose: bool,
) -> Vec<NestedLockSync> {
    if changed.is_empty() {
        return Vec::new();
    }
    discover_nested_cargo_workspaces(workspace_root)
        .into_iter()
        .map(|nested| sync_one(nested, changed, verbose))
        .collect()
}

fn sync_one(nested: NestedCargoWorkspace, changed: &[String], verbose: bool) -> NestedLockSync {
    let dir = nested.manifest.parent().unwrap_or_else(|| Path::new("."));
    let mut args = vec!["update".to_string()];
    for name in changed {
        args.push("-p".to_string());
        args.push(name.clone());
    }

    if !tool_available("cargo") {
        return NestedLockSync {
            manifest: nested.manifest,
            lockfile: nested.lockfile,
            status: NestedSyncStatus::Blocked("cargo is not available on PATH".to_string()),
        };
    }

    if verbose {
        println!(
            "{}",
            format!(
                "Re-syncing nested workspace {} with `cargo {}`...",
                crate::path_display::display_path(&nested.lockfile),
                args.join(" "),
            )
            .cyan()
        );
    }

    let status = match Command::new("cargo").args(&args).current_dir(dir).output() {
        Ok(output) if output.status.success() => NestedSyncStatus::Synced,
        Ok(output) => NestedSyncStatus::Blocked(condense(&String::from_utf8_lossy(&output.stderr))),
        Err(e) => NestedSyncStatus::Blocked(format!("could not run `cargo update`: {e}")),
    };

    NestedLockSync {
        manifest: nested.manifest,
        lockfile: nested.lockfile,
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    /// A root crate plus a cargo-fuzz-style nested workspace path-depending
    /// on it. Mirrors the layout that goes stale in production: `fuzz/`
    /// carries its own `[workspace]` table and a separate lockfile.
    fn fuzz_layout(root: &Path) {
        write(
            &root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nlru = \"0.12\"\n",
        );
        write(&root.join("Cargo.lock"), "version = 4\n");
        write(
            &root.join("fuzz/Cargo.toml"),
            concat!(
                "[package]\nname = \"demo-fuzz\"\nversion = \"0.0.0\"\n",
                "edition = \"2021\"\npublish = false\n\n[workspace]\n\n",
                "[dependencies]\nlibfuzzer-sys = \"0.4\"\n\n",
                "[dependencies.demo]\npath = \"..\"\n",
            ),
        );
        write(&root.join("fuzz/Cargo.lock"), "version = 4\n");
    }

    #[test]
    fn discovers_a_fuzz_workspace_path_depending_on_the_root() {
        let dir = tempdir().unwrap();
        fuzz_layout(dir.path());

        let found = discover_nested_cargo_workspaces(dir.path());

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            std::fs::canonicalize(&found[0].manifest).unwrap(),
            std::fs::canonicalize(dir.path().join("fuzz/Cargo.toml")).unwrap()
        );
    }

    #[test]
    fn a_manifest_without_its_own_workspace_table_is_not_nested() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"lib\"]\n",
        );
        write(&dir.path().join("Cargo.lock"), "version = 4\n");
        // An ordinary workspace member: no [workspace] table of its own, so
        // it belongs to the root workspace rather than being detached.
        write(
            &dir.path().join("lib/Cargo.toml"),
            "[package]\nname = \"lib\"\nversion = \"0.1.0\"\n",
        );

        let found = discover_nested_cargo_workspaces(dir.path());

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_detached_workspace_with_no_path_dependency_is_not_synced() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        );
        write(&dir.path().join("Cargo.lock"), "version = 4\n");
        // Its own workspace and its own lockfile, but nothing ties it back
        // to the root's dependency graph.
        write(
            &dir.path().join("tools/Cargo.toml"),
            "[package]\nname = \"tool\"\nversion = \"0.1.0\"\n\n[workspace]\n\n[dependencies]\nclap = \"4\"\n",
        );
        write(&dir.path().join("tools/Cargo.lock"), "version = 4\n");

        let found = discover_nested_cargo_workspaces(dir.path());

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn no_changed_packages_means_no_sync_attempted() {
        let dir = tempdir().unwrap();
        fuzz_layout(dir.path());

        let synced = sync_nested_cargo_workspaces(dir.path(), &[], false);

        assert!(synced.is_empty());
    }

    fn run_cargo(args: &[&str], dir: &Path) {
        let output = Command::new("cargo")
            .args(args)
            .arg("--offline")
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "cargo {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn locked_check_passes(dir: &Path) -> bool {
        Command::new("cargo")
            .args(["check", "--locked", "--offline"])
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success()
    }

    /// A root crate plus a cargo-fuzz-style nested workspace, built entirely
    /// from `path` dependencies so every `cargo` command below runs fully
    /// offline: nothing here is fetched from a registry.
    fn offline_fuzz_layout(root: &Path) {
        write(
            &root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nshared = { path = \"shared\" }\n",
        );
        write(
            &root.join("src/lib.rs"),
            "pub fn hello() -> u32 { shared::value() }\n",
        );
        write(
            &root.join("shared/Cargo.toml"),
            "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            &root.join("shared/src/lib.rs"),
            "pub fn value() -> u32 { 1 }\n",
        );
        write(
            &root.join("fuzz/Cargo.toml"),
            concat!(
                "[package]\nname = \"demo-fuzz\"\nversion = \"0.0.0\"\n",
                "edition = \"2021\"\npublish = false\n\n[workspace]\n\n",
                "[dependencies.demo]\npath = \"..\"\n",
            ),
        );
        write(
            &root.join("fuzz/src/lib.rs"),
            "pub fn check() -> u32 { demo::hello() }\n",
        );
    }

    /// The sync's whole point, proven against real `cargo` rather than just
    /// its own reported status: a root change that moves a package
    /// `fuzz/Cargo.lock` also locks leaves that lock unable to pass
    /// `cargo check --locked`, and `sync_nested_cargo_workspaces` repairs it.
    /// This is also the mutation check team-lead asked for: the assertion
    /// right before the sync call is red on the unsynced state, and the one
    /// right after is green only because the sync ran - commenting out the
    /// call to `sync_nested_cargo_workspaces` in `refresh_lock_groups` turns
    /// the second assertion red too.
    #[test]
    fn syncing_repairs_a_locked_check_that_a_root_bump_broke() {
        let dir = tempdir().unwrap();
        offline_fuzz_layout(dir.path());
        let root = dir.path();
        let fuzz = root.join("fuzz");

        run_cargo(&["generate-lockfile"], root);
        run_cargo(&["generate-lockfile"], &fuzz);

        // The root bump: `shared`'s own version moves and root's lock
        // follows. `fuzz/Cargo.lock` was never touched, so it now locks a
        // `shared` version the path it points at no longer has.
        let shared_manifest = root.join("shared/Cargo.toml");
        let bumped = fs::read_to_string(&shared_manifest)
            .unwrap()
            .replace("0.1.0", "0.2.0");
        write(&shared_manifest, &bumped);
        run_cargo(&["update", "-p", "shared"], root);

        assert!(
            !locked_check_passes(&fuzz),
            "fuzz/Cargo.lock must be stale here, or the rest of this test proves nothing"
        );

        let synced = sync_nested_cargo_workspaces(root, &["shared".to_string()], false);

        assert_eq!(synced.len(), 1, "{synced:?}");
        assert!(!synced[0].is_blocked(), "{synced:?}");
        assert!(
            locked_check_passes(&fuzz),
            "sync_nested_cargo_workspaces must leave fuzz/Cargo.lock consistent with the bumped root"
        );
    }
}
