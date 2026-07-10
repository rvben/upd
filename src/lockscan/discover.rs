//! Which lockfiles are scannable, and which coverage warnings apply.

use crate::updater::FileType;
use ignore::WalkBuilder;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    Uv,
    Poetry,
    Npm,
    Cargo,
}

#[derive(Debug)]
pub struct ScannableLock {
    pub path: PathBuf,
    pub kind: LockKind,
}

#[derive(Debug, Default)]
pub struct LockDiscovery {
    pub locks: Vec<ScannableLock>,
    pub warnings: Vec<String>,
}

/// Directories that can never contain workspace-member manifests of their
/// own ecosystem: dependency-installation and build trees. The raw
/// membership walk skips them so a gitignored node_modules or target full
/// of vendored manifests does not read as undiscovered members.
const INSTALL_TREES: &[&str] = &[
    "node_modules",
    ".venv",
    "venv",
    "site-packages",
    "target",
    ".git",
];

/// The manifest filename whose workspace the lock kind belongs to.
fn member_manifest_name(kind: LockKind) -> &'static str {
    match kind {
        LockKind::Uv | LockKind::Poetry => "pyproject.toml",
        LockKind::Npm => "package.json",
        LockKind::Cargo => "Cargo.toml",
    }
}

/// Raw recursive walk for `name` files under `root`, skipping INSTALL_TREES.
///
/// Built on `ignore::WalkBuilder` rather than a hand-rolled `read_dir`
/// recursion for one reason: WalkBuilder does not follow symlinks unless
/// told to, so a symlink cycle under `root` (e.g. a farm of directories that
/// symlink each other) terminates instead of recursing unboundedly. Standard
/// ignore filtering is explicitly OFF (`.standard_filters(false)`) because
/// this walk's whole purpose is finding workspace members that discovery may
/// have dropped for being gitignored or config-excluded - the raw membership
/// walk must still see them, or the very files it exists to catch become
/// invisible to it.
fn walk_manifests(root: &Path, name: &str, out: &mut Vec<PathBuf>) {
    let walker = WalkBuilder::new(root)
        .standard_filters(false)
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            !entry
                .file_name()
                .to_str()
                .is_some_and(|d| INSTALL_TREES.contains(&d))
        })
        .build();

    for entry in walker.flatten() {
        let is_file = entry.file_type().is_some_and(|ft| ft.is_file());
        if is_file && entry.path().file_name().and_then(|n| n.to_str()) == Some(name) {
            out.push(entry.path().to_path_buf());
        }
    }
}

/// True when the adjacent package.json declares npm workspaces.
fn has_workspaces_field(package_json: &Path) -> bool {
    std::fs::read_to_string(package_json)
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .is_some_and(|doc| doc.get("workspaces").is_some())
}

