//! Explicit per-target fix actions: routing vulnerable (name, version) pairs
//! into manifest edits and version floors. Writers live in uv/npm;
//! transactional application in apply.

pub mod apply;
pub mod npm;
pub mod uv;

use crate::align::PackageOccurrence;
use crate::audit::{AuditResult, Ecosystem, Package, manifest_fix_version};
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
    Range,
    DollarName,
}

/// A single concrete fix action: bump one manifest entry, or write one
/// version floor.
#[derive(Debug, Clone)]
pub struct FixTarget {
    pub package: String,
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
                package: pkg.name.clone(),
                dependency_key: Some(owner.dependency_key.clone()),
                from_version: pkg.version.clone(),
                to_version: Some(to_version.to_string()),
                method: Some(FixKind::ManifestEdit.method()),
                path: Some(owner.manifest.clone()),
                reason: format!(
                    "declared as npm alias \"{}\" in {}; upd cannot rewrite alias specs - update it manually to \"{}\": \"npm:{}@>={}\"",
                    owner.dependency_key,
                    owner.manifest.display(),
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
            dependency_key: dep_key,
            from_version: pkg.version.clone(),
            to_version: Some(to_version.to_string()),
            method: Some(FixKind::ManifestEdit.method()),
            path: Some(owner.manifest.clone()),
            reason: format!(
                "could not re-derive the manifest requirement for \"{}\" in {}",
                owner.dependency_key,
                owner.manifest.display()
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
        LockKind::Poetry => {
            sink.unfixable.push(UnfixableTarget {
                package: pkg.name.clone(),
                dependency_key: None,
                from_version: pkg.version.clone(),
                to_version: Some(to_version.to_string()),
                method: None,
                path: Some(lockfile.to_path_buf()),
                reason: format!(
                    "no floor mechanism exists for poetry.lock; add {}>={} as a direct dependency",
                    pkg.name, to_version
                ),
                no_fixed_version: false,
            });
        }
        LockKind::Cargo => {
            sink.cargo_targets.push(FixTarget {
                package: pkg.name.clone(),
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

    let matching_direct = prov.npm_direct.get(&host).and_then(|deps| {
        deps.iter()
            .find(|d| normalized_name(&d.package, pkg.ecosystem) == norm)
    });

    match matching_direct {
        None => {
            sink.floor_targets.push(FixTarget {
                package: pkg.name.clone(),
                dependency_key: None,
                from_version: pkg.version.clone(),
                to_version: to_version.to_string(),
                vulnerable_version: pkg.version.clone(),
                kind: FixKind::NpmOverride,
                path: host,
                file_type: Some(FileType::PackageJson),
                lockfile: Some(lockfile.to_path_buf()),
                line_number: None,
                npm_form: Some(NpmOverrideForm::Range),
            });
        }
        Some(d) if d.spec.starts_with("npm:") => {
            sink.unfixable.push(UnfixableTarget {
                package: pkg.name.clone(),
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
                        package: pkg.name.clone(),
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
    (target.kind.method(), target.path.clone(), norm)
}

/// uv/npm floor targets merge by `(kind, path, normalized package)`: keep
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

/// Route every vulnerable (name, version) pair in `audit` into explicit
/// manifest-edit and version-floor targets, or into `unfixable` with a
/// human-readable reason. A pair with no fix at all is unfixable; a
/// Manifest-covered pair
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
) -> FixRouting {
    let mut sink = Sink::default();

    for pkg_result in &audit.vulnerable {
        let pkg = &pkg_result.package;

        let blocker = pkg_result
            .vulnerabilities
            .iter()
            .find(|v| v.fixed_version.is_none());
        if let Some(v) = blocker {
            sink.unfixable.push(UnfixableTarget {
                package: pkg.name.clone(),
                dependency_key: None,
                from_version: pkg.version.clone(),
                to_version: None,
                method: None,
                path: None,
                reason: format!("{} has no fixed version", v.id),
                no_fixed_version: true,
            });
            continue;
        }

        let Some(fixed) = pkg_result
            .vulnerabilities
            .iter()
            .filter_map(|v| v.fixed_version.as_deref())
            .max_by(|a, b| compare_versions(a, b))
        else {
            continue;
        };
        let to_version = manifest_fix_version(pkg, fixed);

        let norm = normalized_name(&pkg.name, pkg.ecosystem);
        let pair_key = (norm, pkg.version.clone(), pkg.ecosystem.as_str());

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
            if crate::align::compare_versions(pinned, locked, lang) == Ordering::Greater {
                FloorResolution::Floor(pinned.to_string())
            } else {
                FloorResolution::NotNeeded
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
    if !options.allows_bump(locked, &candidate) {
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
    fn no_fixed_version_pair_is_unfixable_with_flag() {
        let audit = audit_of(vec![vulnerable(
            pkg("examplepkg", "1.0.0", Ecosystem::PyPI),
            vec![vuln("GHSA-aaaa-bbbb-cccc", None)],
        )]);
        let prov = ProvenanceIndex::default();
        let packages = HashMap::new();

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

        assert!(routing.unfixable.is_empty());
        assert_eq!(routing.targets.len(), 1);
        let t = &routing.targets[0];
        assert_eq!(t.kind, FixKind::NpmOverride);
        assert_eq!(t.npm_form, Some(NpmOverrideForm::Range));
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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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
                pkg("examplepkg", "1.2.0", Ecosystem::Npm),
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
                    ("examplepkg", "1.2.0", "npm"),
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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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

        let routing = route_fix_targets(&audit, &prov, &packages);

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
}
