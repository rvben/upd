//! Release-age gating for lockfile refreshes.
//!
//! A cooldown keeps the manifest from naming a release younger than it, but
//! the package manager that relocks the manifest resolves on its own and takes
//! whatever the registry published an hour ago. A refresh under a cooldown is
//! therefore handed the tool's own release-age setting where it has one
//! (`native`, `uv`), and every refreshed lockfile upd can read is read back
//! afterwards (`check`), since that setting exempts packages and admits
//! releases with no publish date: Cargo's young crates are moved back
//! with `cargo update --precise` (`cargo`), and whatever is still inside the
//! cooldown is reported.

mod cargo;
pub mod check;
mod gemfile;
pub(crate) mod native;
pub(crate) mod uv;

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Duration, Utc};

use crate::cooldown::humanize_cooldown;
use crate::lockfile::LockfileType;

/// The oldest a release may be published and still be locked: now minus the
/// cooldown in force for the lockfile's manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseAgeGate {
    min_age: Duration,
    now: DateTime<Utc>,
}

impl ReleaseAgeGate {
    /// A gate for `min_age` measured from `now`, or `None` when the cooldown
    /// is disabled.
    pub fn new(min_age: Duration, now: DateTime<Utc>) -> Option<Self> {
        (min_age > Duration::zero()).then_some(Self { min_age, now })
    }

    pub fn min_age(&self) -> Duration {
        self.min_age
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.now
    }

    /// The latest publish time a release may carry and still be locked.
    pub fn cutoff(&self) -> DateTime<Utc> {
        self.now - self.min_age
    }

    /// Whether a release published at `published` is outside the cooldown.
    pub fn admits(&self, published: DateTime<Utc>) -> bool {
        published <= self.cutoff()
    }

    /// The gate with the longer cooldown, for a lockfile shared by manifests
    /// whose configs disagree.
    pub fn stricter(self, other: Self) -> Self {
        if other.cutoff() < self.cutoff() {
            other
        } else {
            self
        }
    }

    /// The cooldown as it is configured, e.g. `7d`.
    pub fn humanized(&self) -> String {
        humanize_cooldown(self.min_age)
    }
}

/// How far a refresh under a gate kept to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateStatus {
    /// The tool resolved under the gate, or under a stricter setting of the
    /// project's own. What it introduced is still checked, since a tool
    /// exempts packages from its own gate (npm `min-release-age-exclude`, a
    /// uv package exempted at its locked release) and admits a release that
    /// has no publish date.
    Enforced,
    /// The tool has no release-age setting upd can hand it, for `reason`.
    Unenforced { reason: String },
    /// The tool failed under the gate and the lockfile was refreshed without
    /// it; `reason` is what the gated run said.
    Bypassed { reason: String },
}

impl GateStatus {
    fn rank(&self) -> u8 {
        match self {
            GateStatus::Enforced => 0,
            GateStatus::Unenforced { .. } => 1,
            GateStatus::Bypassed { .. } => 2,
        }
    }
}

/// One lockfile a gated refresh rewrote, with what it looked like before.
#[derive(Debug, Clone)]
pub struct GateReport {
    pub lockfile: PathBuf,
    pub lockfile_type: LockfileType,
    pub gate: ReleaseAgeGate,
    pub status: GateStatus,
    /// The lockfile's bytes before the refresh, so only the entries the
    /// refresh introduced are checked. `None` when it could not be read.
    pub before: Option<Vec<u8>>,
    /// The version floors the run locked on purpose, as `(name, floor)`: a
    /// hold never moves an entry at its floor, nor a crate below its floor.
    pub keep: Vec<LockEntry>,
}

/// Fold reports for the same lockfile into one. A lockfile refreshed twice in
/// a run (a `--lock` refresh, then a floor relock) keeps the bytes from before
/// the first refresh, the stricter gate, the least enforced status and every
/// entry either one locked on purpose, so an entry either refresh introduced
/// is checked against the tightest cooldown. The two refreshes can name the
/// file differently (`./Cargo.lock`, `Cargo.lock`), so reports are matched by
/// [`file_identity`].
pub fn merge_reports(reports: Vec<GateReport>) -> Vec<GateReport> {
    let mut merged: Vec<(PathBuf, GateReport)> = Vec::new();
    for report in reports {
        let identity = file_identity(&report.lockfile);
        match merged.iter_mut().find(|(seen, _)| *seen == identity) {
            Some((_, existing)) => {
                existing.gate = existing.gate.stricter(report.gate);
                existing.keep.extend(report.keep);
                if report.status.rank() > existing.status.rank() {
                    existing.status = report.status;
                }
            }
            None => merged.push((identity, report)),
        }
    }
    merged.into_iter().map(|(_, report)| report).collect()
}

/// The file `path` names: its canonical path when it exists, and otherwise
/// the path without its `.` components.
pub fn file_identity(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        path.components()
            .filter(|component| !matches!(component, Component::CurDir))
            .collect()
    })
}

/// Runs `cmd args` in the project directory and answers its stdout.
pub(crate) type Query<'a> = &'a dyn Fn(&str, &[&str]) -> Result<String, String>;

/// Reads one environment variable, so configuration lookups can be tested
/// without touching the process environment.
pub(crate) type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// A locked `(name, version)`.
pub(crate) type LockEntry = (String, String);

pub(crate) fn run_query(dir: &Path, cmd: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("`{cmd} {}` could not run: {e}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            condense(&String::from_utf8_lossy(&output.stderr))
        ))
    }
}

