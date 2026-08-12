//! Positional provenance inputs: direct dependency declarations read from
//! manifests, keyed by DEPENDENCY KEY. Provenance is positional, never
//! inferred from range admission alone: the Manifest tag applies only to
//! the lock entry that IS the direct resolved target of a manifest
//! dependency (Task 3 consumes these inputs for classification).

use anyhow::{Context, Result};
use std::path::Path;

/// A direct dependency declaration read from a manifest.
#[derive(Debug, Clone)]
pub struct DirectDep {
    /// Manifest-side dependency key (package.json key / Cargo.toml key).
    pub key: String,
    /// Registry package name: the alias target for npm `npm:` specs, the
    /// `package` field for Cargo renames; equals `key` otherwise.
    pub package: String,
    /// Raw spec / version-requirement fragment as written in the manifest.
    pub spec: String,
}

/// Registry name and range of an npm alias spec: `npm:react@^18` splits to
/// ("react", "^18"), `npm:@scope/name@1.2.3` to ("@scope/name", "1.2.3").
/// The range separator is the LAST `@` so scoped alias targets keep their
/// leading `@scope/`. Returns None for non-alias specs.
pub(crate) fn split_npm_alias(spec: &str) -> Option<(&str, &str)> {
    let rest = spec.strip_prefix("npm:")?;
    match rest.rfind('@') {
        Some(idx) if idx > 0 => Some((&rest[..idx], &rest[idx + 1..])),
        _ => Some((rest, "")),
    }
}

/// Direct dependency declarations of a package.json, across the same
/// sections the updater reads (src/updater/package_json.rs
/// DEPENDENCY_SECTIONS): dependencies, devDependencies, peerDependencies,
/// optionalDependencies. Non-string specs are skipped.
pub fn npm_direct_deps(package_json: &Path) -> Result<Vec<DirectDep>> {
    let content = std::fs::read_to_string(package_json)
        .with_context(|| format!("reading {}", package_json.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("parsing {}", package_json.display()))?;
    let mut deps = Vec::new();
    for section in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        let Some(table) = doc.get(section).and_then(|v| v.as_object()) else {
            continue;
        };
        for (key, value) in table {
            let Some(spec) = value.as_str() else { continue };
            let package = match split_npm_alias(spec) {
                Some((target, _)) => target.to_string(),
                None => key.clone(),
            };
            deps.push(DirectDep {
                key: key.clone(),
                package,
                spec: spec.to_string(),
            });
        }
    }
    Ok(deps)
}

/// Direct dependency declarations of a Cargo.toml, across the same sections
/// parse_dependencies walks (src/updater/cargo_toml.rs): [dependencies],
/// [dev-dependencies], [build-dependencies], [workspace.dependencies], and
/// target.*.{dependencies,dev-dependencies,build-dependencies}. Path/git
/// dependencies are skipped (no registry identity). Renamed entries record
/// both the TOML key and the real package name.
pub fn cargo_direct_deps(cargo_toml: &Path) -> Result<Vec<DirectDep>> {
    let content = std::fs::read_to_string(cargo_toml)
        .with_context(|| format!("reading {}", cargo_toml.display()))?;
    let doc: toml::Table = content
        .parse()
        .with_context(|| format!("parsing {}", cargo_toml.display()))?;

    fn parse_table(table: &toml::Table, deps: &mut Vec<DirectDep>) {
        for (key, item) in table {
            match item {
                toml::Value::String(spec) => deps.push(DirectDep {
                    key: key.clone(),
                    package: key.clone(),
                    spec: spec.clone(),
                }),
                toml::Value::Table(t) => {
                    if t.contains_key("path") || t.contains_key("git") {
                        continue;
                    }
                    let Some(spec) = t.get("version").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let package = t
                        .get("package")
                        .and_then(|p| p.as_str())
                        .unwrap_or(key)
                        .to_string();
                    deps.push(DirectDep {
                        key: key.clone(),
                        package,
                        spec: spec.to_string(),
                    });
                }
                _ => {}
            }
        }
    }

    let mut deps = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(toml::Value::Table(t)) = doc.get(section) {
            parse_table(t, &mut deps);
        }
    }
    if let Some(toml::Value::Table(ws)) = doc.get("workspace")
        && let Some(toml::Value::Table(t)) = ws.get("dependencies")
    {
        parse_table(t, &mut deps);
    }
    if let Some(toml::Value::Table(targets)) = doc.get("target") {
        for target_item in targets.values() {
            if let toml::Value::Table(tt) = target_item {
                for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    if let Some(toml::Value::Table(t)) = tt.get(section) {
                        parse_table(t, &mut deps);
                    }
                }
            }
        }
    }
    Ok(deps)
}

