//! Holding crates back in `Cargo.lock`.
//!
//! Cargo has no release-age setting, so a refresh can lock a crate published
//! an hour ago. Each crates.io entry the refresh introduced inside the
//! cooldown is moved back with `cargo update -p name@fresh --precise older`,
//! to the newest compatible release outside the cooldown, or to the release
//! the lockfile held before the run.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::lockfile::{RegenOutcome, cargo_compat_key, cargo_update_precise, containing_dir};
use crate::output::LockfileHold;
use crate::path_display::display_path;
use crate::registry::VersionMeta;

use super::check::{MetaCache, Metas};
use super::{GateReport, LockEntry, ReleaseAgeGate, condense};

const CRATES_IO_SOURCES: [&str; 2] = [
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
];

/// Every crates.io `(name, version)` in a `Cargo.lock`, in lockfile order.
pub(crate) fn crates_io_entries(content: &str) -> Result<Vec<LockEntry>, String> {
    registry_entries(content).map(|(crates_io, _)| crates_io)
}

/// Entries from a registry other than crates.io, each with the index URL its
/// lockfile records.
pub(crate) type OtherRegistryEntries = Vec<(LockEntry, String)>;

/// A `Cargo.lock`'s registry `(name, version)` entries in lockfile order,
/// split into those from crates.io and those from any other registry. Path
/// and git entries are neither.
pub(crate) fn registry_entries(
    content: &str,
) -> Result<(Vec<LockEntry>, OtherRegistryEntries), String> {
    #[derive(serde::Deserialize)]
    struct Lock {
        #[serde(default)]
        package: Vec<Package>,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        name: String,
        version: String,
        source: Option<String>,
    }
    let lock: Lock = toml::from_str(content).map_err(|e| e.to_string())?;
    let mut crates_io = Vec::new();
    let mut other = Vec::new();
    for package in lock.package {
        match package.source.as_deref() {
            Some(source) if CRATES_IO_SOURCES.contains(&source) => {
                crates_io.push((package.name, package.version));
            }
            Some(source) => {
                if let Some(index) = source
                    .strip_prefix("registry+")
                    .or_else(|| source.strip_prefix("sparse+"))
                {
                    other.push(((package.name, package.version), index.to_string()));
                }
            }
            _ => {}
        }
    }
    Ok((crates_io, other))
}

/// The release to hold `fresh` at: the newest non-yanked release of the same
/// compatibility line that is older than `fresh`, no older than what the
/// lockfile held on that line before the run, stable when `fresh` is, and
/// either outside the cooldown or exactly what the lockfile held before.
pub(crate) fn hold_candidate(
    metas: &[VersionMeta],
    fresh: &str,
    previous: &[String],
    gate: ReleaseAgeGate,
) -> Option<String> {
    let fresh_version = semver::Version::parse(fresh).ok()?;
    let key = cargo_compat_key(fresh)?;
    let previous = previous
        .iter()
        .filter(|version| cargo_compat_key(version) == Some(key))
        .filter_map(|version| semver::Version::parse(version).ok())
        .max();
    metas
        .iter()
        .filter(|meta| !meta.yanked && cargo_compat_key(&meta.version) == Some(key))
        .filter_map(|meta| Some((semver::Version::parse(&meta.version).ok()?, meta)))
        .filter(|(version, meta)| {
            let prerelease = meta.prerelease || !version.pre.is_empty();
            let was_locked = previous.as_ref() == Some(version);
            *version < fresh_version
                && (!prerelease || !fresh_version.pre.is_empty())
                && previous.as_ref().is_none_or(|floor| version >= floor)
                && (was_locked || meta.published_at.is_some_and(|at| gate.admits(at)))
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, meta)| meta.version.clone())
}

/// What holding one lockfile's young crates came to.
pub(crate) struct Holds {
    pub(crate) holds: Vec<LockfileHold>,
    /// For each entry that could not be held, why.
    pub(crate) notes: HashMap<LockEntry, String>,
    /// Why holding stopped with the lockfile still carrying a refused hold.
    pub(crate) error: Option<String>,
}

