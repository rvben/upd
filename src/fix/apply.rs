//! Transactional application of fix targets: per-(file, lockfile) groups,
//! pre-apply snapshots covering upd's own edits AND lockfile bytes, one
//! relock per group, byte-for-byte restore on relock failure.

use crate::align::compare_versions;
use crate::fix::npm::write_npm_override_floor;
use crate::fix::uv::write_uv_constraint_floor;
use crate::fix::{FixKind, FixTarget, FloorWriteOutcome, NpmOverrideForm};
use crate::lockfile::{
    LockfileType, RegenOutcome, RestoreFailure, Snapshot, cargo_update_precise, containing_dir,
    detect_lockfiles, regenerate_lockfile, regenerate_lockfiles,
};
use crate::lockgate::{GateReport, GateStatus, RefreshGate};
use crate::lockscan::cargo::scan_cargo_lock;
use crate::lockscan::npm::scan_npm_lock;
use crate::lockscan::poetry::scan_poetry_lock;
use crate::lockscan::uv::scan_uv_lock;
use crate::normalize::pep503_normalize;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// The exact error a suppressed `$name` override reports (see
/// [`apply_edit_group`]'s companion-failure check).
const DOLLAR_NAME_SUPPRESSED_ERROR: &str = "companion manifest edit failed; not writing a $name override that would defer to an unbumped spec";

/// Guidance attached to a `CargoPrecise` target skipped under `--no-lock`.
const CARGO_PRECISE_NO_LOCK_HINT: &str =
    "cargo-precise floors only mutate Cargo.lock; rerun without --no-lock";

/// Guidance attached to a floor group's relock failure without assuming
/// that the new floor caused the resolver error.
const RELOCK_ROLLBACK_HINT: &str = "hint: resolve the lockfile error above and retry, or pass --no-lock to keep the file edits without relocking";

/// The final disposition of one [`FixTarget`] after `apply_fix_targets` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixStatus {
    /// Dry-run: this target would be written and (if applicable) relocked.
    Planned,
    /// Written (or precise-pinned) and, if a relock ran, it succeeded.
    Applied,
    /// Written, but no relock ran (`--no-lock`); the lockfile is stale.
    PendingRelock,
    /// A `CargoPrecise` target skipped entirely under `--no-lock`.
    Skipped,
    /// The writer refused; `error` carries guidance for a manual fix.
    Unfixable,
    /// Nothing needed to change; an existing entry already satisfies the
    /// floor, or the manifest spec already covers the fixed version.
    AlreadySatisfied,
    /// A write attempt itself failed (parse error, I/O error, etc.), or the
    /// group's relock failed and a file in the group could not be restored.
    /// In the second case `error` names that file and every written target
    /// in the group takes this status, because `RolledBack` would promise a
    /// restore that did not happen.
    Failed,
    /// A write succeeded but the group's relock failed; every file the
    /// group's snapshot covered was restored byte-for-byte.
    RolledBack,
    /// A `CargoPrecise` floor was rejected because another crate's own
    /// manifest requirement on the same package excludes the version cargo
    /// was asked to pin to; `error` names the constraining crate and its
    /// requirement. Not an error: other targets in the same group and run
    /// still apply, and the group is not rolled back on account of it alone.
    Blocked,
}

impl FixStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FixStatus::Planned => "planned",
            FixStatus::Applied => "applied",
            FixStatus::PendingRelock => "pending_relock",
            FixStatus::Skipped => "skipped",
            FixStatus::Unfixable => "unfixable",
            FixStatus::AlreadySatisfied => "already_satisfied",
            FixStatus::Failed => "failed",
            FixStatus::RolledBack => "rolled_back",
            FixStatus::Blocked => "blocked",
        }
    }
}

/// One target's outcome: the target it was applied for, its final status,
/// and (for non-terminal-success statuses) an explanatory message.
#[derive(Debug)]
pub struct AppliedFix {
    pub target: FixTarget,
    pub status: FixStatus,
    pub error: Option<String>,
}

/// Controls how `apply_fix_targets` writes and relocks.
#[derive(Debug, Clone)]
pub struct FixApplyOptions {
    pub dry_run: bool,
    /// Relock groups containing only manifest edits (--lock semantics;
    /// implied for `audit --fix-audit --apply` unless --no-lock).
    pub relock_manifests: bool,
    /// Relock groups containing floor targets (always on unless --no-lock:
    /// a floor without a relock is a no-op).
    pub relock_floors: bool,
    pub verbose: bool,
    /// The cooldown in force for each relock, and whether the relock keeps
    /// to it or only reports what it locks, by the path of the file a group
    /// edits. A path without an entry relocks without one.
    pub gates: HashMap<PathBuf, RefreshGate>,
}

/// What [`apply_fix_targets`] did.
#[derive(Debug, Default)]
pub struct FixApplyReport {
    /// The outcome of every target.
    pub outcomes: Vec<AppliedFix>,
    /// Stderr-destined informational notes (dry-run "would regenerate"
    /// listings, "no lockfile found" skips).
    pub notes: Vec<String>,
    /// How far each relock that ran under a cooldown kept to it.
    pub gates: Vec<GateReport>,
}

/// Applies the ManifestEdit targets for one file; returns Ok(true) when the
/// file content changed. Supplied by the caller because the per-file-type
/// edit dispatcher (apply_version_updates) lives in the binary crate.
pub type ManifestEditFn<'a> = &'a dyn Fn(
    &std::path::Path,
    crate::updater::FileType,
    &[&crate::fix::FixTarget],
) -> anyhow::Result<bool>;

/// A group of targets that share one write-then-relock transaction: either
/// every `ManifestEdit`/floor target editing the same file and completed by
/// the same lockfile (rule 1), or every `CargoPrecise` target for one
/// `Cargo.lock` (`cargo_precise` groups never mix with edit groups).
struct Group {
    path: PathBuf,
    lockfile: Option<PathBuf>,
    targets: Vec<FixTarget>,
    cargo_precise: bool,
}

/// Group targets by `(path, lockfile)`, with `CargoPrecise` targets forming
/// their own groups keyed purely by lockfile (rule 1). A `Vec`-based linear
/// scan is used (no ordered-map dependency is available) so groups keep
/// stable, first-seen order across a run.
fn group_targets(targets: Vec<FixTarget>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for target in targets {
        if target.kind == FixKind::CargoPrecise {
            let lock = target
                .lockfile
                .clone()
                .unwrap_or_else(|| target.path.clone());
            match groups
                .iter_mut()
                .find(|g| g.cargo_precise && g.lockfile.as_deref() == Some(lock.as_path()))
            {
                Some(g) => g.targets.push(target),
                None => groups.push(Group {
                    path: lock.clone(),
                    lockfile: Some(lock),
                    targets: vec![target],
                    cargo_precise: true,
                }),
            }
            continue;
        }

        match groups
            .iter_mut()
            .find(|g| !g.cargo_precise && g.path == target.path && g.lockfile == target.lockfile)
        {
            Some(g) => g.targets.push(target),
            None => groups.push(Group {
                path: target.path.clone(),
                lockfile: target.lockfile.clone(),
                targets: vec![target],
                cargo_precise: false,
            }),
        }
    }
    groups
}