/// Ancestors of `start` up to a BOUNDARY: the enclosing git root when one
/// exists (it may lie outside the scanned subdirectory - `upd audit member/`
/// inside a repo still probes up to the repo root), otherwise the deepest
/// scan root containing `start`. Locks are only ever probed within that
/// boundary, so in non-git temp/CI layouts an unrelated parent lock outside
/// the requested scan is never probed. Locating the git root itself stats
/// `.git` on ancestors (metadata-only, possibly to filesystem root when no
/// repo exists); those stats probe no locks and produce no warnings.
fn bounded_ancestors(start: &Path, scan_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut boundary: Option<PathBuf> = None;
    let mut current = Some(start.to_path_buf());
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            boundary = Some(dir);
            break;
        }
        current = dir.parent().map(|p| p.to_path_buf());
    }
    let boundary = boundary.or_else(|| {
        scan_roots
            .iter()
            .filter(|root| start.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned()
    });
    let Some(boundary) = boundary else {
        return vec![start.to_path_buf()];
    };
    let mut out = Vec::new();
    let mut dir = start.to_path_buf();
    loop {
        out.push(dir.clone());
        if dir == boundary {
            break;
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    out
}

pub fn discover_locks(files: &[(PathBuf, FileType)], scan_roots: &[PathBuf]) -> LockDiscovery {
    let mut discovery = LockDiscovery::default();
    let discovered: HashSet<&Path> = files.iter().map(|(p, _)| p.as_path()).collect();
    let mut seen_locks: HashSet<PathBuf> = HashSet::new();

    // Pass 1: sibling locks of discovered manifests.
    for (manifest, file_type) in files {
        let Some(dir) = manifest.parent() else {
            continue;
        };
        let candidates: Vec<(PathBuf, LockKind)> = match file_type {
            FileType::PyProject => vec![
                (dir.join("uv.lock"), LockKind::Uv),
                (dir.join("poetry.lock"), LockKind::Poetry),
            ],
            FileType::PackageJson => {
                let shrinkwrap = dir.join("npm-shrinkwrap.json");
                if shrinkwrap.exists() {
                    vec![(shrinkwrap, LockKind::Npm)]
                } else {
                    vec![(dir.join("package-lock.json"), LockKind::Npm)]
                }
            }
            FileType::CargoToml => vec![(dir.join("Cargo.lock"), LockKind::Cargo)],
            _ => vec![],
        };
        for (lock, kind) in candidates {
            if !lock.exists() || seen_locks.contains(&lock) {
                continue;
            }
            seen_locks.insert(lock.clone());

            if kind == LockKind::Npm && has_workspaces_field(manifest) {
                discovery.warnings.push(format!(
                    "{}: npm workspaces are not yet supported for lock scanning: transitive dependencies of this lockfile are not audited",
                    lock.display()
                ));
                continue;
            }

            if matches!(kind, LockKind::Uv | LockKind::Cargo) {
                let mut members = Vec::new();
                walk_manifests(dir, member_manifest_name(kind), &mut members);
                if let Some(missing) = members.iter().find(|m| !discovered.contains(m.as_path())) {
                    discovery.warnings.push(format!(
                        "{}: workspace membership incomplete ({} not in the discovered set): lockfile not scanned",
                        lock.display(),
                        missing.display()
                    ));
                    continue;
                }
            }

            discovery.locks.push(ScannableLock { path: lock, kind });
        }
    }

    // Pass 2: ancestor-lock warnings for manifests with no lock coverage.
    // Uv/Cargo only - poetry has no workspace-root lock concept - and
    // strictly same-ecosystem: a Cargo lock never covers a Python manifest.
    for (manifest, file_type) in files {
        let (lock_name, kind, kind_name): (&str, LockKind, &str) = match file_type {
            FileType::PyProject => ("uv.lock", LockKind::Uv, "pyproject.toml"),
            FileType::CargoToml => ("Cargo.lock", LockKind::Cargo, "Cargo.toml"),
            _ => continue,
        };
        let Some(dir) = manifest.parent() else {
            continue;
        };
        // Covered if a scannable lock OF THIS KIND sits in this dir or an ancestor.
        let covered = discovery.locks.iter().any(|l| {
            l.kind == kind
                && l.path
                    .parent()
                    .is_some_and(|lock_dir| dir.starts_with(lock_dir))
        });
        if covered || dir.join(lock_name).exists() {
            continue;
        }
        for ancestor in bounded_ancestors(dir, scan_roots).into_iter().skip(1) {
            let manifest_here = ancestor.join(kind_name);
            let lock_here = ancestor.join(lock_name);
            if lock_here.exists()
                && manifest_here.exists()
                && !discovered.contains(manifest_here.as_path())
            {
                discovery.warnings.push(format!(
                    "{}: workspace root lockfile {} exists outside the scanned paths: transitive dependencies not audited",
                    manifest.display(),
                    lock_here.display()
                ));
                break;
            }
        }
    }

    discovery
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::FileType;
    use std::fs;
    use std::path::PathBuf;

    fn touch(path: &PathBuf, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn sibling_locks_discovered_per_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let py = dir.path().join("pyproject.toml");
        touch(&py, "[project]\nname='t'\n");
        touch(&dir.path().join("uv.lock"), "version = 1\n");
        let files = vec![(py, FileType::PyProject)];
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);
        assert_eq!(d.locks.len(), 1);
        assert_eq!(d.locks[0].kind, LockKind::Uv);
        assert!(d.warnings.is_empty());
    }

    #[test]
    fn npm_shrinkwrap_preferred_and_workspaces_skip() {
        let dir = tempfile::tempdir().unwrap();
        let pj = dir.path().join("package.json");
        touch(&pj, r#"{ "name": "t", "version": "1.0.0" }"#);
        touch(&dir.path().join("package-lock.json"), "{}");
        touch(&dir.path().join("npm-shrinkwrap.json"), "{}");
        let files = vec![(pj.clone(), FileType::PackageJson)];
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);
        assert_eq!(d.locks.len(), 1);
        assert_eq!(d.locks[0].kind, LockKind::Npm);
        assert!(d.locks[0].path.ends_with("npm-shrinkwrap.json"));

        // workspaces field disables scanning with a warning
        touch(&pj, r#"{ "name": "t", "workspaces": ["packages/*"] }"#);
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);
        assert!(d.locks.is_empty());
        assert_eq!(d.warnings.len(), 1);
        assert!(d.warnings[0].contains("npm workspaces are not yet supported"));
    }

    #[test]
    fn partial_workspace_guard_skips_lock_with_undiscovered_member() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Cargo.toml");
        touch(&root, "[workspace]\nmembers=[\"member\"]\n");
        touch(&dir.path().join("Cargo.lock"), "version = 4\n");
        // Member manifest exists on disk but is NOT in the discovered set
        // (simulating gitignore/config-exclude dropping it).
        touch(
            &dir.path().join("member/Cargo.toml"),
            "[package]\nname='m'\nversion='0.1.0'\n",
        );
        let files = vec![(root, FileType::CargoToml)];
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);
        assert!(d.locks.is_empty(), "partial workspace must not be scanned");
        assert_eq!(d.warnings.len(), 1);
        assert!(d.warnings[0].contains("workspace membership incomplete"));
    }

    #[test]
    fn partial_workspace_guard_ignores_installation_trees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Cargo.toml");
        touch(&root, "[package]\nname='t'\nversion='0.1.0'\n");
        touch(&dir.path().join("Cargo.lock"), "version = 4\n");
        // A vendored manifest inside target/ must NOT count as a member.
        touch(
            &dir.path().join("target/vendored/Cargo.toml"),
            "[package]\nname='v'\nversion='0.1.0'\n",
        );
        let files = vec![(root, FileType::CargoToml)];
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);
        assert_eq!(
            d.locks.len(),
            1,
            "installation trees never make membership partial"
        );
        assert!(d.warnings.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn walk_manifests_does_not_follow_symlink_cycles() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Cargo.toml");
        touch(&root, "[package]\nname='t'\nversion='0.1.0'\n");
        touch(&dir.path().join("Cargo.lock"), "version = 4\n");

        // A symlink farm: three directories, each holding a symlink to all
        // three (including itself), forming cycles. Following symlinks here
        // recurses forever; the walk must simply never follow them. The
        // test completing at all (well within nextest's default timeout) is
        // the assertion.
        let farm = dir.path().join("farm");
        for name in ["a", "b", "c"] {
            fs::create_dir_all(farm.join(name)).unwrap();
        }
        for from in ["a", "b", "c"] {
            for to in ["a", "b", "c"] {
                symlink(farm.join(to), farm.join(from).join(format!("to_{to}"))).unwrap();
            }
        }

        let files = vec![(root, FileType::CargoToml)];
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);
        assert_eq!(
            d.locks.len(),
            1,
            "lock still scannable despite the symlink farm"
        );
        assert!(d.warnings.is_empty(), "no spurious membership warnings");
    }

    #[test]
    fn member_manifests_associate_to_ancestor_lock_without_extra_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Cargo.toml");
        let member = dir.path().join("member/Cargo.toml");
        touch(&root, "[workspace]\nmembers=[\"member\"]\n");
        touch(&member, "[package]\nname='m'\nversion='0.1.0'\n");
        touch(&dir.path().join("Cargo.lock"), "version = 4\n");
        let files = vec![(root, FileType::CargoToml), (member, FileType::CargoToml)];
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);
        assert_eq!(d.locks.len(), 1, "one shared lock, discovered once");
        assert!(
            d.warnings.is_empty(),
            "fully discovered workspace has no warnings"
        );
    }

    #[test]
    fn member_only_scan_warns_about_ancestor_lock_within_git_root() {
        let dir = tempfile::tempdir().unwrap();
        // Simulate `upd audit member/` inside a git repo: the workspace root
        // (with lock + manifest) is above the scan root but within the repo.
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        touch(
            &dir.path().join("Cargo.toml"),
            "[workspace]\nmembers=[\"member\"]\n",
        );
        touch(&dir.path().join("Cargo.lock"), "version = 4\n");
        let member = dir.path().join("member/Cargo.toml");
        touch(&member, "[package]\nname='m'\nversion='0.1.0'\n");
        let files = vec![(member.clone(), FileType::CargoToml)];
        let scan_roots = vec![dir.path().join("member")];
        let d = discover_locks(&files, &scan_roots);
        assert!(d.locks.is_empty());
        assert_eq!(d.warnings.len(), 1);
        assert!(d.warnings[0].contains("outside the scanned paths"));
    }

    #[test]
    fn non_git_parent_lock_outside_scan_root_is_not_probed() {
        let dir = tempfile::tempdir().unwrap();
        // No .git anywhere: the boundary is the scan root, so an unrelated
        // parent lock (temp/CI layout) must produce NO warning.
        touch(
            &dir.path().join("Cargo.toml"),
            "[workspace]\nmembers=[\"member\"]\n",
        );
        touch(&dir.path().join("Cargo.lock"), "version = 4\n");
        let member = dir.path().join("member/Cargo.toml");
        touch(&member, "[package]\nname='m'\nversion='0.1.0'\n");
        let files = vec![(member.clone(), FileType::CargoToml)];
        let scan_roots = vec![dir.path().join("member")];
        let d = discover_locks(&files, &scan_roots);
        assert!(d.locks.is_empty());
        assert!(
            d.warnings.is_empty(),
            "no git root: probe stays inside the scan root"
        );
    }

    #[test]
    fn cargo_lock_never_covers_python_manifest() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        // Ancestor has a Cargo lock+manifest pair; the discovered manifest
        // is a pyproject. Strict ecosystem matching: no warning, no coverage.
        touch(
            &dir.path().join("Cargo.toml"),
            "[package]\nname='r'\nversion='0.1.0'\n",
        );
        touch(&dir.path().join("Cargo.lock"), "version = 4\n");
        let py = dir.path().join("svc/pyproject.toml");
        touch(&py, "[project]\nname='s'\n");
        let files = vec![(py, FileType::PyProject)];
        let scan_roots = vec![dir.path().join("svc")];
        let d = discover_locks(&files, &scan_roots);
        assert!(d.locks.is_empty());
        assert!(
            d.warnings.is_empty(),
            "cross-ecosystem locks are invisible to the rule"
        );
    }
}
