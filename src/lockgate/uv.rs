//! uv's release-age gate.
//!
//! `uv lock --exclude-newer <cutoff>` resolves under the cooldown, but it
//! would move a release the lockfile already holds back if that release is
//! younger than the cutoff, and it records the cutoff in `uv.lock`, where
//! `uv lock --locked` rejects it in any project that does not configure the
//! same one. So the gated pass exempts every locked package at its own upload
//! time, the recorded settings are removed from its lockfile, and a plain pass
//! confirms the result, which is checked for anything it moved.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, SubsecRound, Utc};

use super::native::format_instant;
use super::{EnvLookup, Query, ReleaseAgeGate};

const SETTINGS: [&str; 2] = ["exclude-newer", "exclude-newer-package"];

/// The arguments of the gated `uv lock` pass for the lockfile in `dir`, or why
/// uv cannot be gated there.
pub(crate) fn plan(
    dir: &Path,
    lock_before: Option<&[u8]>,
    gate: ReleaseAgeGate,
    query: Query<'_>,
    env: EnvLookup<'_>,
) -> Result<Vec<String>, String> {
    let help = query("uv", &["lock", "--help"])
        .map_err(|e| format!("uv's lock options could not be read ({e})"))?;
    if !help.contains("--exclude-newer-package") {
        return Err("this uv predates --exclude-newer-package".to_string());
    }
    if let Some(source) = configured_setting(dir, env)? {
        return Err(format!("{source} sets its own exclude-newer"));
    }
    let lock = match lock_before {
        Some(bytes) => Some(parse_lock(bytes)?),
        None => None,
    };
    if lock
        .as_ref()
        .and_then(|doc| doc.get("options"))
        .is_some_and(|options| SETTINGS.iter().any(|key| options.get(key).is_some()))
    {
        return Err("uv.lock was resolved with its own exclude-newer".to_string());
    }

    let cutoff = gate.cutoff().trunc_subsecs(0);
    let mut args = vec!["--exclude-newer".to_string(), format_instant(cutoff)];
    if let Some(lock) = &lock {
        for (name, uploaded) in latest_uploads(lock) {
            if uploaded > cutoff {
                args.push("--exclude-newer-package".to_string());
                args.push(format!(
                    "{name}={}",
                    format_instant(ceil_to_second(uploaded))
                ));
            }
        }
    }
    Ok(args)
}

fn ceil_to_second(at: DateTime<Utc>) -> DateTime<Utc> {
    let whole = at.trunc_subsecs(0);
    if whole == at {
        at
    } else {
        whole + Duration::seconds(1)
    }
}

fn parse_lock(bytes: &[u8]) -> Result<toml::Table, String> {
    std::str::from_utf8(bytes)
        .map_err(|e| e.to_string())
        .and_then(|text| text.parse::<toml::Table>().map_err(|e| e.to_string()))
        .map_err(|e| format!("uv.lock could not be parsed ({e})"))
}

/// Where a uv release-age setting that `--exclude-newer` would override is
/// configured: the environment, the project configuration uv discovers from
/// `dir`, or the user and system configuration files.
fn configured_setting(dir: &Path, env: EnvLookup<'_>) -> Result<Option<String>, String> {
    for key in ["UV_EXCLUDE_NEWER", "UV_EXCLUDE_NEWER_PACKAGE"] {
        if env(key).is_some_and(|value| !value.is_empty()) {
            return Ok(Some(key.to_string()));
        }
    }

    for ancestor in dir.ancestors() {
        let uv_toml = ancestor.join("uv.toml");
        if let Some(doc) = read_toml(&uv_toml)? {
            if sets_exclude_newer(&doc) {
                return Ok(Some(uv_toml.display().to_string()));
            }
            break;
        }
        let pyproject = ancestor.join("pyproject.toml");
        if let Some(doc) = read_toml(&pyproject)?
            && let Some(tool_uv) = doc.get("tool").and_then(|tool| tool.get("uv"))
        {
            if tool_uv.as_table().is_some_and(sets_exclude_newer) {
                return Ok(Some(pyproject.display().to_string()));
            }
            break;
        }
    }

    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(file) = env("UV_CONFIG_FILE").filter(|v| !v.is_empty()) {
        files.push(PathBuf::from(file));
    }
    match env("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(xdg) => files.push(PathBuf::from(xdg).join("uv/uv.toml")),
        None => {
            if let Some(home) = env("HOME").filter(|v| !v.is_empty()) {
                files.push(PathBuf::from(home).join(".config/uv/uv.toml"));
            }
        }
    }
    files.push(PathBuf::from("/etc/uv/uv.toml"));
    for file in files {
        if read_toml(&file)?.is_some_and(|doc| sets_exclude_newer(&doc)) {
            return Ok(Some(file.display().to_string()));
        }
    }
    Ok(None)
}