/// Hold every young crates.io entry the refresh behind `report` introduced,
/// one `cargo update --precise` at a time, rereading the lockfile after each
/// since one hold can move a crate's exact-pinned companions with it. A
/// refused hold is put back before the next; when it cannot be, holding stops.
pub(crate) async fn hold_young_crates(
    report: &GateReport,
    metas: &mut MetaCache<'_>,
    verbose: bool,
) -> Holds {
    let mut holds = Vec::new();
    let mut notes = HashMap::new();
    let mut error = None;
    let before: Option<Vec<LockEntry>> = report
        .before
        .as_deref()
        .and_then(|bytes| crates_io_entries(&String::from_utf8_lossy(bytes)).ok());
    let mut previous: HashMap<String, Vec<String>> = HashMap::new();
    for (name, version) in before.iter().flatten() {
        previous
            .entry(name.clone())
            .or_default()
            .push(version.clone());
    }
    let dir = containing_dir(&report.lockfile).to_path_buf();
    let mut attempted: HashSet<LockEntry> = HashSet::new();
    let locked = read_lock(&report.lockfile)
        .map(|(_, entries)| entries)
        .unwrap_or_default();
    let keep = spelled_as_locked(&report.keep, &locked);
    for entry in &keep {
        notes.insert(
            entry.clone(),
            "it is the version floor the run chose".to_string(),
        );
    }

    while let Some((bytes, current)) = read_lock(&report.lockfile) {
        let fresh: Vec<LockEntry> = current
            .into_iter()
            .filter(|entry| before.as_ref().is_none_or(|before| !before.contains(entry)))
            .collect();
        metas
            .fetch(fresh.iter().map(|(name, _)| name.as_str()))
            .await;

        let mut young: Vec<(String, String, &VersionMeta)> = fresh
            .iter()
            .filter(|entry| !attempted.contains(*entry) && !keep.contains(*entry))
            .filter_map(|(name, version)| {
                let Some(Metas::Listed(list)) = metas.get(name) else {
                    return None;
                };
                let meta = list.iter().find(|meta| meta.version == *version)?;
                let published = meta.published_at?;
                (!report.gate.admits(published)).then(|| (name.clone(), version.clone(), meta))
            })
            .collect();
        young.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        let Some((name, version, meta)) = young.into_iter().next() else {
            break;
        };
        let published_at = meta.published_at;
        attempted.insert((name.clone(), version.clone()));

        let Some(Metas::Listed(list)) = metas.get(&name) else {
            continue;
        };
        let empty = Vec::new();
        let candidate = hold_candidate(
            list,
            &version,
            previous.get(&name).unwrap_or(&empty),
            report.gate,
        );
        let Some(to) = candidate else {
            notes.insert(
                (name, version),
                "no release outside the cooldown can replace it".to_string(),
            );
            continue;
        };
        let refused = match cargo_update_precise(&dir, &name, &version, &to, verbose) {
            RegenOutcome::Ok(_) => {
                match held(
                    &report.lockfile,
                    &current_entries(&bytes),
                    &previous,
                    &keep,
                    &name,
                    &version,
                    &to,
                ) {
                    Ok(()) => {
                        holds.push(LockfileHold {
                            lockfile: display_path(&report.lockfile),
                            package: name,
                            from: version,
                            to,
                            published_at: published_at.expect("a young entry has a publish time"),
                            cooldown: report.gate.humanized(),
                        });
                        continue;
                    }
                    Err(why) => why,
                }
            }
            failed => format!(
                "cargo could not hold it at {to}: {}",
                failed
                    .error_message()
                    .map(|m| condense(&m))
                    .unwrap_or_default()
            ),
        };
        if let Err(e) = restore(&report.lockfile, &bytes) {
            error = Some(format!(
                "{} could not be restored after holding {name} at {to} was refused ({refused}): {e}",
                display_path(&report.lockfile)
            ));
            notes.insert((name, version), refused);
            break;
        }
        notes.insert((name, version), refused);
    }
    // One hold can move a crate an earlier hold moved, so a hold is only
    // reported when the lockfile the run leaves behind still carries it.
    if let Some((_, settled)) = read_lock(&report.lockfile) {
        holds.retain(|hold| {
            let still_held = settled
                .iter()
                .any(|(name, version)| *name == hold.package && *version == hold.to);
            if !still_held {
                notes.insert(
                    (hold.package.clone(), hold.from.clone()),
                    format!("a later hold moved it away from {}", hold.to),
                );
            }
            still_held
        });
    }
    Holds {
        holds,
        notes,
        error,
    }
}

