//! Which lockfiles are scannable, and which coverage warnings apply.

use crate::updater::FileType;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use std::collections::{HashMap, HashSet};
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
    /// Discovered same-ecosystem manifests this lock resolves for, per the
    /// nearest-ancestor rule (always contains at least the sibling manifest).
    pub associated_manifests: Vec<PathBuf>,
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

fn workspace_string_list(workspace: &toml::value::Table, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = workspace.get(key) else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err(format!("tool.uv.workspace.{key} must be an array"));
    };
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| format!("tool.uv.workspace.{key} entries must be strings"))
        })
        .collect()
}

fn workspace_globs(patterns: &[String], key: &str) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let normalized = pattern
            .strip_prefix("./")
            .unwrap_or(pattern)
            .trim_end_matches('/');
        let glob = GlobBuilder::new(normalized)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .map_err(|error| {
                format!("invalid tool.uv.workspace.{key} pattern '{pattern}': {error}")
            })?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|error| format!("could not compile tool.uv.workspace.{key}: {error}"))
}

/// Resolve the manifests that uv actually considers members of the workspace
/// rooted at `root_manifest`.
///
/// A nested pyproject is not implicitly a uv workspace member. Only the root
/// and directories selected by `[tool.uv.workspace].members` (minus
/// `exclude`) share the root lockfile. Looking at every pyproject recursively
/// is both over-broad and especially noisy for generated hidden trees such as
/// `.ansible/`.
fn uv_workspace_manifests(root_manifest: &Path) -> Result<HashSet<PathBuf>, String> {
    let mut result = HashSet::from([root_manifest.to_path_buf()]);
    let content = std::fs::read_to_string(root_manifest)
        .map_err(|error| format!("could not read {}: {error}", root_manifest.display()))?;
    let document: toml::Value = toml::from_str(&content)
        .map_err(|error| format!("could not parse {}: {error}", root_manifest.display()))?;
    let Some(workspace) = document
        .get("tool")
        .and_then(|tool| tool.get("uv"))
        .and_then(|uv| uv.get("workspace"))
        .and_then(toml::Value::as_table)
    else {
        return Ok(result);
    };

    let member_patterns = workspace_string_list(workspace, "members")?;
    let exclude_patterns = workspace_string_list(workspace, "exclude")?;
    let members = workspace_globs(&member_patterns, "members")?;
    let excludes = workspace_globs(&exclude_patterns, "exclude")?;
    let Some(root) = root_manifest.parent() else {
        return Ok(result);
    };

    let mut manifests = Vec::new();
    walk_manifests(root, "pyproject.toml", &mut manifests);
    for manifest in manifests {
        if manifest == root_manifest {
            continue;
        }
        let Some(directory) = manifest.parent() else {
            continue;
        };
        let Ok(relative) = directory.strip_prefix(root) else {
            continue;
        };
        let portable = relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if members.is_match(&portable) && !excludes.is_match(&portable) {
            result.insert(manifest);
        }
    }

    Ok(result)
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
    let mut uv_members_by_lock: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();

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
                    crate::path_display::display_path(&lock)
                ));
                continue;
            }

            if matches!(kind, LockKind::Uv | LockKind::Cargo) {
                let members: HashSet<PathBuf> = if kind == LockKind::Uv {
                    match uv_workspace_manifests(manifest) {
                        Ok(members) => members,
                        Err(error) => {
                            discovery.warnings.push(format!(
                                "{}: workspace membership could not be determined ({error}): lockfile not scanned (no workspace member gets lock-based transitive coverage until this is resolved)",
                                crate::path_display::display_path(&lock),
                            ));
                            continue;
                        }
                    }
                } else {
                    let mut members = Vec::new();
                    walk_manifests(dir, member_manifest_name(kind), &mut members);
                    members.into_iter().collect()
                };
                if let Some(missing) = members.iter().find(|m| !discovered.contains(m.as_path())) {
                    discovery.warnings.push(format!(
                        "{}: workspace membership incomplete ({} not in the discovered set): lockfile not scanned (no workspace member gets lock-based transitive coverage until this is resolved)",
                        crate::path_display::display_path(&lock),
                        crate::path_display::display_path(missing)
                    ));
                    continue;
                }
                if kind == LockKind::Uv {
                    uv_members_by_lock.insert(lock.clone(), members);
                }
            }

            discovery.locks.push(ScannableLock {
                path: lock,
                kind,
                associated_manifests: Vec::new(),
            });
        }
    }

    // Association: each discovered same-ecosystem manifest belongs to the
    // scannable lock of its nearest ancestor directory (mirroring how cargo
    // and uv themselves resolve the workspace root); a manifest with a
    // closer lock of its own belongs to that lock instead. Npm and poetry
    // locks cover adjacent manifests only. Only already-scannable locks
    // participate: skipped locks (guards, workspaces) associate nothing.
    for (manifest, file_type) in files {
        let kinds: &[LockKind] = match file_type {
            FileType::PyProject => &[LockKind::Uv, LockKind::Poetry],
            FileType::PackageJson => &[LockKind::Npm],
            FileType::CargoToml => &[LockKind::Cargo],
            _ => continue,
        };
        let Some(dir) = manifest.parent() else {
            continue;
        };
        for &kind in kinds {
            let adjacent_only = matches!(kind, LockKind::Npm | LockKind::Poetry);
            let mut best: Option<usize> = None;
            for (idx, lock) in discovery.locks.iter().enumerate() {
                if lock.kind != kind {
                    continue;
                }
                let Some(lock_dir) = lock.path.parent() else {
                    continue;
                };
                if kind == LockKind::Uv {
                    if uv_members_by_lock
                        .get(&lock.path)
                        .is_some_and(|members| members.contains(manifest))
                    {
                        best = Some(idx);
                    }
                } else if adjacent_only {
                    if lock_dir == dir {
                        best = Some(idx);
                        break;
                    }
                } else if dir.starts_with(lock_dir) {
                    let deeper = |i: usize| {
                        discovery.locks[i]
                            .path
                            .parent()
                            .map(|p| p.components().count())
                            .unwrap_or(0)
                    };
                    if best.is_none_or(|b| deeper(idx) > deeper(b)) {
                        best = Some(idx);
                    }
                }
            }
            if let Some(idx) = best {
                discovery.locks[idx]
                    .associated_manifests
                    .push(manifest.clone());
            }
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
                    crate::path_display::display_path(manifest),
                    crate::path_display::display_path(&lock_here)
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
        assert!(
            d.warnings[0].contains(
                "no workspace member gets lock-based transitive coverage until this is resolved"
            ),
            "warning must spell out the scope of the coverage gap: {}",
            d.warnings[0]
        );
    }

    #[test]
    fn uv_ignores_unrelated_nested_pyprojects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("pyproject.toml");
        touch(
            &root,
            "[project]\nname='root'\nversion='0.1.0'\ndependencies=[]\n",
        );
        touch(&dir.path().join("uv.lock"), "version = 1\n");
        touch(
            &dir.path()
                .join(".ansible/ansible_collections/amazon/aws/pyproject.toml"),
            "[project]\nname='vendored'\nversion='0.1.0'\n",
        );

        let files = vec![(root.clone(), FileType::PyProject)];
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);

        assert_eq!(d.locks.len(), 1, "the root uv.lock remains scannable");
        assert!(
            d.warnings.is_empty(),
            "unrelated pyprojects are not members"
        );
        assert_eq!(d.locks[0].associated_manifests, vec![root]);
    }

    #[test]
    fn uv_guard_only_requires_declared_workspace_members() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("pyproject.toml");
        touch(
            &root,
            "[project]\nname='root'\nversion='0.1.0'\n\n[tool.uv.workspace]\nmembers=['packages/*']\n",
        );
        touch(&dir.path().join("uv.lock"), "version = 1\n");
        let member = dir.path().join("packages/member/pyproject.toml");
        touch(&member, "[project]\nname='member'\nversion='0.1.0'\n");
        touch(
            &dir.path()
                .join(".ansible/ansible_collections/amazon/aws/pyproject.toml"),
            "[project]\nname='vendored'\nversion='0.1.0'\n",
        );

        let files = vec![(root, FileType::PyProject)];
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);

        assert!(d.locks.is_empty(), "an undiscovered real member is unsafe");
        assert_eq!(d.warnings.len(), 1);
        assert!(
            d.warnings[0].contains(&member.display().to_string()),
            "the warning identifies the declared member: {}",
            d.warnings[0]
        );
        assert!(
            !d.warnings[0].contains(".ansible"),
            "unrelated generated projects do not cause the warning"
        );
    }

    #[test]
    fn uv_workspace_globs_and_excludes_drive_association() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("pyproject.toml");
        touch(
            &root,
            "[project]\nname='root'\nversion='0.1.0'\n\n[tool.uv.workspace]\nmembers=['packages/*']\nexclude=['packages/excluded']\n",
        );
        touch(&dir.path().join("uv.lock"), "version = 1\n");
        let member = dir.path().join("packages/member/pyproject.toml");
        let excluded = dir.path().join("packages/excluded/pyproject.toml");
        let unrelated = dir.path().join("tools/unrelated/pyproject.toml");
        for manifest in [&member, &excluded, &unrelated] {
            touch(manifest, "[project]\nname='nested'\nversion='0.1.0'\n");
        }

        let files = vec![
            (root.clone(), FileType::PyProject),
            (member.clone(), FileType::PyProject),
            (excluded, FileType::PyProject),
            (unrelated, FileType::PyProject),
        ];
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);

        assert_eq!(d.locks.len(), 1);
        assert!(d.warnings.is_empty());
        assert_eq!(
            d.locks[0].associated_manifests,
            vec![root, member],
            "only the root and included, non-excluded member share uv.lock"
        );
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
    fn virtual_cargo_workspace_root_associates_member_manifests() {
        let dir = tempfile::tempdir().unwrap();
        // Virtual workspace root: bare [workspace] Cargo.toml + Cargo.lock.
        let root = dir.path().join("Cargo.toml");
        touch(&root, "[workspace]\nmembers=[\"member\"]\n");
        touch(&dir.path().join("Cargo.lock"), "version = 4\n");
        let member = dir.path().join("member/Cargo.toml");
        touch(&member, "[package]\nname='m'\nversion='0.1.0'\n");
        let files = vec![
            (root.clone(), FileType::CargoToml),
            (member.clone(), FileType::CargoToml),
        ];
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);
        assert_eq!(d.locks.len(), 1);
        let assoc = &d.locks[0].associated_manifests;
        assert!(assoc.contains(&root), "sibling root manifest associated");
        assert!(
            assoc.contains(&member),
            "member manifest associated to ancestor lock"
        );
    }

    #[test]
    fn nested_crate_with_closer_lock_associates_there_not_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Cargo.toml");
        touch(&root, "[workspace]\nmembers=[\"member\"]\n");
        touch(&dir.path().join("Cargo.lock"), "version = 4\n");
        let nested = dir.path().join("member/Cargo.toml");
        touch(&nested, "[package]\nname='m'\nversion='0.1.0'\n");
        touch(&dir.path().join("member/Cargo.lock"), "version = 4\n");
        let files = vec![
            (root.clone(), FileType::CargoToml),
            (nested.clone(), FileType::CargoToml),
        ];
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);
        assert_eq!(d.locks.len(), 2);
        let root_lock = d
            .locks
            .iter()
            .find(|l| l.path == dir.path().join("Cargo.lock"))
            .unwrap();
        let nested_lock = d
            .locks
            .iter()
            .find(|l| l.path == dir.path().join("member/Cargo.lock"))
            .unwrap();
        assert!(
            nested_lock.associated_manifests.contains(&nested),
            "closer lock wins"
        );
        assert!(
            !root_lock.associated_manifests.contains(&nested),
            "member no longer belongs to the root lock"
        );
        assert!(root_lock.associated_manifests.contains(&root));
    }

    #[test]
    fn npm_and_poetry_locks_associate_adjacent_manifest_only() {
        let dir = tempfile::tempdir().unwrap();
        let pj = dir.path().join("package.json");
        touch(&pj, r#"{ "name": "t", "version": "1.0.0" }"#);
        touch(
            &dir.path().join("package-lock.json"),
            r#"{ "lockfileVersion": 3 }"#,
        );
        let sub_pj = dir.path().join("sub/package.json");
        touch(&sub_pj, r#"{ "name": "s", "version": "1.0.0" }"#);
        let files = vec![
            (pj.clone(), FileType::PackageJson),
            (sub_pj.clone(), FileType::PackageJson),
        ];
        let scan_roots = vec![dir.path().to_path_buf()];
        let d = discover_locks(&files, &scan_roots);
        assert_eq!(d.locks.len(), 1);
        assert_eq!(
            d.locks[0].associated_manifests,
            vec![pj],
            "npm association is adjacent-only; the subdirectory manifest is not a member"
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
