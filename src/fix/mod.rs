//! Explicit per-target fix actions: routing vulnerable (name, version) pairs
//! into manifest edits and version floors. Writers live in uv/npm;
//! transactional application in apply.

pub mod apply;
pub mod npm;
pub mod uv;

use crate::align::PackageOccurrence;
use crate::audit::{AuditResult, Ecosystem, Package, manifest_fix_version};
use crate::lockscan::LockedPackage;
use crate::lockscan::discover::LockKind;
use crate::lockscan::provenance::{Owner, Provenance, ProvenanceIndex};
use crate::normalize::pep503_normalize;
use crate::updater::{FileType, Lang};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Outcome of writing (or checking) a version floor through a
/// package-manager-specific mechanism (uv constraint-dependencies, npm
/// overrides, `cargo update --precise`). Shared across the floor writers so
/// dispatch and transactional application handle all of them uniformly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloorWriteOutcome {
    /// The floor entry was written (or would be written, in dry-run).
    Written,
    /// An existing entry already floors at or above the target; no write.
    AlreadySatisfied,
    /// Refused; guidance for the user in the payload.
    Unfixable(String),
}

/// How a fix is applied: an in-place manifest edit, or a version floor
/// written through a package-manager-specific mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixKind {
    ManifestEdit,
    UvConstraint,
    NpmOverride,
    CargoPrecise,
}

impl FixKind {
    pub fn method(&self) -> &'static str {
        match self {
            FixKind::ManifestEdit => "manifest",
            FixKind::UvConstraint => "uv-constraint",
            FixKind::NpmOverride => "npm-override",
            FixKind::CargoPrecise => "cargo-precise",
        }
    }
}

/// Which form an npm `overrides` entry takes, per the EOVERRIDE guard: a
/// plain semver range when the package is not also a direct dependency, or
/// a `$name` reference (plus a companion manifest edit bumping the direct
/// spec) when it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpmOverrideForm {
    /// An explicit update may cross compatibility branches.
    Range,
    /// An audit repair stays on the installed compatibility branch.
    CompatibleRange,
    DollarName,
}

/// A single concrete fix action: bump one manifest entry, or write one
/// version floor.
#[derive(Debug, Clone)]
pub struct FixTarget {
    pub package: String,
    /// The ecosystem of the audited package this target fixes.
    pub ecosystem: Ecosystem,
    pub dependency_key: Option<String>,
    pub from_version: String,
    pub to_version: String,
    pub vulnerable_version: String,
    pub kind: FixKind,
    pub path: PathBuf,
    /// File type of `path` when known (drives the manifest-edit dispatcher;
    /// informational for floors).
    pub file_type: Option<FileType>,
    pub lockfile: Option<PathBuf>,
    pub line_number: Option<usize>,
    pub npm_form: Option<NpmOverrideForm>,
}

/// A vulnerable pair (or manifest occurrence) that routing could not turn
/// into a fix target, with a human-readable reason.
#[derive(Debug, Clone)]
pub struct UnfixableTarget {
    pub package: String,
    /// The ecosystem of the audited package left unfixed.
    pub ecosystem: Ecosystem,
    pub dependency_key: Option<String>,
    pub from_version: String,
    pub to_version: Option<String>,
    pub method: Option<&'static str>,
    pub path: Option<PathBuf>,
    pub reason: String,
    pub no_fixed_version: bool,
}

/// The full routing outcome across every vulnerable pair.
#[derive(Debug, Default)]
pub struct FixRouting {
    pub targets: Vec<FixTarget>,
    pub unfixable: Vec<UnfixableTarget>,
}

/// What a registry says about the release a vulnerable pair's advisories
/// name as fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixRelease {
    /// The release a fix moves to: the lowest installable release at or
    /// above the advisories' bound (stable, unless it is the bound itself)
    /// that no window of the pair's advisories covers.
    Release(String),
    /// No installable release is at or above the bound, so there is nothing
    /// to move to. RustSec's unmaintained notices carry such a bound: one
    /// past the last release (`0.4.21-0` for a crate whose last is `0.4.20`).
    Unpublished,
    /// Releases exist at or above the bound, but an advisory still covers
    /// every one of them: a bound on one branch whose release never shipped,
    /// followed by a branch that is itself affected. `release` is the lowest
    /// such release and `advisory` one that covers it.
    StillAffected { release: String, advisory: String },
}

/// Registry answers keyed like [`ProvenanceIndex`]: (normalized name,
/// vulnerable version, OSV ecosystem). A pair without an entry is routed to
/// the bound its advisories name.
pub type FixReleases = HashMap<(String, String, &'static str), FixRelease>;

/// Accumulator for the per-pair walk. `manifest_edits` and `npm_companions`
/// are tracked in separate `Vec`s only to keep their producers readable;
/// [`route_fix_targets`] concatenates them into ONE pool before merging
/// (see [`merge_manifest_edits`]), since a direct-vulnerable pair's own
/// edit and its DollarName companion edit can target the very same
/// manifest line. `floor_targets` and `cargo_targets` merge under their own
/// separate policies (see the merge functions below) before all four
/// groups combine into the final [`FixRouting::targets`].
#[derive(Default)]
struct Sink {
    preserve_npm_compatibility: bool,
    manifest_edits: Vec<FixTarget>,
    npm_companions: Vec<FixTarget>,
    floor_targets: Vec<FixTarget>,
    cargo_targets: Vec<FixTarget>,
    unfixable: Vec<UnfixableTarget>,
}

/// PyPI names are matched PEP 503-normalized; every other ecosystem matches
/// on a plain lowercase (npm and crates.io names are already
/// lowercase-only by registry convention, and Go/RubyGems/NuGet have no
/// equivalent canonicalization in this codebase).
fn normalized_name(name: &str, ecosystem: Ecosystem) -> String {
    if ecosystem == Ecosystem::PyPI {
        pep503_normalize(name)
    } else {
        name.to_lowercase()
    }
}

/// The `Lang` an OSV `Ecosystem` corresponds to, mirroring the reverse
/// mapping used when packages are queued for audit (main.rs).
fn ecosystem_lang(ecosystem: Ecosystem) -> Lang {
    match ecosystem {
        Ecosystem::PyPI => Lang::Python,
        Ecosystem::Npm => Lang::Node,
        Ecosystem::CratesIo => Lang::Rust,
        Ecosystem::Go => Lang::Go,
        Ecosystem::RubyGems => Lang::Ruby,
        Ecosystem::NuGet => Lang::DotNet,
        Ecosystem::Maven => Lang::Gradle,
    }
}

/// `Some(key)` when a manifest-declared key differs from the registry name
/// (Cargo renames, npm aliases); `None` otherwise, so ordinary occurrences
/// don't carry a redundant `dependency_key`.
fn dependency_key_if_different(key: &str, package: &str) -> Option<String> {
    if key == package {
        None
    } else {
        Some(key.to_string())
    }
}

/// Every occurrence of `norm` (already normalized per `ecosystem`) across
/// the languages files scanned for `ecosystem`. The occurrence map key is
/// `(name.to_lowercase(), Lang)` (align.rs), never PEP 503-normalized, so
/// PyPI candidates are normalized again here before comparing.
fn matching_occurrences<'a>(
    packages: &'a HashMap<(String, Lang), Vec<PackageOccurrence>>,
    norm: &str,
    ecosystem: Ecosystem,
) -> Vec<&'a PackageOccurrence> {
    let lang = ecosystem_lang(ecosystem);
    let mut result = Vec::new();
    for ((name, l), occs) in packages {
        if *l != lang {
            continue;
        }
        let candidate = if ecosystem == Ecosystem::PyPI {
            pep503_normalize(name)
        } else {
            name.clone()
        };
        if candidate == norm {
            result.extend(occs.iter());
        }
    }
    result
}

/// Route a Manifest-covered pair (rule 2). Attribution depends on how many
/// DISTINCT dependency keys the pair's owners declare in a given manifest:
/// with exactly one key, one `ManifestEdit` per occurrence of the pair's
/// name in that manifest (unchanged from pre-lockscan behavior); with more
/// than one key (e.g. a Cargo rename `old_serde = { package = "serde", ... }`
/// coexisting with a plain `serde = "..."`, both admitting the same locked
/// version), occurrences can no longer be attributed by manifest path alone,
/// because matching every occurrence in the manifest against every owner
/// would emit an owners-by-occurrences cross product. Each owner instead
/// gets exactly one edit, sourced from its own requirement fragment (see
/// [`route_manifest_covered_owner`]). An owner declared as an npm alias is
/// always unfixable, regardless of how many keys share the manifest.
fn route_manifest_covered(
    pkg: &Package,
    owners: &[Owner],
    lockfile: &Path,
    to_version: &str,
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
    sink: &mut Sink,
) {
    let norm = normalized_name(&pkg.name, pkg.ecosystem);

    let mut manifest_keys: HashMap<&Path, HashSet<&str>> = HashMap::new();
    for owner in owners {
        if owner.npm_alias {
            continue;
        }
        manifest_keys
            .entry(owner.manifest.as_path())
            .or_default()
            .insert(owner.dependency_key.as_str());
    }

    for owner in owners {
        if owner.npm_alias {
            sink.unfixable.push(UnfixableTarget {
                package: pkg.name.clone(), ecosystem: pkg.ecosystem,
                dependency_key: Some(owner.dependency_key.clone()),
                from_version: pkg.version.clone(),
                to_version: Some(to_version.to_string()),
                method: Some(FixKind::ManifestEdit.method()),
                path: Some(owner.manifest.clone()),
                reason: format!(
                    "declared as npm alias \"{}\" in {}; upd cannot rewrite alias specs - update it manually to \"{}\": \"npm:{}@>={}\"",
                    owner.dependency_key,
                    crate::path_display::display_path(&owner.manifest),
                    owner.dependency_key,
                    pkg.name,
                    to_version
                ),
                no_fixed_version: false,
            });
            continue;
        }

        let dep_key = dependency_key_if_different(&owner.dependency_key, &pkg.name);
        let multi_owner = manifest_keys
            .get(owner.manifest.as_path())
            .is_some_and(|keys| keys.len() > 1);

        if multi_owner {
            route_manifest_covered_owner(pkg, owner, dep_key, lockfile, to_version, packages, sink);
            continue;
        }

        let occurrences: Vec<&PackageOccurrence> =
            matching_occurrences(packages, &norm, pkg.ecosystem)
                .into_iter()
                .filter(|o| o.file_path == owner.manifest)
                .collect();

        for occ in occurrences {
            if !occ.is_bumpable {
                sink.unfixable.push(UnfixableTarget {
                    package: pkg.name.clone(),
                    ecosystem: pkg.ecosystem,
                    dependency_key: dep_key.clone(),
                    from_version: occ.version.clone(),
                    to_version: Some(to_version.to_string()),
                    method: Some(FixKind::ManifestEdit.method()),
                    path: Some(owner.manifest.clone()),
                    reason: "no bumpable manifest entry (e.g. a commit-pinned version)".to_string(),
                    no_fixed_version: false,
                });
                continue;
            }
            sink.manifest_edits.push(FixTarget {
                package: pkg.name.clone(),
                ecosystem: pkg.ecosystem,
                dependency_key: dep_key.clone(),
                from_version: occ.version.clone(),
                to_version: to_version.to_string(),
                vulnerable_version: pkg.version.clone(),
                kind: FixKind::ManifestEdit,
                path: owner.manifest.clone(),
                file_type: Some(occ.file_type),
                lockfile: Some(lockfile.to_path_buf()),
                line_number: occ.line_number,
                npm_form: None,
            });
        }
    }
}

/// One `ManifestEdit` for a single owner sharing its manifest with at least
/// one other distinct dependency key for the same pair (the multi-owner
/// branch of rule 2). The edit cannot be attributed to a specific occurrence,
/// because every occurrence of the pair's name in the manifest would
/// otherwise be attributed to every owner, so `from_version` is re-derived
/// directly from the owner's own requirement fragment in the manifest, and
/// no specific line is claimed. `cargo_direct_deps` is the only
/// re-derivation source today because Cargo renames are the only manifest
/// shape that reaches this branch in practice; a manifest of another kind
/// whose direct deps can't be re-derived this way falls back to unfixable
/// rather than guessing.
fn route_manifest_covered_owner(
    pkg: &Package,
    owner: &Owner,
    dep_key: Option<String>,
    lockfile: &Path,
    to_version: &str,
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
    sink: &mut Sink,
) {
    let norm = normalized_name(&pkg.name, pkg.ecosystem);
    let spec = crate::lockscan::provenance::cargo_direct_deps(&owner.manifest)
        .ok()
        .and_then(|deps| {
            deps.into_iter()
                .find(|d| d.key == owner.dependency_key)
                .map(|d| d.spec)
        });

    let Some(from_version) = spec else {
        sink.unfixable.push(UnfixableTarget {
            package: pkg.name.clone(),
            ecosystem: pkg.ecosystem,
            dependency_key: dep_key,
            from_version: pkg.version.clone(),
            to_version: Some(to_version.to_string()),
            method: Some(FixKind::ManifestEdit.method()),
            path: Some(owner.manifest.clone()),
            reason: format!(
                "could not re-derive the manifest requirement for \"{}\" in {}",
                owner.dependency_key,
                crate::path_display::display_path(&owner.manifest)
            ),
            no_fixed_version: false,
        });
        return;
    };

    let file_type = matching_occurrences(packages, &norm, pkg.ecosystem)
        .into_iter()
        .find(|o| o.file_path == owner.manifest)
        .map(|o| o.file_type)
        .unwrap_or(owner.file_type);

    sink.manifest_edits.push(FixTarget {
        package: pkg.name.clone(),
        ecosystem: pkg.ecosystem,
        dependency_key: dep_key,
        from_version,
        to_version: to_version.to_string(),
        vulnerable_version: pkg.version.clone(),
        kind: FixKind::ManifestEdit,
        path: owner.manifest.clone(),
        file_type: Some(file_type),
        lockfile: Some(lockfile.to_path_buf()),
        line_number: None,
        npm_form: None,
    });
}

/// Route a pair with no provenance entry at all (rule 4: Go, RubyGems,
/// NuGet, or any manifest whose lock wasn't scanned): one `ManifestEdit` per
/// occurrence of the name across the pair's ecosystem, unchanged from
/// pre-lockscan behavior.
fn route_no_provenance(
    pkg: &Package,
    to_version: &str,
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
    sink: &mut Sink,
) {
    let norm = normalized_name(&pkg.name, pkg.ecosystem);
    for occ in matching_occurrences(packages, &norm, pkg.ecosystem) {
        let dep_key = dependency_key_if_different(&occ.original_name, &pkg.name);
        if !occ.is_bumpable {
            sink.unfixable.push(UnfixableTarget {
                package: pkg.name.clone(),
                ecosystem: pkg.ecosystem,
                dependency_key: dep_key,
                from_version: occ.version.clone(),
                to_version: Some(to_version.to_string()),
                method: Some(FixKind::ManifestEdit.method()),
                path: Some(occ.file_path.clone()),
                reason: "no bumpable manifest entry (e.g. a commit-pinned version)".to_string(),
                no_fixed_version: false,
            });
            continue;
        }
        sink.manifest_edits.push(FixTarget {
            package: pkg.name.clone(),
            ecosystem: pkg.ecosystem,
            dependency_key: dep_key,
            from_version: occ.version.clone(),
            to_version: to_version.to_string(),
            vulnerable_version: pkg.version.clone(),
            kind: FixKind::ManifestEdit,
            path: occ.file_path.clone(),
            file_type: Some(occ.file_type),
            lockfile: None,
            line_number: occ.line_number,
            npm_form: None,
        });
    }
}