/// Each floor named as its lockfile spells it. A `[pin]` is read for the
/// crate whose name matches once case and separators are normalized, so the
/// configuration can spell it `clap-builder` where `Cargo.lock` has
/// `clap_builder`; holds compare and report floors as the lockfile does.
fn spelled_as_locked(keep: &[LockEntry], locked: &[LockEntry]) -> Vec<LockEntry> {
    keep.iter()
        .map(|(name, version)| {
            let canonical = crate::config::normalize_package_name(name);
            let locked_name =
                locked
                    .iter()
                    .map(|(locked_name, _)| locked_name)
                    .find(|locked_name| {
                        crate::config::normalize_package_name(locked_name) == canonical
                    });
            (locked_name.unwrap_or(name).clone(), version.clone())
        })
        .collect()
}

fn current_entries(bytes: &[u8]) -> Vec<LockEntry> {
    crates_io_entries(&String::from_utf8_lossy(bytes)).unwrap_or_default()
}

/// Whether `cargo update --precise` held `name` at `to` without taking a
/// crate it moved along with it below the release the lockfile held on that
/// line before the run, or a crate below a version floor the run chose
/// (`keep`).
/// `held_before` is the lockfile just before this hold.
fn held(
    path: &Path,
    held_before: &[LockEntry],
    previous: &HashMap<String, Vec<String>>,
    keep: &[LockEntry],
    name: &str,
    from: &str,
    to: &str,
) -> Result<(), String> {
    let Some((_, after)) = read_lock(path) else {
        return Err(format!(
            "{} could not be read after holding it at {to}",
            path.display()
        ));
    };
    let has = |version: &str| after.iter().any(|(n, v)| n == name && v == version);
    if !has(to) || has(from) {
        return Err(format!("cargo did not move it to {to}"));
    }
    for (kept, floor) in keep {
        let Ok(minimum) = semver::Version::parse(floor) else {
            continue;
        };
        let meets = |entries: &[LockEntry]| {
            entries.iter().any(|(n, v)| {
                n == kept
                    && cargo_compat_key(v) == cargo_compat_key(floor)
                    && semver::Version::parse(v).is_ok_and(|v| v >= minimum)
            })
        };
        if meets(held_before) && !meets(&after) {
            return Err(format!(
                "holding it at {to} would move {kept} below {floor}, the version floor the run chose"
            ));
        }
    }
    for (companion, version) in after.iter().filter(|entry| !held_before.contains(entry)) {
        let Ok(now) = semver::Version::parse(version) else {
            continue;
        };
        let floor = previous
            .get(companion)
            .into_iter()
            .flatten()
            .filter(|was| cargo_compat_key(was) == cargo_compat_key(version))
            .filter_map(|was| semver::Version::parse(was).ok())
            .max();
        if let Some(floor) = floor.filter(|floor| now < *floor) {
            return Err(format!(
                "holding it at {to} would move {companion} to {now}, below the {floor} locked before the run"
            ));
        }
    }
    Ok(())
}

/// The bytes of the `Cargo.lock` at `path` and its crates.io entries, or
/// `None` when it cannot be read or parsed; the check after the holds names
/// that lockfile as unchecked.
fn read_lock(path: &Path) -> Option<(Vec<u8>, Vec<LockEntry>)> {
    let bytes = std::fs::read(path).ok()?;
    let entries = crates_io_entries(&String::from_utf8_lossy(&bytes)).ok()?;
    Some((bytes, entries))
}