use super::LockedPackage;
use super::discover::{LockKind, ScannableLock};
use crate::align::PackageOccurrence;
use crate::normalize::pep503_normalize;
use crate::updater::{FileType, Lang};
use std::collections::HashMap;
use std::path::PathBuf;

/// A single manifest declaration behind a Manifest-classified pair.
#[derive(Debug, Clone)]
pub struct Owner {
    pub manifest: PathBuf,
    pub file_type: FileType,
    /// Key under which the manifest declares the dependency; differs from
    /// the registry name for Cargo renames and npm aliases.
    pub dependency_key: String,
    /// True when the declaration is an npm `npm:` alias spec; v1 cannot
    /// rewrite alias specs (routing reports unfixable-with-guidance).
    pub npm_alias: bool,
}

/// Where a locked (name, version) pair came from: a direct manifest
/// declaration (one or more owners) or the lockfile alone.
#[derive(Debug, Clone)]
pub enum Provenance {
    Manifest {
        owners: Vec<Owner>,
        lockfile: PathBuf,
    },
    LockOnly {
        lockfile: PathBuf,
        kind: LockKind,
    },
}

impl Provenance {
    /// The lockfile that produced this classification; entries for the same
    /// pair only merge when this matches (see `ProvenanceIndex::map`).
    fn lockfile(&self) -> &Path {
        match self {
            Provenance::Manifest { lockfile, .. } | Provenance::LockOnly { lockfile, .. } => {
                lockfile
            }
        }
    }
}

/// (normalized name, version, OSV ecosystem string) -> provenance.
pub type PairKey = (String, String, &'static str);

#[derive(Debug, Default)]
pub struct ProvenanceIndex {
    /// One entry PER LOCKFILE that resolves the pair: a monorepo with two
    /// independent same-ecosystem projects must not shadow one project's
    /// lock-only copy behind the other project's direct dependency. Merging
    /// (Manifest-wins, owner dedup) happens only WITHIN a lockfile.
    pub map: std::collections::HashMap<PairKey, Vec<Provenance>>,
    /// Direct deps per npm host package.json, for the EOVERRIDE guard.
    pub npm_direct: HashMap<PathBuf, Vec<DirectDep>>,
}

/// The package identity of a TOP-LEVEL `packages`-map entry: the path must
/// be exactly `node_modules/` + one identity (two path components when the
/// first begins with `@`). Deeper paths return None - such entries are
/// never direct, even when a direct range would admit their version.
pub(crate) fn top_level_identity(locator: &str) -> Option<&str> {
    let rest = locator.strip_prefix("node_modules/")?;
    let mut parts = rest.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(single), None, _) if !single.is_empty() && !single.starts_with('@') => Some(rest),
        (Some(scope), Some(name), None) if scope.starts_with('@') && !name.is_empty() => Some(rest),
        _ => None,
    }
}