/// A tool's complaint, trimmed to fit in one warning line.
pub(crate) fn condense(detail: &str) -> String {
    const LIMIT: usize = 300;
    let joined = detail
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if joined.chars().count() <= LIMIT {
        return joined;
    }
    let mut cut: String = joined.chars().take(LIMIT).collect();
    cut.push_str("...");
    cut
}

/// The first `major.minor.patch` in a tool's `--version` output, with a
/// missing minor or patch read as zero.
pub(crate) fn tool_version(output: &str) -> Option<(u64, u64, u64)> {
    let start = output.find(|c: char| c.is_ascii_digit())?;
    let token: String = output[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = token.split('.').filter(|p| !p.is_empty());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn a_disabled_cooldown_has_no_gate() {
        let now = at("2026-09-15T12:00:00Z");
        assert!(ReleaseAgeGate::new(Duration::zero(), now).is_none());
        assert!(ReleaseAgeGate::new(Duration::seconds(-1), now).is_none());
    }

    #[test]
    fn the_gate_admits_releases_up_to_its_cutoff() {
        let gate = ReleaseAgeGate::new(Duration::days(7), at("2026-09-15T12:00:00Z")).unwrap();
        assert_eq!(gate.cutoff(), at("2026-09-08T12:00:00Z"));
        assert!(gate.admits(at("2026-09-08T12:00:00Z")));
        assert!(!gate.admits(at("2026-09-08T12:00:01Z")));
        assert_eq!(gate.humanized(), "7d");
    }

    #[test]
    fn the_stricter_gate_has_the_earlier_cutoff() {
        let now = at("2026-09-15T12:00:00Z");
        let week = ReleaseAgeGate::new(Duration::days(7), now).unwrap();
        let month = ReleaseAgeGate::new(Duration::days(30), now).unwrap();
        assert_eq!(week.stricter(month), month);
        assert_eq!(month.stricter(week), month);
    }

    fn report(lockfile: &str, days: i64, status: GateStatus, before: &str) -> GateReport {
        GateReport {
            lockfile: Path::new(lockfile).to_path_buf(),
            lockfile_type: LockfileType::UvLock,
            gate: ReleaseAgeGate::new(Duration::days(days), at("2026-09-15T12:00:00Z")).unwrap(),
            status,
            before: Some(before.as_bytes().to_vec()),
            keep: Vec::new(),
        }
    }

    #[test]
    fn merged_reports_keep_the_first_bytes_the_stricter_gate_and_the_weakest_status() {
        let bypassed = GateStatus::Bypassed {
            reason: "first".to_string(),
        };
        let merged = merge_reports(vec![
            report("a/uv.lock", 7, GateStatus::Enforced, "original"),
            report("b/uv.lock", 3, GateStatus::Enforced, "other"),
            report("a/uv.lock", 30, bypassed.clone(), "after the first refresh"),
            report(
                "a/uv.lock",
                1,
                GateStatus::Unenforced {
                    reason: "later".to_string(),
                },
                "after the second refresh",
            ),
            report(
                "a/uv.lock",
                1,
                GateStatus::Bypassed {
                    reason: "second".to_string(),
                },
                "after the third refresh",
            ),
        ]);

        assert_eq!(merged.len(), 2);
        let a = &merged[0];
        assert_eq!(a.lockfile, Path::new("a/uv.lock"));
        assert_eq!(a.before.as_deref(), Some("original".as_bytes()));
        assert_eq!(a.gate.min_age(), Duration::days(30));
        assert_eq!(a.status, bypassed);
        assert_eq!(merged[1].lockfile, Path::new("b/uv.lock"));
    }

    #[test]
    fn reports_naming_the_same_file_differently_are_merged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("uv.lock"), "").unwrap();
        let path = |name: &str| dir.path().join(name).to_string_lossy().into_owned();

        let merged = merge_reports(vec![
            report(&path("uv.lock"), 7, GateStatus::Enforced, "original"),
            report(&path("sub/../uv.lock"), 30, GateStatus::Enforced, "later"),
            report("a/uv.lock", 7, GateStatus::Enforced, "missing"),
            report("./a/./uv.lock", 30, GateStatus::Enforced, "missing later"),
        ]);

        assert_eq!(merged.len(), 2, "{merged:?}");
        assert_eq!(merged[0].lockfile, dir.path().join("uv.lock"));
        assert_eq!(merged[0].gate.min_age(), Duration::days(30));
        assert_eq!(merged[1].lockfile, Path::new("a/uv.lock"));
        assert_eq!(merged[1].gate.min_age(), Duration::days(30));
    }

    #[test]
    fn condense_joins_lines_and_caps_the_length() {
        assert_eq!(
            condense("  error: No solution found\n\n  hint: try again \n"),
            "error: No solution found hint: try again"
        );
        let long = "x".repeat(400);
        let condensed = condense(&long);
        assert_eq!(condensed.chars().count(), 303);
        assert!(condensed.ends_with("..."));
    }

    #[test]
    fn tool_version_reads_the_first_version_in_the_output() {
        assert_eq!(tool_version("10.16.1"), Some((10, 16, 1)));
        assert_eq!(
            tool_version("uv 0.9.0 (Homebrew 2026-01-01)"),
            Some((0, 9, 0))
        );
        assert_eq!(tool_version("4.10"), Some((4, 10, 0)));
        assert_eq!(tool_version("1.3.0-canary.1+abc"), Some((1, 3, 0)));
        assert_eq!(tool_version("no version here"), None);
    }
}