/// Map a lockfile's filename to its `LockfileType`, for the lock shapes
/// `apply_fix_targets` can re-parse via a lockscan reader (rule 5).
fn lockfile_type_for(lock: &Path) -> Option<LockfileType> {
    match lock.file_name().and_then(|n| n.to_str())? {
        "uv.lock" => Some(LockfileType::UvLock),
        "poetry.lock" => Some(LockfileType::PoetryLock),
        "package-lock.json" => Some(LockfileType::PackageLockJson),
        "npm-shrinkwrap.json" => Some(LockfileType::NpmShrinkwrap),
        "Cargo.lock" => Some(LockfileType::CargoLock),
        _ => None,
    }
}

/// Re-parse the lockfile with the matching lockscan reader and report
/// whether any (normalized name, version) pair is still present. Parse
/// failures (and lock shapes with no reader) count as still-present: a
/// broken or unrecognized lock must still relock rather than be assumed
/// fixed.
fn vulnerable_still_locked(lock: &Path, pairs: &[(String, String)]) -> bool {
    if pairs.is_empty() {
        return false;
    }
    let Some(lockfile_type) = lockfile_type_for(lock) else {
        return true;
    };
    let pypi = matches!(
        lockfile_type,
        LockfileType::UvLock | LockfileType::PoetryLock
    );
    let scan = match lockfile_type {
        LockfileType::UvLock => scan_uv_lock(lock),
        LockfileType::PoetryLock => scan_poetry_lock(lock),
        LockfileType::PackageLockJson | LockfileType::NpmShrinkwrap => scan_npm_lock(lock),
        LockfileType::CargoLock => scan_cargo_lock(lock),
        _ => return true,
    };
    let Ok(scan) = scan else {
        return true;
    };
    let normalize = |name: &str| {
        if pypi {
            pep503_normalize(name)
        } else {
            name.to_lowercase()
        }
    };
    pairs.iter().any(|(name, version)| {
        let target_name = normalize(name);
        scan.packages
            .iter()
            .any(|p| normalize(&p.name) == target_name && p.version == *version)
    })
}

/// Packages a relock must explicitly target: every `Wrote` target (its
/// manifest spec just changed), plus any other target whose own vulnerable
/// pair is still locked. The second half matters for `AlreadySatisfied`
/// targets - the manifest text already covers the fix, so nothing was
/// written, but if the lock still pins the vulnerable version, a relock that
/// only names the group's `Wrote` packages (or, with none, falls back to an
/// untargeted refresh) can leave that package's stale entry untouched. Naming
/// it explicitly asks the resolver to reconsider it rather than relying on
/// the untargeted refresh to reach it incidentally.
fn changed_packages(items: &[(FixTarget, Provisional)], lockfile: Option<&Path>) -> Vec<String> {
    let mut changed: Vec<String> = Vec::new();
    for (target, prov) in items {
        let include = match prov {
            Provisional::Wrote => true,
            Provisional::AlreadySatisfied => lockfile.is_some_and(|lock| {
                vulnerable_still_locked(
                    lock,
                    &[(target.package.clone(), target.vulnerable_version.clone())],
                )
            }),
            Provisional::Unfixable(_) | Provisional::Failed(_) | Provisional::Blocked(_) => false,
        };
        if include && !changed.contains(&target.package) {
            changed.push(target.package.clone());
        }
    }
    changed
}

/// The paths a group's snapshot must cover: the edited file itself, every
/// lockfile `detect_lockfiles` maps for it, and the group's own lockfile.
fn snapshot_paths_for(path: &Path, lockfile: &Option<PathBuf>) -> Vec<PathBuf> {
    let mut paths = vec![path.to_path_buf()];
    if let Some(dir) = path.parent() {
        for lt in detect_lockfiles(path) {
            paths.push(dir.join(lt.filename()));
        }
    }
    if let Some(lock) = lockfile {
        paths.push(lock.clone());
    }
    paths
}

fn filename_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A target's disposition before the group's relock decision is known.
/// `Wrote` covers both "actually wrote" (apply mode) and "would write"
/// (dry-run); the two are resolved to different final [`FixStatus`]es by
/// the caller.
enum Provisional {
    Wrote,
    AlreadySatisfied,
    Unfixable(String),
    Failed(String),
    /// A `CargoPrecise` floor cargo refused on account of a dependent's own
    /// requirement; carries the human-readable reason (see
    /// [`classify_manifest_blocked_precise`]).
    Blocked(String),
}

/// Resolve one target's provisional outcome into its final [`AppliedFix`].
/// `wrote_status` is the status a `Wrote` target resolves to in the
/// non-rollback case (`Planned`, `PendingRelock`, or `Applied` depending on
/// which phase is finalizing).
/// Every version floor the run chose, each as the crate and the version it
/// is floored at. A floor an earlier target's update already reached counts
/// too: it is just as much a floor no hold may take the lockfile back below.
/// A `Blocked`, `Failed` or `Unfixable` target wrote nothing, so it floors
/// nothing to keep.
fn floors_to_keep(items: &[(FixTarget, Provisional)]) -> Vec<(String, String)> {
    items
        .iter()
        .filter(|(_, prov)| matches!(prov, Provisional::Wrote | Provisional::AlreadySatisfied))
        .map(|(target, _)| (target.package.clone(), target.to_version.clone()))
        .collect()
}

fn finalize(target: FixTarget, prov: Provisional, wrote_status: FixStatus) -> AppliedFix {
    match prov {
        Provisional::Wrote => AppliedFix {
            target,
            status: wrote_status,
            error: None,
        },
        Provisional::AlreadySatisfied => AppliedFix {
            target,
            status: FixStatus::AlreadySatisfied,
            error: None,
        },
        Provisional::Unfixable(error) => AppliedFix {
            target,
            status: FixStatus::Unfixable,
            error: Some(error),
        },
        Provisional::Failed(error) => AppliedFix {
            target,
            status: FixStatus::Failed,
            error: Some(error),
        },
        Provisional::Blocked(reason) => AppliedFix {
            target,
            status: FixStatus::Blocked,
            error: Some(reason),
        },
    }
}

/// Resolve one target's provisional outcome after a relock failure: targets
/// that were written or already satisfied lose that progress to the
/// restore and become `RolledBack`; targets that were already terminal
/// (`Unfixable`/`Failed`/`Blocked`) keep their own status and error (rule 6).
///
/// `RolledBack` promises that every file the snapshot covered is back at its
/// pre-run bytes. A group with a file that could not be put back is reported
/// `Failed` instead, with the file named beside the relock error.
fn finalize_rolled_back(
    target: FixTarget,
    prov: Provisional,
    message: &str,
    restore_failures: &[RestoreFailure],
) -> AppliedFix {
    match prov {
        Provisional::Wrote | Provisional::AlreadySatisfied if restore_failures.is_empty() => {
            AppliedFix {
                target,
                status: FixStatus::RolledBack,
                error: Some(message.to_string()),
            }
        }
        Provisional::Wrote | Provisional::AlreadySatisfied => {
            let mut error = message.to_string();
            for failure in restore_failures {
                error.push('\n');
                error.push_str(&failure.to_string());
            }
            AppliedFix {
                target,
                status: FixStatus::Failed,
                error: Some(error),
            }
        }
        Provisional::Unfixable(error) => AppliedFix {
            target,
            status: FixStatus::Unfixable,
            error: Some(error),
        },
        Provisional::Failed(error) => AppliedFix {
            target,
            status: FixStatus::Failed,
            error: Some(error),
        },
        Provisional::Blocked(reason) => AppliedFix {
            target,
            status: FixStatus::Blocked,
            error: Some(reason),
        },
    }
}