/// Insert preferring Manifest, merging only WITHIN the same lockfile: a
/// monorepo with two independent same-ecosystem projects gets one
/// Provenance entry per lockfile that resolves the pair, so one project's
/// direct dependency never shadows another's lock-only copy. Within a
/// lockfile, a Manifest classification is never replaced by LockOnly; two
/// Manifest classifications merge owners (deduped by manifest + dependency
/// key).
fn insert_provenance(
    map: &mut std::collections::HashMap<PairKey, Vec<Provenance>>,
    key: PairKey,
    prov: Provenance,
) {
    let entries = map.entry(key).or_default();
    let existing = entries.iter_mut().find(|p| p.lockfile() == prov.lockfile());
    match (existing, prov) {
        (Some(Provenance::Manifest { owners, .. }), Provenance::Manifest { owners: new, .. }) => {
            for owner in new {
                if !owners.iter().any(|o| {
                    o.manifest == owner.manifest && o.dependency_key == owner.dependency_key
                }) {
                    owners.push(owner);
                }
            }
        }
        (Some(Provenance::Manifest { .. }), Provenance::LockOnly { .. }) => {}
        (Some(slot @ Provenance::LockOnly { .. }), prov @ Provenance::Manifest { .. }) => {
            *slot = prov;
        }
        (Some(Provenance::LockOnly { .. }), Provenance::LockOnly { .. }) => {}
        (None, prov) => {
            entries.push(prov);
        }
    }
}