/// Route a LockOnly pair (rule 3) by lock kind: uv floors via
/// `constraint-dependencies`, poetry has no floor mechanism, Cargo floors
/// via `cargo update --precise`, npm goes through the EOVERRIDE guard.
fn route_lock_only(
    pkg: &Package,
    lockfile: &Path,
    kind: LockKind,
    to_version: &str,
    prov: &ProvenanceIndex,
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
    sink: &mut Sink,
) {
    match kind {
        LockKind::Uv => {
            let Some(dir) = lockfile.parent() else {
                return;
            };
            let host = dir.join("pyproject.toml");
            sink.floor_targets.push(FixTarget {
                package: pkg.name.clone(),
                ecosystem: pkg.ecosystem,
                dependency_key: None,
                from_version: pkg.version.clone(),
                to_version: to_version.to_string(),
                vulnerable_version: pkg.version.clone(),
                kind: FixKind::UvConstraint,
                path: host,
                file_type: Some(FileType::PyProject),
                lockfile: Some(lockfile.to_path_buf()),
                line_number: None,
                npm_form: None,
            });
        }
        LockKind::Poetry | LockKind::Gradle => {
            sink.unfixable.push(UnfixableTarget {
                package: pkg.name.clone(), ecosystem: pkg.ecosystem,
                dependency_key: None,
                from_version: pkg.version.clone(),
                to_version: Some(to_version.to_string()),
                method: None,
                path: Some(lockfile.to_path_buf()),
                reason: if kind == LockKind::Poetry {
                    format!("no floor mechanism exists for poetry.lock; add {}>={} as a direct dependency", pkg.name, to_version)
                } else {
                    "Gradle audit fixes require updating the owning build configuration and regenerating its lockfile".into()
                },
                no_fixed_version: false,
            });
        }
        LockKind::Cargo => {
            sink.cargo_targets.push(FixTarget {
                package: pkg.name.clone(),
                ecosystem: pkg.ecosystem,
                dependency_key: None,
                from_version: pkg.version.clone(),
                to_version: to_version.to_string(),
                vulnerable_version: pkg.version.clone(),
                kind: FixKind::CargoPrecise,
                path: lockfile.to_path_buf(),
                file_type: None,
                lockfile: Some(lockfile.to_path_buf()),
                line_number: None,
                npm_form: None,
            });
        }
        LockKind::Npm => {
            route_npm_lock_only(pkg, lockfile, to_version, prov, packages, sink);
        }
    }
}

/// The npm EOVERRIDE guard (rule 3, npm branch): npm refuses to override a
/// package that is also a direct dependency unless the override uses a
/// `$name` reference, which in turn requires the direct spec itself to be
/// bumped to at least the floor (the companion `ManifestEdit`).
fn route_npm_lock_only(
    pkg: &Package,
    lockfile: &Path,
    to_version: &str,
    prov: &ProvenanceIndex,
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
    sink: &mut Sink,
) {
    let Some(dir) = lockfile.parent() else {
        return;
    };
    let host = dir.join("package.json");
    let norm = normalized_name(&pkg.name, pkg.ecosystem);

    let range = npm::compatibility_range(&pkg.version);
    if sink.preserve_npm_compatibility
        && range
            .as_ref()
            .is_none_or(|range| crate::npm_range::admits(range, to_version) != Some(true))
    {
        sink.unfixable.push(UnfixableTarget {
            package: pkg.name.clone(), ecosystem: pkg.ecosystem, dependency_key: None,
            from_version: pkg.version.clone(), to_version: Some(to_version.to_string()),
            method: Some("npm-override"), path: Some(host),
            reason: format!("fixing {}@{} requires {to_version}, outside its compatibility range; update its parent dependency instead of forcing an incompatible override", pkg.name, pkg.version),
            no_fixed_version: false,
        });
        return;
    }

    let matching_direct = prov.npm_direct.get(&host).and_then(|deps| {
        deps.iter()
            .find(|d| normalized_name(&d.package, pkg.ecosystem) == norm)
    });

    // A $name override applies to every copy. It cannot safely unify
    // different compatibility branches just because one is also direct.
    if sink.preserve_npm_compatibility
        && matching_direct.is_some()
        && prov
            .map
            .iter()
            .any(|((name, version, ecosystem), entries)| {
                name == &norm
                    && *ecosystem == "npm"
                    && npm::compatibility_range(version) != range
                    && entries.iter().any(|entry| match entry {
                        Provenance::Manifest { lockfile: path, .. }
                        | Provenance::LockOnly { lockfile: path, .. } => path == lockfile,
                    })
            })
    {
        sink.unfixable.push(UnfixableTarget {
            package: pkg.name.clone(), ecosystem: pkg.ecosystem, dependency_key: None,
            from_version: pkg.version.clone(), to_version: Some(to_version.to_string()),
            method: Some("npm-override"), path: Some(host),
            reason: format!("{} has direct and transitive copies on incompatible branches; update its parent dependencies instead of applying a global $-reference override", pkg.name),
            no_fixed_version: false,
        });
        return;
    }

    match matching_direct {
        None => {
            sink.floor_targets.push(FixTarget {
                package: pkg.name.clone(),
                ecosystem: pkg.ecosystem,
                dependency_key: None,
                from_version: pkg.version.clone(),
                to_version: to_version.to_string(),
                vulnerable_version: pkg.version.clone(),
                kind: FixKind::NpmOverride,
                path: host,
                file_type: Some(FileType::PackageJson),
                lockfile: Some(lockfile.to_path_buf()),
                line_number: None,
                npm_form: Some(if sink.preserve_npm_compatibility {
                    NpmOverrideForm::CompatibleRange
                } else {
                    NpmOverrideForm::Range
                }),
            });
        }
        Some(d) if d.spec.starts_with("npm:") => {
            sink.unfixable.push(UnfixableTarget {
                package: pkg.name.clone(), ecosystem: pkg.ecosystem,
                dependency_key: Some(d.key.clone()),
                from_version: pkg.version.clone(),
                to_version: Some(to_version.to_string()),
                method: None,
                path: Some(host),
                reason: format!(
                    "only reachable through npm alias \"{}\"; an npm override cannot be expressed with a $-reference here - add \"{}\": \">={}\" to overrides manually if desired",
                    d.key, pkg.name, to_version
                ),
                no_fixed_version: false,
            });
        }
        Some(d) => {
            let occ = matching_occurrences(packages, &norm, pkg.ecosystem)
                .into_iter()
                .find(|o| o.file_path == host);
            match occ {
                Some(o) => {
                    sink.floor_targets.push(FixTarget {
                        package: pkg.name.clone(),
                        ecosystem: pkg.ecosystem,
                        dependency_key: None,
                        from_version: pkg.version.clone(),
                        to_version: to_version.to_string(),
                        vulnerable_version: pkg.version.clone(),
                        kind: FixKind::NpmOverride,
                        path: host.clone(),
                        file_type: Some(FileType::PackageJson),
                        lockfile: Some(lockfile.to_path_buf()),
                        line_number: None,
                        npm_form: Some(NpmOverrideForm::DollarName),
                    });
                    let dep_key = dependency_key_if_different(&d.key, &pkg.name);
                    sink.npm_companions.push(FixTarget {
                        package: pkg.name.clone(),
                        ecosystem: pkg.ecosystem,
                        dependency_key: dep_key,
                        from_version: o.version.clone(),
                        to_version: to_version.to_string(),
                        vulnerable_version: pkg.version.clone(),
                        kind: FixKind::ManifestEdit,
                        path: host,
                        file_type: Some(o.file_type),
                        lockfile: Some(lockfile.to_path_buf()),
                        line_number: o.line_number,
                        npm_form: None,
                    });
                }
                None => {
                    sink.unfixable.push(UnfixableTarget {
                        package: pkg.name.clone(), ecosystem: pkg.ecosystem,
                        dependency_key: Some(d.key.clone()),
                        from_version: pkg.version.clone(),
                        to_version: Some(to_version.to_string()),
                        method: None,
                        path: Some(host),
                        reason: format!(
                            "direct dependency \"{}\" has a spec upd cannot bump; floor it manually",
                            d.key
                        ),
                        no_fixed_version: false,
                    });
                }
            }
        }
    }
}

/// Grouping key for the unified `ManifestEdit` merge pool: `dependency_key`
/// (falling back to the registry `package` name when the manifest declares
/// no distinct key), lowercased for case-insensitive matching, plus `path`
/// and `line_number` so two declarations of the same package on different
/// lines of the same manifest (multi-section declarations, e.g.
/// `dependencies` and `dev-dependencies`) are never collapsed into one
/// edit. `from_version` is included too: two edits at the same location but
/// starting from different declared versions describe different states and
/// must not merge.
fn manifest_edit_key(target: &FixTarget) -> (PathBuf, String, Option<usize>, String) {
    let effective_key = target
        .dependency_key
        .clone()
        .unwrap_or_else(|| target.package.clone())
        .to_lowercase();
    (
        target.path.clone(),
        effective_key,
        target.line_number,
        target.from_version.clone(),
    )
}

/// All `ManifestEdit` targets - rule-2 owner edits AND npm `$name` companion
/// edits - merge in ONE pool keyed by [`manifest_edit_key`], keeping the max
/// `to_version` and `vulnerable_version`. A single pool (rather than two
/// disjoint ones keyed differently) is required because a direct-vulnerable
/// pair's own edit and its DollarName companion edit for a nested copy of
/// the same package can target the exact same manifest line: merging them
/// here is what keeps that line to one edit at the highest required
/// version instead of two conflicting edits.
fn merge_manifest_edits(edits: Vec<FixTarget>) -> Vec<FixTarget> {
    let mut map: HashMap<(PathBuf, String, Option<usize>, String), FixTarget> = HashMap::new();
    for edit in edits {
        let key = manifest_edit_key(&edit);
        map.entry(key)
            .and_modify(|existing| {
                if compare_versions(&edit.to_version, &existing.to_version) == Ordering::Greater {
                    existing.to_version = edit.to_version.clone();
                }
                if compare_versions(&edit.vulnerable_version, &existing.vulnerable_version)
                    == Ordering::Greater
                {
                    existing.vulnerable_version = edit.vulnerable_version.clone();
                }
            })
            .or_insert(edit);
    }
    map.into_values().collect()
}

/// Prefer the `DollarName` form once any merged pair required it: the
/// direct-dependency relationship that triggers `DollarName` is a static
/// property of the package/host pair, not of which vulnerable version
/// triggered routing, so a mix only arises from routing order, never from
/// conflicting facts.
fn merge_npm_form(
    a: Option<NpmOverrideForm>,
    b: Option<NpmOverrideForm>,
) -> Option<NpmOverrideForm> {
    match (a, b) {
        (Some(NpmOverrideForm::DollarName), _) | (_, Some(NpmOverrideForm::DollarName)) => {
            Some(NpmOverrideForm::DollarName)
        }
        (Some(f), _) | (_, Some(f)) => Some(f),
        (None, None) => None,
    }
}

/// `(kind, path, normalized package)` for grouping uv/npm floor targets;
/// `kind` alone determines which normalization applies since `UvConstraint`
/// targets are always PyPI and `NpmOverride` targets are always npm.
fn floor_group_key(target: &FixTarget) -> (&'static str, PathBuf, String) {
    let norm = if target.kind == FixKind::UvConstraint {
        pep503_normalize(&target.package)
    } else {
        target.package.to_lowercase()
    };
    let norm = if target.kind == FixKind::NpmOverride
        && target.npm_form == Some(NpmOverrideForm::CompatibleRange)
    {
        format!(
            "{}@{}",
            norm,
            npm::compatibility_range(&target.vulnerable_version)
                .unwrap_or_else(|| target.vulnerable_version.clone())
        )
    } else {
        norm
    };
    (target.kind.method(), target.path.clone(), norm)
}

/// Floor targets merge by kind, path and normalized package; npm range
/// overrides also retain the installed compatibility branch. Keep
/// the max `to_version` (the floor must clear every vulnerable version) and
/// the max `from_version`/`vulnerable_version` (the highest vulnerable
/// locked version among the merged group).
fn merge_floor_group(targets: Vec<FixTarget>) -> Vec<FixTarget> {
    let mut map: HashMap<(&'static str, PathBuf, String), FixTarget> = HashMap::new();
    for target in targets {
        let key = floor_group_key(&target);
        map.entry(key)
            .and_modify(|existing| {
                if compare_versions(&target.to_version, &existing.to_version) == Ordering::Greater {
                    existing.to_version = target.to_version.clone();
                }
                if compare_versions(&target.from_version, &existing.from_version)
                    == Ordering::Greater
                {
                    existing.from_version = target.from_version.clone();
                }
                if compare_versions(&target.vulnerable_version, &existing.vulnerable_version)
                    == Ordering::Greater
                {
                    existing.vulnerable_version = target.vulnerable_version.clone();
                }
                existing.npm_form = merge_npm_form(existing.npm_form, target.npm_form);
            })
            .or_insert(target);
    }
    map.into_values().collect()
}

fn compare_versions(a: &str, b: &str) -> Ordering {
    crate::version::compare::compare_versions(a, b)
}

/// The fix a vulnerable pair's advisories name together.
enum FixBound<'a> {
    /// The pair carries no advisory.
    None,
    /// This advisory names no fixed version, so no release clears the pair.
    Missing(&'a str),
    /// The highest fixed version any advisory names, which clears them all,
    /// and the advisory naming it.
    Named { bound: &'a str, advisory: &'a str },
}

fn fix_bound(vulnerabilities: &[crate::audit::Vulnerability]) -> FixBound<'_> {
    if let Some(v) = vulnerabilities.iter().find(|v| v.fixed_version.is_none()) {
        return FixBound::Missing(&v.id);
    }
    vulnerabilities
        .iter()
        .filter_map(|v| Some((v.fixed_version.as_deref()?, v.id.as_str())))
        .max_by(|a, b| compare_versions(a.0, b.0))
        .map_or(FixBound::None, |(bound, advisory)| FixBound::Named {
            bound,
            advisory,
        })
}

fn no_fix(pkg: &Package, reason: String) -> UnfixableTarget {
    UnfixableTarget {
        package: pkg.name.clone(),
        ecosystem: pkg.ecosystem,
        dependency_key: None,
        from_version: pkg.version.clone(),
        to_version: None,
        method: None,
        path: None,
        reason,
        no_fixed_version: true,
    }
}

/// How releases of an ecosystem's packages are compared, for an ecosystem
/// whose registry lists every release it has published, and whether that
/// listing's `yanked` flag means a release cannot be installed. npm reports
/// deprecation there, which leaves a release installable. The Go proxy
/// listing is capped, so a release missing from it proves nothing.
fn listed_ecosystem(ecosystem: Ecosystem) -> Option<(Lang, bool)> {
    match ecosystem {
        Ecosystem::CratesIo => Some((Lang::Rust, true)),
        Ecosystem::PyPI => Some((Lang::Python, true)),
        Ecosystem::Npm => Some((Lang::Node, false)),
        Ecosystem::Go | Ecosystem::RubyGems | Ecosystem::NuGet | Ecosystem::Maven => None,
    }
}

/// The release to move to for `bound`: the lowest installable release at
/// or above it that none of `vulnerabilities`' windows covers, spelled as
/// the advisory spells the bound when it is the bound. A release above the
/// bound must be stable; the bound itself is taken as named.
fn select_fix_release(
    versions: &[crate::registry::VersionMeta],
    bound: &str,
    vulnerabilities: &[crate::audit::Vulnerability],
    lang: Lang,
    honors_yanked: bool,
) -> FixRelease {
    let compare = |a: &str, b: &str| crate::align::compare_versions(a, b, lang);
    let mut candidates: Vec<&str> = versions
        .iter()
        .filter(|v| !(honors_yanked && v.yanked))
        .filter(|v| match compare(&v.version, bound) {
            Ordering::Equal => true,
            Ordering::Greater => !v.prerelease,
            Ordering::Less => false,
        })
        .map(|v| v.version.as_str())
        .collect();
    candidates.sort_by(|a, b| compare(a, b));
    let covering = |release: &str| {
        vulnerabilities
            .iter()
            .find(|vuln| vuln.affected.iter().any(|w| w.contains(release, compare)))
    };
    let mut lowest_affected = None;
    for release in candidates {
        match covering(release) {
            None if compare(release, bound).is_eq() => {
                return FixRelease::Release(bound.to_string());
            }
            None => return FixRelease::Release(release.to_string()),
            Some(vuln) => {
                lowest_affected.get_or_insert_with(|| (release.to_string(), vuln.id.clone()));
            }
        }
    }
    match lowest_affected {
        Some((release, advisory)) => FixRelease::StillAffected { release, advisory },
        None => FixRelease::Unpublished,
    }
}

/// A registry upd can list a package's releases from, with the source URL
/// it lists: an index URL for PyPI and crates.io, the registry URL for npm.
pub struct ReleaseSource<'r> {
    pub url: String,
    pub registry: &'r dyn crate::registry::Registry,
}