/// Hand one floor target to its writer. `dry_run` decides only whether the
/// file is written; every outcome, including a refusal the writer can reach
/// only by reading the manifest, is the same either way.
fn write_floor_target(target: &FixTarget, dry_run: bool) -> anyhow::Result<FloorWriteOutcome> {
    match target.kind {
        FixKind::UvConstraint => {
            write_uv_constraint_floor(&target.path, &target.package, &target.to_version, dry_run)
        }
        FixKind::NpmOverride if target.npm_form == Some(NpmOverrideForm::CompatibleRange) => {
            super::npm::write_compatible_npm_override_floor(
                &target.path,
                &target.package,
                &target.vulnerable_version,
                &target.to_version,
                dry_run,
            )
        }
        FixKind::NpmOverride => write_npm_override_floor(
            &target.path,
            &target.package,
            &target.to_version,
            target.npm_form.unwrap_or(NpmOverrideForm::Range),
            dry_run,
        ),
        FixKind::ManifestEdit | FixKind::CargoPrecise => {
            unreachable!("partitioned into manifest_targets / never grouped here")
        }
    }
}

/// What would refuse this floor target whatever the bump ceiling says, with
/// nothing written. `None` means nothing would, and the target is held back by
/// the ceiling alone.
///
/// Routing places a target without reading the manifest or consulting the apply
/// options, so a routed target is not evidence the floor can be written. The
/// manifest may already hold a uv constraint upd will not rewrite or an
/// `overrides` entry that is not an object, and `--no-lock` blocks a
/// `CargoPrecise` floor outright, since that floor mutates nothing but
/// `Cargo.lock`. A caller classifying a target it is not going to apply asks
/// here rather than inferring writability from the fact that routing produced
/// a target, and gets the same answer a dry run would reach.
///
/// A manifest already holding a floor at or above the candidate answers `None`
/// too, because the floor is not the only thing the target moves. A candidate
/// reaches a classifying caller only while the lock still sits below it, so
/// such a target needs no manifest edit and a relock all the same, which is
/// work a ceiling can hold back. Reporting it as satisfied would answer for the
/// floor and leave the lock unaccounted for.
///
/// `relock_floors` is [`FixApplyOptions::relock_floors`]. `ManifestEdit` never
/// answers here: it is dispatched through the caller's own edit function.
pub fn probe_floor_target(
    target: &FixTarget,
    relock_floors: bool,
) -> Option<(FixStatus, Option<String>)> {
    match target.kind {
        FixKind::CargoPrecise if !relock_floors => Some((
            FixStatus::Skipped,
            Some(CARGO_PRECISE_NO_LOCK_HINT.to_string()),
        )),
        FixKind::CargoPrecise | FixKind::ManifestEdit => None,
        FixKind::UvConstraint | FixKind::NpmOverride => match write_floor_target(target, true) {
            Ok(FloorWriteOutcome::Written | FloorWriteOutcome::AlreadySatisfied) => None,
            Ok(FloorWriteOutcome::Unfixable(reason)) => Some((FixStatus::Unfixable, Some(reason))),
            Err(e) => Some((FixStatus::Failed, Some(e.to_string()))),
        },
    }
}

/// Apply one non-`CargoPrecise` group: the `ManifestEdit` cluster runs
/// first, then the floor writers (uv-constraint / npm-override), then the
/// group's relock decision resolves every target's final status (rules
/// 2-6, 8).
fn apply_edit_group(
    group: Group,
    opts: &FixApplyOptions,
    apply_manifest_edits: ManifestEditFn<'_>,
    report: &mut FixApplyReport,
) {
    let FixApplyReport {
        outcomes,
        notes,
        gates,
    } = report;
    let Group {
        path,
        lockfile,
        targets,
        cargo_precise: _,
    } = group;

    let snapshot = Snapshot::capture(&snapshot_paths_for(&path, &lockfile));

    let has_floor = targets.iter().any(|t| t.kind != FixKind::ManifestEdit);
    let has_manifest_edit = targets.iter().any(|t| t.kind == FixKind::ManifestEdit);
    let relock_enabled = if has_floor {
        opts.relock_floors
    } else {
        opts.relock_manifests
    };

    let pairs: Vec<(String, String)> = targets
        .iter()
        .map(|t| (t.package.clone(), t.vulnerable_version.clone()))
        .collect();

    let (manifest_targets, floor_targets): (Vec<FixTarget>, Vec<FixTarget>) = targets
        .into_iter()
        .partition(|t| t.kind == FixKind::ManifestEdit);

    let mut items: Vec<(FixTarget, Provisional)> = Vec::new();

    // ManifestEdit cluster runs first (rule 3, bullet 1; amendment ordering).
    let mut cluster: Vec<FixTarget> = Vec::new();
    for target in manifest_targets {
        let satisfied = target.file_type.is_some_and(|ft| {
            compare_versions(&target.from_version, &target.to_version, ft.lang()) != Ordering::Less
        });
        if satisfied {
            items.push((target, Provisional::AlreadySatisfied));
        } else {
            cluster.push(target);
        }
    }

    let cluster_package_names: HashSet<String> =
        cluster.iter().map(|t| t.package.clone()).collect();
    let mut cluster_failed = false;

    if !cluster.is_empty() {
        if opts.dry_run {
            for target in cluster {
                items.push((target, Provisional::Wrote));
            }
        } else {
            match cluster.first().and_then(|t| t.file_type) {
                Some(file_type) => {
                    let refs: Vec<&FixTarget> = cluster.iter().collect();
                    match apply_manifest_edits(&path, file_type, &refs) {
                        Ok(true) => {
                            for target in cluster {
                                items.push((target, Provisional::Wrote));
                            }
                        }
                        Ok(false) => {
                            for target in cluster {
                                items.push((target, Provisional::AlreadySatisfied));
                            }
                        }
                        Err(e) => {
                            cluster_failed = true;
                            let msg = e.to_string();
                            for target in cluster {
                                items.push((target, Provisional::Failed(msg.clone())));
                            }
                        }
                    }
                }
                None => {
                    cluster_failed = true;
                    let msg = "manifest edit target is missing a file type".to_string();
                    for target in cluster {
                        items.push((target, Provisional::Failed(msg.clone())));
                    }
                }
            }
        }
    }

    // Floor writers (uv-constraint / npm-override), rule 3 bullets 2-3. A
    // failed closure writes nothing (atomic per file), so a DollarName
    // override whose companion ManifestEdit just failed must not be
    // written either: it would silently defer to the unbumped spec while
    // reporting an enforced floor.
    for target in floor_targets {
        if target.kind == FixKind::NpmOverride
            && target.npm_form == Some(NpmOverrideForm::DollarName)
            && cluster_failed
            && cluster_package_names.contains(&target.package)
        {
            items.push((
                target,
                Provisional::Failed(DOLLAR_NAME_SUPPRESSED_ERROR.to_string()),
            ));
            continue;
        }

        match write_floor_target(&target, opts.dry_run) {
            Ok(FloorWriteOutcome::Written) => items.push((target, Provisional::Wrote)),
            Ok(FloorWriteOutcome::AlreadySatisfied) => {
                items.push((target, Provisional::AlreadySatisfied))
            }
            Ok(FloorWriteOutcome::Unfixable(msg)) => {
                items.push((target, Provisional::Unfixable(msg)))
            }
            Err(e) => items.push((target, Provisional::Failed(e.to_string()))),
        }
    }

    if opts.dry_run {
        if relock_enabled {
            let any_would_write = items.iter().any(|(_, p)| matches!(p, Provisional::Wrote));
            let still_locked = lockfile
                .as_ref()
                .is_some_and(|lock| vulnerable_still_locked(lock, &pairs));
            if any_would_write || still_locked {
                if has_manifest_edit {
                    if !detect_lockfiles(&path).is_empty() {
                        notes.push(format!(
                            "{}: would regenerate lockfiles",
                            filename_of(&path)
                        ));
                    }
                } else if let Some(lock) = &lockfile {
                    notes.push(format!("{}: would regenerate", filename_of(lock)));
                }
            }
        }
        for (target, prov) in items {
            outcomes.push(finalize(target, prov, FixStatus::Planned));
        }
        return;
    }

    if !relock_enabled {
        for (target, prov) in items {
            outcomes.push(finalize(target, prov, FixStatus::PendingRelock));
        }
        return;
    }

    let any_wrote = items.iter().any(|(_, p)| matches!(p, Provisional::Wrote));
    let still_locked = lockfile
        .as_ref()
        .is_some_and(|lock| vulnerable_still_locked(lock, &pairs));
    if !any_wrote && !still_locked {
        for (target, prov) in items {
            debug_assert!(
                !matches!(prov, Provisional::Wrote),
                "relock_needed must be true whenever a target wrote"
            );
            outcomes.push(finalize(target, prov, FixStatus::Applied));
        }
        return;
    }

    let changed = changed_packages(&items, lockfile.as_deref());

    let gate = opts.gates.get(&path).copied();
    // Kept only if the group stands: a rolled-back relock leaves no refreshed
    // lockfile to check.
    let mut relock_gates: Vec<GateReport> = Vec::new();
    let relock_result: Result<(), String> = if has_manifest_edit {
        let mut result = regenerate_lockfiles(&path, &changed, gate, opts.verbose);
        relock_gates.append(&mut result.gates);
        if result.no_lockfiles {
            notes.push(format!(
                "no lockfile found for {} - skipping (nothing to regenerate)",
                filename_of(&path)
            ));
            Ok(())
        } else {
            let errors = result.error_messages();
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        }
    } else {
        match lockfile.as_ref().and_then(|l| lockfile_type_for(l)) {
            Some(lockfile_type) => {
                let relock =
                    regenerate_lockfile(&path, lockfile_type, &changed, gate, opts.verbose);
                relock_gates.extend(relock.gate);
                match relock.outcome {
                    RegenOutcome::Ok(_) => Ok(()),
                    other => Err(other
                        .error_message()
                        .unwrap_or_else(|| "relock failed".to_string())),
                }
            }
            None => Err("could not determine the lockfile type to regenerate".to_string()),
        }
    };

    match relock_result {
        Ok(()) => {
            for report in &mut relock_gates {
                if report.status == GateStatus::Exempt {
                    report.keep = floors_to_keep(&items);
                }
            }
            gates.append(&mut relock_gates);
            for (target, prov) in items {
                let recheckable =
                    matches!(prov, Provisional::Wrote | Provisional::AlreadySatisfied);
                let still_vulnerable = recheckable
                    && lockfile.as_deref().is_some_and(|lock| {
                        vulnerable_still_locked(
                            lock,
                            &[(target.package.clone(), target.vulnerable_version.clone())],
                        )
                    });
                if still_vulnerable {
                    let message = format!(
                        "relock finished but {} is still locked at {}; a manual bump or --full-precision may be required",
                        target.package, target.vulnerable_version
                    );
                    outcomes.push(finalize(
                        target,
                        Provisional::Failed(message),
                        FixStatus::Applied,
                    ));
                } else {
                    outcomes.push(finalize(target, prov, FixStatus::Applied));
                }
            }
        }
        Err(message) => {
            let restore_failures = snapshot.restore();
            let message = if has_floor {
                format!("{message}\n{RELOCK_ROLLBACK_HINT}")
            } else {
                message
            };
            for (target, prov) in items {
                outcomes.push(finalize_rolled_back(
                    target,
                    prov,
                    &message,
                    &restore_failures,
                ));
            }
        }
    }
}