/// Classify every locked (name, version) pair across `locks` as
/// manifest-owned or lock-only, per the positional rules: npm identity is
/// the top-level `packages`-map path matching a manifest dependency key,
/// Cargo direct requirements cover only the maximal admitted locked
/// version, and uv/poetry coverage follows PEP 503-normalized declared
/// names from the production occurrence map.
pub fn classify(
    locks: &[ScannableLock],
    lock_packages: &[LockedPackage],
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
) -> ProvenanceIndex {
    let mut index = ProvenanceIndex::default();
    for lock in locks {
        let entries: Vec<&LockedPackage> = lock_packages
            .iter()
            .filter(|lp| lp.lockfile_path == lock.path)
            .collect();
        match lock.kind {
            LockKind::Uv | LockKind::Poetry => {
                // Declared names across associated manifests, from the
                // production parse output (the occurrence map) so config,
                // extras, and spelling behavior cannot diverge from audit.
                let mut declared: HashMap<String, Vec<Owner>> = HashMap::new();
                for ((_, lang), occs) in packages {
                    if *lang != Lang::Python {
                        continue;
                    }
                    for occ in occs {
                        if !lock.associated_manifests.contains(&occ.file_path) {
                            continue;
                        }
                        declared
                            .entry(pep503_normalize(&occ.original_name))
                            .or_default()
                            .push(Owner {
                                manifest: occ.file_path.clone(),
                                file_type: occ.file_type,
                                dependency_key: occ.original_name.clone(),
                                npm_alias: false,
                            });
                    }
                }
                for lp in &entries {
                    let norm = pep503_normalize(&lp.name);
                    let prov = match declared.get(&norm) {
                        Some(owners) => Provenance::Manifest {
                            owners: owners.clone(),
                            lockfile: lock.path.clone(),
                        },
                        None => Provenance::LockOnly {
                            lockfile: lock.path.clone(),
                            kind: lock.kind,
                        },
                    };
                    insert_provenance(
                        &mut index.map,
                        (norm, lp.version.clone(), lp.ecosystem.as_str()),
                        prov,
                    );
                }
            }
            LockKind::Cargo => {
                for lp in &entries {
                    insert_provenance(
                        &mut index.map,
                        (
                            lp.name.to_lowercase(),
                            lp.version.clone(),
                            lp.ecosystem.as_str(),
                        ),
                        Provenance::LockOnly {
                            lockfile: lock.path.clone(),
                            kind: LockKind::Cargo,
                        },
                    );
                }
                for manifest in &lock.associated_manifests {
                    let Ok(deps) = cargo_direct_deps(manifest) else {
                        continue;
                    };
                    for d in deps {
                        let Ok(req) = semver::VersionReq::parse(&d.spec) else {
                            continue;
                        };
                        // The requirement's Manifest-covered version is the
                        // MAXIMAL locked version it admits; other locked
                        // versions of the package stay LockOnly.
                        let covered = entries
                            .iter()
                            .filter(|lp| lp.name.eq_ignore_ascii_case(&d.package))
                            .filter(|lp| {
                                semver::Version::parse(&lp.version).is_ok_and(|v| req.matches(&v))
                            })
                            .max_by(|a, b| {
                                crate::version::compare::compare_versions(&a.version, &b.version)
                            });
                        if let Some(lp) = covered {
                            insert_provenance(
                                &mut index.map,
                                (
                                    lp.name.to_lowercase(),
                                    lp.version.clone(),
                                    lp.ecosystem.as_str(),
                                ),
                                Provenance::Manifest {
                                    owners: vec![Owner {
                                        manifest: manifest.clone(),
                                        file_type: FileType::CargoToml,
                                        dependency_key: d.key.clone(),
                                        npm_alias: false,
                                    }],
                                    lockfile: lock.path.clone(),
                                },
                            );
                        }
                    }
                }
            }
            LockKind::Npm => {
                let Some(dir) = lock.path.parent() else {
                    continue;
                };
                let host = dir.join("package.json");
                let direct = npm_direct_deps(&host).unwrap_or_default();
                for lp in &entries {
                    let identity = lp.locator.as_deref().and_then(top_level_identity);
                    let owner = identity
                        .and_then(|id| direct.iter().find(|d| d.key == id))
                        .map(|d| Owner {
                            manifest: host.clone(),
                            file_type: FileType::PackageJson,
                            dependency_key: d.key.clone(),
                            npm_alias: d.spec.starts_with("npm:"),
                        });
                    let prov = match owner {
                        Some(o) => Provenance::Manifest {
                            owners: vec![o],
                            lockfile: lock.path.clone(),
                        },
                        None => Provenance::LockOnly {
                            lockfile: lock.path.clone(),
                            kind: LockKind::Npm,
                        },
                    };
                    insert_provenance(
                        &mut index.map,
                        (
                            lp.name.to_lowercase(),
                            lp.version.clone(),
                            lp.ecosystem.as_str(),
                        ),
                        prov,
                    );
                }
                index.npm_direct.insert(host, direct);
            }
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn split_npm_alias_handles_plain_scoped_and_rangeless() {
        assert_eq!(split_npm_alias("npm:react@^18"), Some(("react", "^18")));
        assert_eq!(
            split_npm_alias("npm:@scope/name@1.2.3"),
            Some(("@scope/name", "1.2.3")),
            "range separator is the LAST @ so scoped targets keep @scope/"
        );
        assert_eq!(split_npm_alias("npm:react"), Some(("react", "")));
        assert_eq!(split_npm_alias("^18.0.0"), None);
    }

    #[test]
    fn npm_direct_deps_records_keys_aliases_and_specs() {
        let dir = tempfile::tempdir().unwrap();
        let pj = write(
            &dir,
            "package.json",
            r#"{
  "name": "t",
  "dependencies": {
    "examplepkg": "^4.0.0",
    "my-react": "npm:realpkg@^18",
    "@scope/direct": "1.1.0"
  },
  "devDependencies": { "devtool": "~2.0.0" }
}"#,
        );
        let deps = npm_direct_deps(&pj).unwrap();
        let by_key = |k: &str| deps.iter().find(|d| d.key == k).unwrap();
        assert_eq!(by_key("examplepkg").package, "examplepkg");
        assert_eq!(
            by_key("my-react").package,
            "realpkg",
            "alias target is the registry name"
        );
        assert_eq!(by_key("my-react").spec, "npm:realpkg@^18");
        assert_eq!(by_key("@scope/direct").package, "@scope/direct");
        assert_eq!(
            by_key("devtool").package,
            "devtool",
            "devDependencies are read too"
        );
        assert_eq!(deps.len(), 4);
    }

    #[test]
    fn cargo_direct_deps_records_key_and_package_for_renames() {
        let dir = tempfile::tempdir().unwrap();
        let ct = write(
            &dir,
            "Cargo.toml",
            r#"[package]
name = "t"
version = "0.1.0"

[dependencies]
plain = "1.0"
old_serde = { package = "serde", version = "1.0" }
local = { path = "../local" }

[dev-dependencies]
devcrate = "0.5"

[workspace.dependencies]
shared = "2.0"

[target.'cfg(unix)'.dependencies]
unixdep = "0.3"
"#,
        );
        let deps = cargo_direct_deps(&ct).unwrap();
        let by_key = |k: &str| deps.iter().find(|d| d.key == k).unwrap();
        assert_eq!(by_key("plain").package, "plain");
        assert_eq!(by_key("plain").spec, "1.0");
        assert_eq!(
            by_key("old_serde").package,
            "serde",
            "rename resolves the real package name"
        );
        assert_eq!(by_key("old_serde").spec, "1.0");
        assert!(
            deps.iter().all(|d| d.key != "local"),
            "path deps have no registry identity"
        );
        assert_eq!(by_key("devcrate").package, "devcrate");
        assert_eq!(by_key("shared").package, "shared");
        assert_eq!(by_key("unixdep").package, "unixdep");
    }

    use crate::align::scan_packages;
    use crate::audit::Ecosystem;
    use crate::lockscan::LockedPackage;
    use crate::lockscan::discover::{LockKind, ScannableLock};
    use crate::updater::FileType;
    use crate::updater::ParseWarnings;
    use std::path::Path;

    fn lp(
        name: &str,
        version: &str,
        eco: Ecosystem,
        lock: &Path,
        locator: Option<&str>,
    ) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            ecosystem: eco,
            lockfile_path: lock.to_path_buf(),
            line_number: None,
            locator: locator.map(str::to_string),
        }
    }

    #[test]
    fn top_level_identity_rules() {
        assert_eq!(top_level_identity("node_modules/react"), Some("react"));
        assert_eq!(
            top_level_identity("node_modules/@scope/name"),
            Some("@scope/name")
        );
        assert_eq!(top_level_identity("node_modules/a/node_modules/b"), None);
        assert_eq!(top_level_identity("node_modules/@scope/name/extra"), None);
        assert_eq!(top_level_identity("../local-lib"), None);
    }

    #[test]
    fn npm_nested_duplicate_satisfying_direct_range_stays_lock_only() {
        let dir = tempfile::tempdir().unwrap();
        let pj = write(
            &dir,
            "package.json",
            r#"{ "name": "t", "dependencies": { "examplepkg": "^1.0.0" } }"#,
        );
        let lock = dir.path().join("package-lock.json");
        let locks = vec![ScannableLock {
            path: lock.clone(),
            kind: LockKind::Npm,
            associated_manifests: vec![pj.clone()],
        }];
        // Top-level direct copy AND a nested duplicate whose version a direct
        // range would admit. Positional rule: nested is LockOnly regardless.
        let lps = vec![
            lp(
                "examplepkg",
                "1.5.0",
                Ecosystem::Npm,
                &lock,
                Some("node_modules/examplepkg"),
            ),
            lp(
                "examplepkg",
                "1.2.0",
                Ecosystem::Npm,
                &lock,
                Some("node_modules/other/node_modules/examplepkg"),
            ),
        ];
        let idx = classify(&locks, &lps, &Default::default());
        let entries = idx
            .map
            .get(&("examplepkg".into(), "1.5.0".into(), "npm"))
            .unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            Provenance::Manifest { owners, .. } => {
                assert_eq!(owners[0].dependency_key, "examplepkg");
                assert!(!owners[0].npm_alias);
            }
            other => panic!("top-level direct must be Manifest, got {other:?}"),
        }
        let entries = idx
            .map
            .get(&("examplepkg".into(), "1.2.0".into(), "npm"))
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0], Provenance::LockOnly { .. }),
            "nested copy admitted by the direct range is still LockOnly"
        );
    }

    #[test]
    fn npm_hoisted_transitive_without_key_stays_lock_only_and_alias_is_covered() {
        let dir = tempfile::tempdir().unwrap();
        let pj = write(
            &dir,
            "package.json",
            r#"{ "name": "t", "dependencies": { "my-react": "npm:realpkg@^18" } }"#,
        );
        let lock = dir.path().join("package-lock.json");
        let locks = vec![ScannableLock {
            path: lock.clone(),
            kind: LockKind::Npm,
            associated_manifests: vec![pj.clone()],
        }];
        let lps = vec![
            // Aliased direct: reified under the alias folder, name field = registry name.
            lp(
                "realpkg",
                "18.0.0",
                Ecosystem::Npm,
                &lock,
                Some("node_modules/my-react"),
            ),
            // Hoisted transitive at top level under its own name: no key match.
            lp(
                "hoisted",
                "3.0.0",
                Ecosystem::Npm,
                &lock,
                Some("node_modules/hoisted"),
            ),
        ];
        let idx = classify(&locks, &lps, &Default::default());
        let entries = idx
            .map
            .get(&("realpkg".into(), "18.0.0".into(), "npm"))
            .unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            Provenance::Manifest { owners, .. } => {
                assert_eq!(owners[0].dependency_key, "my-react");
                assert!(owners[0].npm_alias, "alias declaration flagged");
            }
            other => panic!("aliased direct must be Manifest under registry name, got {other:?}"),
        }
        let entries = idx
            .map
            .get(&("hoisted".into(), "3.0.0".into(), "npm"))
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0], Provenance::LockOnly { .. }));
        assert!(
            idx.npm_direct.contains_key(&pj),
            "direct deps cached for the EOVERRIDE guard"
        );
    }

    #[test]
    fn cargo_rename_and_max_admitted_version_rules() {
        let dir = tempfile::tempdir().unwrap();
        let ct = write(
            &dir,
            "Cargo.toml",
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\n\n[dependencies]\nold_serde = { package = \"serde\", version = \"1.0\" }\n",
        );
        let lock = dir.path().join("Cargo.lock");
        let locks = vec![ScannableLock {
            path: lock.clone(),
            kind: LockKind::Cargo,
            associated_manifests: vec![ct.clone()],
        }];
        // Duplicate locked versions: the requirement ^1.0 admits both 1.0.5
        // and 1.2.0; only the MAXIMAL admitted version is Manifest-covered.
        let lps = vec![
            lp("serde", "1.0.5", Ecosystem::CratesIo, &lock, None),
            lp("serde", "1.2.0", Ecosystem::CratesIo, &lock, None),
        ];
        let idx = classify(&locks, &lps, &Default::default());
        let entries = idx
            .map
            .get(&("serde".into(), "1.2.0".into(), "crates.io"))
            .unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            Provenance::Manifest { owners, .. } => {
                assert_eq!(
                    owners[0].dependency_key, "old_serde",
                    "edits go through the TOML key"
                );
            }
            other => panic!("max admitted version must be Manifest, got {other:?}"),
        }
        let entries = idx
            .map
            .get(&("serde".into(), "1.0.5".into(), "crates.io"))
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0], Provenance::LockOnly { .. }));
    }

    #[test]
    fn uv_declared_name_is_covered_and_transitive_is_lock_only() {
        let dir = tempfile::tempdir().unwrap();
        let py = write(
            &dir,
            "pyproject.toml",
            "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = [\"Typing_Extensions>=4.0\"]\n",
        );
        let lock = dir.path().join("uv.lock");
        let locks = vec![ScannableLock {
            path: lock.clone(),
            kind: LockKind::Uv,
            associated_manifests: vec![py.clone()],
        }];
        let lps = vec![
            lp("typing-extensions", "4.9.0", Ecosystem::PyPI, &lock, None),
            lp("lockonly", "0.40.0", Ecosystem::PyPI, &lock, None),
        ];
        let files = vec![(py.clone(), FileType::PyProject)];
        let occurrences = scan_packages(&files, &[], ParseWarnings::Suppress).unwrap();
        let idx = classify(&locks, &lps, &occurrences);
        let entries = idx
            .map
            .get(&("typing-extensions".into(), "4.9.0".into(), "PyPI"))
            .unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            Provenance::Manifest { owners, .. } => {
                assert_eq!(
                    owners[0].dependency_key, "Typing_Extensions",
                    "original spelling retained for display/edit"
                );
            }
            other => panic!("declared name (PEP 503 variant) must be Manifest, got {other:?}"),
        }
        let entries = idx
            .map
            .get(&("lockonly".into(), "0.40.0".into(), "PyPI"))
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0], Provenance::LockOnly { .. }));
    }

    #[test]
    fn two_manifests_declaring_same_direct_dep_yield_two_owners() {
        let dir = tempfile::tempdir().unwrap();
        let root = write(
            &dir,
            "Cargo.toml",
            "[workspace]\nmembers=[\"m\"]\n\n[workspace.dependencies]\nshared = \"1.0\"\n",
        );
        std::fs::create_dir_all(dir.path().join("m")).unwrap();
        let member = dir.path().join("m/Cargo.toml");
        std::fs::write(
            &member,
            "[package]\nname = \"m\"\nversion = \"0.1.0\"\n\n[dependencies]\nshared = \"1.0\"\n",
        )
        .unwrap();
        let lock = dir.path().join("Cargo.lock");
        let locks = vec![ScannableLock {
            path: lock.clone(),
            kind: LockKind::Cargo,
            associated_manifests: vec![root.clone(), member.clone()],
        }];
        let lps = vec![lp("shared", "1.3.0", Ecosystem::CratesIo, &lock, None)];
        let idx = classify(&locks, &lps, &Default::default());
        let entries = idx
            .map
            .get(&("shared".into(), "1.3.0".into(), "crates.io"))
            .unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            Provenance::Manifest { owners, .. } => {
                let mut manifests: Vec<_> = owners.iter().map(|o| o.manifest.clone()).collect();
                manifests.sort();
                assert_eq!(
                    manifests,
                    vec![root, member],
                    "one owner per declaring manifest"
                );
            }
            other => panic!("expected Manifest with two owners, got {other:?}"),
        }
    }

    #[test]
    fn independent_locks_do_not_shadow_each_other() {
        // Two SEPARATE Cargo projects in one scan: foo is a direct dep in
        // project a (Manifest under a's lock) and a transitive-only copy in
        // project b (LockOnly under b's lock). Both classifications must
        // coexist; the original single-entry map let a's direct dep shadow
        // b's floor action.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::create_dir_all(dir.path().join("b")).unwrap();
        let a_manifest = dir.path().join("a/Cargo.toml");
        std::fs::write(
            &a_manifest,
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n[dependencies]\nfoo = \"1.0\"\n",
        )
        .unwrap();
        let b_manifest = dir.path().join("b/Cargo.toml");
        std::fs::write(
            &b_manifest,
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let a_lock = dir.path().join("a/Cargo.lock");
        let b_lock = dir.path().join("b/Cargo.lock");
        let locks = vec![
            ScannableLock {
                path: a_lock.clone(),
                kind: LockKind::Cargo,
                associated_manifests: vec![a_manifest.clone()],
            },
            ScannableLock {
                path: b_lock.clone(),
                kind: LockKind::Cargo,
                associated_manifests: vec![b_manifest.clone()],
            },
        ];
        let lps = vec![
            lp("foo", "1.2.3", Ecosystem::CratesIo, &a_lock, None),
            lp("foo", "1.2.3", Ecosystem::CratesIo, &b_lock, None),
        ];
        let idx = classify(&locks, &lps, &Default::default());
        let entries = idx
            .map
            .get(&("foo".into(), "1.2.3".into(), "crates.io"))
            .expect("pair classified");
        assert_eq!(
            entries.len(),
            2,
            "one provenance entry per lockfile: {entries:?}"
        );
        let a_entry = entries
            .iter()
            .find(|p| matches!(p, Provenance::Manifest { lockfile, .. } if *lockfile == a_lock))
            .expect("project a's direct dep is Manifest under a's lock");
        if let Provenance::Manifest { owners, .. } = a_entry {
            assert_eq!(owners[0].dependency_key, "foo");
        }
        assert!(
            entries
                .iter()
                .any(|p| matches!(p, Provenance::LockOnly { lockfile, .. } if *lockfile == b_lock)),
            "project b's transitive copy stays LockOnly under b's lock: {entries:?}"
        );
    }
}