fn sets_exclude_newer(table: &toml::Table) -> bool {
    SETTINGS.iter().any(|key| table.contains_key(*key))
}

/// A TOML file, `None` when there is none at `path`.
fn read_toml(path: &Path) -> Result<Option<toml::Table>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse()
            .map(Some)
            .map_err(|e| format!("{} could not be parsed ({e})", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} could not be read ({e})", path.display())),
    }
}

/// Registry packages in the lock by name, each with the latest upload time
/// across its distributions.
fn latest_uploads(lock: &toml::Table) -> Vec<(String, DateTime<Utc>)> {
    let mut latest: Vec<(String, DateTime<Utc>)> = Vec::new();
    for package in registry_packages(lock) {
        let Some(name) = package.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let files = package.get("sdist").into_iter().chain(
            package
                .get("wheels")
                .and_then(|w| w.as_array())
                .into_iter()
                .flatten(),
        );
        for uploaded in files.filter_map(|file| upload_time(file.get("upload-time")?)) {
            match latest.iter_mut().find(|(held, _)| held == name) {
                Some((_, at)) => *at = (*at).max(uploaded),
                None => latest.push((name.to_string(), uploaded)),
            }
        }
    }
    latest
}

fn registry_packages(lock: &toml::Table) -> impl Iterator<Item = &toml::Value> {
    lock.get("package")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
        .filter(|package| {
            package
                .get("source")
                .and_then(|s| s.as_table())
                .is_some_and(|source| source.contains_key("registry"))
        })
}