/// Put `bytes` back at `path` after a refused hold.
fn restore(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, Utc};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-15T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn gate() -> ReleaseAgeGate {
        ReleaseAgeGate::new(Duration::days(7), now()).unwrap()
    }

    fn meta(version: &str, days_old: Option<i64>) -> VersionMeta {
        VersionMeta {
            version: version.to_string(),
            published_at: days_old.map(|days| now() - Duration::days(days)),
            yanked: false,
            prerelease: version.contains('-'),
        }
    }

    #[test]
    fn crates_io_entries_skip_workspace_git_and_other_registries() {
        let lock = r#"version = 4

[[package]]
name = "t"
version = "0.1.0"

[[package]]
name = "clap"
version = "4.6.7"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "serde"
version = "1.0.228"
source = "sparse+https://index.crates.io/"

[[package]]
name = "internal"
version = "1.0.0"
source = "sparse+https://crates.example.com/index/"

[[package]]
name = "gitdep"
version = "0.2.0"
source = "git+https://github.com/example/gitdep#abc"
"#;
        assert_eq!(
            crates_io_entries(lock).unwrap(),
            [
                ("clap".to_string(), "4.6.7".to_string()),
                ("serde".to_string(), "1.0.228".to_string()),
            ]
        );
        assert!(crates_io_entries("[[package]\n").is_err());
    }

    #[test]
    fn the_candidate_is_the_newest_compatible_release_outside_the_cooldown() {
        let metas = [
            meta("4.5.0", Some(400)),
            meta("4.6.5", Some(20)),
            meta("4.6.6", Some(10)),
            meta("4.6.7", Some(1)),
            meta("5.0.0", Some(30)),
        ];
        assert_eq!(
            hold_candidate(&metas, "4.6.7", &["4.5.0".to_string()], gate()).as_deref(),
            Some("4.6.6")
        );
    }

    #[test]
    fn the_candidate_never_goes_below_what_was_locked_before() {
        let metas = [
            meta("4.5.0", Some(400)),
            meta("4.6.6", Some(3)),
            meta("4.6.7", Some(1)),
        ];
        assert_eq!(
            hold_candidate(&metas, "4.6.7", &["4.6.6".to_string()], gate()).as_deref(),
            Some("4.6.6"),
            "the young release that was already locked is a valid hold"
        );
        assert_eq!(
            hold_candidate(
                &metas,
                "4.6.7",
                &["3.0.0".to_string(), "4.6.6".to_string()],
                gate()
            )
            .as_deref(),
            Some("4.6.6"),
            "only the version locked on the same line is the floor"
        );
        let metas = [meta("4.5.0", Some(400)), meta("4.6.7", Some(1))];
        assert_eq!(
            hold_candidate(&metas, "4.6.7", &["4.6.0".to_string()], gate()),
            None,
            "4.5.0 is below the version locked before"
        );
    }

    #[test]
    fn the_candidate_skips_yanked_undated_and_prerelease_releases() {
        let mut yanked = meta("4.6.6", Some(10));
        yanked.yanked = true;
        let metas = [
            meta("4.6.3", Some(30)),
            meta("4.6.4", None),
            yanked,
            meta("4.6.7-rc.1", Some(20)),
            meta("4.6.7", Some(1)),
        ];
        assert_eq!(
            hold_candidate(&metas, "4.6.7", &[], gate()).as_deref(),
            Some("4.6.3")
        );
        assert_eq!(
            hold_candidate(&metas, "4.6.8-rc.1", &[], gate()).as_deref(),
            Some("4.6.7-rc.1"),
            "a prerelease may be held at an older prerelease"
        );
    }

    #[test]
    fn the_candidate_stays_on_the_compatibility_line() {
        let metas = [meta("0.38.9", Some(100)), meta("0.39.0", Some(1))];
        assert_eq!(hold_candidate(&metas, "0.39.0", &[], gate()), None);
    }

    fn entries(pairs: &[(&str, &str)]) -> Vec<LockEntry> {
        pairs
            .iter()
            .map(|(name, version)| (name.to_string(), version.to_string()))
            .collect()
    }

    /// A `Cargo.lock` holding `pairs`, every entry from crates.io.
    fn lock_of(pairs: &[(&str, &str)]) -> tempfile::NamedTempFile {
        let mut text = String::from("version = 4\n");
        for (name, version) in pairs {
            text.push_str(&format!(
                "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n"
            ));
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), text).unwrap();
        file
    }

    #[test]
    fn a_crate_cargo_moved_to_the_held_release_is_held() {
        let after = lock_of(&[("clap", "4.6.6"), ("clap_builder", "4.6.7")]);
        assert_eq!(
            held(
                after.path(),
                &entries(&[("clap", "4.6.7"), ("clap_builder", "4.6.7")]),
                &HashMap::new(),
                &[],
                "clap",
                "4.6.7",
                "4.6.6",
            ),
            Ok(())
        );
    }

    #[test]
    fn a_crate_cargo_left_where_it_was_is_not_held() {
        let after = lock_of(&[("clap", "4.6.7")]);
        assert_eq!(
            held(
                after.path(),
                &entries(&[("clap", "4.6.7")]),
                &HashMap::new(),
                &[],
                "clap",
                "4.6.7",
                "4.6.6",
            ),
            Err("cargo did not move it to 4.6.6".to_string())
        );
    }

    #[test]
    fn a_crate_cargo_moved_somewhere_else_is_not_held() {
        let after = lock_of(&[("clap", "4.5.0")]);
        assert_eq!(
            held(
                after.path(),
                &entries(&[("clap", "4.6.7")]),
                &HashMap::new(),
                &[],
                "clap",
                "4.6.7",
                "4.6.6",
            ),
            Err("cargo did not move it to 4.6.6".to_string())
        );
    }

    #[test]
    fn a_crate_the_lockfile_still_lists_at_the_old_release_is_not_held() {
        let after = lock_of(&[("clap", "4.6.6"), ("clap", "4.6.7")]);
        assert_eq!(
            held(
                after.path(),
                &entries(&[("clap", "4.6.7")]),
                &HashMap::new(),
                &[],
                "clap",
                "4.6.7",
                "4.6.6",
            ),
            Err("cargo did not move it to 4.6.6".to_string())
        );
    }

    #[test]
    fn a_hold_that_takes_a_crate_below_a_version_floor_is_not_held() {
        let after = lock_of(&[("clap", "4.6.6"), ("clap_builder", "4.6.5")]);
        assert_eq!(
            held(
                after.path(),
                &entries(&[("clap", "4.6.7"), ("clap_builder", "4.6.7")]),
                &HashMap::new(),
                &entries(&[("clap_builder", "4.6.6")]),
                "clap",
                "4.6.7",
                "4.6.6",
            ),
            Err(
                "holding it at 4.6.6 would move clap_builder below 4.6.6, the version floor the run chose"
                    .to_string()
            )
        );
    }

    #[test]
    fn a_hold_that_takes_a_companion_below_its_locked_release_is_not_held() {
        let after = lock_of(&[("clap", "4.6.6"), ("clap_builder", "4.6.5")]);
        let previous = HashMap::from([("clap_builder".to_string(), vec!["4.6.6".to_string()])]);
        assert_eq!(
            held(
                after.path(),
                &entries(&[("clap", "4.6.7"), ("clap_builder", "4.6.7")]),
                &previous,
                &[],
                "clap",
                "4.6.7",
                "4.6.6",
            ),
            Err(
                "holding it at 4.6.6 would move clap_builder to 4.6.5, below the 4.6.6 locked before the run"
                    .to_string()
            )
        );
    }
}
