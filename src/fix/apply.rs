//! Transactional application of fix targets: per-(file, lockfile) groups,
//! pre-apply snapshots covering upd's own edits AND lockfile bytes, one
//! relock per group, byte-for-byte restore on relock failure.

use crate::align::compare_versions;
use crate::fix::npm::write_npm_override_floor;
use crate::fix::uv::write_uv_constraint_floor;
use crate::fix::{FixKind, FixTarget, FloorWriteOutcome, NpmOverrideForm};
use crate::lockfile::{
    LockfileType, RegenOutcome, cargo_update_precise, detect_lockfiles, regenerate_lockfile,
    regenerate_lockfiles,
};
use crate::lockscan::cargo::scan_cargo_lock;
use crate::lockscan::npm::scan_npm_lock;
use crate::lockscan::poetry::scan_poetry_lock;
use crate::lockscan::uv::scan_uv_lock;
use crate::normalize::pep503_normalize;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The exact error a suppressed `$name` override reports (see
/// [`apply_edit_group`]'s companion-failure check).
const DOLLAR_NAME_SUPPRESSED_ERROR: &str = "companion manifest edit failed; not writing a $name override that would defer to an unbumped spec";

/// Guidance attached to a `CargoPrecise` target skipped under `--no-lock`.
const CARGO_PRECISE_NO_LOCK_HINT: &str =
    "cargo-precise floors only mutate Cargo.lock; rerun without --no-lock";

/// Appended to a floor group's relock-failure error: the resolver's stderr
/// alone does not tell the user the fix is *why* the direct dependency
/// still needs attention.
const RELOCK_ROLLBACK_HINT: &str = "hint: a direct dependency may pin this transitive below the floor; update the direct dependency first, or pass --no-lock to keep the file edits without relocking";

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
    /// A write attempt itself failed (parse error, I/O error, etc.).
    Failed,
    /// A write succeeded but the group's relock failed; every file the
    /// group's snapshot covered was restored byte-for-byte.
    RolledBack,
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
#[derive(Debug, Clone, Copy)]
pub struct FixApplyOptions {
    pub dry_run: bool,
    /// Relock groups containing only manifest edits (--lock semantics;
    /// implied for `audit --fix-audit --apply` unless --no-lock).
    pub relock_manifests: bool,
    /// Relock groups containing floor targets (always on unless --no-lock:
    /// a floor without a relock is a no-op).
    pub relock_floors: bool,
    pub verbose: bool,
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

/// A pre-write byte capture of every file a group's transaction may touch.
/// `None` marks a path that did not exist yet, so `restore` deletes it
/// rather than writing empty bytes (rule 2).
struct Snapshot {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl Snapshot {
    fn capture(paths: &[PathBuf]) -> Self {
        let mut seen = HashSet::new();
        let mut files = Vec::new();
        for path in paths {
            if !seen.insert(path.clone()) {
                continue;
            }
            files.push((path.clone(), std::fs::read(path).ok()));
        }
        Snapshot { files }
    }

    fn restore(&self) {
        for (path, bytes) in &self.files {
            match bytes {
                Some(bytes) => {
                    let _ = std::fs::write(path, bytes);
                }
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
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
}

/// Resolve one target's provisional outcome into its final [`AppliedFix`].
/// `wrote_status` is the status a `Wrote` target resolves to in the
/// non-rollback case (`Planned`, `PendingRelock`, or `Applied` depending on
/// which phase is finalizing).
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
    }
}

/// Resolve one target's provisional outcome after a relock failure: targets
/// that were written or already satisfied lose that progress to the
/// restore and become `RolledBack`; targets that were already terminal
/// (`Unfixable`/`Failed`) keep their own status and error (rule 6).
fn finalize_rolled_back(target: FixTarget, prov: Provisional, message: &str) -> AppliedFix {
    match prov {
        Provisional::Wrote | Provisional::AlreadySatisfied => AppliedFix {
            target,
            status: FixStatus::RolledBack,
            error: Some(message.to_string()),
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
    outcomes: &mut Vec<AppliedFix>,
    notes: &mut Vec<String>,
) {
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

        let result = match target.kind {
            FixKind::UvConstraint => {
                write_uv_constraint_floor(&path, &target.package, &target.to_version, opts.dry_run)
            }
            FixKind::NpmOverride => write_npm_override_floor(
                &path,
                &target.package,
                &target.to_version,
                target.npm_form.unwrap_or(NpmOverrideForm::Range),
                opts.dry_run,
            ),
            FixKind::ManifestEdit | FixKind::CargoPrecise => {
                unreachable!("partitioned into manifest_targets / never grouped here")
            }
        };
        match result {
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

    let mut changed: Vec<String> = Vec::new();
    for (target, prov) in &items {
        if matches!(prov, Provisional::Wrote) && !changed.contains(&target.package) {
            changed.push(target.package.clone());
        }
    }

    let relock_result: Result<(), String> = if has_manifest_edit {
        let result = regenerate_lockfiles(&path, &changed, opts.verbose);
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
                match regenerate_lockfile(&path, lockfile_type, &changed, opts.verbose) {
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
            for (target, prov) in items {
                outcomes.push(finalize(target, prov, FixStatus::Applied));
            }
        }
        Err(message) => {
            snapshot.restore();
            let message = if has_floor {
                format!("{message}\n{RELOCK_ROLLBACK_HINT}")
            } else {
                message
            };
            for (target, prov) in items {
                outcomes.push(finalize_rolled_back(target, prov, &message));
            }
        }
    }
}

/// Apply one `CargoPrecise` group: `--no-lock` skips with guidance
/// regardless of dry-run (a dry run must preview what `--apply` would
/// actually do, and cargo-precise floors only ever mutate Cargo.lock, so
/// `--no-lock` leaves nothing for either mode to do), otherwise dry-run
/// plans and emits a "would regenerate" note (rule 8), and a real run has
/// each target self-repair (if its vulnerable pair is no longer locked) or
/// run `cargo update --precise`; any failure restores Cargo.lock and rolls
/// back the group (rule 7).
fn apply_cargo_precise_group(
    group: Group,
    opts: &FixApplyOptions,
    outcomes: &mut Vec<AppliedFix>,
    notes: &mut Vec<String>,
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

    let lock = lockfile.unwrap_or(path);
    let snapshot = Snapshot::capture(std::slice::from_ref(&lock));
    let lock_dir = lock.parent().unwrap_or(Path::new(".")).to_path_buf();

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
                items.push((target, Provisional::Failed(msg)));
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
        for (target, prov) in items {
            outcomes.push(finalize(target, prov, FixStatus::Applied));
        }
    } else {
        snapshot.restore();
        let combined = failure_messages.join("; ");
        for (target, prov) in items {
            outcomes.push(finalize_rolled_back(target, prov, &combined));
        }
    }
}

/// Applies every fix target in transactional per-`(path, lockfile)` groups.
/// Returns the per-target outcomes and stderr-destined informational notes
/// (dry-run "would regenerate" listings, "no lockfile found" skips).
pub fn apply_fix_targets(
    targets: Vec<FixTarget>,
    opts: &FixApplyOptions,
    apply_manifest_edits: ManifestEditFn<'_>,
) -> (Vec<AppliedFix>, Vec<String>) {
    let mut outcomes = Vec::new();
    let mut notes = Vec::new();
    for group in group_targets(targets) {
        if group.cargo_precise {
            apply_cargo_precise_group(group, opts, &mut outcomes, &mut notes);
        } else {
            apply_edit_group(group, opts, apply_manifest_edits, &mut outcomes, &mut notes);
        }
    }
    (outcomes, notes)
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

    fn noop_closure() -> impl Fn(&Path, FileType, &[&FixTarget]) -> anyhow::Result<bool> {
        |_, _, _| panic!("apply_manifest_edits should not be called")
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
        };
        let closure = noop_closure();
        let (outcomes, notes) = apply_fix_targets(vec![target], &opts, &closure);

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
        };
        let closure = noop_closure();
        let (outcomes, _notes) = apply_fix_targets(vec![target], &opts, &closure);

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
        };
        let closure = noop_closure();
        let (outcomes, _notes) = apply_fix_targets(vec![target], &opts, &closure);

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
        };
        let closure = noop_closure();
        let (outcomes, notes) = apply_fix_targets(vec![target], &opts, &closure);

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
        };
        let (outcomes, _notes) = apply_fix_targets(vec![target], &opts, &closure);

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
            dependency_key: None,
            from_version: "1.2.0".to_string(),
            to_version: "2.5.0".to_string(),
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
        };
        let closure = noop_closure();
        let (outcomes, _notes) = apply_fix_targets(vec![target], &opts, &closure);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, FixStatus::Unfixable);
        let error = outcomes[0].error.as_ref().expect("error present");
        assert!(error.contains("object"), "{error}");
    }

    #[test]
    fn groups_share_one_relock_key() {
        let dir = tempfile::tempdir().unwrap();
        let package_json = write(dir.path(), "package.json", PACKAGE_JSON_BARE);
        let package_lock = write(dir.path(), "package-lock.json", "{}\n");

        let override_target = FixTarget {
            package: "examplepkg".to_string(),
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
        };
        let closure = noop_closure();
        let (outcomes, notes) =
            apply_fix_targets(vec![override_target, manifest_target], &opts, &closure);

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
        };
        let (outcomes, notes) =
            apply_fix_targets(vec![manifest_target, override_target], &opts, &closure);

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
        };
        let closure = noop_closure();
        let (outcomes, notes) = apply_fix_targets(vec![target], &opts, &closure);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, FixStatus::Planned);
        assert!(outcomes[0].error.is_none());
        assert_eq!(notes, vec!["Cargo.lock: would regenerate".to_string()]);
        assert_eq!(std::fs::read_to_string(&cargo_lock).unwrap(), before);
    }
}