/// Where the ecosystem's tool resolves a package from when nothing
/// configures otherwise, as a lockfile records it.
fn public_source(ecosystem: Ecosystem) -> &'static str {
    match ecosystem {
        Ecosystem::CratesIo => "registry+https://github.com/rust-lang/crates.io-index",
        Ecosystem::PyPI => "https://pypi.org/simple",
        Ecosystem::Npm => "https://registry.npmjs.org",
        Ecosystem::Go | Ecosystem::RubyGems | Ecosystem::NuGet | Ecosystem::Maven => "",
    }
}

/// `url` in one spelling per source, so a lockfile's record and a
/// configured registry compare equal when they name the same source: no
/// Cargo `registry+`/`sparse+` prefix, no credentials, no trailing slash,
/// no PyPI `/simple` suffix, and each public registry's aliases folded
/// together. Only the scheme and host are case-folded; a path can tell two
/// repositories on one server apart by case alone.
fn canonical_source(ecosystem: Ecosystem, url: &str) -> String {
    let url = url.trim();
    let url = url
        .strip_prefix("registry+")
        .or_else(|| url.strip_prefix("sparse+"))
        .unwrap_or(url);
    let url = match url::Url::parse(url) {
        Ok(mut parsed) => {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.to_string()
        }
        Err(_) => url.to_string(),
    };
    let url = url.trim_end_matches('/');
    let url = match ecosystem {
        Ecosystem::PyPI => url.strip_suffix("/simple").unwrap_or(url),
        _ => url,
    };
    match (ecosystem, url) {
        (Ecosystem::CratesIo, "https://github.com/rust-lang/crates.io-index")
        | (Ecosystem::CratesIo, "https://index.crates.io") => "crates.io".to_string(),
        (Ecosystem::PyPI, "https://pypi.org") => "pypi.org".to_string(),
        _ => url.to_string(),
    }
}

/// True when `recorded`, a lockfile's record of where it resolved a
/// package from, names the source `configured` lists. npm records the
/// tarball URL, which sits under its registry's URL; the other lockfiles
/// record the index itself. No record is the ecosystem's public registry.
fn same_source(ecosystem: Ecosystem, configured: &str, recorded: Option<&str>) -> bool {
    let recorded = recorded.unwrap_or(public_source(ecosystem));
    match ecosystem {
        Ecosystem::Npm => {
            let registry = canonical_source(ecosystem, configured);
            let recorded = canonical_source(ecosystem, recorded);
            recorded == registry || recorded.starts_with(&format!("{registry}/"))
        }
        _ => canonical_source(ecosystem, configured) == canonical_source(ecosystem, recorded),
    }
}

/// Which of `sources` lists the releases `pkg` actually resolves from, or
/// why none can be trusted to. A lockfile's record of where it resolved
/// the pair decides, and every record must name the same configured
/// source. A pair no lockfile records resolves wherever the environment
/// says, which only a single configured source answers for, and a Python
/// pin whose manifest names its own index resolves from that index.
fn release_source<'s, 'r>(
    pkg: &Package,
    locked: &[LockedPackage],
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
    sources: &'s [ReleaseSource<'r>],
) -> Result<&'s ReleaseSource<'r>, String> {
    let norm = normalized_name(&pkg.name, pkg.ecosystem);
    let recorded = locked.iter().filter(|l| {
        l.ecosystem == pkg.ecosystem
            && l.version == pkg.version
            && normalized_name(&l.name, l.ecosystem) == norm
    });
    let mut chosen: Option<&ReleaseSource<'r>> = None;
    for entry in recorded {
        let index = entry.index.as_deref();
        let Some(source) = sources
            .iter()
            .find(|source| same_source(pkg.ecosystem, &source.url, index))
        else {
            return Err(format!(
                "{} resolves {} from {}, which is not a registry upd is configured to list releases from",
                crate::path_display::display_path(&entry.lockfile_path),
                pkg.name,
                index.unwrap_or(public_source(pkg.ecosystem))
            ));
        };
        if chosen.is_some_and(|c| !std::ptr::eq(c, source)) {
            return Err(format!(
                "the lockfiles resolve {} from more than one registry",
                pkg.name
            ));
        }
        chosen = Some(source);
    }
    if let Some(source) = chosen {
        return Ok(source);
    }
    if pkg.ecosystem == Ecosystem::PyPI
        && let Some(reason) = manifest_index(&norm, pkg.ecosystem, packages)
    {
        return Err(reason);
    }
    match sources {
        [source] => Ok(source),
        _ => Err(format!(
            "{} may resolve from any of the {} package indexes upd is configured with",
            pkg.name,
            sources.len()
        )),
    }
}

/// Why a Python pin no lockfile records may resolve from an index of its
/// own: a manifest declaring it `norm` names a package index, or cannot be
/// read to tell.
fn manifest_index(
    norm: &str,
    ecosystem: Ecosystem,
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
) -> Option<String> {
    matching_occurrences(packages, norm, ecosystem)
        .into_iter()
        .find_map(|occ| {
            let path = crate::path_display::display_path(&occ.file_path);
            let declares = match occ.file_type {
                FileType::PyProject => {
                    crate::updater::PyProjectUpdater::declares_package_index(&occ.file_path)
                }
                FileType::Requirements => std::fs::read_to_string(&occ.file_path)
                    .map(|content| {
                        crate::updater::RequirementsUpdater::declares_package_index(&content)
                    })
                    .map_err(anyhow::Error::from),
                _ => Ok(false),
            };
            match declares {
                Ok(false) => None,
                Ok(true) => Some(format!("{path} declares its own package index")),
                Err(error) => Some(format!("{path} could not be read ({error:#})")),
            }
        })
}

/// Ask each vulnerable pair's registry which release its advisories' fix
/// is, before routing turns the fix into a write. An advisory's `fixed`
/// event is a range bound, not a promise that the release exists: a floor
/// at an unpublished version fails in the package manager and rolls back
/// every sibling fix sharing its relock. Only ecosystems whose registry
/// lists every release are asked, each pair only through the one of the
/// sources `sources_for` gives its ecosystem and name that its lockfile
/// says it resolves from (see `release_source`). A pair left unasked, or whose lookup fails or lists
/// no release, gets no entry and is fixed to the bound as named, with a
/// note saying the bound went unconfirmed. `offline` asks nothing and says
/// so once.
pub async fn confirm_fix_releases<'r>(
    audit: &AuditResult,
    locked: &[LockedPackage],
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
    offline: bool,
    sources_for: &dyn Fn(Ecosystem, &str) -> Vec<ReleaseSource<'r>>,
) -> (FixReleases, Vec<String>) {
    const UNCONFIRMED: &str =
        "so its fix is written to the version its advisory names, unconfirmed";
    let mut releases = FixReleases::new();
    let mut notes = Vec::new();
    let mut offline_skipped = false;
    let mut listings: HashMap<
        (&'static str, String, String),
        Option<Vec<crate::registry::VersionMeta>>,
    > = HashMap::new();
    for pkg_result in &audit.vulnerable {
        let pkg = &pkg_result.package;
        let FixBound::Named { bound, .. } = fix_bound(&pkg_result.vulnerabilities) else {
            continue;
        };
        let Some((lang, honors_yanked)) = listed_ecosystem(pkg.ecosystem) else {
            continue;
        };
        if offline {
            offline_skipped = true;
            continue;
        }
        let configured = sources_for(pkg.ecosystem, &pkg.name);
        if configured.is_empty() {
            continue;
        }
        let source = match release_source(pkg, locked, packages, &configured) {
            Ok(source) => source,
            Err(reason) => {
                notes.push(format!("{reason}, {UNCONFIRMED}"));
                continue;
            }
        };
        let registry = source.registry;
        let norm = normalized_name(&pkg.name, pkg.ecosystem);
        let listing_key = (
            pkg.ecosystem.as_str(),
            canonical_source(pkg.ecosystem, &source.url),
            norm.clone(),
        );
        if !listings.contains_key(&listing_key) {
            let listing = match registry.list_versions(&pkg.name).await {
                Ok(versions) if !versions.is_empty() => Some(versions),
                Ok(_) => {
                    notes.push(format!(
                        "{} lists no release of {}, {UNCONFIRMED}",
                        registry.name(),
                        pkg.name
                    ));
                    None
                }
                Err(error) => {
                    notes.push(format!(
                        "could not list the releases of {} ({error:#}), {UNCONFIRMED}",
                        pkg.name
                    ));
                    None
                }
            };
            listings.insert(listing_key.clone(), listing);
        }
        if let Some(versions) = &listings[&listing_key] {
            releases.insert(
                (norm, pkg.version.clone(), pkg.ecosystem.as_str()),
                select_fix_release(
                    versions,
                    bound,
                    &pkg_result.vulnerabilities,
                    lang,
                    honors_yanked,
                ),
            );
        }
    }
    if offline_skipped {
        notes.push(
            "--offline asks no registry whether a fix is published, so each fix is written to the version its advisory names, unconfirmed"
                .to_string(),
        );
    }
    (releases, notes)
}

/// Route every vulnerable (name, version) pair in `audit` into explicit
/// manifest-edit and version-floor targets, or into `unfixable` with a
/// human-readable reason. A pair with no fix at all is unfixable, and so
/// is one whose fix `releases` found no published release for; a pair
/// `releases` confirmed moves to the release it names. A Manifest-covered
/// pair
/// gets one edit per occurrence when its manifest declares a single owner
/// key, or one edit per owner (never a cross product) when it declares
/// several (see [`route_manifest_covered`]); a LockOnly pair floors by lock
/// kind; a pair with no provenance entry falls back to today's
/// occurrence-based manifest edits. Multiple provenance entries for the
/// same pair (one per lockfile that resolves it) are all routed, and
/// same-target floors/edits from different pairs are merged rather than
/// duplicated or left conflicting.
pub fn route_fix_targets(
    audit: &AuditResult,
    prov: &ProvenanceIndex,
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
    releases: &FixReleases,
) -> FixRouting {
    route_targets(audit, prov, packages, releases, true)
}

/// Moves every target the configuration governing its file holds back into
/// `unfixable`, keeping the known fix visible. An ignored package is never
/// written, and a pin below the fix would be reverted by the next update, so
/// neither is a fix. A pin at or above the fix already satisfies it, so that
/// target stays.
pub fn hold_configured_targets(
    routing: FixRouting,
    mut config_for: impl FnMut(
        &Path,
    ) -> anyhow::Result<Option<std::sync::Arc<crate::config::UpdConfig>>>,
) -> anyhow::Result<FixRouting> {
    let FixRouting {
        targets,
        mut unfixable,
    } = routing;
    let mut kept = Vec::with_capacity(targets.len());
    for target in targets {
        match config_for(&target.path)?.and_then(|config| configured_hold(&target, &config)) {
            Some(reason) => unfixable.push(UnfixableTarget {
                package: target.package,
                ecosystem: target.ecosystem,
                dependency_key: target.dependency_key,
                from_version: target.from_version,
                to_version: Some(target.to_version),
                method: Some(target.kind.method()),
                path: Some(target.path),
                reason,
                no_fixed_version: false,
            }),
            None => kept.push(target),
        }
    }
    Ok(FixRouting {
        targets: kept,
        unfixable,
    })
}

/// Why `config` keeps `target` from being written, if it does.
fn configured_hold(target: &FixTarget, config: &crate::config::UpdConfig) -> Option<String> {
    let names = std::iter::once(target.package.as_str()).chain(target.dependency_key.as_deref());
    let to = &target.to_version;
    for name in names {
        if config.should_ignore(name) {
            return Some(format!(
                "ignored by configuration; the fix needs {to} or later"
            ));
        }
        if let Some(pinned) = config.get_pinned_version(name) {
            // Only a pin naming a release at or above the fix satisfies it. A
            // constraint pin or an unknown ordering is treated as below the
            // fix, so the fix is never written past what the pin allows.
            let satisfies = target_lang(target)
                .and_then(|lang| crate::align::release_at_least(pinned, to, lang))
                == Some(true);
            if !satisfies {
                return Some(format!(
                    "pinned to {pinned} by configuration; the fix needs {to} or later"
                ));
            }
        }
    }
    None
}

/// The ecosystem a target's versions are compared in: a floor's mechanism
/// names it, and a manifest edit carries its file type.
fn target_lang(target: &FixTarget) -> Option<Lang> {
    match target.kind {
        FixKind::UvConstraint => Some(Lang::Python),
        FixKind::NpmOverride => Some(Lang::Node),
        FixKind::CargoPrecise => Some(Lang::Rust),
        FixKind::ManifestEdit => target.file_type.map(|file_type| file_type.lang()),
    }
}

/// Route explicitly requested version updates. Unlike automatic audit repairs,
/// these retain the user's chosen bump policy, including major upgrades.
pub fn route_update_targets(
    audit: &AuditResult,
    prov: &ProvenanceIndex,
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
) -> FixRouting {
    route_targets(audit, prov, packages, &FixReleases::new(), false)
}

fn route_targets(
    audit: &AuditResult,
    prov: &ProvenanceIndex,
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
    releases: &FixReleases,
    preserve_npm_compatibility: bool,
) -> FixRouting {
    let mut sink = Sink {
        preserve_npm_compatibility,
        ..Sink::default()
    };

    for pkg_result in &audit.vulnerable {
        let pkg = &pkg_result.package;

        let (bound, advisory) = match fix_bound(&pkg_result.vulnerabilities) {
            FixBound::Missing(advisory) => {
                sink.unfixable
                    .push(no_fix(pkg, format!("{advisory} has no fixed version")));
                continue;
            }
            FixBound::None => continue,
            FixBound::Named { bound, advisory } => (bound, advisory),
        };

        let norm = normalized_name(&pkg.name, pkg.ecosystem);
        let pair_key = (norm, pkg.version.clone(), pkg.ecosystem.as_str());

        let fixed = match releases.get(&pair_key) {
            Some(FixRelease::Release(release)) => release.as_str(),
            Some(FixRelease::Unpublished) => {
                sink.unfixable.push(no_fix(
                    pkg,
                    format!(
                        "{advisory} names {bound} as fixed, but no release of {} at or above it is published",
                        pkg.name
                    ),
                ));
                continue;
            }
            Some(FixRelease::StillAffected {
                release,
                advisory: covering,
            }) => {
                sink.unfixable.push(no_fix(
                    pkg,
                    format!(
                        "{advisory} names {bound} as fixed, but every published release of {} at or above it is still affected (the lowest, {release}, by {covering})",
                        pkg.name
                    ),
                ));
                continue;
            }
            None => bound,
        };
        let to_version = manifest_fix_version(pkg, fixed);

        match prov.map.get(&pair_key) {
            Some(entries) if !entries.is_empty() => {
                for entry in entries {
                    match entry {
                        Provenance::Manifest { owners, lockfile } => {
                            route_manifest_covered(
                                pkg,
                                owners,
                                lockfile,
                                &to_version,
                                packages,
                                &mut sink,
                            );
                        }
                        Provenance::LockOnly { lockfile, kind } => {
                            route_lock_only(
                                pkg,
                                lockfile,
                                *kind,
                                &to_version,
                                prov,
                                packages,
                                &mut sink,
                            );
                        }
                    }
                }
            }
            _ => {
                route_no_provenance(pkg, &to_version, packages, &mut sink);
            }
        }
    }

    let mut manifest_edit_pool = sink.manifest_edits;
    manifest_edit_pool.extend(sink.npm_companions);

    let mut targets = merge_manifest_edits(manifest_edit_pool);
    targets.extend(merge_floor_group(sink.floor_targets));
    targets.extend(sink.cargo_targets);

    FixRouting {
        targets,
        unfixable: sink.unfixable,
    }
}

/// What resolving a lock-only floor produced. "Nothing to do" and "there is a
/// newer release, the ceiling refused it" are different facts about the
/// dependency, and collapsing them into one `None` is what let a lock-only
/// package sit several majors behind while every run reported it up to date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloorResolution {
    /// No floor needed: the candidate is at or below the locked version, or
    /// cooldown held it back.
    NotNeeded,
    /// The version to floor to.
    Floor(String),
    /// A newer version exists but sits above the `--max-bump`/`--only-bump`
    /// ceiling. Nothing is written; the caller reports it as held back.
    Capped(String),
    /// A configured pin would move the floor to this version, and
    /// `--strict-bump` holds every write that is not a selected bump.
    HeldPin(String),
}

