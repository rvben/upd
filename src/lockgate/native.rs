//! The release-age settings of the JavaScript package managers.
//!
//! Each planner reads the setting the project already has and answers with
//! what to add to the refresh command so it resolves under the stricter of
//! the two: `Ok(Some(_))` to add it, `Ok(None)` when the project's own setting
//! is already at least as strict, and `Err(reason)` when the tool has no
//! setting upd can pass.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, NaiveDate, Utc};

use super::{EnvLookup, Query, ReleaseAgeGate, run_query, tool_version};
use crate::lockfile::LockfileType;

/// What a gated refresh adds to the plain command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GatedInvocation {
    pub extra_args: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub(crate) type Plan = Result<Option<GatedInvocation>, String>;

/// Plan the gate for one lockfile refreshed in `dir` by a tool that printed
/// `version` for `--version`.
pub(crate) fn plan(
    lockfile_type: LockfileType,
    dir: &Path,
    version: Option<&str>,
    gate: ReleaseAgeGate,
) -> Plan {
    let query = |cmd: &str, args: &[&str]| run_query(dir, cmd, args);
    plan_with(
        lockfile_type,
        version,
        gate,
        &query,
        &bunfig_paths(dir, &|key| std::env::var_os(key)),
    )
}

fn plan_with(
    lockfile_type: LockfileType,
    version: Option<&str>,
    gate: ReleaseAgeGate,
    query: Query<'_>,
    bunfigs: &[PathBuf],
) -> Plan {
    match lockfile_type {
        LockfileType::PackageLockJson | LockfileType::NpmShrinkwrap => plan_npm(gate, query),
        LockfileType::PnpmLock => plan_pnpm(version, gate, query),
        LockfileType::YarnLock => plan_yarn(version, gate, query),
        LockfileType::BunLock | LockfileType::BunLockb => plan_bun(version, gate, bunfigs),
        other => {
            let (tool, _) = other.command(&[]);
            Err(format!("{tool} has no release-age setting"))
        }
    }
}

pub(crate) fn format_instant(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// A cooldown in whole units of `unit`, rounded up so the gate is never
/// looser than the cooldown.
fn ceil_units(age: Duration, unit: Duration) -> i64 {
    let (age, unit) = (age.num_milliseconds(), unit.num_milliseconds());
    (age + unit - 1) / unit
}

fn read_version(tool: &str, version: Option<&str>) -> Result<(u64, u64, u64), String> {
    version
        .and_then(tool_version)
        .ok_or_else(|| format!("{tool}'s version could not be read"))
}

/// npm's `--before` overrides the `before` in `.npmrc`, and npm derives
/// `before` from `min-release-age` itself, so the flag carries the earliest of
/// the run's cutoff and both project settings.
fn plan_npm(gate: ReleaseAgeGate, query: Query<'_>) -> Plan {
    let raw = query("npm", &["config", "list", "--json"])
        .map_err(|e| format!("npm's configuration could not be read ({e})"))?;
    let config: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("npm's configuration could not be read ({e})"))?;

    let mut before = gate.cutoff();
    match config.get("before") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::String(value)) => {
            let configured = parse_npm_date(value)
                .ok_or_else(|| format!("npm's before setting {value:?} is not a date"))?;
            before = before.min(configured);
        }
        Some(other) => return Err(format!("npm's before setting {other} is not a date")),
    }
    match config.get("min-release-age") {
        None | Some(serde_json::Value::Null) => {}
        Some(value) => {
            let days = value
                .as_f64()
                .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
                .filter(|days| days.is_finite() && *days >= 0.0)
                .ok_or_else(|| format!("npm's min-release-age setting {value} is not a number"))?;
            let configured = gate.now() - Duration::seconds((days * 86_400.0).ceil() as i64);
            before = before.min(configured);
        }
    }
    Ok(Some(GatedInvocation {
        extra_args: vec![format!("--before={}", format_instant(before))],
        env: Vec::new(),
    }))
}

fn parse_npm_date(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|at| at.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .map(|at| at.and_utc())
        })
}

/// pnpm reads `minimumReleaseAge` in minutes from 10.16 on, and defaults it to
/// a day from 11 on.
fn plan_pnpm(version: Option<&str>, gate: ReleaseAgeGate, query: Query<'_>) -> Plan {
    let version = read_version("pnpm", version)?;
    if version < (10, 16, 0) {
        return Err(format!(
            "pnpm {}.{}.{} predates minimumReleaseAge (10.16)",
            version.0, version.1, version.2
        ));
    }
    let raw = query("pnpm", &["config", "get", "minimumReleaseAge"])
        .map_err(|e| format!("pnpm's minimumReleaseAge could not be read ({e})"))?;
    let configured = match raw.trim() {
        "" | "undefined" => {
            if version.0 >= 11 {
                1440
            } else {
                0
            }
        }
        value => value.parse::<i64>().map_err(|_| {
            format!("pnpm's minimumReleaseAge {value:?} is not a number of minutes")
        })?,
    };
    let ours = ceil_units(gate.min_age(), Duration::minutes(1));
    Ok((ours > configured).then(|| GatedInvocation {
        extra_args: vec![format!("--config.minimum-release-age={ours}")],
        env: Vec::new(),
    }))
}