/// The stable shape of cargo's rejection when a `cargo update --precise`
/// target is excluded by another crate's own requirement on the same
/// package, e.g.:
///
/// ```text
/// error: failed to select a version for the requirement `lru = "^0.16"`
/// candidate versions found which didn't match: 0.18.2
/// location searched: crates.io index
/// required by package `ratatui-core v0.1.0`
///     ... which satisfies dependency `ratatui-core = "^0.1.0"` (locked to 0.1.0) of package `ratatui v0.30.0`
/// ```
///
/// Returns a human-readable reason naming the constraining crate and its
/// requirement (e.g. `"ratatui-core requires lru ^0.16"`), or `None` when
/// `message` is some other cargo or process failure - a genuine error that
/// must still fail the target and roll back the group.
fn classify_manifest_blocked_precise(package: &str, message: &str) -> Option<String> {
    let needle = format!("failed to select a version for the requirement `{package} = \"");
    let needle_pos = message.find(&needle)?;
    let after_needle = &message[needle_pos + needle.len()..];
    let requirement = &after_needle[..after_needle.find('"')?];

    let required_by_needle = "required by package `";
    let rest = &message[needle_pos..];
    let after_required_by = &rest[rest.find(required_by_needle)? + required_by_needle.len()..];
    let constraining_crate = &after_required_by[..after_required_by.find(' ')?];

    Some(format!(
        "{constraining_crate} requires {package} {requirement}"
    ))
}

/// Apply one `CargoPrecise` group: `--no-lock` skips with guidance
/// regardless of dry-run (a dry run must preview what `--apply` would
/// actually do, and cargo-precise floors only ever mutate Cargo.lock, so
/// `--no-lock` leaves nothing for either mode to do), otherwise dry-run
/// plans and emits a "would regenerate" note (rule 8), and a real run has
/// each target self-repair (if its vulnerable pair is no longer locked) or
/// run `cargo update --precise`. A rejection cargo attributes to another
/// crate's own requirement on the package (see
/// `classify_manifest_blocked_precise`) marks only that target `Blocked`
/// and leaves the rest of the group standing; any other failure restores
/// Cargo.lock and rolls back the whole group (rule 7). A group that stands
/// under a cooldown gate reports its `Cargo.lock` for the read-back check,
/// since `--precise` can lock companion crates the cooldown never saw.
fn apply_cargo_precise_group(
    group: Group,
    opts: &FixApplyOptions,
    outcomes: &mut Vec<AppliedFix>,
    notes: &mut Vec<String>,
    gates: &mut Vec<GateReport>,
) {
    let Group {
        path,
        lockfile,
        targets,
        cargo_precise: _,
    } = group;

    if !opts.relock_floors {
        for target in targets {
            outcomes.push(AppliedFix {
                target,
                status: FixStatus::Skipped,
                error: Some(CARGO_PRECISE_NO_LOCK_HINT.to_string()),
            });
        }
        return;
    }

    if opts.dry_run {
        let lock_name = filename_of(lockfile.as_deref().unwrap_or(&path));
        notes.push(format!("{lock_name}: would regenerate"));
        for target in targets {
            outcomes.push(AppliedFix {
                target,
                status: FixStatus::Planned,
                error: None,
            });
        }
        return;
    }

    let gate = opts.gates.get(&path).copied();
    let lock = lockfile.unwrap_or(path);
    let snapshot = Snapshot::capture(std::slice::from_ref(&lock));
    let before = std::fs::read(&lock).ok();
    let lock_dir = containing_dir(&lock).to_path_buf();

    let mut items: Vec<(FixTarget, Provisional)> = Vec::new();
    for target in targets {
        let pair = [(target.package.clone(), target.vulnerable_version.clone())];
        if !vulnerable_still_locked(&lock, &pair) {
            items.push((target, Provisional::AlreadySatisfied));
            continue;
        }
        match cargo_update_precise(
            &lock_dir,
            &target.package,
            &target.vulnerable_version,
            &target.to_version,
            opts.verbose,
        ) {
            RegenOutcome::Ok(_) => items.push((target, Provisional::Wrote)),
            other => {
                let msg = other
                    .error_message()
                    .unwrap_or_else(|| "cargo update --precise failed".to_string());
                match classify_manifest_blocked_precise(&target.package, &msg) {
                    Some(reason) => items.push((target, Provisional::Blocked(reason))),
                    None => items.push((target, Provisional::Failed(msg))),
                }
            }
        }
    }

    let failure_messages: Vec<String> = items
        .iter()
        .filter_map(|(_, p)| match p {
            Provisional::Failed(msg) => Some(msg.clone()),
            _ => None,
        })
        .collect();

    if failure_messages.is_empty() {
        let keep = floors_to_keep(&items);
        if let Some(gate) = gate.filter(|_| !keep.is_empty()) {
            let status = match gate {
                RefreshGate::Keep(_) => GateStatus::Unenforced {
                    reason: "cargo has no release-age setting".to_string(),
                },
                RefreshGate::Report(_) => GateStatus::Exempt,
            };
            gates.push(GateReport {
                lockfile: lock,
                lockfile_type: LockfileType::CargoLock,
                gate: gate.gate(),
                status,
                before,
                keep,
            });
        }
        for (target, prov) in items {
            outcomes.push(finalize(target, prov, FixStatus::Applied));
        }
    } else {
        let restore_failures = snapshot.restore();
        let combined = failure_messages.join("; ");
        for (target, prov) in items {
            outcomes.push(finalize_rolled_back(
                target,
                prov,
                &combined,
                &restore_failures,
            ));
        }
    }
}