/// uv writes `upload-time` as a quoted RFC 3339 string; a TOML datetime is
/// read the same way.
fn upload_time(value: &toml::Value) -> Option<DateTime<Utc>> {
    let text = match value {
        toml::Value::String(text) => text.clone(),
        toml::Value::Datetime(at) => at.to_string(),
        _ => return None,
    };
    DateTime::parse_from_rfc3339(&text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// Why the gated pass cannot be kept: a registry package it resolved lower
/// than the lockfile held it before the run. `None` when nothing moved down.
pub(crate) fn downgrade(before: &[u8], after: &[u8]) -> Option<String> {
    let (Ok(before), Ok(after)) = (parse_lock(before), parse_lock(after)) else {
        return None;
    };
    let before = locked_versions(&before);
    let mut moved: Vec<String> = Vec::new();
    for (name, now) in locked_versions(&after) {
        let Some(was) = before.get(&name) else {
            continue;
        };
        let (dropped, added) = changed(was, &now);
        if dropped.is_empty() || added.is_empty() {
            continue;
        }
        let lower = |a: &str, b: &str| {
            crate::version::pep440::compare_versions(a, b) == Some(std::cmp::Ordering::Less)
        };
        // With as many forks before as after, each moved in order. With a
        // different count, which fork became which is unknown, so every new
        // version below one that went away counts as moved down.
        let pairs: Vec<(&str, &str)> = if dropped.len() == added.len() {
            dropped.iter().copied().zip(added.iter().copied()).collect()
        } else {
            dropped
                .iter()
                .flat_map(|was| added.iter().map(move |now| (*was, *now)))
                .collect()
        };
        for (was, now) in pairs {
            let entry = format!("{name} {was} to {now}");
            if lower(now, was) && !moved.contains(&entry) {
                moved.push(entry);
            }
        }
    }
    if moved.is_empty() {
        return None;
    }
    moved.sort();
    Some(format!(
        "resolving under it would downgrade {}",
        moved.join(", ")
    ))
}

/// Every version each registry package is locked at, by name, lowest first.
/// A package forked across environment markers is locked at several.
fn locked_versions(lock: &toml::Table) -> HashMap<String, Vec<String>> {
    let mut versions: HashMap<String, Vec<String>> = HashMap::new();
    for package in registry_packages(lock) {
        let (Some(name), Some(version)) = (
            package.get("name").and_then(|v| v.as_str()),
            package.get("version").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let held = versions.entry(name.to_string()).or_default();
        if !held.iter().any(|v| v == version) {
            held.push(version.to_string());
        }
    }
    for held in versions.values_mut() {
        held.sort_by(|a, b| {
            crate::version::pep440::compare_versions(a, b).unwrap_or_else(|| a.cmp(b))
        });
    }
    versions
}

/// The versions of one package `was` held that `now` no longer does, and the
/// ones `now` holds that `was` did not, each lowest first.
fn changed<'a>(was: &'a [String], now: &'a [String]) -> (Vec<&'a str>, Vec<&'a str>) {
    let dropped = was
        .iter()
        .filter(|v| !now.contains(v))
        .map(String::as_str)
        .collect();
    let added = now
        .iter()
        .filter(|v| !was.contains(v))
        .map(String::as_str)
        .collect();
    (dropped, added)
}

/// `uv.lock` without the release-age settings a gated pass recorded under
/// `[options]`, dropping the table when nothing else is left in it. The plan
/// refuses a project or lockfile that sets them itself, so every one removed
/// is the gate's own.
pub(crate) fn strip_cutoff(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut doc = std::str::from_utf8(bytes)
        .map_err(|e| e.to_string())
        .and_then(|text| {
            text.parse::<toml_edit::DocumentMut>()
                .map_err(|e| e.to_string())
        })
        .map_err(|e| format!("uv.lock could not be parsed after the gated refresh ({e})"))?;
    if let Some(options) = doc.get_mut("options").and_then(|o| o.as_table_like_mut()) {
        for key in SETTINGS {
            options.remove(key);
        }
        if options.is_empty() {
            doc.remove("options");
        }
    }
    Ok(doc.to_string().into_bytes())
}

/// Why a plain pass cannot be reported as keeping to the cooldown: the
/// registry packages it locked at another version than `gated` held. `None`
/// when it kept every one.
pub(crate) fn drift(gated: &[u8], after: &[u8]) -> Option<String> {
    let after = match parse_lock(after) {
        Ok(lock) => locked_versions(&lock),
        Err(reason) => return Some(reason),
    };
    let gated = parse_lock(gated)
        .map(|lock| locked_versions(&lock))
        .unwrap_or_default();
    let none = Vec::new();
    let mut moved: Vec<String> = Vec::new();
    for (name, now) in &after {
        let (dropped, added) = changed(gated.get(name).unwrap_or(&none), now);
        if dropped.len() == added.len() {
            moved.extend(
                dropped
                    .iter()
                    .zip(&added)
                    .map(|(was, now)| format!("{name} {was} to {now}")),
            );
        } else {
            moved.extend(added.iter().map(|now| format!("{name} {now}")));
        }
    }
    if moved.is_empty() {
        return None;
    }
    moved.sort();
    Some(format!(
        "uv resolved it again once the cutoff was removed, locking {}",
        moved.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    const HELP: &str = "      --exclude-newer <EXCLUDE_NEWER>\n      --exclude-newer-package <EXCLUDE_NEWER_PACKAGE>\n";

    fn gate() -> ReleaseAgeGate {
        let now = DateTime::parse_from_rfc3339("2026-09-15T12:00:00.750Z")
            .unwrap()
            .with_timezone(&Utc);
        ReleaseAgeGate::new(Duration::days(7), now).unwrap()
    }

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    fn help(_: &str, args: &[&str]) -> Result<String, String> {
        assert_eq!(args, ["lock", "--help"]);
        Ok(HELP.to_string())
    }

    fn entry(name: &str, version: &str, source: &str, sdist: &str, wheel: &str) -> String {
        format!(
            "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = {source}\nsdist = {{ url = \"u\", upload-time = {sdist} }}\nwheels = [\n    {{ url = \"w\", upload-time = {wheel} }},\n]\n"
        )
    }

    fn registry(name: &str, version: &str, sdist: &str, wheel: &str) -> String {
        entry(
            name,
            version,
            "{ registry = \"https://pypi.org/simple\" }",
            sdist,
            wheel,
        )
    }

    #[test]
    fn the_gated_pass_exempts_locked_packages_younger_than_the_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let lock = format!(
            "version = 1\n{}{}{}{}",
            // The wheel is the latest upload, and carries a fractional second.
            registry(
                "requests",
                "2.31.0",
                "\"2026-09-13T00:00:00Z\"",
                "\"2026-09-13T08:00:00.250Z\""
            ),
            registry(
                "idna",
                "3.10",
                "\"2025-01-01T00:00:00Z\"",
                "\"2025-01-01T00:00:00Z\""
            ),
            // An unquoted TOML datetime reads the same.
            registry(
                "certifi",
                "2026.9.1",
                "2026-09-10T00:00:00Z",
                "2026-09-10T00:00:00Z"
            ),
            entry(
                "local",
                "0.1.0",
                "{ editable = \".\" }",
                "\"2026-09-14T00:00:00Z\"",
                "\"2026-09-14T00:00:00Z\""
            ),
        );

        let args = plan(dir.path(), Some(lock.as_bytes()), gate(), &help, &no_env).unwrap();

        assert_eq!(
            args,
            [
                "--exclude-newer",
                "2026-09-08T12:00:00Z",
                "--exclude-newer-package",
                "requests=2026-09-13T08:00:01Z",
                "--exclude-newer-package",
                "certifi=2026-09-10T00:00:00Z",
            ]
        );
    }

    #[test]
    fn a_lock_that_does_not_exist_yet_is_gated_without_exemptions() {
        let dir = tempfile::tempdir().unwrap();
        let args = plan(dir.path(), None, gate(), &help, &no_env).unwrap();
        assert_eq!(args, ["--exclude-newer", "2026-09-08T12:00:00Z"]);
    }

    #[test]
    fn a_uv_without_package_exemptions_is_unenforced() {
        let dir = tempfile::tempdir().unwrap();
        let old = |_: &str, _: &[&str]| Ok("      --exclude-newer <EXCLUDE_NEWER>\n".to_string());
        let reason = plan(dir.path(), None, gate(), &old, &no_env).unwrap_err();
        assert!(reason.contains("--exclude-newer-package"), "{reason}");

        let broken = |_: &str, _: &[&str]| Err("uv exploded".to_string());
        let reason = plan(dir.path(), None, gate(), &broken, &no_env).unwrap_err();
        assert!(reason.contains("uv exploded"), "{reason}");
    }

    #[test]
    fn a_lock_resolved_with_its_own_cutoff_is_unenforced() {
        let dir = tempfile::tempdir().unwrap();
        let lock = "version = 1\n\n[options]\nexclude-newer = \"2026-01-01T00:00:00Z\"\n";
        let reason = plan(dir.path(), Some(lock.as_bytes()), gate(), &help, &no_env).unwrap_err();
        assert!(reason.contains("uv.lock"), "{reason}");
    }

    #[test]
    fn the_environment_setting_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        for key in ["UV_EXCLUDE_NEWER", "UV_EXCLUDE_NEWER_PACKAGE"] {
            let env = |asked: &str| (asked == key).then(|| OsString::from("2026-01-01"));
            let reason = plan(dir.path(), None, gate(), &help, &env).unwrap_err();
            assert!(reason.contains(key), "{reason}");
        }
        let empty = |asked: &str| (asked == "UV_EXCLUDE_NEWER").then(OsString::new);
        assert!(plan(dir.path(), None, gate(), &help, &empty).is_ok());
    }

    #[test]
    fn project_configuration_is_found_up_to_the_first_uv_config() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("ws/member");
        std::fs::create_dir_all(&project).unwrap();

        // A pyproject without [tool.uv] is not uv configuration, so the search
        // continues to the workspace root that sets the cutoff.
        std::fs::write(project.join("pyproject.toml"), "[project]\nname = \"m\"\n").unwrap();
        std::fs::write(
            root.path().join("ws/pyproject.toml"),
            "[tool.uv]\nexclude-newer = \"2026-01-01T00:00:00Z\"\n",
        )
        .unwrap();
        let reason = plan(&project, None, gate(), &help, &no_env).unwrap_err();
        assert!(reason.contains("ws/pyproject.toml"), "{reason}");

        // The nearer [tool.uv] without the setting ends the search.
        std::fs::write(
            project.join("pyproject.toml"),
            "[tool.uv]\npackage = true\n",
        )
        .unwrap();
        assert!(plan(&project, None, gate(), &help, &no_env).is_ok());

        // uv.toml beside it wins over the pyproject in the same directory.
        std::fs::write(
            project.join("uv.toml"),
            "exclude-newer-package = { a = \"2026-01-01\" }\n",
        )
        .unwrap();
        let reason = plan(&project, None, gate(), &help, &no_env).unwrap_err();
        assert!(reason.contains("uv.toml"), "{reason}");
    }

    #[test]
    fn user_configuration_is_respected() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("p");
        let xdg = root.path().join("xdg");
        std::fs::create_dir_all(xdg.join("uv")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            xdg.join("uv/uv.toml"),
            "exclude-newer = \"2026-01-01T00:00:00Z\"\n",
        )
        .unwrap();

        let env = |key: &str| (key == "XDG_CONFIG_HOME").then(|| xdg.clone().into_os_string());
        let reason = plan(&project, None, gate(), &help, &env).unwrap_err();
        assert!(reason.contains("xdg/uv/uv.toml"), "{reason}");

        let explicit = root.path().join("explicit.toml");
        std::fs::write(&explicit, "exclude-newer = \"2026-01-01T00:00:00Z\"\n").unwrap();
        let env = |key: &str| (key == "UV_CONFIG_FILE").then(|| explicit.clone().into_os_string());
        let reason = plan(&project, None, gate(), &help, &env).unwrap_err();
        assert!(reason.contains("explicit.toml"), "{reason}");
    }

    #[test]
    fn a_downgrade_names_every_package_that_moved_down() {
        let before = format!(
            "version = 1\n{}{}{}",
            registry(
                "requests",
                "2.32.0",
                "\"2026-01-01T00:00:00Z\"",
                "\"2026-01-01T00:00:00Z\""
            ),
            registry(
                "idna",
                "3.10",
                "\"2026-01-01T00:00:00Z\"",
                "\"2026-01-01T00:00:00Z\""
            ),
            registry(
                "certifi",
                "2026.1.1",
                "\"2026-01-01T00:00:00Z\"",
                "\"2026-01-01T00:00:00Z\""
            ),
        );
        let after = format!(
            "version = 1\n{}{}{}{}",
            registry(
                "requests",
                "2.31.0",
                "\"2026-01-01T00:00:00Z\"",
                "\"2026-01-01T00:00:00Z\""
            ),
            registry(
                "idna",
                "3.9",
                "\"2026-01-01T00:00:00Z\"",
                "\"2026-01-01T00:00:00Z\""
            ),
            registry(
                "certifi",
                "2026.2.1",
                "\"2026-01-01T00:00:00Z\"",
                "\"2026-01-01T00:00:00Z\""
            ),
            registry(
                "new",
                "1.0",
                "\"2026-01-01T00:00:00Z\"",
                "\"2026-01-01T00:00:00Z\""
            ),
        );
        assert_eq!(
            downgrade(before.as_bytes(), after.as_bytes()).as_deref(),
            Some("resolving under it would downgrade idna 3.10 to 3.9, requests 2.32.0 to 2.31.0")
        );
        assert_eq!(downgrade(before.as_bytes(), before.as_bytes()), None);
    }

    #[test]
    fn the_recorded_cutoff_is_removed_and_every_other_option_kept() {
        let gated = "version = 1\nrevision = 3\n\n[options]\nresolution-mode = \"lowest-direct\"\nexclude-newer = \"2026-09-08T12:00:00Z\"\n\n[options.exclude-newer-package]\nrequests = \"2026-09-13T08:00:01Z\"\n\n[[package]]\nname = \"requests\"\nversion = \"2.31.0\"\n";
        let stripped = String::from_utf8(strip_cutoff(gated.as_bytes()).unwrap()).unwrap();
        assert!(!stripped.contains("exclude-newer"), "{stripped}");
        let doc: toml::Table = stripped.parse().unwrap();
        assert_eq!(
            doc["options"]["resolution-mode"].as_str(),
            Some("lowest-direct"),
            "{stripped}"
        );
        assert_eq!(doc["revision"].as_integer(), Some(3), "{stripped}");
        assert_eq!(
            doc["package"][0]["version"].as_str(),
            Some("2.31.0"),
            "{stripped}"
        );
    }

    #[test]
    fn an_options_table_holding_only_the_cutoff_is_removed() {
        let gated = "version = 1\n\n[options]\nexclude-newer = \"2026-09-08T12:00:00Z\"\n\n[options.exclude-newer-package]\nrequests = \"2026-09-13T08:00:01Z\"\n\n[[package]]\nname = \"requests\"\nversion = \"2.31.0\"\n";
        let stripped = String::from_utf8(strip_cutoff(gated.as_bytes()).unwrap()).unwrap();
        assert!(!stripped.contains("options"), "{stripped}");
        let doc: toml::Table = stripped.parse().unwrap();
        assert_eq!(
            doc["package"][0]["version"].as_str(),
            Some("2.31.0"),
            "{stripped}"
        );

        let without = "version = 1\n\n[[package]]\nname = \"requests\"\nversion = \"2.31.0\"\n";
        assert_eq!(
            strip_cutoff(without.as_bytes()).unwrap(),
            without.as_bytes(),
            "a lockfile with nothing to remove is written back unchanged"
        );
        let reason = strip_cutoff(b"[[package]\n").unwrap_err();
        assert!(reason.contains("uv.lock could not be parsed"), "{reason}");
    }

    #[test]
    fn drift_names_every_package_the_plain_pass_moved_or_added() {
        let at = "\"2026-01-01T00:00:00Z\"";
        let gated = format!(
            "version = 1\n{}{}",
            registry("requests", "2.32.0", at, at),
            registry("idna", "3.10", at, at),
        );
        assert_eq!(drift(gated.as_bytes(), gated.as_bytes()), None);

        let after = format!(
            "version = 1\n{}{}{}",
            registry("requests", "2.33.0", at, at),
            registry("idna", "3.10", at, at),
            registry("urllib3", "2.8.0", at, at),
        );
        assert_eq!(
            drift(gated.as_bytes(), after.as_bytes()).as_deref(),
            Some(
                "uv resolved it again once the cutoff was removed, locking requests 2.32.0 to 2.33.0, urllib3 2.8.0"
            )
        );

        let reason = drift(gated.as_bytes(), b"[[package]\n").unwrap();
        assert!(reason.contains("uv.lock could not be parsed"), "{reason}");
    }

    /// A package forked across environment markers is locked at several
    /// versions, and a change to the lower one is as real as to the higher.
    #[test]
    fn a_package_locked_at_several_versions_is_compared_version_by_version() {
        let at = "\"2026-01-01T00:00:00Z\"";
        let lock = |versions: &[&str]| {
            let mut lock = String::from("version = 1\n");
            for version in versions {
                lock.push_str(&registry("numpy", version, at, at));
            }
            lock
        };
        let forked = lock(&["2.3.0", "1.26.4"]);

        assert_eq!(
            drift(forked.as_bytes(), lock(&["1.26.5", "2.3.0"]).as_bytes()).as_deref(),
            Some(
                "uv resolved it again once the cutoff was removed, locking numpy 1.26.4 to 1.26.5"
            )
        );
        assert_eq!(
            drift(forked.as_bytes(), lock(&["1.26.4"]).as_bytes()),
            None,
            "dropping a fork locks nothing new"
        );
        assert_eq!(
            drift(lock(&["2.3.0"]).as_bytes(), forked.as_bytes()).as_deref(),
            Some("uv resolved it again once the cutoff was removed, locking numpy 1.26.4")
        );

        assert_eq!(
            downgrade(forked.as_bytes(), lock(&["1.26.3", "2.3.0"]).as_bytes()).as_deref(),
            Some("resolving under it would downgrade numpy 1.26.4 to 1.26.3")
        );
        assert_eq!(
            downgrade(forked.as_bytes(), lock(&["1.26.3"]).as_bytes()).as_deref(),
            Some(
                "resolving under it would downgrade numpy 1.26.4 to 1.26.3, numpy 2.3.0 to 1.26.3"
            )
        );
        assert_eq!(
            downgrade(lock(&["2.3.0"]).as_bytes(), forked.as_bytes()),
            None,
            "a new fork at a lower version moves nothing the lockfile held"
        );
        assert_eq!(
            downgrade(forked.as_bytes(), lock(&["1.26.5", "2.3.0"]).as_bytes()),
            None
        );
        assert_eq!(
            downgrade(
                lock(&["2.0", "5.0", "8.0"]).as_bytes(),
                lock(&["3.0", "9.0"]).as_bytes()
            )
            .as_deref(),
            Some("resolving under it would downgrade numpy 5.0 to 3.0, numpy 8.0 to 3.0"),
            "with fewer forks after, a fork between the ends can have moved down"
        );
    }
}