/// Yarn Berry reads `npmMinimalAgeGate` from 4.10 on; the environment
/// variable overrides `.yarnrc.yml` without touching it.
fn plan_yarn(version: Option<&str>, gate: ReleaseAgeGate, query: Query<'_>) -> Plan {
    let version = read_version("yarn", version)?;
    if version.0 < 2 {
        return Err("yarn 1 has no release-age setting".to_string());
    }
    if version < (4, 10, 0) {
        return Err(format!(
            "yarn {}.{}.{} predates npmMinimalAgeGate (4.10)",
            version.0, version.1, version.2
        ));
    }
    let raw = query("yarn", &["config", "get", "npmMinimalAgeGate"])
        .map_err(|e| format!("yarn's npmMinimalAgeGate could not be read ({e})"))?;
    let value = raw.trim();
    let configured = value
        .parse::<i64>()
        .ok()
        .or_else(|| {
            crate::cooldown::parse_duration(value)
                .ok()
                .map(|age| ceil_units(age, Duration::minutes(1)))
        })
        .ok_or_else(|| format!("yarn's npmMinimalAgeGate {value:?} is not a duration"))?;
    let ours = ceil_units(gate.min_age(), Duration::minutes(1));
    Ok((ours > configured).then(|| GatedInvocation {
        extra_args: Vec::new(),
        env: vec![("YARN_NPM_MINIMAL_AGE_GATE".to_string(), ours.to_string())],
    }))
}

/// Where bun reads `[install] minimumReleaseAge` from, the project's file
/// first, since it overrides the global one.
fn bunfig_paths(dir: &Path, env: EnvLookup<'_>) -> Vec<PathBuf> {
    let mut paths = vec![dir.join("bunfig.toml")];
    if let Some(xdg) = env("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        paths.push(PathBuf::from(xdg).join(".bunfig.toml"));
    }
    if let Some(home) = env("HOME").filter(|v| !v.is_empty()) {
        paths.push(PathBuf::from(home).join(".bunfig.toml"));
    }
    paths
}