/// Applies every fix target in transactional per-`(path, lockfile)` groups.
pub fn apply_fix_targets(
    targets: Vec<FixTarget>,
    opts: &FixApplyOptions,
    apply_manifest_edits: ManifestEditFn<'_>,
) -> FixApplyReport {
    let mut report = FixApplyReport::default();
    for group in group_targets(targets) {
        if group.cargo_precise {
            apply_cargo_precise_group(
                group,
                opts,
                &mut report.outcomes,
                &mut report.notes,
                &mut report.gates,
            );
        } else {
            apply_edit_group(group, opts, apply_manifest_edits, &mut report);
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::FileType;
    use std::cell::Cell;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    const UV_LOCK_LOCKONLY: &str = "version = 1\n\n[[package]]\nname = \"lockonly\"\nversion = \"0.40.0\"\nsource = { registry = \"https://pypi.org/simple\" }\n";

    const PYPROJECT_BARE: &str =
        "[project]\nname = \"t\"\nversion = \"1.0.0\"\ndependencies = []\n";

    const PACKAGE_JSON_BARE: &str = "{\n  \"name\": \"t\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": {\n    \"examplepkg\": \"^1.0.0\"\n  }\n}\n";

    fn uv_constraint_target(package_json: PathBuf, lockfile: PathBuf) -> FixTarget {
        FixTarget {
            package: "lockonly".to_string(),
            ecosystem: crate::audit::Ecosystem::PyPI,
            dependency_key: None,
            from_version: "0.40.0".to_string(),
            to_version: "0.49.1".to_string(),
            vulnerable_version: "0.40.0".to_string(),
            kind: FixKind::UvConstraint,
            path: package_json,
            file_type: Some(FileType::PyProject),
            lockfile: Some(lockfile),
            line_number: None,
            npm_form: None,
        }
    }

    fn cargo_floor_target(package: &str, to_version: &str) -> FixTarget {
        FixTarget {
            package: package.to_string(),
            ecosystem: crate::audit::Ecosystem::CratesIo,
            dependency_key: None,
            from_version: "1.0.0".to_string(),
            to_version: to_version.to_string(),
            vulnerable_version: "1.0.0".to_string(),
            kind: FixKind::CargoPrecise,
            path: PathBuf::from("Cargo.toml"),
            file_type: None,
            lockfile: Some(PathBuf::from("Cargo.lock")),
            line_number: None,
            npm_form: None,
        }
    }

    /// A floor an earlier target's update already reached is a floor the run
    /// chose just as much as one this run wrote, so both are kept.
    #[test]
    fn a_floor_already_satisfied_is_kept_like_one_the_run_wrote() {
        let items = vec![
            (
                cargo_floor_target("clap_builder", "4.6.6"),
                Provisional::Wrote,
            ),
            (
                cargo_floor_target("anstream", "0.6.9"),
                Provisional::AlreadySatisfied,
            ),
        ];

        assert_eq!(
            floors_to_keep(&items),
            vec![
                ("clap_builder".to_string(), "4.6.6".to_string()),
                ("anstream".to_string(), "0.6.9".to_string()),
            ]
        );
    }

    fn noop_closure() -> impl Fn(&Path, FileType, &[&FixTarget]) -> anyhow::Result<bool> {
        |_, _, _| panic!("apply_manifest_edits should not be called")
    }

    /// `RolledBack` promises the group is back at its pre-run bytes. A file
    /// the restore could not put back breaks that promise, so the target is
    /// `Failed` and the error names the file beside the relock error.
    #[test]
    fn a_restore_failure_demotes_rolled_back_to_failed() {
        let target =
            uv_constraint_target(PathBuf::from("pyproject.toml"), PathBuf::from("uv.lock"));
        let failure = RestoreFailure {
            path: PathBuf::from("uv.lock"),
            reason: "permission denied".to_string(),
        };

        let fix = finalize_rolled_back(target, Provisional::Wrote, "uv lock failed", &[failure]);

        assert_eq!(fix.status, FixStatus::Failed);
        assert_eq!(
            fix.error.as_deref(),
            Some("uv lock failed\nuv.lock was not restored: permission denied")
        );
    }

    /// The control for the test above: a clean restore is `RolledBack` and
    /// carries the relock error alone.
    #[test]
    fn a_clean_restore_reports_rolled_back() {
        let target =
            uv_constraint_target(PathBuf::from("pyproject.toml"), PathBuf::from("uv.lock"));

        let fix =
            finalize_rolled_back(target, Provisional::AlreadySatisfied, "uv lock failed", &[]);

        assert_eq!(fix.status, FixStatus::RolledBack);
        assert_eq!(fix.error.as_deref(), Some("uv lock failed"));
    }

    #[test]
    fn dry_run_reports_planned_and_lists_relocks() {
        let dir = tempfile::tempdir().unwrap();
        let pyproject = write(dir.path(), "pyproject.toml", PYPROJECT_BARE);
        let uv_lock = write(dir.path(), "uv.lock", UV_LOCK_LOCKONLY);
        let before = std::fs::read_to_string(&pyproject).unwrap();

        let target = uv_constraint_target(pyproject.clone(), uv_lock);
        let opts = FixApplyOptions {
            dry_run: true,
            relock_manifests: true,
            relock_floors: true,
            verbose: false,
            gates: HashMap::new(),
        };
        let closure = noop_closure();
        let FixApplyReport {
            outcomes, notes, ..
        } = apply_fix_targets(vec![target], &opts, &closure);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, FixStatus::Planned);
        assert!(
            notes.iter().any(|n| n.contains("would regenerate")),
            "{notes:?}"
        );
        assert_eq!(std::fs::read_to_string(&pyproject).unwrap(), before);
    }

    #[test]
    fn no_lock_marks_written_floors_pending_relock() {
        let dir = tempfile::tempdir().unwrap();
        let pyproject = write(dir.path(), "pyproject.toml", PYPROJECT_BARE);
        let uv_lock = write(dir.path(), "uv.lock", UV_LOCK_LOCKONLY);

        let target = uv_constraint_target(pyproject.clone(), uv_lock);
        let opts = FixApplyOptions {
            dry_run: false,
            relock_manifests: true,
            relock_floors: false,
            verbose: false,
            gates: HashMap::new(),
        };
        let closure = noop_closure();
        let FixApplyReport { outcomes, .. } = apply_fix_targets(vec![target], &opts, &closure);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, FixStatus::PendingRelock);
        let content = std::fs::read_to_string(&pyproject).unwrap();
        assert!(
            content.contains("lockonly>=0.49.1"),
            "constraint written: {content}"
        );
    }

    #[test]
    fn no_lock_skips_cargo_precise_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let cargo_lock = write(
            dir.path(),
            "Cargo.lock",
            "# Cargo.lock placeholder\nversion = 3\n",
        );
        let before = std::fs::read_to_string(&cargo_lock).unwrap();

        let target = FixTarget {
            package: "dupcrate".to_string(),
            ecosystem: crate::audit::Ecosystem::CratesIo,
            dependency_key: None,
            from_version: "1.2.3".to_string(),
            to_version: "2.0.1".to_string(),
            vulnerable_version: "1.2.3".to_string(),
            kind: FixKind::CargoPrecise,
            path: cargo_lock.clone(),
            file_type: None,
            lockfile: Some(cargo_lock.clone()),
            line_number: None,
            npm_form: None,
        };
        let opts = FixApplyOptions {
            dry_run: false,
            relock_manifests: true,
            relock_floors: false,
            verbose: false,
            gates: HashMap::new(),
        };
        let closure = noop_closure();
        let FixApplyReport { outcomes, .. } = apply_fix_targets(vec![target], &opts, &closure);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, FixStatus::Skipped);
        let error = outcomes[0].error.as_ref().expect("error present");
        assert!(error.contains("rerun without --no-lock"), "{error}");
        assert_eq!(std::fs::read_to_string(&cargo_lock).unwrap(), before);
    }

    /// Sibling of `no_lock_skips_cargo_precise_with_guidance`: `--no-lock`
    /// must skip a `CargoPrecise` target under dry-run too, not just under
    /// `--apply`. A dry run previews what `--apply` would actually do, so it
    /// must not report `Planned` (and a "would regenerate" note) for a
    /// target that `--apply` would then turn around and skip.
    #[test]
    fn no_lock_skips_cargo_precise_with_guidance_in_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        let cargo_lock = write(
            dir.path(),
            "Cargo.lock",
            "# Cargo.lock placeholder\nversion = 3\n",
        );
        let before = std::fs::read_to_string(&cargo_lock).unwrap();

        let target = FixTarget {
            package: "dupcrate".to_string(),
            ecosystem: crate::audit::Ecosystem::CratesIo,
            dependency_key: None,
            from_version: "1.2.3".to_string(),
            to_version: "2.0.1".to_string(),
            vulnerable_version: "1.2.3".to_string(),
            kind: FixKind::CargoPrecise,
            path: cargo_lock.clone(),
            file_type: None,
            lockfile: Some(cargo_lock.clone()),
            line_number: None,
            npm_form: None,
        };
        let opts = FixApplyOptions {
            dry_run: true,
            relock_manifests: true,
            relock_floors: false,
            verbose: false,
            gates: HashMap::new(),
        };
        let closure = noop_closure();
        let FixApplyReport {
            outcomes, notes, ..
        } = apply_fix_targets(vec![target], &opts, &closure);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, FixStatus::Skipped);
        let error = outcomes[0].error.as_ref().expect("error present");
        assert!(error.contains("rerun without --no-lock"), "{error}");
        assert!(
            !notes.iter().any(|n| n.contains("would regenerate")),
            "{notes:?}"
        );
        assert_eq!(std::fs::read_to_string(&cargo_lock).unwrap(), before);
    }

    #[test]
    fn manifest_edit_at_or_above_target_is_already_satisfied() {
        let dir = tempfile::tempdir().unwrap();
        let package_json = write(dir.path(), "package.json", PACKAGE_JSON_BARE);

        let called = Cell::new(false);
        let closure = |_: &Path, _: FileType, _: &[&FixTarget]| -> anyhow::Result<bool> {
            called.set(true);
            Ok(true)
        };

        let target = FixTarget {
            package: "examplepkg".to_string(),
            ecosystem: crate::audit::Ecosystem::Npm,
            dependency_key: None,
            from_version: "2.5.0".to_string(),
            to_version: "2.5.0".to_string(),
            vulnerable_version: "1.0.0".to_string(),
            kind: FixKind::ManifestEdit,
            path: package_json,
            file_type: Some(FileType::PackageJson),
            lockfile: None,
            line_number: Some(4),
            npm_form: None,
        };
        let opts = FixApplyOptions {
            dry_run: false,
            relock_manifests: false,
            relock_floors: false,
            verbose: false,
            gates: HashMap::new(),
        };
        let FixApplyReport { outcomes, .. } = apply_fix_targets(vec![target], &opts, &closure);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, FixStatus::AlreadySatisfied);
        assert!(outcomes[0].error.is_none());
        assert!(!called.get(), "closure must not be invoked");
    }

    #[test]
    fn writer_unfixable_flows_through_with_error() {
        let dir = tempfile::tempdir().unwrap();
        let package_json = write(
            dir.path(),
            "package.json",
            "{\n  \"overrides\": {\n    \"examplepkg\": { \".\": \">=1.0.0\" }\n  }\n}\n",
        );

        let target = FixTarget {
            package: "examplepkg".to_string(),
            ecosystem: crate::audit::Ecosystem::Npm,
            dependency_key: None,
            from_version: "1.2.0".to_string(),
            to_version: "1.5.0".to_string(),
            vulnerable_version: "1.2.0".to_string(),
            kind: FixKind::NpmOverride,
            path: package_json,
            file_type: Some(FileType::PackageJson),
            lockfile: None,
            line_number: None,
            npm_form: Some(NpmOverrideForm::Range),
        };
        let opts = FixApplyOptions {
            dry_run: false,
            relock_manifests: false,
            relock_floors: false,
            verbose: false,
            gates: HashMap::new(),
        };
        let closure = noop_closure();
        let FixApplyReport { outcomes, .. } = apply_fix_targets(vec![target], &opts, &closure);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, FixStatus::Unfixable);
        let error = outcomes[0].error.as_ref().expect("error present");
        assert!(error.contains("existing override"), "{error}");
    }

    #[test]
    fn groups_share_one_relock_key() {
        let dir = tempfile::tempdir().unwrap();
        let package_json = write(dir.path(), "package.json", PACKAGE_JSON_BARE);
        let package_lock = write(dir.path(), "package-lock.json", "{}\n");

        let override_target = FixTarget {
            package: "examplepkg".to_string(),
            ecosystem: crate::audit::Ecosystem::Npm,
            dependency_key: None,
            from_version: "1.2.0".to_string(),
            to_version: "1.5.0".to_string(),
            vulnerable_version: "1.2.0".to_string(),
            kind: FixKind::NpmOverride,
            path: package_json.clone(),
            file_type: Some(FileType::PackageJson),
            lockfile: Some(package_lock.clone()),
            line_number: None,
            npm_form: Some(NpmOverrideForm::DollarName),
        };
        let manifest_target = FixTarget {
            package: "examplepkg".to_string(),
            ecosystem: crate::audit::Ecosystem::Npm,
            dependency_key: None,
            from_version: "1.0.0".to_string(),
            to_version: "1.5.0".to_string(),
            vulnerable_version: "1.2.0".to_string(),
            kind: FixKind::ManifestEdit,
            path: package_json.clone(),
            file_type: Some(FileType::PackageJson),
            lockfile: Some(package_lock),
            line_number: Some(4),
            npm_form: None,
        };
        let opts = FixApplyOptions {
            dry_run: true,
            relock_manifests: true,
            relock_floors: true,
            verbose: false,
            gates: HashMap::new(),
        };
        let closure = noop_closure();
        let FixApplyReport {
            outcomes, notes, ..
        } = apply_fix_targets(vec![override_target, manifest_target], &opts, &closure);

        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|o| o.status == FixStatus::Planned));
        let relock_notes: Vec<&String> = notes
            .iter()
            .filter(|n| n.contains("would regenerate"))
            .collect();
        assert_eq!(relock_notes.len(), 1, "{notes:?}");
    }

    #[test]
    fn vulnerable_still_locked_detects_stale_lock() {
        let dir = tempfile::tempdir().unwrap();
        let uv_lock = write(dir.path(), "uv.lock", UV_LOCK_LOCKONLY);

        assert!(vulnerable_still_locked(
            &uv_lock,
            &[("lockonly".to_string(), "0.40.0".to_string())]
        ));
        assert!(!vulnerable_still_locked(
            &uv_lock,
            &[("lockonly".to_string(), "0.49.1".to_string())]
        ));
    }

    const CARGO_LOCK_TWO_PACKAGES: &str = "version = 3\n\n[[package]]\nname = \"alpha\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[[package]]\nname = \"beta\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";

    fn manifest_edit_target(
        package: &str,
        from_version: &str,
        to_version: &str,
        vulnerable_version: &str,
        path: PathBuf,
    ) -> FixTarget {
        FixTarget {
            package: package.to_string(),
            ecosystem: crate::audit::Ecosystem::CratesIo,
            dependency_key: None,
            from_version: from_version.to_string(),
            to_version: to_version.to_string(),
            vulnerable_version: vulnerable_version.to_string(),
            kind: FixKind::ManifestEdit,
            path,
            file_type: Some(FileType::CargoToml),
            lockfile: None,
            line_number: Some(4),
            npm_form: None,
        }
    }

    #[test]
    fn changed_packages_includes_still_locked_already_satisfied_target() {
        let dir = tempfile::tempdir().unwrap();
        let cargo_lock = write(dir.path(), "Cargo.lock", CARGO_LOCK_TWO_PACKAGES);

        // alpha: manifest already covers the fix (AlreadySatisfied), but its
        // lock entry is still the vulnerable one - a relock must name it.
        let alpha = manifest_edit_target("alpha", "1.5.0", "1.5.0", "1.0.0", cargo_lock.clone());
        // beta: manifest needed a real bump (Wrote), always included.
        let beta = manifest_edit_target("beta", "1.0.0", "1.5.0", "1.0.0", cargo_lock.clone());

        let items = vec![
            (alpha, Provisional::AlreadySatisfied),
            (beta, Provisional::Wrote),
        ];

        let changed = changed_packages(&items, Some(&cargo_lock));

        assert_eq!(changed, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn changed_packages_omits_already_satisfied_target_once_lock_catches_up() {
        let dir = tempfile::tempdir().unwrap();
        let cargo_lock = write(
            dir.path(),
            "Cargo.lock",
            "version = 3\n\n[[package]]\nname = \"alpha\"\nversion = \"1.5.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
        );
        let alpha = manifest_edit_target("alpha", "1.5.0", "1.5.0", "1.0.0", cargo_lock.clone());
        let items = vec![(alpha, Provisional::AlreadySatisfied)];

        let changed = changed_packages(&items, Some(&cargo_lock));

        assert!(changed.is_empty(), "{changed:?}");
    }

    #[test]
    fn relock_success_does_not_hide_a_still_vulnerable_lock() {
        // package.json lives alone, with no sibling package-lock.json, so
        // `detect_lockfiles` finds nothing and the relock takes the
        // no-lockfiles shortcut (`Ok(())`) without running any external
        // tool. The target's own `lockfile` still points at a real fixture
        // (as it would when routing found the lock elsewhere), so the
        // post-relock check reads real, deliberately stale content.
        let manifest_dir = tempfile::tempdir().unwrap();
        let package_json = write(manifest_dir.path(), "package.json", PACKAGE_JSON_BARE);

        let lock_dir = tempfile::tempdir().unwrap();
        let package_lock = write(
            lock_dir.path(),
            "package-lock.json",
            "{\n  \"packages\": {\n    \"node_modules/examplepkg\": { \"version\": \"1.0.0\" }\n  }\n}\n",
        );

        let target = FixTarget {
            package: "examplepkg".to_string(),
            ecosystem: crate::audit::Ecosystem::Npm,
            dependency_key: None,
            from_version: "1.0.0".to_string(),
            to_version: "1.5.0".to_string(),
            vulnerable_version: "1.0.0".to_string(),
            kind: FixKind::ManifestEdit,
            path: package_json,
            file_type: Some(FileType::PackageJson),
            lockfile: Some(package_lock.clone()),
            line_number: Some(4),
            npm_form: None,
        };
        let opts = FixApplyOptions {
            dry_run: false,
            relock_manifests: true,
            relock_floors: true,
            verbose: false,
            gates: HashMap::new(),
        };
        let closure =
            |_: &Path, _: FileType, _: &[&FixTarget]| -> anyhow::Result<bool> { Ok(true) };
        let FixApplyReport { outcomes, .. } = apply_fix_targets(vec![target], &opts, &closure);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, FixStatus::Failed);
        let error = outcomes[0].error.as_ref().expect("error present");
        assert!(error.contains("examplepkg"), "{error}");
        assert!(error.contains("still locked"), "{error}");
        // The lock fixture itself is untouched by the shortcut relock.
        assert!(
            std::fs::read_to_string(&package_lock)
                .unwrap()
                .contains("1.0.0")
        );
    }

    #[test]
    fn dollar_name_override_suppressed_when_companion_edit_fails() {
        let dir = tempfile::tempdir().unwrap();
        let package_json = write(dir.path(), "package.json", PACKAGE_JSON_BARE);
        let before = std::fs::read_to_string(&package_json).unwrap();

        let closure = |_: &Path, _: FileType, _: &[&FixTarget]| -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("simulated closure failure"))
        };

        let manifest_target = FixTarget {
            package: "examplepkg".to_string(),
            ecosystem: crate::audit::Ecosystem::Npm,
            dependency_key: None,
            from_version: "1.0.0".to_string(),
            to_version: "1.5.0".to_string(),
            vulnerable_version: "1.2.0".to_string(),
            kind: FixKind::ManifestEdit,
            path: package_json.clone(),
            file_type: Some(FileType::PackageJson),
            lockfile: None,
            line_number: Some(4),
            npm_form: None,
        };
        let override_target = FixTarget {
            package: "examplepkg".to_string(),
            ecosystem: crate::audit::Ecosystem::Npm,
            dependency_key: None,
            from_version: "1.2.0".to_string(),
            to_version: "1.5.0".to_string(),
            vulnerable_version: "1.2.0".to_string(),
            kind: FixKind::NpmOverride,
            path: package_json.clone(),
            file_type: Some(FileType::PackageJson),
            lockfile: None,
            line_number: None,
            npm_form: Some(NpmOverrideForm::DollarName),
        };
        let opts = FixApplyOptions {
            dry_run: false,
            relock_manifests: false,
            relock_floors: false,
            verbose: false,
            gates: HashMap::new(),
        };
        let FixApplyReport {
            outcomes, notes, ..
        } = apply_fix_targets(vec![manifest_target, override_target], &opts, &closure);

        assert_eq!(outcomes.len(), 2);
        assert_eq!(std::fs::read_to_string(&package_json).unwrap(), before);
        assert!(notes.is_empty(), "{notes:?}");

        let manifest_outcome = outcomes
            .iter()
            .find(|o| o.target.kind == FixKind::ManifestEdit)
            .expect("manifest outcome present");
        assert_eq!(manifest_outcome.status, FixStatus::Failed);
        assert!(
            manifest_outcome
                .error
                .as_ref()
                .unwrap()
                .contains("simulated closure failure")
        );

        let override_outcome = outcomes
            .iter()
            .find(|o| o.target.kind == FixKind::NpmOverride)
            .expect("override outcome present");
        assert_eq!(override_outcome.status, FixStatus::Failed);
        assert_eq!(
            override_outcome.error.as_ref().unwrap(),
            DOLLAR_NAME_SUPPRESSED_ERROR
        );
    }

    #[test]
    fn cargo_precise_dry_run_emits_would_regenerate_note() {
        let dir = tempfile::tempdir().unwrap();
        let cargo_lock = write(
            dir.path(),
            "Cargo.lock",
            "# Cargo.lock placeholder\nversion = 3\n",
        );
        let before = std::fs::read_to_string(&cargo_lock).unwrap();

        let target = FixTarget {
            package: "dupcrate".to_string(),
            ecosystem: crate::audit::Ecosystem::CratesIo,
            dependency_key: None,
            from_version: "1.2.3".to_string(),
            to_version: "2.0.1".to_string(),
            vulnerable_version: "1.2.3".to_string(),
            kind: FixKind::CargoPrecise,
            path: cargo_lock.clone(),
            file_type: None,
            lockfile: Some(cargo_lock.clone()),
            line_number: None,
            npm_form: None,
        };
        let opts = FixApplyOptions {
            dry_run: true,
            relock_manifests: true,
            relock_floors: true,
            verbose: false,
            gates: HashMap::new(),
        };
        let closure = noop_closure();
        let FixApplyReport {
            outcomes, notes, ..
        } = apply_fix_targets(vec![target], &opts, &closure);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, FixStatus::Planned);
        assert!(outcomes[0].error.is_none());
        assert_eq!(notes, vec!["Cargo.lock: would regenerate".to_string()]);
        assert_eq!(std::fs::read_to_string(&cargo_lock).unwrap(), before);
    }

    /// The exact cargo rejection text captured from a real `unifi-cli`
    /// repro: `ratatui-core` pins `lru` to `^0.16`, so a `--precise 0.18.2`
    /// floor is refused. The reason names both the constraining crate and
    /// its own requirement, not the version this run tried to pin to.
    #[test]
    fn classify_manifest_blocked_precise_names_the_constraining_crate() {
        let message = "Failed to run `cargo update -p lru@0.16.4 --precise 0.18.2`: Updating crates.io index\nerror: failed to select a version for the requirement `lru = \"^0.16\"`\ncandidate versions found which didn't match: 0.18.2\nlocation searched: crates.io index\nrequired by package `ratatui-core v0.1.0`\n    ... which satisfies dependency `ratatui-core = \"^0.1.0\"` (locked to 0.1.0) of package `ratatui v0.30.0`\n    ... which satisfies dependency `ratatui = \"^0.30\"` (locked to 0.30.0) of package `unifi-cli v0.4.4 (/private/tmp/repro/unifi-cli)`";

        assert_eq!(
            classify_manifest_blocked_precise("lru", message),
            Some("ratatui-core requires lru ^0.16".to_string())
        );
    }

    /// The control for the test above: an unrelated cargo failure (here, a
    /// plain network error) must not be misread as a manifest block, since
    /// that would demote a genuine error into `Blocked` and skip the group
    /// rollback it needs.
    #[test]
    fn classify_manifest_blocked_precise_returns_none_for_unrelated_cargo_error() {
        let message = "Failed to run `cargo update -p lru@0.16.4 --precise 0.18.2`: Updating crates.io index\nerror: failed to fetch `https://github.com/rust-lang/crates.io-index`\n\nCaused by:\n  network failure seems to have happened";

        assert_eq!(classify_manifest_blocked_precise("lru", message), None);
    }

    /// A `Blocked`, `Failed` or `Unfixable` floor wrote nothing, so unlike
    /// `Wrote`/`AlreadySatisfied` it must not appear among the floors a
    /// later cooldown re-check treats as chosen by this run: its name would
    /// hide a young release the relock locked for it.
    #[test]
    fn a_floor_that_wrote_nothing_is_not_kept() {
        let items = vec![
            (
                cargo_floor_target("clap_builder", "4.6.6"),
                Provisional::Wrote,
            ),
            (
                cargo_floor_target("lru", "0.18.2"),
                Provisional::Blocked("ratatui-core requires lru ^0.16".to_string()),
            ),
            (
                cargo_floor_target("serde", "1.0.200"),
                Provisional::Failed("write failed".to_string()),
            ),
            (
                cargo_floor_target("time", "0.3.47"),
                Provisional::Unfixable("no override form fits".to_string()),
            ),
        ];

        assert_eq!(
            floors_to_keep(&items),
            vec![("clap_builder".to_string(), "4.6.6".to_string())]
        );
    }

    /// `finalize` (the no-rollback path) reports a `Blocked` target as
    /// `FixStatus::Blocked` with the constraining-crate reason as its error,
    /// not folded into `Failed`.
    #[test]
    fn finalize_reports_blocked_status_with_reason() {
        let target = cargo_floor_target("lru", "0.18.2");
        let fix = finalize(
            target,
            Provisional::Blocked("ratatui-core requires lru ^0.16".to_string()),
            FixStatus::Applied,
        );

        assert_eq!(fix.status, FixStatus::Blocked);
        assert_eq!(
            fix.error.as_deref(),
            Some("ratatui-core requires lru ^0.16")
        );
    }

    /// `finalize_rolled_back` runs when some OTHER target in the group hit a
    /// genuine failure and the group is being rolled back. A `Blocked`
    /// target was already terminal before the rollback decision, so it must
    /// keep reporting `Blocked` with its own reason rather than being swept
    /// into `RolledBack` alongside the targets that actually wrote files.
    #[test]
    fn finalize_rolled_back_keeps_blocked_target_blocked() {
        let target = cargo_floor_target("lru", "0.18.2");
        let fix = finalize_rolled_back(
            target,
            Provisional::Blocked("ratatui-core requires lru ^0.16".to_string()),
            "cargo update --precise failed for another target in the group",
            &[],
        );

        assert_eq!(fix.status, FixStatus::Blocked);
        assert_eq!(
            fix.error.as_deref(),
            Some("ratatui-core requires lru ^0.16")
        );
    }
}