/// Resolve the floor version for a lock-only package: config pin if above
/// the locked version, else registry latest gated by cooldown and the bump
/// filter. Registry failures return Err - the caller pushes them into the
/// update error channel (exit 2); they are NEVER collapsed into
/// `NotNeeded`, which would silently exit 0. Lives here rather than in the
/// binary because the per-lang comparison (crate::align::compare_versions)
/// is crate-private.
pub async fn resolve_floor_version(
    registry: &dyn crate::registry::Registry,
    package: &str,
    locked: &str,
    lang: Lang,
    options: &crate::updater::UpdateOptions,
) -> anyhow::Result<FloorResolution> {
    if let Some(pinned) = options.get_pinned_version(package) {
        return Ok(
            if crate::align::compare_versions(pinned, locked, lang) != Ordering::Greater {
                FloorResolution::NotNeeded
            } else if options.allows_write(crate::updater::WriteKind::Pin) {
                FloorResolution::Floor(pinned.to_string())
            } else {
                FloorResolution::HeldPin(pinned.to_string())
            },
        );
    }

    let latest = registry.get_latest_version(package).await?;
    let (outcome, note) =
        crate::updater::apply_cooldown(registry, package, locked, &latest, None, false, options)
            .await;
    if let Some(msg) = note {
        options.note_cooldown_unavailable(&msg);
    }
    let candidate = match outcome {
        crate::updater::CooldownOutcome::Unchanged(v) => v,
        crate::updater::CooldownOutcome::HeldBack { chosen, .. } => chosen,
        crate::updater::CooldownOutcome::Skipped { .. } => return Ok(FloorResolution::NotNeeded),
    };

    if crate::align::compare_versions(&candidate, locked, lang) != Ordering::Greater {
        return Ok(FloorResolution::NotNeeded);
    }
    if !options.allows_bump_for(lang, locked, &candidate) {
        return Ok(FloorResolution::Capped(candidate));
    }
    Ok(FloorResolution::Floor(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{PackageAuditResult, Vulnerability};
    use crate::lockscan::provenance::DirectDep;

    fn vuln(id: &str, fixed: Option<&str>) -> Vulnerability {
        Vulnerability {
            id: id.to_string(),
            summary: None,
            severity: None,
            url: None,
            fixed_version: fixed.map(str::to_string),
            aliases: Vec::new(),
            source: String::new(),
            affected: Vec::new(),
        }
    }

    fn pkg(name: &str, version: &str, ecosystem: Ecosystem) -> Package {
        Package {
            name: name.to_string(),
            version: version.to_string(),
            ecosystem,
        }
    }

    fn vulnerable(package: Package, vulns: Vec<Vulnerability>) -> PackageAuditResult {
        PackageAuditResult {
            package,
            vulnerabilities: vulns,
        }
    }

    fn audit_of(results: Vec<PackageAuditResult>) -> AuditResult {
        AuditResult {
            vulnerable: results,
            safe_count: 0,
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn occ(
        file_path: &str,
        file_type: FileType,
        version: &str,
        line_number: Option<usize>,
        original_name: &str,
        is_bumpable: bool,
    ) -> PackageOccurrence {
        PackageOccurrence {
            file_path: PathBuf::from(file_path),
            file_type,
            version: version.to_string(),
            line_number,
            has_upper_bound: false,
            original_name: original_name.to_string(),
            is_bumpable,
        }
    }

    fn packages_map(
        entries: Vec<((&str, Lang), Vec<PackageOccurrence>)>,
    ) -> HashMap<(String, Lang), Vec<PackageOccurrence>> {
        entries
            .into_iter()
            .map(|((name, lang), occs)| ((name.to_string(), lang), occs))
            .collect()
    }

    fn owner(manifest: &str, file_type: FileType, key: &str, alias: bool) -> Owner {
        Owner {
            manifest: PathBuf::from(manifest),
            file_type,
            dependency_key: key.to_string(),
            npm_alias: alias,
        }
    }

    fn manifest_prov(owners: Vec<Owner>, lockfile: &str) -> Provenance {
        Provenance::Manifest {
            owners,
            lockfile: PathBuf::from(lockfile),
        }
    }

    fn lock_only_prov(lockfile: &str, kind: LockKind) -> Provenance {
        Provenance::LockOnly {
            lockfile: PathBuf::from(lockfile),
            kind,
        }
    }

    type ProvEntry<'a> = ((&'a str, &'a str, &'static str), Vec<Provenance>);

    fn prov_index(
        entries: Vec<ProvEntry<'_>>,
        npm_direct: Vec<(&str, Vec<DirectDep>)>,
    ) -> ProvenanceIndex {
        let mut map = HashMap::new();
        for ((name, version, eco), provs) in entries {
            map.insert((name.to_string(), version.to_string(), eco), provs);
        }
        let mut nd = HashMap::new();
        for (host, deps) in npm_direct {
            nd.insert(PathBuf::from(host), deps);
        }
        ProvenanceIndex {
            map,
            npm_direct: nd,
        }
    }

    fn direct(key: &str, package: &str, spec: &str) -> DirectDep {
        DirectDep {
            key: key.to_string(),
            package: package.to_string(),
            spec: spec.to_string(),
        }
    }

    #[test]
    fn npm_dollar_override_cannot_replace_a_healthy_sibling_branch() {
        let audit = audit_of(vec![vulnerable(
            pkg("pkg", "2.0.2", Ecosystem::Npm),
            vec![vuln("GHSA-1", Some("2.1.4"))],
        )]);
        let prov = prov_index(
            vec![
                (
                    ("pkg", "2.0.2", "npm"),
                    vec![lock_only_prov("proj/package-lock.json", LockKind::Npm)],
                ),
                (
                    ("pkg", "5.0.9", "npm"),
                    vec![manifest_prov(
                        vec![owner(
                            "proj/package.json",
                            FileType::PackageJson,
                            "pkg",
                            false,
                        )],
                        "proj/package-lock.json",
                    )],
                ),
            ],
            vec![("proj/package.json", vec![direct("pkg", "pkg", "^5.0.9")])],
        );
        let routing = route_fix_targets(&audit, &prov, &HashMap::new(), &FixReleases::new());
        assert!(routing.targets.is_empty());
        assert_eq!(routing.unfixable.len(), 1);
        assert!(
            routing.unfixable[0]
                .reason
                .contains("incompatible branches")
        );
    }

    #[test]
    fn npm_transitive_branches_do_not_merge_into_one_major_upgrade() {
        let audit = audit_of(vec![
            vulnerable(
                pkg("brace-expansion", "2.0.2", Ecosystem::Npm),
                vec![vuln("GHSA-2", Some("2.1.4"))],
            ),
            vulnerable(
                pkg("brace-expansion", "5.0.6", Ecosystem::Npm),
                vec![vuln("GHSA-5", Some("5.0.8"))],
            ),
            vulnerable(
                pkg("brace-expansion", "5.0.7", Ecosystem::Npm),
                vec![vuln("GHSA-5b", Some("5.0.9"))],
            ),
        ]);
        let prov = prov_index(
            vec![
                (
                    ("brace-expansion", "2.0.2", "npm"),
                    vec![lock_only_prov("proj/package-lock.json", LockKind::Npm)],
                ),
                (
                    ("brace-expansion", "5.0.6", "npm"),
                    vec![lock_only_prov("proj/package-lock.json", LockKind::Npm)],
                ),
                (
                    ("brace-expansion", "5.0.7", "npm"),
                    vec![lock_only_prov("proj/package-lock.json", LockKind::Npm)],
                ),
            ],
            vec![],
        );
        let routing = route_fix_targets(&audit, &prov, &HashMap::new(), &FixReleases::new());
        assert!(routing.unfixable.is_empty());
        assert_eq!(routing.targets.len(), 2);
        let mut fixes: Vec<_> = routing
            .targets
            .iter()
            .map(|t| (t.from_version.as_str(), t.to_version.as_str()))
            .collect();
        fixes.sort();
        assert_eq!(fixes, [("2.0.2", "2.1.4"), ("5.0.7", "5.0.9")]);
    }

    #[test]
    fn npm_cross_branch_transitive_fix_is_blocked_before_planning() {
        let audit = audit_of(vec![vulnerable(
            pkg("pkg", "2.0.2", Ecosystem::Npm),
            vec![vuln("GHSA-1", Some("5.0.9"))],
        )]);
        let prov = prov_index(
            vec![(
                ("pkg", "2.0.2", "npm"),
                vec![lock_only_prov("proj/package-lock.json", LockKind::Npm)],
            )],
            vec![],
        );
        let routing = route_fix_targets(&audit, &prov, &HashMap::new(), &FixReleases::new());
        assert!(routing.targets.is_empty());
        assert_eq!(routing.unfixable.len(), 1);
        assert!(routing.unfixable[0].reason.contains("parent dependency"));
    }

    #[test]
    fn no_fixed_version_pair_is_unfixable_with_flag() {
        let audit = audit_of(vec![vulnerable(
            pkg("examplepkg", "1.0.0", Ecosystem::PyPI),
            vec![vuln("GHSA-aaaa-bbbb-cccc", None)],
        )]);
        let prov = ProvenanceIndex::default();
        let packages = HashMap::new();

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.targets.is_empty());
        assert_eq!(routing.unfixable.len(), 1);
        let u = &routing.unfixable[0];
        assert!(u.no_fixed_version);
        assert_eq!(u.reason, "GHSA-aaaa-bbbb-cccc has no fixed version");
        assert_eq!(u.package, "examplepkg");
    }

    #[test]
    fn lock_only_uv_pair_floors_to_adjacent_pyproject() {
        let audit = audit_of(vec![vulnerable(
            pkg("examplepkg", "1.0.0", Ecosystem::PyPI),
            vec![vuln("GHSA-1", Some("1.2.0"))],
        )]);
        let prov = prov_index(
            vec![(
                ("examplepkg", "1.0.0", "PyPI"),
                vec![lock_only_prov("proj/uv.lock", LockKind::Uv)],
            )],
            vec![],
        );
        let packages = HashMap::new();

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.unfixable.is_empty());
        assert_eq!(routing.targets.len(), 1);
        let t = &routing.targets[0];
        assert_eq!(t.kind, FixKind::UvConstraint);
        assert_eq!(t.path, PathBuf::from("proj/pyproject.toml"));
        assert_eq!(t.from_version, "1.0.0");
        assert_eq!(t.to_version, "1.2.0");
        assert_eq!(t.vulnerable_version, "1.0.0");
        assert_eq!(t.lockfile, Some(PathBuf::from("proj/uv.lock")));
    }

    #[test]
    fn lock_only_poetry_pair_is_unfixable_with_guidance() {
        let audit = audit_of(vec![vulnerable(
            pkg("examplepkg", "1.0.0", Ecosystem::PyPI),
            vec![vuln("GHSA-1", Some("1.2.0"))],
        )]);
        let prov = prov_index(
            vec![(
                ("examplepkg", "1.0.0", "PyPI"),
                vec![lock_only_prov("proj/poetry.lock", LockKind::Poetry)],
            )],
            vec![],
        );
        let packages = HashMap::new();

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.targets.is_empty());
        assert_eq!(routing.unfixable.len(), 1);
        assert_eq!(
            routing.unfixable[0].reason,
            "no floor mechanism exists for poetry.lock; add examplepkg>=1.2.0 as a direct dependency"
        );
    }

    #[test]
    fn lock_only_cargo_duplicates_get_one_precise_each() {
        let audit = audit_of(vec![
            vulnerable(
                pkg("examplecrate", "1.0.0", Ecosystem::CratesIo),
                vec![vuln("RUSTSEC-1", Some("1.2.0"))],
            ),
            vulnerable(
                pkg("examplecrate", "1.1.0", Ecosystem::CratesIo),
                vec![vuln("RUSTSEC-2", Some("1.2.0"))],
            ),
        ]);
        let prov = prov_index(
            vec![
                (
                    ("examplecrate", "1.0.0", "crates.io"),
                    vec![lock_only_prov("proj/Cargo.lock", LockKind::Cargo)],
                ),
                (
                    ("examplecrate", "1.1.0", "crates.io"),
                    vec![lock_only_prov("proj/Cargo.lock", LockKind::Cargo)],
                ),
            ],
            vec![],
        );
        let packages = HashMap::new();

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.unfixable.is_empty());
        assert_eq!(routing.targets.len(), 2);
        assert!(
            routing
                .targets
                .iter()
                .all(|t| t.kind == FixKind::CargoPrecise)
        );
        let mut froms: Vec<&str> = routing
            .targets
            .iter()
            .map(|t| t.from_version.as_str())
            .collect();
        froms.sort();
        assert_eq!(froms, vec!["1.0.0", "1.1.0"]);
        assert!(routing.targets.iter().all(|t| t.to_version == "1.2.0"));
        assert!(
            routing
                .targets
                .iter()
                .all(|t| t.path == Path::new("proj/Cargo.lock"))
        );
    }

    #[test]
    fn npm_lock_only_not_reachable_gets_range_override() {
        let audit = audit_of(vec![vulnerable(
            pkg("examplepkg", "1.2.0", Ecosystem::Npm),
            vec![vuln("GHSA-1", Some("1.5.0"))],
        )]);
        let prov = prov_index(
            vec![(
                ("examplepkg", "1.2.0", "npm"),
                vec![lock_only_prov("proj/package-lock.json", LockKind::Npm)],
            )],
            vec![("proj/package.json", vec![])],
        );
        let packages = HashMap::new();

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.unfixable.is_empty());
        assert_eq!(routing.targets.len(), 1);
        let t = &routing.targets[0];
        assert_eq!(t.kind, FixKind::NpmOverride);
        assert_eq!(t.npm_form, Some(NpmOverrideForm::CompatibleRange));
        assert_eq!(t.path, PathBuf::from("proj/package.json"));
        assert_eq!(t.to_version, "1.5.0");
    }

    #[test]
    fn npm_both_direct_and_transitive_gets_dollar_name_plus_manifest_edit() {
        let audit = audit_of(vec![vulnerable(
            pkg("examplepkg", "1.2.0", Ecosystem::Npm),
            vec![vuln("GHSA-1", Some("1.5.0"))],
        )]);
        let prov = prov_index(
            vec![(
                ("examplepkg", "1.2.0", "npm"),
                vec![lock_only_prov("proj/package-lock.json", LockKind::Npm)],
            )],
            vec![(
                "proj/package.json",
                vec![direct("examplepkg", "examplepkg", "^1.0.0")],
            )],
        );
        let packages = packages_map(vec![(
            ("examplepkg", Lang::Node),
            vec![occ(
                "proj/package.json",
                FileType::PackageJson,
                "^1.0.0",
                Some(5),
                "examplepkg",
                true,
            )],
        )]);

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.unfixable.is_empty());
        assert_eq!(routing.targets.len(), 2);

        let override_target = routing
            .targets
            .iter()
            .find(|t| t.kind == FixKind::NpmOverride)
            .unwrap();
        assert_eq!(override_target.npm_form, Some(NpmOverrideForm::DollarName));
        assert_eq!(override_target.to_version, "1.5.0");

        let edit_target = routing
            .targets
            .iter()
            .find(|t| t.kind == FixKind::ManifestEdit)
            .unwrap();
        assert_eq!(edit_target.from_version, "^1.0.0");
        assert_eq!(edit_target.to_version, "1.5.0");
        assert_eq!(edit_target.line_number, Some(5));
        assert_eq!(edit_target.dependency_key, None);
    }

    #[test]
    fn npm_alias_reachable_only_is_unfixable() {
        let audit = audit_of(vec![vulnerable(
            pkg("realpkg", "1.2.0", Ecosystem::Npm),
            vec![vuln("GHSA-1", Some("1.5.0"))],
        )]);
        let prov = prov_index(
            vec![(
                ("realpkg", "1.2.0", "npm"),
                vec![lock_only_prov("proj/package-lock.json", LockKind::Npm)],
            )],
            vec![(
                "proj/package.json",
                vec![direct("my-react", "realpkg", "npm:realpkg@^1.0.0")],
            )],
        );
        let packages = HashMap::new();

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.targets.is_empty());
        assert_eq!(routing.unfixable.len(), 1);
        assert_eq!(
            routing.unfixable[0].reason,
            "only reachable through npm alias \"my-react\"; an npm override cannot be expressed with a $-reference here - add \"realpkg\": \">=1.5.0\" to overrides manually if desired"
        );
    }

    #[test]
    fn manifest_covered_alias_owner_is_unfixable() {
        let audit = audit_of(vec![vulnerable(
            pkg("realpkg", "18.0.0", Ecosystem::Npm),
            vec![vuln("GHSA-1", Some("18.2.0"))],
        )]);
        let prov = prov_index(
            vec![(
                ("realpkg", "18.0.0", "npm"),
                vec![manifest_prov(
                    vec![owner(
                        "proj/package.json",
                        FileType::PackageJson,
                        "my-react",
                        true,
                    )],
                    "proj/package-lock.json",
                )],
            )],
            vec![],
        );
        let packages = HashMap::new();

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.targets.is_empty());
        assert_eq!(routing.unfixable.len(), 1);
        assert_eq!(
            routing.unfixable[0].reason,
            "declared as npm alias \"my-react\" in proj/package.json; upd cannot rewrite alias specs - update it manually to \"my-react\": \"npm:realpkg@>=18.2.0\""
        );
    }

    #[test]
    fn cargo_rename_manifest_edit_carries_dependency_key() {
        let audit = audit_of(vec![vulnerable(
            pkg("serde", "1.0.5", Ecosystem::CratesIo),
            vec![vuln("RUSTSEC-1", Some("1.0.10"))],
        )]);
        let prov = prov_index(
            vec![(
                ("serde", "1.0.5", "crates.io"),
                vec![manifest_prov(
                    vec![owner(
                        "proj/Cargo.toml",
                        FileType::CargoToml,
                        "old_serde",
                        false,
                    )],
                    "proj/Cargo.lock",
                )],
            )],
            vec![],
        );
        let packages = packages_map(vec![(
            ("serde", Lang::Rust),
            vec![occ(
                "proj/Cargo.toml",
                FileType::CargoToml,
                "1.0.5",
                Some(8),
                "serde",
                true,
            )],
        )]);

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.unfixable.is_empty());
        assert_eq!(routing.targets.len(), 1);
        let t = &routing.targets[0];
        assert_eq!(t.kind, FixKind::ManifestEdit);
        assert_eq!(t.dependency_key, Some("old_serde".to_string()));
        assert_eq!(t.package, "serde");
        assert_eq!(t.from_version, "1.0.5");
        assert_eq!(t.to_version, "1.0.10");
        assert_eq!(t.line_number, Some(8));
    }

    #[test]
    fn unbumpable_occurrence_is_unfixable() {
        let audit = audit_of(vec![vulnerable(
            pkg(
                "example.com/mod",
                "v0.0.0-20240101000000-abcdef123456",
                Ecosystem::Go,
            ),
            vec![vuln("GO-1", Some("1.2.0"))],
        )]);
        let prov = ProvenanceIndex::default();
        let packages = packages_map(vec![(
            ("example.com/mod", Lang::Go),
            vec![occ(
                "proj/go.mod",
                FileType::GoMod,
                "v0.0.0-20240101000000-abcdef123456",
                Some(12),
                "example.com/mod",
                false,
            )],
        )]);

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.targets.is_empty());
        assert_eq!(routing.unfixable.len(), 1);
        assert_eq!(
            routing.unfixable[0].reason,
            "no bumpable manifest entry (e.g. a commit-pinned version)"
        );
    }

    #[test]
    fn multiple_lock_only_versions_merge_to_max_floor() {
        let audit = audit_of(vec![
            vulnerable(
                pkg("examplepkg", "1.0.0", Ecosystem::PyPI),
                vec![vuln("GHSA-1", Some("1.2.0"))],
            ),
            vulnerable(
                pkg("examplepkg", "1.1.0", Ecosystem::PyPI),
                vec![vuln("GHSA-2", Some("1.3.0"))],
            ),
        ]);
        let prov = prov_index(
            vec![
                (
                    ("examplepkg", "1.0.0", "PyPI"),
                    vec![lock_only_prov("proj/uv.lock", LockKind::Uv)],
                ),
                (
                    ("examplepkg", "1.1.0", "PyPI"),
                    vec![lock_only_prov("proj/uv.lock", LockKind::Uv)],
                ),
            ],
            vec![],
        );
        let packages = HashMap::new();

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.unfixable.is_empty());
        assert_eq!(routing.targets.len(), 1);
        let t = &routing.targets[0];
        assert_eq!(t.kind, FixKind::UvConstraint);
        assert_eq!(t.to_version, "1.3.0");
        assert_eq!(t.from_version, "1.1.0");
    }

    #[test]
    fn go_pair_without_provenance_routes_manifest_edits_as_today() {
        let audit = audit_of(vec![vulnerable(
            pkg("example.com/mod", "v1.0.0", Ecosystem::Go),
            vec![vuln("GO-1", Some("1.2.0"))],
        )]);
        let prov = ProvenanceIndex::default();
        let packages = packages_map(vec![(
            ("example.com/mod", Lang::Go),
            vec![occ(
                "proj/go.mod",
                FileType::GoMod,
                "v1.0.0",
                Some(6),
                "example.com/mod",
                true,
            )],
        )]);

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.unfixable.is_empty());
        assert_eq!(routing.targets.len(), 1);
        let t = &routing.targets[0];
        assert_eq!(t.kind, FixKind::ManifestEdit);
        assert_eq!(t.path, PathBuf::from("proj/go.mod"));
        assert_eq!(
            t.to_version, "v1.2.0",
            "Go fixed version normalized with v prefix"
        );
        assert_eq!(t.lockfile, None);
        assert_eq!(t.dependency_key, None);
    }

    #[test]
    fn multiple_npm_lock_only_versions_merge_override_and_companion_edit() {
        let audit = audit_of(vec![
            vulnerable(
                pkg("examplepkg", "1.2.0", Ecosystem::Npm),
                vec![vuln("GHSA-1", Some("1.5.0"))],
            ),
            vulnerable(
                pkg("examplepkg", "1.3.0", Ecosystem::Npm),
                vec![vuln("GHSA-2", Some("1.6.0"))],
            ),
        ]);
        let prov = prov_index(
            vec![
                (
                    ("examplepkg", "1.2.0", "npm"),
                    vec![lock_only_prov("proj/package-lock.json", LockKind::Npm)],
                ),
                (
                    ("examplepkg", "1.3.0", "npm"),
                    vec![lock_only_prov("proj/package-lock.json", LockKind::Npm)],
                ),
            ],
            vec![(
                "proj/package.json",
                vec![direct("examplepkg", "examplepkg", "^1.0.0")],
            )],
        );
        let packages = packages_map(vec![(
            ("examplepkg", Lang::Node),
            vec![occ(
                "proj/package.json",
                FileType::PackageJson,
                "^1.0.0",
                Some(5),
                "examplepkg",
                true,
            )],
        )]);

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.unfixable.is_empty());
        assert_eq!(routing.targets.len(), 2);

        let override_target = routing
            .targets
            .iter()
            .find(|t| t.kind == FixKind::NpmOverride)
            .unwrap();
        assert_eq!(override_target.npm_form, Some(NpmOverrideForm::DollarName));
        assert_eq!(override_target.to_version, "1.6.0");
        assert_eq!(override_target.from_version, "1.3.0");

        let edit_target = routing
            .targets
            .iter()
            .find(|t| t.kind == FixKind::ManifestEdit)
            .unwrap();
        assert_eq!(edit_target.to_version, "1.6.0");
        assert_eq!(edit_target.from_version, "^1.0.0");
    }

    #[test]
    fn npm_own_name_direct_without_occurrence_is_unfixable() {
        let audit = audit_of(vec![vulnerable(
            pkg("examplepkg", "1.2.0", Ecosystem::Npm),
            vec![vuln("GHSA-1", Some("1.5.0"))],
        )]);
        let prov = prov_index(
            vec![(
                ("examplepkg", "1.2.0", "npm"),
                vec![lock_only_prov("proj/package-lock.json", LockKind::Npm)],
            )],
            vec![(
                "proj/package.json",
                vec![direct("examplepkg", "examplepkg", "file:../local")],
            )],
        );
        let packages = HashMap::new();

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.targets.is_empty());
        assert_eq!(routing.unfixable.len(), 1);
        assert_eq!(
            routing.unfixable[0].reason,
            "direct dependency \"examplepkg\" has a spec upd cannot bump; floor it manually"
        );
    }

    #[test]
    fn pair_covered_in_one_lock_and_lock_only_in_another_routes_both() {
        let audit = audit_of(vec![vulnerable(
            pkg("examplecrate", "1.2.3", Ecosystem::CratesIo),
            vec![vuln("RUSTSEC-1", Some("1.3.0"))],
        )]);
        let prov = prov_index(
            vec![(
                ("examplecrate", "1.2.3", "crates.io"),
                vec![
                    manifest_prov(
                        vec![owner(
                            "a/Cargo.toml",
                            FileType::CargoToml,
                            "examplecrate",
                            false,
                        )],
                        "a/Cargo.lock",
                    ),
                    lock_only_prov("b/Cargo.lock", LockKind::Cargo),
                ],
            )],
            vec![],
        );
        let packages = packages_map(vec![(
            ("examplecrate", Lang::Rust),
            vec![occ(
                "a/Cargo.toml",
                FileType::CargoToml,
                "1.2.3",
                Some(4),
                "examplecrate",
                true,
            )],
        )]);

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.unfixable.is_empty());
        assert_eq!(routing.targets.len(), 2);
        assert!(
            routing
                .targets
                .iter()
                .any(|t| t.kind == FixKind::ManifestEdit && t.path == Path::new("a/Cargo.toml"))
        );
        assert!(
            routing
                .targets
                .iter()
                .any(|t| t.kind == FixKind::CargoPrecise && t.path == Path::new("b/Cargo.lock"))
        );
    }

    #[test]
    fn rename_and_plain_declaration_of_same_crate_get_one_edit_per_key() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1.0\"\nold_serde = { package = \"serde\", version = \"1.0\" }\n",
        )
        .unwrap();
        let manifest_str = manifest.to_str().unwrap();

        let audit = audit_of(vec![vulnerable(
            pkg("serde", "1.0.5", Ecosystem::CratesIo),
            vec![vuln("RUSTSEC-1", Some("1.0.10"))],
        )]);
        let prov = prov_index(
            vec![(
                ("serde", "1.0.5", "crates.io"),
                vec![manifest_prov(
                    vec![
                        owner(manifest_str, FileType::CargoToml, "serde", false),
                        owner(manifest_str, FileType::CargoToml, "old_serde", false),
                    ],
                    "proj/Cargo.lock",
                )],
            )],
            vec![],
        );
        // Occurrences parse_dependencies would yield for both declarations:
        // the resolved registry name "serde" for each, on distinct lines.
        let packages = packages_map(vec![(
            ("serde", Lang::Rust),
            vec![
                occ(
                    manifest_str,
                    FileType::CargoToml,
                    "1.0.5",
                    Some(5),
                    "serde",
                    true,
                ),
                occ(
                    manifest_str,
                    FileType::CargoToml,
                    "1.0.5",
                    Some(6),
                    "serde",
                    true,
                ),
            ],
        )]);

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.unfixable.is_empty());
        assert_eq!(
            routing.targets.len(),
            2,
            "must never be a cross product of owners and occurrences: {:?}",
            routing.targets
        );

        let plain = routing
            .targets
            .iter()
            .find(|t| t.dependency_key.is_none())
            .expect("plain serde key edit");
        assert_eq!(
            plain.from_version, "1.0",
            "re-derived from serde's own spec fragment"
        );
        assert_eq!(plain.line_number, None);

        let renamed = routing
            .targets
            .iter()
            .find(|t| t.dependency_key.as_deref() == Some("old_serde"))
            .expect("old_serde key edit");
        assert_eq!(
            renamed.from_version, "1.0",
            "re-derived from old_serde's own spec fragment"
        );
        assert_eq!(renamed.line_number, None);
    }

    #[test]
    fn same_key_in_two_sections_keeps_one_edit_per_line() {
        let audit = audit_of(vec![vulnerable(
            pkg("serde", "1.0.5", Ecosystem::CratesIo),
            vec![vuln("RUSTSEC-1", Some("1.0.10"))],
        )]);
        let prov = prov_index(
            vec![(
                ("serde", "1.0.5", "crates.io"),
                vec![manifest_prov(
                    vec![owner(
                        "proj/Cargo.toml",
                        FileType::CargoToml,
                        "serde",
                        false,
                    )],
                    "proj/Cargo.lock",
                )],
            )],
            vec![],
        );
        let packages = packages_map(vec![(
            ("serde", Lang::Rust),
            vec![
                occ(
                    "proj/Cargo.toml",
                    FileType::CargoToml,
                    "1.0.5",
                    Some(5),
                    "serde",
                    true,
                ),
                occ(
                    "proj/Cargo.toml",
                    FileType::CargoToml,
                    "1.0.5",
                    Some(12),
                    "serde",
                    true,
                ),
            ],
        )]);

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.unfixable.is_empty());
        assert_eq!(
            routing.targets.len(),
            2,
            "one edit per declaration line; the merge key must not lose line_number: {:?}",
            routing.targets
        );
        let mut lines: Vec<Option<usize>> = routing.targets.iter().map(|t| t.line_number).collect();
        lines.sort();
        assert_eq!(lines, vec![Some(5), Some(12)]);
        assert!(routing.targets.iter().all(|t| t.to_version == "1.0.10"));
    }

    #[test]
    fn direct_vulnerable_pair_and_companion_edit_merge_to_max() {
        let audit = audit_of(vec![
            vulnerable(
                pkg("examplepkg", "2.4.0", Ecosystem::Npm),
                vec![vuln("GHSA-1", Some("2.5.0"))],
            ),
            vulnerable(
                pkg("examplepkg", "2.2.0", Ecosystem::Npm),
                vec![vuln("GHSA-2", Some("2.6.0"))],
            ),
        ]);
        let prov = prov_index(
            vec![
                (
                    ("examplepkg", "2.4.0", "npm"),
                    vec![manifest_prov(
                        vec![owner(
                            "proj/package.json",
                            FileType::PackageJson,
                            "examplepkg",
                            false,
                        )],
                        "proj/package-lock.json",
                    )],
                ),
                (
                    ("examplepkg", "2.2.0", "npm"),
                    vec![lock_only_prov("proj/package-lock.json", LockKind::Npm)],
                ),
            ],
            vec![(
                "proj/package.json",
                vec![direct("examplepkg", "examplepkg", "^2.0.0")],
            )],
        );
        let packages = packages_map(vec![(
            ("examplepkg", Lang::Node),
            vec![occ(
                "proj/package.json",
                FileType::PackageJson,
                "^2.0.0",
                Some(7),
                "examplepkg",
                true,
            )],
        )]);

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.unfixable.is_empty());
        assert_eq!(
            routing.targets.len(),
            2,
            "the direct pair's own edit and its DollarName companion must merge on the same line, not conflict: {:?}",
            routing.targets
        );

        let edits: Vec<_> = routing
            .targets
            .iter()
            .filter(|t| t.kind == FixKind::ManifestEdit)
            .collect();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].to_version, "2.6.0");
        assert_eq!(edits[0].from_version, "^2.0.0");
        assert_eq!(edits[0].line_number, Some(7));

        let overrides: Vec<_> = routing
            .targets
            .iter()
            .filter(|t| t.kind == FixKind::NpmOverride)
            .collect();
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].npm_form, Some(NpmOverrideForm::DollarName));
        assert_eq!(overrides[0].to_version, "2.6.0");
    }

    #[test]
    fn manifest_covered_unbumpable_occurrence_is_unfixable() {
        let audit = audit_of(vec![vulnerable(
            pkg("examplecrate", "1.0.0", Ecosystem::CratesIo),
            vec![vuln("RUSTSEC-1", Some("1.2.0"))],
        )]);
        let prov = prov_index(
            vec![(
                ("examplecrate", "1.0.0", "crates.io"),
                vec![manifest_prov(
                    vec![owner(
                        "proj/Cargo.toml",
                        FileType::CargoToml,
                        "examplecrate",
                        false,
                    )],
                    "proj/Cargo.lock",
                )],
            )],
            vec![],
        );
        let packages = packages_map(vec![(
            ("examplecrate", Lang::Rust),
            vec![occ(
                "proj/Cargo.toml",
                FileType::CargoToml,
                "1.0.0",
                Some(4),
                "examplecrate",
                false,
            )],
        )]);

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert!(routing.targets.is_empty());
        assert_eq!(routing.unfixable.len(), 1);
        let u = &routing.unfixable[0];
        assert_eq!(
            u.reason,
            "no bumpable manifest entry (e.g. a commit-pinned version)"
        );
        assert!(!u.no_fixed_version);
    }

    /// FORWARD GUARD, vacuous in v1 by construction. `matching_occurrences`
    /// (`src/fix/mod.rs:162-183`) only considers keys whose `Lang` equals
    /// `ecosystem_lang(ecosystem)`, and `ecosystem_lang`'s image is
    /// Python/Node/Rust/Go/Ruby/DotNet - never `Lang::Annotated`. So no audit
    /// result can route an annotated occurrence today, and this test would pass
    /// against an empty implementation of the rule.
    ///
    /// It is here for V2.1, which gives annotated dependencies their own
    /// ecosystem `Lang` and makes this same fixture route a `ManifestEdit` into a
    /// Makefile that `apply_fix` has no writer for. When it goes red, the router
    /// needs an explicit `FileType::Annotated` exclusion - do not relax the
    /// assertion.
    ///
    /// The `pyproject.toml` occurrence is the control: routing must still produce
    /// its `ManifestEdit`, or this passes because nothing routed at all.
    #[test]
    fn annotated_occurrences_are_never_routed_to_a_fix_target() {
        use crate::align::scan_packages;
        use crate::updater::ParseWarnings;

        let audit = audit_of(vec![vulnerable(
            pkg("ruff", "0.1.0", Ecosystem::PyPI),
            vec![vuln("GHSA-1", Some("0.2.0"))],
        )]);
        // No lockfile was scanned, so the pair has no provenance entry and
        // routing takes `route_no_provenance` (rule 4) - the one path that walks
        // the occurrence map directly, which is where an annotated occurrence
        // would leak in.
        let prov = prov_index(vec![], vec![]);
        let mut packages = packages_map(vec![(
            ("ruff", Lang::Python),
            vec![occ(
                "pyproject.toml",
                FileType::PyProject,
                "0.1.0",
                Some(4),
                "ruff",
                true,
            )],
        )]);

        // Scan the annotated half through production parsing and keying. This
        // makes the forward guard sensitive to V2.1 changing the occurrence's
        // Lang from Annotated to Python; a hand-built map could never see that
        // transition and would stay vacuously green forever.
        let tmp = tempfile::tempdir().unwrap();
        let makefile = tmp.path().join("Makefile");
        std::fs::write(&makefile, "RUFF ?= 0.1.0  # upd: pypi ruff\n").unwrap();
        let scanned = scan_packages(
            &[(makefile.clone(), FileType::Annotated)],
            &[],
            ParseWarnings::Suppress,
        )
        .unwrap();
        let scanned_occurrences: Vec<_> = scanned.values().flatten().collect();
        assert_eq!(
            scanned_occurrences.len(),
            1,
            "precondition: the real scan must find exactly one annotated occurrence: {scanned:?}"
        );
        assert_eq!(
            scanned_occurrences[0].file_path, makefile,
            "precondition: the real scan must see the Makefile"
        );
        assert_eq!(
            scanned_occurrences[0].file_type,
            FileType::Annotated,
            "precondition: the scanned occurrence must retain its file type"
        );
        let scanned_lang = scanned
            .keys()
            .find_map(|(name, lang)| (name == "ruff").then_some(*lang));
        for (key, occurrences) in scanned {
            packages.entry(key).or_default().extend(occurrences);
        }

        let routing = route_fix_targets(&audit, &prov, &packages, &FixReleases::new());

        assert_eq!(routing.targets.len(), 1, "{:?}", routing.targets);
        assert_eq!(routing.targets[0].path, PathBuf::from("pyproject.toml"));
        assert!(
            routing
                .targets
                .iter()
                .all(|t| t.path.as_path() != makefile.as_path()
                    && t.file_type != Some(FileType::Annotated)),
            "no fix target may name an annotated file: {:?}",
            routing.targets
        );
        assert!(
            routing
                .unfixable
                .iter()
                .all(|u| u.path.as_deref() != Some(makefile.as_path())),
            "an annotated occurrence must not even be reported as unfixable: {:?}",
            routing.unfixable
        );
        assert_eq!(
            scanned_lang,
            Some(Lang::Annotated),
            "v1 precondition: the real scan must key the Makefile occurrence as Lang::Annotated"
        );
    }

    mod resolve_floor_version_tests {
        use super::*;
        use crate::config::UpdConfig;
        use crate::registry::mock::MockRegistry;
        use crate::updater::{BumpFilter, UpdateOptions};
        use std::sync::Arc;

        #[tokio::test]
        async fn registry_latest_above_locked_is_floored() {
            let registry = MockRegistry::new("PyPI").with_version("lockonly", "0.49.1");
            let options = UpdateOptions::new(false, false);

            let result =
                resolve_floor_version(&registry, "lockonly", "0.40.0", Lang::Python, &options)
                    .await
                    .unwrap();

            assert_eq!(result, FloorResolution::Floor("0.49.1".to_string()));
        }

        #[tokio::test]
        async fn candidate_at_or_below_locked_yields_no_floor() {
            let registry = MockRegistry::new("PyPI").with_version("lockonly", "0.40.0");
            let options = UpdateOptions::new(false, false);

            let result =
                resolve_floor_version(&registry, "lockonly", "0.40.0", Lang::Python, &options)
                    .await
                    .unwrap();

            assert_eq!(result, FloorResolution::NotNeeded);
        }

        /// A candidate above the ceiling is a distinct outcome from "no floor
        /// needed": there IS a newer release, and the caller has to be able to
        /// report it as held back rather than as up to date.
        #[tokio::test]
        async fn max_bump_caps_the_floor() {
            let registry = MockRegistry::new("PyPI").with_version("lockonly", "1.2.0");
            let options = UpdateOptions::new(false, false).with_bump_filter(BumpFilter {
                major: false,
                minor: true,
                patch: true,
            });

            let result =
                resolve_floor_version(&registry, "lockonly", "0.40.0", Lang::Python, &options)
                    .await
                    .unwrap();

            assert_eq!(result, FloorResolution::Capped("1.2.0".to_string()));
        }

        #[tokio::test]
        async fn config_pin_above_locked_wins_over_registry() {
            let registry = MockRegistry::new("PyPI").with_version("lockonly", "0.49.1");
            let config = Arc::new(UpdConfig {
                pin: HashMap::from([("lockonly".to_string(), "0.45.0".to_string())]),
                ..Default::default()
            });
            let options = UpdateOptions::new(false, false).with_config(config);

            let result =
                resolve_floor_version(&registry, "lockonly", "0.40.0", Lang::Python, &options)
                    .await
                    .unwrap();

            assert_eq!(result, FloorResolution::Floor("0.45.0".to_string()));
        }

        #[tokio::test]
        async fn config_pin_at_or_below_locked_yields_no_floor() {
            let registry = MockRegistry::new("PyPI").with_version("lockonly", "0.49.1");
            let config = Arc::new(UpdConfig {
                pin: HashMap::from([("lockonly".to_string(), "0.40.0".to_string())]),
                ..Default::default()
            });
            let options = UpdateOptions::new(false, false).with_config(config);

            let result =
                resolve_floor_version(&registry, "lockonly", "0.40.0", Lang::Python, &options)
                    .await
                    .unwrap();

            assert_eq!(result, FloorResolution::NotNeeded);
        }

        #[tokio::test]
        async fn registry_failure_is_an_error_not_none() {
            let registry = MockRegistry::new("PyPI");
            let options = UpdateOptions::new(false, false);

            let result = resolve_floor_version(
                &registry,
                "missing-package",
                "0.40.0",
                Lang::Python,
                &options,
            )
            .await;

            assert!(result.is_err());
        }
    }

    /// Whether an advisory's fix bound is a release the package manager can
    /// install. The bound is a range edge: RustSec writes one past the last
    /// release for an unmaintained crate, and a floor at it fails the relock
    /// it shares with every sibling fix.
    mod fix_release_confirmation {
        use super::*;
        use crate::registry::Registry;
        use crate::registry::mock::MockRegistry;

        fn cargo_lock_only(name: &str, version: &str) -> ProvenanceIndex {
            prov_index(
                vec![(
                    (name, version, "crates.io"),
                    vec![lock_only_prov("proj/Cargo.lock", LockKind::Cargo)],
                )],
                vec![],
            )
        }

        fn crate_release(registry: MockRegistry, name: &str, version: &str) -> MockRegistry {
            registry.with_version_meta(name, version, None, false, false)
        }

        async fn confirm(
            audit: &AuditResult,
            registry: &MockRegistry,
            ecosystem: Ecosystem,
        ) -> (FixReleases, Vec<String>) {
            confirm_from(audit, registry, ecosystem, &[], &HashMap::new(), false).await
        }

        async fn confirm_from(
            audit: &AuditResult,
            registry: &MockRegistry,
            ecosystem: Ecosystem,
            locked: &[LockedPackage],
            packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
            offline: bool,
        ) -> (FixReleases, Vec<String>) {
            confirm_via(
                audit,
                &[(public_source(ecosystem), registry)],
                ecosystem,
                locked,
                packages,
                offline,
            )
            .await
        }

        /// `confirm_fix_releases` with `ecosystem` listed from `sources`,
        /// each a configured source URL and the registry answering for it.
        async fn confirm_via(
            audit: &AuditResult,
            sources: &[(&str, &MockRegistry)],
            ecosystem: Ecosystem,
            locked: &[LockedPackage],
            packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
            offline: bool,
        ) -> (FixReleases, Vec<String>) {
            let sources_for = |asked: Ecosystem, _: &str| -> Vec<ReleaseSource<'_>> {
                if asked != ecosystem {
                    return Vec::new();
                }
                sources
                    .iter()
                    .map(|(url, registry)| ReleaseSource {
                        url: url.to_string(),
                        registry: *registry as &dyn Registry,
                    })
                    .collect()
            };
            confirm_fix_releases(audit, locked, packages, offline, &sources_for).await
        }

        fn vuln_in(id: &str, fixed: &str, windows: &[(&str, Option<&str>)]) -> Vulnerability {
            Vulnerability {
                affected: windows
                    .iter()
                    .map(|(introduced, fixed)| crate::audit::AffectedRange {
                        introduced: introduced.to_string(),
                        fixed: fixed.map(str::to_string),
                        last_affected: None,
                        limit: None,
                    })
                    .collect(),
                ..vuln(id, Some(fixed))
            }
        }

        fn locked_at(
            name: &str,
            version: &str,
            lockfile: &str,
            index: Option<&str>,
        ) -> LockedPackage {
            LockedPackage {
                name: name.to_string(),
                version: version.to_string(),
                ecosystem: Ecosystem::PyPI,
                lockfile_path: PathBuf::from(lockfile),
                line_number: None,
                locator: None,
                index: index.map(str::to_string),
            }
        }

        fn release_of(
            releases: &FixReleases,
            name: &str,
            version: &str,
            eco: &'static str,
        ) -> Option<FixRelease> {
            releases
                .get(&(name.to_string(), version.to_string(), eco))
                .cloned()
        }

        #[test]
        fn an_unpublished_fix_is_unfixable_rather_than_a_write() {
            let audit = audit_of(vec![vulnerable(
                pkg("stalecrate", "0.4.20", Ecosystem::CratesIo),
                vec![vuln("RUSTSEC-2020-0001", Some("0.4.21-0"))],
            )]);
            let releases = FixReleases::from([(
                ("stalecrate".to_string(), "0.4.20".to_string(), "crates.io"),
                FixRelease::Unpublished,
            )]);

            let routing = route_fix_targets(
                &audit,
                &cargo_lock_only("stalecrate", "0.4.20"),
                &HashMap::new(),
                &releases,
            );

            assert!(routing.targets.is_empty(), "{:?}", routing.targets);
            assert_eq!(routing.unfixable.len(), 1);
            let u = &routing.unfixable[0];
            assert!(
                u.no_fixed_version,
                "no release fixes it, like a missing bound"
            );
            assert_eq!(u.to_version, None);
            assert_eq!(
                u.reason,
                "RUSTSEC-2020-0001 names 0.4.21-0 as fixed, but no release of stalecrate at or above it is published"
            );
        }

        #[test]
        fn a_confirmed_release_replaces_the_bound_in_the_write() {
            let audit = audit_of(vec![vulnerable(
                pkg("examplecrate", "1.0.0", Ecosystem::CratesIo),
                vec![vuln("RUSTSEC-1", Some("1.2.0-0"))],
            )]);
            let releases = FixReleases::from([(
                ("examplecrate".to_string(), "1.0.0".to_string(), "crates.io"),
                FixRelease::Release("1.2.1".to_string()),
            )]);

            let routing = route_fix_targets(
                &audit,
                &cargo_lock_only("examplecrate", "1.0.0"),
                &HashMap::new(),
                &releases,
            );

            assert!(routing.unfixable.is_empty(), "{:?}", routing.unfixable);
            assert_eq!(routing.targets.len(), 1);
            assert_eq!(routing.targets[0].kind, FixKind::CargoPrecise);
            assert_eq!(routing.targets[0].to_version, "1.2.1");
        }

        #[tokio::test]
        async fn a_bound_past_the_last_release_is_unpublished() {
            let registry = crate_release(
                crate_release(MockRegistry::new("crates.io"), "stalecrate", "0.4.19"),
                "stalecrate",
                "0.4.20",
            );
            let audit = audit_of(vec![vulnerable(
                pkg("stalecrate", "0.4.20", Ecosystem::CratesIo),
                vec![vuln("RUSTSEC-2020-0001", Some("0.4.21-0"))],
            )]);

            let (releases, notes) = confirm(&audit, &registry, Ecosystem::CratesIo).await;

            assert_eq!(
                release_of(&releases, "stalecrate", "0.4.20", "crates.io"),
                Some(FixRelease::Unpublished)
            );
            assert!(notes.is_empty(), "{notes:?}");
        }

        #[tokio::test]
        async fn a_published_bound_is_kept_as_the_advisory_spells_it() {
            // PEP 440 reads 2.0 and 2.0.0 as one release; the advisory's
            // spelling is what the fix writes.
            let registry = MockRegistry::new("PyPI")
                .with_version_meta("Example_Pkg", "2.0.0", None, false, false)
                .with_version_meta("Example_Pkg", "2.1.0", None, false, false);
            let audit = audit_of(vec![vulnerable(
                pkg("Example_Pkg", "1.0.0", Ecosystem::PyPI),
                vec![vuln("PYSEC-1", Some("2.0"))],
            )]);

            let (releases, _) = confirm(&audit, &registry, Ecosystem::PyPI).await;

            assert_eq!(
                release_of(&releases, "example-pkg", "1.0.0", "PyPI"),
                Some(FixRelease::Release("2.0".to_string()))
            );
        }

        #[tokio::test]
        async fn an_unpublished_bound_moves_to_the_lowest_installable_stable_release_above_it() {
            let registry = MockRegistry::new("crates.io")
                .with_version_meta("examplecrate", "1.1.0", None, false, false)
                .with_version_meta("examplecrate", "1.2.0-rc.1", None, false, true)
                .with_version_meta("examplecrate", "1.2.0", None, true, false)
                .with_version_meta("examplecrate", "1.3.0", None, false, false)
                .with_version_meta("examplecrate", "1.2.1", None, false, false);
            let audit = audit_of(vec![vulnerable(
                pkg("examplecrate", "1.1.0", Ecosystem::CratesIo),
                vec![vuln("RUSTSEC-1", Some("1.2.0-0"))],
            )]);

            let (releases, _) = confirm(&audit, &registry, Ecosystem::CratesIo).await;

            assert_eq!(
                release_of(&releases, "examplecrate", "1.1.0", "crates.io"),
                Some(FixRelease::Release("1.2.1".to_string())),
                "skips the prerelease and the yanked 1.2.0, and takes 1.2.1 over 1.3.0"
            );
        }

        #[tokio::test]
        async fn a_yanked_only_successor_leaves_the_crate_unpublished() {
            let registry = MockRegistry::new("crates.io")
                .with_version_meta("examplecrate", "1.1.0", None, false, false)
                .with_version_meta("examplecrate", "1.2.0", None, true, false);
            let audit = audit_of(vec![vulnerable(
                pkg("examplecrate", "1.1.0", Ecosystem::CratesIo),
                vec![vuln("RUSTSEC-1", Some("1.1.1-0"))],
            )]);

            let (releases, _) = confirm(&audit, &registry, Ecosystem::CratesIo).await;

            assert_eq!(
                release_of(&releases, "examplecrate", "1.1.0", "crates.io"),
                Some(FixRelease::Unpublished)
            );
        }

        #[tokio::test]
        async fn a_deprecated_npm_release_still_fixes() {
            // npm's listing flags deprecation where others flag a yank, and
            // a deprecated release installs.
            let registry = MockRegistry::new("npm")
                .with_version_meta("examplepkg", "1.0.0", None, true, false)
                .with_version_meta("examplepkg", "1.0.5", None, true, false);
            let audit = audit_of(vec![vulnerable(
                pkg("examplepkg", "1.0.0", Ecosystem::Npm),
                vec![vuln("GHSA-aaaa-bbbb-cccc", Some("1.0.1-0"))],
            )]);

            let (releases, _) = confirm(&audit, &registry, Ecosystem::Npm).await;

            assert_eq!(
                release_of(&releases, "examplepkg", "1.0.0", "npm"),
                Some(FixRelease::Release("1.0.5".to_string()))
            );
        }

        #[tokio::test]
        async fn the_highest_bound_across_advisories_is_the_one_confirmed() {
            let registry = MockRegistry::new("crates.io")
                .with_version_meta("examplecrate", "1.0.0", None, false, false)
                .with_version_meta("examplecrate", "1.1.0", None, false, false);
            let audit = audit_of(vec![vulnerable(
                pkg("examplecrate", "1.0.0", Ecosystem::CratesIo),
                vec![
                    vuln("RUSTSEC-1", Some("1.1.0")),
                    vuln("RUSTSEC-2", Some("1.1.1-0")),
                ],
            )]);

            let (releases, _) = confirm(&audit, &registry, Ecosystem::CratesIo).await;

            assert_eq!(
                release_of(&releases, "examplecrate", "1.0.0", "crates.io"),
                Some(FixRelease::Unpublished),
                "1.1.0 clears RUSTSEC-1 but not RUSTSEC-2"
            );
        }

        #[tokio::test]
        async fn a_failed_listing_leaves_the_bound_unconfirmed_and_says_so() {
            let registry = MockRegistry::new("crates.io").with_unavailable_versions("examplecrate");
            let audit = audit_of(vec![vulnerable(
                pkg("examplecrate", "1.0.0", Ecosystem::CratesIo),
                vec![vuln("RUSTSEC-1", Some("1.1.0"))],
            )]);

            let (releases, notes) = confirm(&audit, &registry, Ecosystem::CratesIo).await;

            assert!(releases.is_empty(), "{releases:?}");
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert!(
                notes[0].starts_with("could not list the releases of examplecrate (")
                    && notes[0].ends_with("unconfirmed"),
                "{notes:?}"
            );
        }

        #[tokio::test]
        async fn an_empty_listing_leaves_the_bound_unconfirmed_and_says_so() {
            // A package the registry lists nothing for was looked up in the
            // wrong place, not proven to have no fix.
            let registry = MockRegistry::new("crates.io");
            let audit = audit_of(vec![vulnerable(
                pkg("examplecrate", "1.0.0", Ecosystem::CratesIo),
                vec![vuln("RUSTSEC-1", Some("1.1.0"))],
            )]);

            let (releases, notes) = confirm(&audit, &registry, Ecosystem::CratesIo).await;

            assert!(releases.is_empty(), "{releases:?}");
            assert_eq!(
                notes,
                vec![
                    "crates.io lists no release of examplecrate, so its fix is written to the version its advisory names, unconfirmed"
                        .to_string()
                ]
            );
        }

        #[tokio::test]
        async fn a_capped_listing_is_never_asked() {
            // The Go proxy listing stops at the newest releases, so a fix
            // missing from it is not evidence of anything.
            let registry = MockRegistry::new("go").with_unavailable_versions("example.com/mod");
            let audit = audit_of(vec![vulnerable(
                pkg("example.com/mod", "v1.0.0", Ecosystem::Go),
                vec![vuln("GO-2026-0001", Some("1.0.1"))],
            )]);

            let (releases, notes) = confirm(&audit, &registry, Ecosystem::Go).await;

            assert!(releases.is_empty(), "{releases:?}");
            assert!(
                notes.is_empty(),
                "a listing that fails was asked: {notes:?}"
            );
        }

        #[tokio::test]
        async fn a_pair_without_a_fix_is_never_asked() {
            let registry = MockRegistry::new("crates.io").with_unavailable_versions("examplecrate");
            let audit = audit_of(vec![vulnerable(
                pkg("examplecrate", "1.0.0", Ecosystem::CratesIo),
                vec![vuln("RUSTSEC-1", Some("1.1.0")), vuln("RUSTSEC-2", None)],
            )]);

            let (releases, notes) = confirm(&audit, &registry, Ecosystem::CratesIo).await;

            assert!(releases.is_empty(), "{releases:?}");
            assert!(
                notes.is_empty(),
                "a listing that fails was asked: {notes:?}"
            );
        }
        #[tokio::test]
        async fn a_successor_inside_another_affected_branch_is_passed_over() {
            // 1.2.5 never shipped, and the 1.3 branch the advisory also
            // covers is still affected until 1.3.2.
            let registry = MockRegistry::new("crates.io")
                .with_version_meta("examplecrate", "1.2.4", None, false, false)
                .with_version_meta("examplecrate", "1.3.0", None, false, false)
                .with_version_meta("examplecrate", "1.3.1", None, false, false)
                .with_version_meta("examplecrate", "1.3.2", None, false, false);
            let audit = audit_of(vec![vulnerable(
                pkg("examplecrate", "1.2.4", Ecosystem::CratesIo),
                vec![vuln_in(
                    "RUSTSEC-1",
                    "1.2.5",
                    &[("0", Some("1.2.5")), ("1.3.0", Some("1.3.2"))],
                )],
            )]);

            let (releases, _) = confirm(&audit, &registry, Ecosystem::CratesIo).await;

            assert_eq!(
                release_of(&releases, "examplecrate", "1.2.4", "crates.io"),
                Some(FixRelease::Release("1.3.2".to_string()))
            );
        }

        #[tokio::test]
        async fn a_release_another_advisory_covers_is_passed_over() {
            let registry = MockRegistry::new("crates.io")
                .with_version_meta("examplecrate", "1.0.0", None, false, false)
                .with_version_meta("examplecrate", "1.1.0", None, false, false)
                .with_version_meta("examplecrate", "1.1.1", None, false, false);
            let audit = audit_of(vec![vulnerable(
                pkg("examplecrate", "1.0.0", Ecosystem::CratesIo),
                vec![
                    vuln_in("RUSTSEC-1", "1.1.0", &[("0", Some("1.1.0"))]),
                    vuln_in(
                        "RUSTSEC-2",
                        "1.0.1",
                        &[("0", Some("1.0.1")), ("1.1.0", Some("1.1.1"))],
                    ),
                ],
            )]);

            let (releases, _) = confirm(&audit, &registry, Ecosystem::CratesIo).await;

            assert_eq!(
                release_of(&releases, "examplecrate", "1.0.0", "crates.io"),
                Some(FixRelease::Release("1.1.1".to_string())),
                "the published bound 1.1.0 is inside RUSTSEC-2's second window"
            );
        }

        #[tokio::test]
        async fn every_release_still_affected_names_the_lowest_and_its_advisory() {
            let registry = MockRegistry::new("crates.io")
                .with_version_meta("examplecrate", "1.2.4", None, false, false)
                .with_version_meta("examplecrate", "1.4.0", None, false, false)
                .with_version_meta("examplecrate", "1.3.0", None, false, false);
            let audit = audit_of(vec![vulnerable(
                pkg("examplecrate", "1.2.4", Ecosystem::CratesIo),
                vec![vuln_in(
                    "RUSTSEC-1",
                    "1.2.5",
                    &[("0", Some("1.2.5")), ("1.3.0", None)],
                )],
            )]);

            let (releases, _) = confirm(&audit, &registry, Ecosystem::CratesIo).await;

            assert_eq!(
                release_of(&releases, "examplecrate", "1.2.4", "crates.io"),
                Some(FixRelease::StillAffected {
                    release: "1.3.0".to_string(),
                    advisory: "RUSTSEC-1".to_string(),
                })
            );
        }

        #[test]
        fn a_fix_every_release_is_still_affected_by_is_unfixable_and_says_why() {
            let audit = audit_of(vec![vulnerable(
                pkg("examplecrate", "1.2.4", Ecosystem::CratesIo),
                vec![vuln("RUSTSEC-1", Some("1.2.5"))],
            )]);
            let releases = FixReleases::from([(
                ("examplecrate".to_string(), "1.2.4".to_string(), "crates.io"),
                FixRelease::StillAffected {
                    release: "1.3.0".to_string(),
                    advisory: "RUSTSEC-1".to_string(),
                },
            )]);

            let routing = route_fix_targets(
                &audit,
                &cargo_lock_only("examplecrate", "1.2.4"),
                &HashMap::new(),
                &releases,
            );

            assert!(routing.targets.is_empty(), "{:?}", routing.targets);
            assert_eq!(routing.unfixable.len(), 1);
            assert!(routing.unfixable[0].no_fixed_version);
            assert_eq!(
                routing.unfixable[0].reason,
                "RUSTSEC-1 names 1.2.5 as fixed, but every published release of examplecrate at or above it is still affected (the lowest, 1.3.0, by RUSTSEC-1)"
            );
        }

        #[tokio::test]
        async fn a_yanked_bound_moves_to_the_next_installable_release() {
            let registry = MockRegistry::new("crates.io")
                .with_version_meta("examplecrate", "1.0.0", None, false, false)
                .with_version_meta("examplecrate", "1.1.0", None, true, false)
                .with_version_meta("examplecrate", "1.1.1", None, false, false);
            let audit = audit_of(vec![vulnerable(
                pkg("examplecrate", "1.0.0", Ecosystem::CratesIo),
                vec![vuln("RUSTSEC-1", Some("1.1.0"))],
            )]);

            let (releases, _) = confirm(&audit, &registry, Ecosystem::CratesIo).await;

            assert_eq!(
                release_of(&releases, "examplecrate", "1.0.0", "crates.io"),
                Some(FixRelease::Release("1.1.1".to_string()))
            );
        }

        #[tokio::test]
        async fn offline_asks_no_registry_and_says_so_once() {
            let registry = MockRegistry::new("PyPI")
                .with_unavailable_versions("firstpkg")
                .with_unavailable_versions("secondpkg");
            let audit = audit_of(vec![
                vulnerable(
                    pkg("firstpkg", "1.0.0", Ecosystem::PyPI),
                    vec![vuln("PYSEC-1", Some("1.1.0"))],
                ),
                vulnerable(
                    pkg("secondpkg", "1.0.0", Ecosystem::PyPI),
                    vec![vuln("PYSEC-2", Some("1.1.0"))],
                ),
            ]);

            let (releases, notes) = confirm_from(
                &audit,
                &registry,
                Ecosystem::PyPI,
                &[],
                &HashMap::new(),
                true,
            )
            .await;

            assert!(releases.is_empty(), "{releases:?}");
            assert_eq!(
                notes,
                vec![
                    "--offline asks no registry whether a fix is published, so each fix is written to the version its advisory names, unconfirmed"
                        .to_string()
                ],
                "a failing listing would add its own note if it were asked"
            );
        }

        #[tokio::test]
        async fn a_pair_locked_from_another_index_is_never_asked() {
            let registry = MockRegistry::new("PyPI").with_unavailable_versions("examplepkg");
            let audit = audit_of(vec![vulnerable(
                pkg("examplepkg", "1.0.0", Ecosystem::PyPI),
                vec![vuln("PYSEC-1", Some("1.1.0"))],
            )]);
            let locked = [
                locked_at(
                    "examplepkg",
                    "1.0.0",
                    "a/uv.lock",
                    Some("https://pypi.org/simple"),
                ),
                locked_at(
                    "examplepkg",
                    "1.0.0",
                    "b/uv.lock",
                    Some("https://packages.example.test/simple"),
                ),
            ];

            let (releases, notes) = confirm_from(
                &audit,
                &registry,
                Ecosystem::PyPI,
                &locked,
                &HashMap::new(),
                false,
            )
            .await;

            assert!(releases.is_empty(), "{releases:?}");
            assert_eq!(
                notes,
                vec![format!(
                    "{} resolves examplepkg from https://packages.example.test/simple, which is not a registry upd is configured to list releases from, so its fix is written to the version its advisory names, unconfirmed",
                    crate::path_display::display_path(Path::new("b/uv.lock"))
                )]
            );
        }

        #[tokio::test]
        async fn a_pair_locked_from_the_public_registry_is_asked() {
            let registry = MockRegistry::new("PyPI")
                .with_version_meta("examplepkg", "1.0.0", None, false, false)
                .with_version_meta("examplepkg", "1.1.0", None, false, false);
            let audit = audit_of(vec![vulnerable(
                pkg("examplepkg", "1.0.0", Ecosystem::PyPI),
                vec![vuln("PYSEC-1", Some("1.1.0"))],
            )]);
            let locked = [
                locked_at(
                    "examplepkg",
                    "1.0.0",
                    "a/uv.lock",
                    Some("https://pypi.org/simple/"),
                ),
                locked_at("examplepkg", "1.0.0", "b/poetry.lock", None),
            ];

            let (releases, notes) = confirm_from(
                &audit,
                &registry,
                Ecosystem::PyPI,
                &locked,
                &HashMap::new(),
                false,
            )
            .await;

            assert_eq!(
                release_of(&releases, "examplepkg", "1.0.0", "PyPI"),
                Some(FixRelease::Release("1.1.0".to_string()))
            );
            assert!(notes.is_empty(), "{notes:?}");
        }

        #[tokio::test]
        async fn each_pair_is_asked_through_the_source_its_lockfile_records() {
            let private = MockRegistry::new("PyPI").with_version_meta(
                "examplepkg",
                "1.0.0",
                None,
                false,
                false,
            );
            let public = MockRegistry::new("PyPI")
                .with_version_meta("examplepkg", "1.0.0", None, false, false)
                .with_version_meta("examplepkg", "1.1.0", None, false, false)
                .with_version_meta("examplepkg", "2.0.0", None, false, false)
                .with_version_meta("examplepkg", "2.1.0", None, false, false);
            let audit = audit_of(vec![
                vulnerable(
                    pkg("examplepkg", "1.0.0", Ecosystem::PyPI),
                    vec![vuln("PYSEC-1", Some("1.1.0"))],
                ),
                vulnerable(
                    pkg("examplepkg", "2.0.0", Ecosystem::PyPI),
                    vec![vuln("PYSEC-2", Some("2.1.0"))],
                ),
            ]);
            let locked = [
                locked_at(
                    "examplepkg",
                    "1.0.0",
                    "a/uv.lock",
                    Some("https://packages.example.test/simple/"),
                ),
                locked_at(
                    "examplepkg",
                    "2.0.0",
                    "b/uv.lock",
                    Some("https://pypi.org/simple"),
                ),
            ];

            let (releases, notes) = confirm_via(
                &audit,
                &[
                    ("https://pypi.org", &public),
                    ("https://packages.example.test/simple", &private),
                ],
                Ecosystem::PyPI,
                &locked,
                &HashMap::new(),
                false,
            )
            .await;

            assert_eq!(
                release_of(&releases, "examplepkg", "1.0.0", "PyPI"),
                Some(FixRelease::Unpublished),
                "the private index lists no 1.1.0"
            );
            assert_eq!(
                release_of(&releases, "examplepkg", "2.0.0", "PyPI"),
                Some(FixRelease::Release("2.1.0".to_string())),
                "the same name from another source is listed on its own"
            );
            assert!(notes.is_empty(), "{notes:?}");
        }

        #[tokio::test]
        async fn a_pair_its_lockfiles_record_from_two_sources_is_never_asked() {
            let public = MockRegistry::new("PyPI").with_unavailable_versions("examplepkg");
            let private = MockRegistry::new("PyPI").with_unavailable_versions("examplepkg");
            let audit = audit_of(vec![vulnerable(
                pkg("examplepkg", "1.0.0", Ecosystem::PyPI),
                vec![vuln("PYSEC-1", Some("1.1.0"))],
            )]);
            let locked = [
                locked_at("examplepkg", "1.0.0", "a/uv.lock", None),
                locked_at(
                    "examplepkg",
                    "1.0.0",
                    "b/uv.lock",
                    Some("https://packages.example.test/simple"),
                ),
            ];

            let (releases, notes) = confirm_via(
                &audit,
                &[
                    ("https://pypi.org", &public),
                    ("https://packages.example.test/simple", &private),
                ],
                Ecosystem::PyPI,
                &locked,
                &HashMap::new(),
                false,
            )
            .await;

            assert!(releases.is_empty(), "{releases:?}");
            assert_eq!(
                notes,
                vec![
                    "the lockfiles resolve examplepkg from more than one registry, so its fix is written to the version its advisory names, unconfirmed"
                        .to_string()
                ]
            );
        }

        #[tokio::test]
        async fn an_unlocked_pin_with_several_configured_indexes_is_never_asked() {
            let public = MockRegistry::new("PyPI").with_unavailable_versions("examplepkg");
            let private = MockRegistry::new("PyPI").with_unavailable_versions("examplepkg");
            let audit = audit_of(vec![vulnerable(
                pkg("examplepkg", "1.0.0", Ecosystem::PyPI),
                vec![vuln("PYSEC-1", Some("1.1.0"))],
            )]);

            let (releases, notes) = confirm_via(
                &audit,
                &[
                    ("https://pypi.org", &public),
                    ("https://packages.example.test/simple", &private),
                ],
                Ecosystem::PyPI,
                &[],
                &HashMap::new(),
                false,
            )
            .await;

            assert!(releases.is_empty(), "{releases:?}");
            assert_eq!(
                notes,
                vec![
                    "examplepkg may resolve from any of the 2 package indexes upd is configured with, so its fix is written to the version its advisory names, unconfirmed"
                        .to_string()
                ]
            );
        }

        #[test]
        fn a_recorded_source_matches_the_configured_source_it_names() {
            let cases: &[(Ecosystem, &str, Option<&str>, bool)] = &[
                (
                    Ecosystem::CratesIo,
                    "registry+https://github.com/rust-lang/crates.io-index",
                    Some("sparse+https://index.crates.io/"),
                    true,
                ),
                (
                    Ecosystem::CratesIo,
                    "sparse+https://index.crates.io/",
                    None,
                    true,
                ),
                (
                    Ecosystem::CratesIo,
                    "sparse+https://crates.example.test/index/",
                    Some("registry+https://github.com/rust-lang/crates.io-index"),
                    false,
                ),
                (
                    Ecosystem::CratesIo,
                    "sparse+https://crates.example.test/index/",
                    Some("sparse+https://crates.example.test/index/"),
                    true,
                ),
                (
                    Ecosystem::PyPI,
                    "https://pypi.org",
                    Some("https://PyPI.org/simple/"),
                    true,
                ),
                (Ecosystem::PyPI, "https://pypi.org", None, true),
                (
                    Ecosystem::PyPI,
                    "https://packages.example.test/simple",
                    None,
                    false,
                ),
                (
                    Ecosystem::PyPI,
                    "https://packages.example.test/simple",
                    Some("https://pypi.org/simple"),
                    false,
                ),
                (
                    Ecosystem::PyPI,
                    "https://user:secret@packages.example.test/simple",
                    Some("https://packages.example.test/simple"),
                    true,
                ),
                (
                    Ecosystem::PyPI,
                    "https://Packages.Example.test/repo/simple",
                    Some("https://packages.example.test/repo/simple/"),
                    true,
                ),
                (
                    Ecosystem::PyPI,
                    "https://packages.example.test/Repo/simple",
                    Some("https://packages.example.test/repo/simple"),
                    false,
                ),
                (
                    Ecosystem::CratesIo,
                    "sparse+https://crates.example.test/Team/index/",
                    Some("sparse+https://crates.example.test/team/index/"),
                    false,
                ),
                (
                    Ecosystem::Npm,
                    "https://user:token@npm.example.test/remote",
                    Some("https://npm.example.test/remote/x/-/x-1.0.0.tgz"),
                    true,
                ),
                (
                    Ecosystem::Npm,
                    "https://npm.example.test/Remote",
                    Some("https://npm.example.test/remote/x/-/x-1.0.0.tgz"),
                    false,
                ),
                (
                    Ecosystem::Npm,
                    "https://registry.npmjs.org/",
                    Some("https://registry.npmjs.org/left-pad/-/left-pad-1.0.0.tgz"),
                    true,
                ),
                (Ecosystem::Npm, "https://registry.npmjs.org", None, true),
                (
                    Ecosystem::Npm,
                    "https://npm.example.test/api/npm/remote",
                    Some("https://registry.npmjs.org/left-pad/-/left-pad-1.0.0.tgz"),
                    false,
                ),
                (
                    Ecosystem::Npm,
                    "https://npm.example.test/api/npm/remote/",
                    Some("https://npm.example.test/api/npm/remote/left-pad/-/left-pad-1.0.0.tgz"),
                    true,
                ),
                (
                    Ecosystem::Npm,
                    "https://npm.example.test/api/npm/remote",
                    Some("https://npm.example.test/api/npm/remote-other/x/-/x-1.0.0.tgz"),
                    false,
                ),
            ];
            for (ecosystem, configured, recorded, expected) in cases {
                assert_eq!(
                    same_source(*ecosystem, configured, *recorded),
                    *expected,
                    "{ecosystem:?} configured {configured} recorded {recorded:?}"
                );
            }
        }

        #[tokio::test]
        async fn an_unlocked_pin_whose_manifest_names_an_index_is_never_asked() {
            let dir = tempfile::tempdir().unwrap();
            let requirements = dir.path().join("requirements.txt");
            std::fs::write(
                &requirements,
                "--extra-index-url https://packages.example.test/simple\nexamplepkg==1.0.0\n",
            )
            .unwrap();
            let project = dir.path().join("svc");
            std::fs::create_dir(&project).unwrap();
            let pyproject = project.join("pyproject.toml");
            std::fs::write(
                &pyproject,
                "[project]\nname = \"svc\"\nversion = \"0.1.0\"\ndependencies = [\"otherpkg==2.0.0\"]\n\n[[tool.uv.index]]\nname = \"internal\"\nurl = \"https://packages.example.test/simple\"\n",
            )
            .unwrap();
            let registry = MockRegistry::new("PyPI")
                .with_unavailable_versions("examplepkg")
                .with_unavailable_versions("otherpkg");
            let audit = audit_of(vec![
                vulnerable(
                    pkg("examplepkg", "1.0.0", Ecosystem::PyPI),
                    vec![vuln("PYSEC-1", Some("1.1.0"))],
                ),
                vulnerable(
                    pkg("otherpkg", "2.0.0", Ecosystem::PyPI),
                    vec![vuln("PYSEC-2", Some("2.1.0"))],
                ),
            ]);
            let mut packages = HashMap::new();
            packages.insert(
                ("examplepkg".to_string(), Lang::Python),
                vec![PackageOccurrence {
                    file_path: requirements.clone(),
                    ..occ(
                        "",
                        FileType::Requirements,
                        "1.0.0",
                        None,
                        "examplepkg",
                        true,
                    )
                }],
            );
            packages.insert(
                ("otherpkg".to_string(), Lang::Python),
                vec![PackageOccurrence {
                    file_path: pyproject.clone(),
                    ..occ("", FileType::PyProject, "2.0.0", None, "otherpkg", true)
                }],
            );

            let (releases, mut notes) =
                confirm_from(&audit, &registry, Ecosystem::PyPI, &[], &packages, false).await;
            notes.sort();

            assert!(releases.is_empty(), "{releases:?}");
            let mut expected = vec![
                format!(
                    "{} declares its own package index, so its fix is written to the version its advisory names, unconfirmed",
                    crate::path_display::display_path(&requirements)
                ),
                format!(
                    "{} declares its own package index, so its fix is written to the version its advisory names, unconfirmed",
                    crate::path_display::display_path(&pyproject)
                ),
            ];
            expected.sort();
            assert_eq!(notes, expected);
        }

        #[tokio::test]
        async fn an_unlocked_pin_whose_manifest_names_no_index_is_asked() {
            let dir = tempfile::tempdir().unwrap();
            let requirements = dir.path().join("requirements.txt");
            std::fs::write(&requirements, "examplepkg==1.0.0\n").unwrap();
            let registry = MockRegistry::new("PyPI")
                .with_version_meta("examplepkg", "1.0.0", None, false, false)
                .with_version_meta("examplepkg", "1.1.0", None, false, false);
            let audit = audit_of(vec![vulnerable(
                pkg("examplepkg", "1.0.0", Ecosystem::PyPI),
                vec![vuln("PYSEC-1", Some("1.1.0"))],
            )]);
            let mut packages = HashMap::new();
            packages.insert(
                ("examplepkg".to_string(), Lang::Python),
                vec![PackageOccurrence {
                    file_path: requirements,
                    ..occ(
                        "",
                        FileType::Requirements,
                        "1.0.0",
                        None,
                        "examplepkg",
                        true,
                    )
                }],
            );

            let (releases, notes) =
                confirm_from(&audit, &registry, Ecosystem::PyPI, &[], &packages, false).await;

            assert_eq!(
                release_of(&releases, "examplepkg", "1.0.0", "PyPI"),
                Some(FixRelease::Release("1.1.0".to_string()))
            );
            assert!(notes.is_empty(), "{notes:?}");
        }
    }

    fn config_target(
        package: &str,
        key: Option<&str>,
        kind: FixKind,
        file_type: Option<FileType>,
    ) -> FixTarget {
        FixTarget {
            package: package.to_string(),
            ecosystem: Ecosystem::PyPI,
            dependency_key: key.map(str::to_string),
            from_version: "1.0.0".to_string(),
            to_version: "2.28.0".to_string(),
            vulnerable_version: "1.0.0".to_string(),
            kind,
            path: PathBuf::from("app/manifest"),
            file_type,
            lockfile: None,
            line_number: None,
            npm_form: None,
        }
    }

    fn held(targets: Vec<FixTarget>, config: &str) -> FixRouting {
        let config = std::sync::Arc::new(
            crate::config::UpdConfig::parse_with_warnings(config, "test")
                .unwrap()
                .0,
        );
        hold_configured_targets(
            FixRouting {
                targets,
                unfixable: Vec::new(),
            },
            |_| Ok(Some(std::sync::Arc::clone(&config))),
        )
        .unwrap()
    }

    #[test]
    fn a_pin_equal_to_the_fix_satisfies_it() {
        let target = config_target(
            "requests",
            None,
            FixKind::ManifestEdit,
            Some(FileType::Requirements),
        );
        let routing = held(vec![target], "[pin]\nrequests = \"2.28\"\n");
        assert_eq!(routing.targets.len(), 1, "{:?}", routing.unfixable);
        assert!(routing.unfixable.is_empty());
    }

    #[test]
    fn a_pin_below_a_floor_holds_it_in_the_floor_ecosystem() {
        // Semver orders 1.10.0 above 1.9.0; a string comparison would not.
        let mut target = config_target("lru", None, FixKind::CargoPrecise, None);
        target.to_version = "1.10.0".to_string();
        let routing = held(vec![target.clone()], "[pin]\nlru = \"1.9.0\"\n");
        assert!(routing.targets.is_empty());
        assert_eq!(
            routing.unfixable[0].reason,
            "pinned to 1.9.0 by configuration; the fix needs 1.10.0 or later"
        );
        assert_eq!(routing.unfixable[0].method, Some("cargo-precise"));
        assert!(
            !routing.unfixable[0].no_fixed_version,
            "a fix exists; configuration holds it"
        );

        let routing = held(vec![target], "[pin]\nlru = \"1.10.0\"\n");
        assert_eq!(routing.targets.len(), 1);
    }

    #[test]
    fn configuration_naming_the_manifest_key_holds_a_renamed_dependency() {
        let target = config_target(
            "tokio",
            Some("rt"),
            FixKind::ManifestEdit,
            Some(FileType::CargoToml),
        );
        let routing = held(vec![target.clone()], "ignore = [\"rt\"]\n");
        assert!(routing.targets.is_empty());
        assert_eq!(routing.unfixable[0].dependency_key.as_deref(), Some("rt"));

        let routing = held(vec![target], "ignore = [\"serde\"]\n");
        assert_eq!(
            routing.targets.len(),
            1,
            "an unrelated ignore holds nothing"
        );
    }

    #[test]
    fn a_pin_that_cannot_be_compared_holds_the_fix() {
        let target = config_target("requests", None, FixKind::ManifestEdit, None);
        let routing = held(vec![target], "[pin]\nrequests = \"9.0.0\"\n");
        assert!(
            routing.targets.is_empty(),
            "an unknown ordering never writes past a pin"
        );
    }

    /// A constraint pin names no single release, so upd cannot show that the
    /// next update keeps the fix. Operators sort above digits, which is why
    /// these are compared as constraints rather than as text.
    #[test]
    fn a_constraint_pin_holds_the_fix() {
        for (kind, file_type, pin) in [
            (
                FixKind::ManifestEdit,
                Some(FileType::Requirements),
                ">=1.0,<2",
            ),
            (FixKind::UvConstraint, None, "~=1.0"),
            (FixKind::NpmOverride, None, "<3"),
            (FixKind::CargoPrecise, None, ">=1, <2"),
            // A prerelease tag leads each of these, and what follows it is
            // still a range, not part of the tag.
            (FixKind::NpmOverride, None, "3.0.0-alpha || 1.0.0"),
            (FixKind::CargoPrecise, None, "3.0.0-rc.1, <2"),
            (
                FixKind::ManifestEdit,
                Some(FileType::PackageJson),
                "3.0.0-beta 1.0.0",
            ),
        ] {
            let target = config_target("requests", None, kind, file_type);
            let routing = held(vec![target], &format!("[pin]\nrequests = \"{pin}\"\n"));
            assert!(
                routing.targets.is_empty(),
                "{kind:?} pin {pin} must hold a fix to 2.28.0"
            );
            assert!(
                routing.unfixable[0]
                    .reason
                    .starts_with(&format!("pinned to {pin} by configuration")),
                "{}",
                routing.unfixable[0].reason
            );
        }
    }

    #[test]
    fn a_plain_pin_above_the_fix_satisfies_it_in_every_floor_ecosystem() {
        for (kind, file_type, pin) in [
            (FixKind::ManifestEdit, Some(FileType::Requirements), "2.29"),
            (FixKind::UvConstraint, None, "3.0.0"),
            (FixKind::NpmOverride, None, "2.28.1"),
            (FixKind::CargoPrecise, None, "2.28.0"),
            (FixKind::ManifestEdit, Some(FileType::GoMod), "v2.30.0"),
        ] {
            let target = config_target("requests", None, kind, file_type);
            let routing = held(vec![target], &format!("[pin]\nrequests = \"{pin}\"\n"));
            assert_eq!(
                routing.targets.len(),
                1,
                "{kind:?} pin {pin}: {:?}",
                routing.unfixable
            );
        }
    }

    /// An exact-equality pin names a single release, spelled as Python and
    /// Cargo or npm spell one, so it satisfies a fix it is at or above.
    #[test]
    fn an_exact_equality_pin_names_its_release() {
        for (kind, file_type, pin) in [
            (
                FixKind::ManifestEdit,
                Some(FileType::Requirements),
                "==2.28.0",
            ),
            (FixKind::UvConstraint, None, "== 2.29"),
            (FixKind::CargoPrecise, None, "=2.28.0"),
            (FixKind::NpmOverride, None, "=2.28.1"),
        ] {
            let target = config_target("requests", None, kind, file_type);
            let routing = held(vec![target], &format!("[pin]\nrequests = \"{pin}\"\n"));
            assert_eq!(
                routing.targets.len(),
                1,
                "{kind:?} pin {pin}: {:?}",
                routing.unfixable
            );
        }
        for (kind, pin) in [
            (FixKind::UvConstraint, "==2.27.0"),
            (FixKind::UvConstraint, "==2.28.*"),
            (FixKind::CargoPrecise, "=2.27.0"),
        ] {
            let target = config_target("requests", None, kind, None);
            let routing = held(vec![target], &format!("[pin]\nrequests = \"{pin}\"\n"));
            assert!(routing.targets.is_empty(), "{kind:?} pin {pin} must hold");
        }
    }

    #[test]
    fn a_file_without_configuration_keeps_its_targets() {
        let target = config_target(
            "requests",
            None,
            FixKind::ManifestEdit,
            Some(FileType::Requirements),
        );
        let routing = hold_configured_targets(
            FixRouting {
                targets: vec![target],
                unfixable: Vec::new(),
            },
            |_| Ok(None),
        )
        .unwrap();
        assert_eq!(routing.targets.len(), 1);
    }
}