/// bun reads `minimumReleaseAge` in seconds from 1.3 on.
fn plan_bun(version: Option<&str>, gate: ReleaseAgeGate, bunfigs: &[PathBuf]) -> Plan {
    let version = read_version("bun", version)?;
    if version < (1, 3, 0) {
        return Err(format!(
            "bun {}.{}.{} predates minimumReleaseAge (1.3)",
            version.0, version.1, version.2
        ));
    }
    let mut configured = 0;
    for path in bunfigs {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{} could not be read ({e})", path.display())),
        };
        let doc: toml::Table = text
            .parse()
            .map_err(|e| format!("{} could not be parsed ({e})", path.display()))?;
        let Some(value) = doc.get("install").and_then(|i| i.get("minimumReleaseAge")) else {
            continue;
        };
        configured = value
            .as_integer()
            .or_else(|| value.as_float().map(|f| f.ceil() as i64))
            .ok_or_else(|| {
                format!(
                    "minimumReleaseAge in {} is not a number of seconds",
                    path.display()
                )
            })?;
        break;
    }
    let ours = ceil_units(gate.min_age(), Duration::seconds(1));
    Ok((ours > configured).then(|| GatedInvocation {
        extra_args: vec!["--minimum-release-age".to_string(), ours.to_string()],
        env: Vec::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn gate(age: Duration) -> ReleaseAgeGate {
        let now = DateTime::parse_from_rfc3339("2026-09-15T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        ReleaseAgeGate::new(age, now).unwrap()
    }

    /// The commands a query was asked, in order.
    type Asked = std::rc::Rc<RefCell<Vec<String>>>;

    /// A query that answers `answer` and records what was asked.
    fn answering(
        answer: Result<&str, &str>,
    ) -> (impl Fn(&str, &[&str]) -> Result<String, String>, Asked) {
        let asked = std::rc::Rc::new(RefCell::new(Vec::new()));
        let log = std::rc::Rc::clone(&asked);
        let query = move |cmd: &str, args: &[&str]| {
            log.borrow_mut().push(format!("{cmd} {}", args.join(" ")));
            answer.map(str::to_string).map_err(str::to_string)
        };
        (query, asked)
    }

    fn args(plan: Plan) -> Vec<String> {
        plan.expect("planned").expect("gated").extra_args
    }

    #[test]
    fn npm_passes_the_cutoff_when_the_project_sets_nothing() {
        let (query, asked) = answering(Ok(r#"{"before": null, "min-release-age": null}"#));
        let plan = plan_with(
            LockfileType::PackageLockJson,
            None,
            gate(Duration::days(7)),
            &query,
            &[],
        );
        assert_eq!(args(plan), ["--before=2026-09-08T12:00:00Z"]);
        assert_eq!(asked.borrow().as_slice(), ["npm config list --json"]);
    }

    #[test]
    fn npm_keeps_the_earliest_of_the_cutoff_and_the_project_settings() {
        let (query, _) = answering(Ok(
            r#"{"before": "2026-09-10T00:00:00.500Z", "min-release-age": null}"#,
        ));
        let plan = plan_with(
            LockfileType::NpmShrinkwrap,
            None,
            gate(Duration::days(7)),
            &query,
            &[],
        );
        assert_eq!(
            args(plan),
            ["--before=2026-09-08T12:00:00Z"],
            "the cutoff is earlier"
        );

        let (query, _) = answering(Ok(r#"{"before": "2024-06-01", "min-release-age": null}"#));
        let plan = plan_with(
            LockfileType::PackageLockJson,
            None,
            gate(Duration::days(7)),
            &query,
            &[],
        );
        assert_eq!(args(plan), ["--before=2024-06-01T00:00:00Z"]);

        let (query, _) = answering(Ok(r#"{"before": null, "min-release-age": 30}"#));
        let plan = plan_with(
            LockfileType::PackageLockJson,
            None,
            gate(Duration::days(7)),
            &query,
            &[],
        );
        assert_eq!(args(plan), ["--before=2026-08-16T12:00:00Z"]);
    }

    #[test]
    fn npm_without_a_readable_configuration_is_unenforced() {
        let (query, _) = answering(Err("npm exploded"));
        let reason = plan_with(
            LockfileType::PackageLockJson,
            None,
            gate(Duration::days(7)),
            &query,
            &[],
        )
        .unwrap_err();
        assert!(reason.contains("npm exploded"), "{reason}");

        let (query, _) = answering(Ok(r#"{"before": "yesterday"}"#));
        let reason = plan_with(
            LockfileType::PackageLockJson,
            None,
            gate(Duration::days(7)),
            &query,
            &[],
        )
        .unwrap_err();
        assert!(reason.contains("yesterday"), "{reason}");
    }

    #[test]
    fn pnpm_raises_a_looser_minimum_release_age_to_the_cooldown() {
        let (query, asked) = answering(Ok("undefined\n"));
        let plan = plan_with(
            LockfileType::PnpmLock,
            Some("10.16.0"),
            gate(Duration::days(7)),
            &query,
            &[],
        );
        assert_eq!(args(plan), ["--config.minimum-release-age=10080"]);
        assert_eq!(
            asked.borrow().as_slice(),
            ["pnpm config get minimumReleaseAge"]
        );

        let (query, _) = answering(Ok("1"));
        let plan = plan_with(
            LockfileType::PnpmLock,
            Some("10.20.0"),
            gate(Duration::seconds(90)),
            &query,
            &[],
        );
        assert_eq!(
            args(plan),
            ["--config.minimum-release-age=2"],
            "rounded up to whole minutes"
        );
    }

    #[test]
    fn pnpm_leaves_an_equal_or_stricter_setting_alone() {
        let (query, _) = answering(Ok("20160"));
        let plan = plan_with(
            LockfileType::PnpmLock,
            Some("10.17.1"),
            gate(Duration::days(7)),
            &query,
            &[],
        );
        assert_eq!(plan, Ok(None));

        let (query, _) = answering(Ok("undefined"));
        let plan = plan_with(
            LockfileType::PnpmLock,
            Some("11.0.0"),
            gate(Duration::hours(24)),
            &query,
            &[],
        );
        assert_eq!(plan, Ok(None), "pnpm 11 already waits a day by default");
    }

    #[test]
    fn pnpm_before_10_16_or_with_an_unreadable_setting_is_unenforced() {
        let (query, asked) = answering(Ok("undefined"));
        let reason = plan_with(
            LockfileType::PnpmLock,
            Some("10.15.9"),
            gate(Duration::days(7)),
            &query,
            &[],
        )
        .unwrap_err();
        assert!(reason.contains("10.16"), "{reason}");
        assert!(asked.borrow().is_empty());

        let (query, _) = answering(Ok("soon"));
        assert!(
            plan_with(
                LockfileType::PnpmLock,
                Some("10.16.0"),
                gate(Duration::days(7)),
                &query,
                &[]
            )
            .is_err()
        );
        assert!(
            plan_with(
                LockfileType::PnpmLock,
                None,
                gate(Duration::days(7)),
                &query,
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn yarn_berry_gets_the_age_gate_through_the_environment() {
        let (query, asked) = answering(Ok("0\n"));
        let plan = plan_with(
            LockfileType::YarnLock,
            Some("4.10.3"),
            gate(Duration::days(7)),
            &query,
            &[],
        );
        let gated = plan.unwrap().unwrap();
        assert!(gated.extra_args.is_empty());
        assert_eq!(
            gated.env,
            [("YARN_NPM_MINIMAL_AGE_GATE".to_string(), "10080".to_string())]
        );
        assert_eq!(
            asked.borrow().as_slice(),
            ["yarn config get npmMinimalAgeGate"]
        );

        let (query, _) = answering(Ok("14d"));
        assert_eq!(
            plan_with(
                LockfileType::YarnLock,
                Some("4.11.0"),
                gate(Duration::days(7)),
                &query,
                &[]
            ),
            Ok(None)
        );
    }

    #[test]
    fn yarn_classic_and_older_berry_are_unenforced() {
        let (query, _) = answering(Ok("0"));
        let reason = plan_with(
            LockfileType::YarnLock,
            Some("1.22.22"),
            gate(Duration::days(7)),
            &query,
            &[],
        )
        .unwrap_err();
        assert!(reason.contains("yarn 1"), "{reason}");
        let reason = plan_with(
            LockfileType::YarnLock,
            Some("4.9.4"),
            gate(Duration::days(7)),
            &query,
            &[],
        )
        .unwrap_err();
        assert!(reason.contains("4.10"), "{reason}");
        let (query, _) = answering(Ok("a while"));
        assert!(
            plan_with(
                LockfileType::YarnLock,
                Some("4.10.0"),
                gate(Duration::days(7)),
                &query,
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn bun_reads_the_first_bunfig_that_sets_the_age() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("bunfig.toml");
        let global = dir.path().join(".bunfig.toml");
        std::fs::write(&global, "[install]\nminimumReleaseAge = 2592000\n").unwrap();
        let (query, _) = answering(Err("bun is never queried"));

        let plan = plan_with(
            LockfileType::BunLock,
            Some("1.3.0"),
            gate(Duration::days(7)),
            &query,
            &[project.clone(), global.clone()],
        );
        assert_eq!(plan, Ok(None), "the global 30 days is stricter");

        std::fs::write(&project, "[install]\nminimumReleaseAge = 60\n").unwrap();
        let plan = plan_with(
            LockfileType::BunLockb,
            Some("1.3.2"),
            gate(Duration::days(7)),
            &query,
            &[project.clone(), global.clone()],
        );
        assert_eq!(
            args(plan),
            ["--minimum-release-age", "604800"],
            "the project file wins"
        );

        std::fs::write(&project, "[install\n").unwrap();
        assert!(
            plan_with(
                LockfileType::BunLock,
                Some("1.3.0"),
                gate(Duration::days(7)),
                &query,
                &[project, global]
            )
            .is_err()
        );
        assert!(
            plan_with(
                LockfileType::BunLock,
                Some("1.2.23"),
                gate(Duration::days(7)),
                &query,
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn bunfig_paths_follow_the_project_then_xdg_then_home() {
        let env = |key: &str| match key {
            "XDG_CONFIG_HOME" => Some("/xdg".into()),
            "HOME" => Some("/home/u".into()),
            _ => None,
        };
        assert_eq!(
            bunfig_paths(Path::new("/p"), &env),
            [
                PathBuf::from("/p/bunfig.toml"),
                PathBuf::from("/xdg/.bunfig.toml"),
                PathBuf::from("/home/u/.bunfig.toml"),
            ]
        );
    }

    #[test]
    fn tools_without_a_setting_are_unenforced() {
        let (query, _) = answering(Err("never queried"));
        for (lockfile, tool) in [
            (LockfileType::PoetryLock, "poetry"),
            (LockfileType::CargoLock, "cargo"),
            (LockfileType::GemfileLock, "bundle"),
            (LockfileType::PackagesLockJson, "dotnet"),
            (LockfileType::TerraformLock, "terraform"),
        ] {
            let reason =
                plan_with(lockfile, None, gate(Duration::days(7)), &query, &[]).unwrap_err();
            assert_eq!(reason, format!("{tool} has no release-age setting"));
        }
    }
}
