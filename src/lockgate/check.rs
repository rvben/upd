//! Reading refreshed lockfiles back against the cooldown.
//!
//! Every refreshed lockfile upd can read is read back once it is done, gated
//! or not, since a tool's own gate exempts packages and admits releases that
//! have no publish date. Every entry the refresh introduced is looked up in
//! its registry, and a release published inside the cooldown is reported.
//! Cargo's young crates are moved back first, so only the ones that could not
//! be held remain.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};

use super::{GateReport, GateStatus, LockEntry, ReleaseAgeGate, cargo, gemfile, merge_reports};
use crate::lockfile::LockfileType;
use crate::lockscan::{npm::parse_npm_lock, poetry::parse_poetry_lock, uv::parse_uv_lock};
use crate::output::{LockfileCooldownEntry, LockfileHold};
use crate::path_display::display_path;
use crate::registry::{PyPiRegistry, Registry, VersionMeta};
use crate::updater::declared_index_urls;

/// How many registry lookups one lockfile check runs at once.
const CONCURRENT_LOOKUPS: usize = 16;

/// The registries a refreshed lockfile's publish dates are read from, and
/// where they read from, so an entry locked from anywhere else is named
/// rather than looked up in the wrong place.
pub struct LockRegistries<'a> {
    pub pypi: &'a dyn Registry,
    pub npm: &'a dyn Registry,
    pub crates_io: &'a dyn Registry,
    pub rubygems: &'a dyn Registry,
    /// The index URLs `pypi` reads, in order, each with a registry reading
    /// only that index.
    pub pypi_indexes: Vec<(String, &'a dyn Registry)>,
    /// The registry URL `npm` reads unscoped packages from.
    pub npm_registry: String,
    /// The registry URL `npm` reads each `@scope`'s packages from.
    pub npm_scopes: HashMap<String, String>,
}

/// How many entries a warning names before it counts the rest.
const NAMED_ENTRIES: usize = 5;

/// What reading the refreshed lockfiles back found.
#[derive(Debug, Default)]
pub struct LockCooldownCheck {
    /// Locked releases published inside the cooldown.
    pub findings: Vec<LockfileCooldownEntry>,
    /// Refreshes that ran without the cooldown, and entries whose publish
    /// date could not be checked.
    pub warnings: Vec<String>,
    pub holds: Vec<LockfileHold>,
    /// Lockfiles left carrying a hold that was refused, each with why; every
    /// one fails the run.
    pub errors: Vec<(PathBuf, String)>,
}

/// What a registry listed for one package.
pub(crate) enum Metas {
    Listed(Vec<VersionMeta>),
    Failed(String),
}

/// Registry listings for one lockfile, each package looked up once.
pub(crate) struct MetaCache<'a> {
    registry: &'a dyn Registry,
    entries: HashMap<String, Metas>,
}

impl<'a> MetaCache<'a> {
    pub(crate) fn new(registry: &'a dyn Registry) -> Self {
        Self {
            registry,
            entries: HashMap::new(),
        }
    }

    /// Look up every name not yet listed.
    pub(crate) async fn fetch<'n>(&mut self, names: impl IntoIterator<Item = &'n str>) {
        let mut missing: Vec<&str> = names
            .into_iter()
            .filter(|name| !self.entries.contains_key(*name))
            .collect();
        missing.sort_unstable();
        missing.dedup();
        let registry = self.registry;
        let fetched: Vec<(String, Metas)> = stream::iter(missing)
            .map(|name| async move {
                let metas = match registry.list_versions(name).await {
                    Ok(list) => Metas::Listed(list),
                    Err(e) => Metas::Failed(format!("{e:#}")),
                };
                (name.to_string(), metas)
            })
            .buffer_unordered(CONCURRENT_LOOKUPS)
            .collect()
            .await;
        self.entries.extend(fetched);
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Metas> {
        self.entries.get(name)
    }
}

/// The registry a lockfile's entries resolve from, for the lockfiles upd can
/// read back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    PyPI,
    Npm,
    CratesIo,
    RubyGems,
}

impl Source {
    fn of(lockfile_type: LockfileType) -> Option<Self> {
        match lockfile_type {
            LockfileType::UvLock | LockfileType::PoetryLock => Some(Source::PyPI),
            LockfileType::PackageLockJson | LockfileType::NpmShrinkwrap => Some(Source::Npm),
            LockfileType::CargoLock => Some(Source::CratesIo),
            LockfileType::GemfileLock => Some(Source::RubyGems),
            LockfileType::YarnLock
            | LockfileType::PnpmLock
            | LockfileType::BunLock
            | LockfileType::BunLockb
            | LockfileType::GoSum
            | LockfileType::PackagesLockJson
            | LockfileType::TerraformLock => None,
        }
    }

    fn registry<'a>(self, registries: &LockRegistries<'a>) -> &'a dyn Registry {
        match self {
            Source::PyPI => registries.pypi,
            Source::Npm => registries.npm,
            Source::CratesIo => registries.crates_io,
            Source::RubyGems => registries.rubygems,
        }
    }

    /// Whether a listed version is the locked one. PyPI spells one release
    /// several ways (`1.0` and `1.0.0`); the other registries do not.
    fn same_version(self, listed: &str, locked: &str) -> bool {
        match self {
            Source::PyPI => {
                crate::version::pep440::compare_versions(listed, locked)
                    == Some(std::cmp::Ordering::Equal)
            }
            Source::Npm | Source::CratesIo | Source::RubyGems => listed == locked,
        }
    }
}

/// Where a URL points, as [`url_key`] reads it.
type UrlKey = (String, String, Option<u16>, String);

/// A lockfile entry with the registry URLs it records: the same release from
/// another index is another entry, which that index may have published on a
/// different date. Empty for an entry from the lockfile's public registry
/// that records no URL.
type Located = (LockEntry, Vec<UrlKey>);

/// The registry entries of one lockfile, by whether upd reads their registry.
struct Entries {
    /// Entries from a registry upd reads publish dates from, each with the
    /// position of the PyPI index to read it from, or `None` for the
    /// lockfile's own registry.
    read: Vec<(Located, Option<usize>)>,
    /// Entries from any other registry, whose publish dates upd cannot see.
    elsewhere: Vec<Located>,
    /// What the reader could not cover.
    warnings: Vec<String>,
}

/// Where a lockfile entry was resolved from.
enum Origin {
    /// A registry upd reads, as in [`Entries::read`].
    Read(Option<usize>),
    /// A registry upd does not read.
    Elsewhere,
    /// No registry: a git or file dependency.
    Unregistered,
}

/// The registry entries in one lockfile's `content`. A PyPI entry is matched
/// against `indexes`, the PyPI indexes upd reads for this lockfile.
fn read_entries(
    lockfile_type: LockfileType,
    path: &Path,
    content: &str,
    registries: &LockRegistries<'_>,
    indexes: &[(&str, &dyn Registry)],
) -> Result<Entries, String> {
    let split = |read: Vec<LockEntry>, elsewhere: Vec<(LockEntry, Vec<&str>)>| Entries {
        read: read
            .into_iter()
            .map(|entry| ((entry, Vec::new()), None))
            .collect(),
        elsewhere: elsewhere
            .into_iter()
            .map(|(entry, urls)| (entry, urls.into_iter().filter_map(url_key).collect()))
            .collect(),
        warnings: Vec::new(),
    };
    let scan = match lockfile_type {
        LockfileType::UvLock => parse_uv_lock(path, content),
        LockfileType::PoetryLock => parse_poetry_lock(path, content),
        LockfileType::PackageLockJson | LockfileType::NpmShrinkwrap => {
            parse_npm_lock(path, content)
        }
        LockfileType::CargoLock => {
            let (crates_io, other) = cargo::registry_entries(content)?;
            let other = other
                .iter()
                .map(|(entry, index)| (entry.clone(), vec![index.as_str()]))
                .collect();
            return Ok(split(crates_io, other));
        }
        _ => {
            let (rubygems, other) = gemfile::gem_entries(content);
            let other = other
                .iter()
                .map(|(entry, remotes)| {
                    (entry.clone(), remotes.iter().map(String::as_str).collect())
                })
                .collect();
            return Ok(split(rubygems, other));
        }
    }
    .map_err(|e| format!("{e:#}"))?;
    let mut entries = Entries {
        read: Vec::new(),
        elsewhere: Vec::new(),
        warnings: scan.warnings,
    };
    // With one index configured, the lockfile's own registry reads exactly
    // it, and keeps the run's cache.
    let configured = registries.pypi_indexes.len();
    for package in scan.packages {
        let origin = match lockfile_type {
            LockfileType::UvLock | LockfileType::PoetryLock => {
                match pypi_index_position(package.index.as_deref(), indexes) {
                    Some(position) if configured == 1 && position == 0 => Origin::Read(None),
                    Some(position) => Origin::Read(Some(position)),
                    None => Origin::Elsewhere,
                }
            }
            _ => match npm_tarball_is_read(&package.name, package.index.as_deref(), registries) {
                Some(true) => Origin::Read(None),
                Some(false) => Origin::Elsewhere,
                None => Origin::Unregistered,
            },
        };
        let entry = (
            (package.name, package.version),
            package
                .index
                .as_deref()
                .and_then(url_key)
                .into_iter()
                .collect(),
        );
        match origin {
            Origin::Read(index) => entries.read.push((entry, index)),
            Origin::Elsewhere => entries.elsewhere.push(entry),
            Origin::Unregistered => {}
        }
    }
    Ok(entries)
}

/// A URL's scheme, host, port and path without trailing slashes: where it
/// points, without the credentials, query or fragment it may carry. Parsing
/// lowercases an http(s) host and drops its default port.
fn url_key(url: &str) -> Option<UrlKey> {
    let parsed = url::Url::parse(url).ok()?;
    Some((
        parsed.scheme().to_string(),
        parsed.host_str()?.to_string(),
        parsed.port_or_known_default(),
        parsed.path().trim_end_matches('/').to_string(),
    ))
}

/// Where in `indexes` a PyPI entry the lockfile records as resolved from
/// `index` (`None`: the default, pypi.org) is read from, if anywhere. An
/// index is the same with or without its `/simple` suffix.
fn pypi_index_position(index: Option<&str>, indexes: &[(&str, &dyn Registry)]) -> Option<usize> {
    let key = |url: &str| {
        url_key(url).map(|(scheme, host, port, path)| {
            let path = path.strip_suffix("/simple").unwrap_or(&path).to_string();
            (scheme, host, port, path)
        })
    };
    let index = key(index.unwrap_or("https://pypi.org"))?;
    indexes
        .iter()
        .position(|(url, _)| key(url).as_ref() == Some(&index))
}

/// Whether an npm entry whose tarball is `resolved` comes from the registry
/// upd reads `name` from; `None` when it is no registry tarball at all (a git
/// or file dependency). An entry recording no tarball is fetched from that
/// registry, and so is one recorded on registry.npmjs.org, whose host npm
/// replaces with the configured registry's when it installs.
fn npm_tarball_is_read(
    name: &str,
    resolved: Option<&str>,
    registries: &LockRegistries<'_>,
) -> Option<bool> {
    let Some(resolved) = resolved else {
        return Some(true);
    };
    let (scheme, host, port, path) = url_key(resolved)?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    if host == "registry.npmjs.org" {
        return Some(true);
    }
    let registry = name
        .split_once('/')
        .filter(|(scope, _)| scope.starts_with('@'))
        .and_then(|(scope, _)| registries.npm_scopes.get(scope))
        .unwrap_or(&registries.npm_registry);
    let Some((r_scheme, r_host, r_port, r_path)) = url_key(registry) else {
        return Some(false);
    };
    Some(
        scheme == r_scheme
            && host == r_host
            && port == r_port
            && path
                .strip_prefix(&r_path)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/')),
    )
}

/// The warning naming the entries a refresh introduced from a registry upd
/// does not read, when there are any.
fn unread_warning(lockfile: &str, cooldown: &str, mut entries: Vec<LockEntry>) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    entries.sort();
    entries.dedup();
    let mut named: Vec<String> = entries
        .iter()
        .take(NAMED_ENTRIES)
        .map(|(name, version)| format!("{name} {version}"))
        .collect();
    if entries.len() > NAMED_ENTRIES {
        named.push(format!("and {} more", entries.len() - NAMED_ENTRIES));
    }
    let (count, subject) = match entries.len() {
        1 => ("1 new entry comes".to_string(), "it was"),
        n => (format!("{n} new entries come"), "they were"),
    };
    Some(format!(
        "{lockfile}: {count} from a registry upd does not read ({}), so {subject} not checked against the {cooldown} cooldown",
        named.join(", ")
    ))
}

/// Read back every lockfile a refresh under a cooldown rewrote. Holds Cargo's
/// young crates, then reports each entry the refreshes introduced that was
/// published inside the cooldown.
pub async fn check_refreshed_lockfiles(
    reports: Vec<GateReport>,
    registries: LockRegistries<'_>,
    verbose: bool,
) -> LockCooldownCheck {
    let mut check = LockCooldownCheck::default();
    for report in merge_reports(reports) {
        let lockfile = display_path(&report.lockfile);
        let cooldown = report.gate.humanized();
        let source = Source::of(report.lockfile_type);
        let dropped = match &report.status {
            GateStatus::Enforced => None,
            GateStatus::Unenforced { reason } => source.is_none().then_some(reason),
            GateStatus::Bypassed { reason } => Some(reason),
        };
        if let Some(reason) = dropped {
            let unchecked = if source.is_none() {
                "; its new entries were not checked"
            } else {
                ""
            };
            check.warnings.push(format!(
                "{lockfile} was refreshed without the {cooldown} cooldown ({reason}){unchecked}"
            ));
        }
        let Some(source) = source else {
            continue;
        };
        let declared: Vec<(String, PyPiRegistry)> = match source {
            Source::PyPI => declared_index_urls(&report.lockfile)
                .into_iter()
                .map(|url| {
                    let registry = PyPiRegistry::from_url(&url);
                    (url, registry)
                })
                .collect(),
            _ => Vec::new(),
        };
        let indexes: Vec<(&str, &dyn Registry)> = registries
            .pypi_indexes
            .iter()
            .map(|(url, registry)| (url.as_str(), *registry))
            .chain(
                declared
                    .iter()
                    .map(|(url, registry)| (url.as_str(), registry as &dyn Registry)),
            )
            .collect();

        let mut metas = MetaCache::new(source.registry(&registries));
        let notes = if report.lockfile_type == LockfileType::CargoLock {
            let held = cargo::hold_young_crates(&report, &mut metas, verbose).await;
            check.holds.extend(held.holds);
            check
                .errors
                .extend(held.error.map(|error| (report.lockfile.clone(), error)));
            held.notes
        } else {
            HashMap::new()
        };

        let unreadable = |why: String| {
            format!(
                "{lockfile} could not be read back to check it against the {cooldown} cooldown ({why})"
            )
        };
        let content = match std::fs::read(&report.lockfile) {
            Ok(bytes) => bytes,
            Err(e) => {
                check.warnings.push(unreadable(e.to_string()));
                continue;
            }
        };
        let current = match read_entries(
            report.lockfile_type,
            &report.lockfile,
            &String::from_utf8_lossy(&content),
            &registries,
            &indexes,
        ) {
            Ok(read) => read,
            Err(e) => {
                check.warnings.push(unreadable(e));
                continue;
            }
        };
        check.warnings.extend(current.warnings);
        let before: HashSet<Located> = report
            .before
            .as_deref()
            .and_then(|bytes| {
                read_entries(
                    report.lockfile_type,
                    &report.lockfile,
                    &String::from_utf8_lossy(bytes),
                    &registries,
                    &indexes,
                )
                .ok()
            })
            .map(|entries| {
                entries
                    .read
                    .into_iter()
                    .map(|(entry, _)| entry)
                    .chain(entries.elsewhere)
                    .collect()
            })
            .unwrap_or_default();
        let mut seen = HashSet::new();
        let mut is_new = |entry: &Located| !before.contains(entry) && seen.insert(entry.clone());
        let introduced: Vec<(Located, Option<usize>)> = current
            .read
            .into_iter()
            .filter(|(entry, _)| is_new(entry))
            .collect();
        let unread: Vec<LockEntry> = current
            .elsewhere
            .into_iter()
            .filter(&mut is_new)
            .map(|(entry, _)| entry)
            .collect();
        check
            .warnings
            .extend(unread_warning(&lockfile, &cooldown, unread));

        let names = |index: Option<usize>| {
            introduced
                .iter()
                .filter(move |(_, from)| *from == index)
                .map(|(((name, _), _), _)| name.as_str())
        };
        metas.fetch(names(None)).await;
        let mut index_metas: HashMap<usize, MetaCache<'_>> = HashMap::new();
        for position in introduced.iter().filter_map(|(_, from)| *from) {
            if let Entry::Vacant(slot) = index_metas.entry(position) {
                let mut cache = MetaCache::new(indexes[position].1);
                cache.fetch(names(Some(position))).await;
                slot.insert(cache);
            }
        }
        for (((name, version), _), from) in introduced {
            let metas = from.map_or(&metas, |position| &index_metas[&position]);
            match verdict(metas, source, report.gate, &name, &version) {
                Verdict::Admitted => {}
                Verdict::Young(published_at) => {
                    let note = notes.get(&(name.clone(), version.clone())).cloned();
                    check.findings.push(LockfileCooldownEntry {
                        lockfile: lockfile.clone(),
                        package: name,
                        version,
                        published_at,
                        cooldown: cooldown.clone(),
                        note,
                    });
                }
                Verdict::Unknown(why) => check.warnings.push(format!(
                    "{lockfile}: {name} {version} could not be checked against the {cooldown} cooldown ({why})"
                )),
            }
        }
    }
    check
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Admitted,
    Young(DateTime<Utc>),
    Unknown(String),
}

fn verdict(
    metas: &MetaCache<'_>,
    source: Source,
    gate: ReleaseAgeGate,
    name: &str,
    version: &str,
) -> Verdict {
    let list = match metas.get(name) {
        Some(Metas::Listed(list)) => list,
        Some(Metas::Failed(e)) => {
            return Verdict::Unknown(format!("the registry lookup failed: {e}"));
        }
        None => return Verdict::Unknown("the registry was not asked".to_string()),
    };
    match list
        .iter()
        .find(|meta| source.same_version(&meta.version, version))
    {
        Some(meta) => match meta.published_at {
            Some(at) if gate.admits(at) => Verdict::Admitted,
            Some(at) => Verdict::Young(at),
            None => Verdict::Unknown("the registry lists no publish date for it".to_string()),
        },
        None => Verdict::Unknown("the registry does not list it".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MockRegistry;
    use chrono::Duration;
    use std::path::PathBuf;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-15T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn gate() -> ReleaseAgeGate {
        ReleaseAgeGate::new(Duration::days(7), now()).unwrap()
    }

    fn uv_lock(entries: &[(&str, &str)]) -> String {
        let mut lock = String::from("version = 1\n");
        for (name, version) in entries {
            lock.push_str(&format!(
                "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = {{ registry = \"https://pypi.org/simple\" }}\n"
            ));
        }
        lock
    }

    struct Project {
        _dir: tempfile::TempDir,
        lockfile: PathBuf,
    }

    fn project(name: &str, content: &str) -> Project {
        let dir = tempfile::tempdir().unwrap();
        let lockfile = dir.path().join(name);
        std::fs::write(&lockfile, content).unwrap();
        Project {
            _dir: dir,
            lockfile,
        }
    }

    fn report(
        project: &Project,
        lockfile_type: LockfileType,
        status: GateStatus,
        before: Option<String>,
    ) -> GateReport {
        GateReport {
            lockfile: project.lockfile.clone(),
            lockfile_type,
            gate: gate(),
            status,
            before: before.map(String::into_bytes),
            keep: Vec::new(),
        }
    }

    fn unenforced() -> GateStatus {
        GateStatus::Unenforced {
            reason: "the tool has no release-age setting".to_string(),
        }
    }

    /// `registry` for every ecosystem, reading the public registries.
    fn registries(registry: &MockRegistry) -> LockRegistries<'_> {
        LockRegistries {
            pypi: registry,
            npm: registry,
            crates_io: registry,
            rubygems: registry,
            pypi_indexes: vec![("https://pypi.org".to_string(), registry as &dyn Registry)],
            npm_registry: "https://registry.npmjs.org".to_string(),
            npm_scopes: HashMap::new(),
        }
    }

    async fn run(reports: Vec<GateReport>, pypi: &MockRegistry) -> LockCooldownCheck {
        check_refreshed_lockfiles(reports, registries(pypi), false).await
    }

    /// A registry listing each of `names` at `version`, released a day ago.
    fn young(names: &[&str], version: &str) -> MockRegistry {
        names
            .iter()
            .fold(MockRegistry::new("young"), |registry, name| {
                registry.with_version_meta(name, version, days(1), false, false)
            })
    }

    fn found(check: &LockCooldownCheck) -> Vec<&str> {
        let mut packages: Vec<&str> = check.findings.iter().map(|f| f.package.as_str()).collect();
        packages.sort_unstable();
        packages
    }

    #[tokio::test]
    async fn uv_entries_from_an_index_upd_does_not_read_are_named_not_checked() {
        let entry = |name: &str, index: &str| {
            format!(
                "\n[[package]]\nname = \"{name}\"\nversion = \"2.0\"\nsource = {{ registry = \"{index}\" }}\n"
            )
        };
        let lock = project(
            "uv.lock",
            &format!(
                "version = 1\n{}{}{}{}{}",
                entry("public", "https://pypi.org/simple"),
                entry("mirrored", "https://Mirror.example:443/simple/"),
                entry("private", "https://private.example/simple"),
                entry("kept", "https://private.example/simple"),
                entry("flat", "../wheels"),
            ),
        );
        let before = format!(
            "version = 1\n{}",
            entry("kept", "https://private.example/simple")
        );
        let registry = young(&["public", "mirrored", "private", "kept", "flat"], "2.0");
        let mut registries = registries(&registry);
        registries.pypi_indexes.push((
            "https://user:secret@mirror.example/simple".to_string(),
            &registry as &dyn Registry,
        ));

        let check = check_refreshed_lockfiles(
            vec![report(
                &lock,
                LockfileType::UvLock,
                unenforced(),
                Some(before),
            )],
            registries,
            false,
        )
        .await;

        assert_eq!(found(&check), ["mirrored", "public"]);
        assert_eq!(check.warnings.len(), 1, "{:?}", check.warnings);
        assert!(
            check.warnings[0].ends_with("uv.lock: 2 new entries come from a registry upd does not read (flat 2.0, private 2.0), so they were not checked against the 7d cooldown"),
            "{:?}",
            check.warnings
        );
    }

    /// Several configured indexes can list the same release with different
    /// dates; the one the lockfile says it resolved from decides.
    #[tokio::test]
    async fn each_pypi_entry_is_dated_by_the_index_its_lockfile_records() {
        let entry = |name: &str| {
            format!(
                "\n[[package]]\nname = \"{name}\"\nversion = \"2.0\"\nsource = {{ registry = \"https://second.example/simple\" }}\n"
            )
        };
        let lock = project(
            "uv.lock",
            &format!("version = 1\n{}{}", entry("alpha"), entry("beta")),
        );
        let first = MockRegistry::new("first")
            .with_version_meta("alpha", "2.0", days(30), false, false)
            .with_version_meta("beta", "2.0", days(1), false, false);
        let second = MockRegistry::new("second")
            .with_version_meta("alpha", "2.0", days(1), false, false)
            .with_version_meta("beta", "2.0", days(30), false, false);
        let mut registries = registries(&first);
        registries.pypi_indexes = vec![
            (
                "https://first.example/simple".to_string(),
                &first as &dyn Registry,
            ),
            (
                "https://second.example/simple".to_string(),
                &second as &dyn Registry,
            ),
        ];

        let check = check_refreshed_lockfiles(
            vec![report(&lock, LockfileType::UvLock, unenforced(), None)],
            registries,
            false,
        )
        .await;

        assert_eq!(found(&check), ["alpha"]);
        assert!(check.warnings.is_empty(), "{:?}", check.warnings);
    }

    #[tokio::test]
    async fn the_same_release_moved_to_another_index_is_checked_again() {
        let lock_from = |index: &str| {
            format!(
                "version = 1\n\n[[package]]\nname = \"alpha\"\nversion = \"2.0\"\nsource = {{ registry = \"https://{index}.example/simple\" }}\n"
            )
        };
        let lock = project("uv.lock", &lock_from("second"));
        let first =
            MockRegistry::new("first").with_version_meta("alpha", "2.0", days(30), false, false);
        let second =
            MockRegistry::new("second").with_version_meta("alpha", "2.0", days(1), false, false);
        let mut registries = registries(&first);
        registries.pypi_indexes = vec![
            (
                "https://first.example/simple".to_string(),
                &first as &dyn Registry,
            ),
            (
                "https://second.example/simple".to_string(),
                &second as &dyn Registry,
            ),
        ];

        let check = check_refreshed_lockfiles(
            vec![report(
                &lock,
                LockfileType::UvLock,
                GateStatus::Enforced,
                Some(lock_from("first")),
            )],
            registries,
            false,
        )
        .await;

        assert_eq!(found(&check), ["alpha"]);
        assert!(check.warnings.is_empty(), "{:?}", check.warnings);
    }

    #[tokio::test]
    async fn poetry_entries_from_a_legacy_source_upd_does_not_read_are_named_not_checked() {
        let lock = project(
            "poetry.lock",
            "[[package]]\nname = \"default\"\nversion = \"2.0\"\n\n\
             [[package]]\nname = \"indexed\"\nversion = \"2.0\"\n\n\
             [package.source]\ntype = \"legacy\"\nurl = \"https://pypi.org/simple/\"\nreference = \"pypi\"\n\n\
             [[package]]\nname = \"legacy\"\nversion = \"2.0\"\n\n\
             [package.source]\ntype = \"legacy\"\nurl = \"https://private.example/simple\"\nreference = \"private\"\n",
        );
        let registry = young(&["default", "indexed", "legacy"], "2.0");

        let check = run(
            vec![report(&lock, LockfileType::PoetryLock, unenforced(), None)],
            &registry,
        )
        .await;

        assert_eq!(found(&check), ["default", "indexed"]);
        assert_eq!(check.warnings.len(), 1, "{:?}", check.warnings);
        assert!(
            check.warnings[0].ends_with("poetry.lock: 1 new entry comes from a registry upd does not read (legacy 2.0), so it was not checked against the 7d cooldown"),
            "{:?}",
            check.warnings
        );
    }

    #[tokio::test]
    async fn npm_entries_from_a_registry_upd_does_not_read_are_named_not_checked() {
        let lock = project(
            "package-lock.json",
            r#"{"lockfileVersion": 3, "packages": {
  "": {"name": "t", "version": "1.0.0"},
  "node_modules/plain": {"version": "2.0.0", "resolved": "https://registry.npmjs.org/plain/-/plain-2.0.0.tgz"},
  "node_modules/mirrored": {"version": "2.0.0", "resolved": "https://mirror.example/npm/mirrored/-/mirrored-2.0.0.tgz"},
  "node_modules/bundled": {"version": "2.0.0"},
  "node_modules/@corp/lib": {"version": "2.0.0", "resolved": "https://npm.corp.example/@corp/lib/-/lib-2.0.0.tgz"},
  "node_modules/@corp/stray": {"version": "2.0.0", "resolved": "https://mirror.example/npm/@corp/stray/-/stray-2.0.0.tgz"},
  "node_modules/yarned": {"version": "2.0.0", "resolved": "https://registry.yarnpkg.com/yarned/-/yarned-2.0.0.tgz"},
  "node_modules/lookalike": {"version": "2.0.0", "resolved": "https://mirror.example/npm-other/lookalike/-/lookalike-2.0.0.tgz"},
  "node_modules/forked": {"version": "2.0.0", "resolved": "git+ssh://git@github.com/example/forked.git#abc"}
}}"#,
        );
        let registry = young(
            &[
                "plain",
                "mirrored",
                "bundled",
                "@corp/lib",
                "@corp/stray",
                "yarned",
                "lookalike",
                "forked",
            ],
            "2.0.0",
        );
        let mut registries = registries(&registry);
        registries.npm_registry = "https://mirror.example/npm/".to_string();
        registries
            .npm_scopes
            .insert("@corp".to_string(), "https://npm.corp.example".to_string());

        let check = check_refreshed_lockfiles(
            vec![report(
                &lock,
                LockfileType::PackageLockJson,
                unenforced(),
                None,
            )],
            registries,
            false,
        )
        .await;

        assert_eq!(
            found(&check),
            ["@corp/lib", "bundled", "mirrored", "plain"],
            "a git dependency is no registry release"
        );
        assert_eq!(check.warnings.len(), 1, "{:?}", check.warnings);
        assert!(
            check.warnings[0].ends_with("package-lock.json: 3 new entries come from a registry upd does not read (@corp/stray 2.0.0, lookalike 2.0.0, yarned 2.0.0), so they were not checked against the 7d cooldown"),
            "{:?}",
            check.warnings
        );
    }

    #[tokio::test]
    async fn cargo_entries_from_another_registry_are_named_not_checked() {
        let lock = project(
            "Cargo.lock",
            "version = 4\n\n\
             [[package]]\nname = \"t\"\nversion = \"0.1.0\"\n\n\
             [[package]]\nname = \"serde\"\nversion = \"2.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
             [[package]]\nname = \"internal\"\nversion = \"2.0.0\"\nsource = \"sparse+https://cargo.corp.example/index/\"\n\n\
             [[package]]\nname = \"vendored\"\nversion = \"2.0.0\"\nsource = \"registry+https://alt.example/index\"\n\n\
             [[package]]\nname = \"forked\"\nversion = \"2.0.0\"\nsource = \"git+https://github.com/example/forked?rev=abc#abc\"\n",
        );
        // Nothing older is listed, so no hold is attempted.
        let registry = young(&["serde", "internal", "vendored", "forked"], "2.0.0");

        let check = run(
            vec![report(&lock, LockfileType::CargoLock, unenforced(), None)],
            &registry,
        )
        .await;

        assert_eq!(found(&check), ["serde"]);
        assert_eq!(check.warnings.len(), 1, "{:?}", check.warnings);
        assert!(
            check.warnings[0].ends_with("Cargo.lock: 2 new entries come from a registry upd does not read (internal 2.0.0, vendored 2.0.0), so they were not checked against the 7d cooldown"),
            "{:?}",
            check.warnings
        );
    }

    #[tokio::test]
    async fn gems_from_another_server_are_named_not_checked() {
        let lock = project(
            "Gemfile.lock",
            "GIT\n  remote: https://github.com/example/gitgem.git\n  revision: abc\n  specs:\n    gitgem (2.0.0)\n\n\
             GEM\n  remote: https://rubygems.org/\n  specs:\n    public (2.0.0)\n\n\
             GEM\n  remote: https://gems.corp.example/\n  specs:\n    private (2.0.0)\n\n\
             PLATFORMS\n  ruby\n",
        );
        let registry = young(&["gitgem", "public", "private"], "2.0.0");

        let check = run(
            vec![report(&lock, LockfileType::GemfileLock, unenforced(), None)],
            &registry,
        )
        .await;

        assert_eq!(found(&check), ["public"]);
        assert_eq!(check.warnings.len(), 1, "{:?}", check.warnings);
        assert!(
            check.warnings[0].ends_with("Gemfile.lock: 1 new entry comes from a registry upd does not read (private 2.0.0), so it was not checked against the 7d cooldown"),
            "{:?}",
            check.warnings
        );
    }

    /// Checks one lockfile, locked from `before` to `after`, that locks
    /// `name` at 2.0.0 published a day ago.
    async fn moved(
        lockfile_type: LockfileType,
        name: &str,
        before: &str,
        after: &str,
    ) -> LockCooldownCheck {
        let lock = project(lockfile_type.filename(), after);
        let registry = young(&[name], "2.0.0");
        run(
            vec![report(
                &lock,
                lockfile_type,
                GateStatus::Enforced,
                Some(before.to_string()),
            )],
            &registry,
        )
        .await
    }

    #[tokio::test]
    async fn a_crate_moved_to_another_registry_at_the_same_version_is_checked_again() {
        let lock = |source: &str| {
            format!(
                "version = 4\n\n[[package]]\nname = \"serde\"\nversion = \"2.0.0\"\nsource = \"{source}\"\n"
            )
        };
        let crates_io = lock("sparse+https://index.crates.io/");
        let github = lock("registry+https://github.com/rust-lang/crates.io-index");
        let corp = lock("sparse+https://cargo.corp.example/index/");
        let alt = lock("registry+https://alt.example/index");
        let unread = "Cargo.lock: 1 new entry comes from a registry upd does not read (serde 2.0.0), so it was not checked against the 7d cooldown";

        let check = moved(LockfileType::CargoLock, "serde", &crates_io, &corp).await;
        assert!(found(&check).is_empty());
        assert_eq!(check.warnings.len(), 1, "{:?}", check.warnings);
        assert!(check.warnings[0].ends_with(unread), "{:?}", check.warnings);

        let check = moved(LockfileType::CargoLock, "serde", &corp, &alt).await;
        assert_eq!(check.warnings.len(), 1, "{:?}", check.warnings);
        assert!(check.warnings[0].ends_with(unread), "{:?}", check.warnings);

        let check = moved(LockfileType::CargoLock, "serde", &corp, &crates_io).await;
        assert_eq!(found(&check), ["serde"]);

        let check = moved(LockfileType::CargoLock, "serde", &github, &crates_io).await;
        assert!(
            found(&check).is_empty() && check.warnings.is_empty(),
            "both name crates.io: {check:?}"
        );
    }

    #[tokio::test]
    async fn a_gem_moved_to_another_server_at_the_same_version_is_checked_again() {
        let lock = |remotes: &[&str]| {
            let remotes: String = remotes
                .iter()
                .map(|remote| format!("  remote: {remote}\n"))
                .collect();
            format!("GEM\n{remotes}  specs:\n    private (2.0.0)\n\nPLATFORMS\n  ruby\n")
        };
        let public = lock(&["https://rubygems.org/"]);
        let corp = lock(&["https://gems.corp.example/"]);
        let unread = "Gemfile.lock: 1 new entry comes from a registry upd does not read (private 2.0.0), so it was not checked against the 7d cooldown";

        let check = moved(LockfileType::GemfileLock, "private", &public, &corp).await;
        assert_eq!(check.warnings.len(), 1, "{:?}", check.warnings);
        assert!(check.warnings[0].ends_with(unread), "{:?}", check.warnings);

        let check = moved(
            LockfileType::GemfileLock,
            "private",
            &corp,
            &lock(&["https://gems.corp.example/", "https://gems.other.example/"]),
        )
        .await;
        assert_eq!(check.warnings.len(), 1, "{:?}", check.warnings);
        assert!(check.warnings[0].ends_with(unread), "{:?}", check.warnings);

        let check = moved(LockfileType::GemfileLock, "private", &corp, &public).await;
        assert_eq!(found(&check), ["private"]);
    }

    #[test]
    fn an_unread_warning_names_five_entries_and_counts_the_rest() {
        let entries: Vec<LockEntry> = (1..=7)
            .map(|n| (format!("p{n}"), "1.0".to_string()))
            .collect();
        assert_eq!(
            unread_warning("uv.lock", "7d", entries).as_deref(),
            Some(
                "uv.lock: 7 new entries come from a registry upd does not read (p1 1.0, p2 1.0, p3 1.0, p4 1.0, p5 1.0, and 2 more), so they were not checked against the 7d cooldown"
            )
        );
        assert_eq!(unread_warning("uv.lock", "7d", Vec::new()), None);
    }

    fn days(n: i64) -> Option<DateTime<Utc>> {
        Some(now() - Duration::days(n))
    }

    #[tokio::test]
    async fn only_entries_the_refresh_introduced_are_checked() {
        let lock = project(
            "uv.lock",
            &uv_lock(&[("young", "2.0"), ("kept", "1.0"), ("older", "3.0")]),
        );
        let pypi = MockRegistry::new("pypi")
            .with_version_meta("young", "2.0.0", days(1), false, false)
            .with_version_meta("kept", "1.0", days(1), false, false)
            .with_version_meta("older", "3.0", days(30), false, false);
        let before = uv_lock(&[("young", "1.0"), ("kept", "1.0"), ("older", "2.0")]);

        let check = run(
            vec![report(
                &lock,
                LockfileType::UvLock,
                unenforced(),
                Some(before),
            )],
            &pypi,
        )
        .await;

        assert_eq!(check.findings.len(), 1, "{:?}", check.findings);
        let finding = &check.findings[0];
        assert_eq!(
            (finding.package.as_str(), finding.version.as_str()),
            ("young", "2.0"),
            "2.0.0 on PyPI is the 2.0 the lockfile holds"
        );
        assert_eq!(Some(finding.published_at), days(1));
        assert_eq!(finding.cooldown, "7d");
        assert_eq!(finding.note, None);
        assert!(check.warnings.is_empty(), "{:?}", check.warnings);
    }

    #[tokio::test]
    async fn a_lockfile_with_nothing_readable_before_is_checked_whole() {
        let lock = project("uv.lock", &uv_lock(&[("young", "2.0")]));
        let pypi =
            MockRegistry::new("pypi").with_version_meta("young", "2.0", days(1), false, false);

        let check = run(
            vec![report(
                &lock,
                LockfileType::UvLock,
                unenforced(),
                Some("not [ toml".to_string()),
            )],
            &pypi,
        )
        .await;
        assert_eq!(check.findings.len(), 1);
    }

    #[tokio::test]
    async fn an_enforced_refresh_is_still_read_back() {
        let lock = project("uv.lock", &uv_lock(&[("young", "2.0")]));
        let pypi =
            MockRegistry::new("pypi").with_version_meta("young", "2.0", days(1), false, false);

        let check = run(
            vec![report(
                &lock,
                LockfileType::UvLock,
                GateStatus::Enforced,
                None,
            )],
            &pypi,
        )
        .await;
        assert_eq!(found(&check), ["young"]);
        assert!(check.warnings.is_empty(), "{:?}", check.warnings);
    }

    #[tokio::test]
    async fn a_bypassed_refresh_is_named_and_still_checked() {
        let lock = project("uv.lock", &uv_lock(&[("young", "2.0")]));
        let pypi =
            MockRegistry::new("pypi").with_version_meta("young", "2.0", days(1), false, false);
        let status = GateStatus::Bypassed {
            reason: "the gated refresh failed: error: No solution found".to_string(),
        };

        let check = run(
            vec![report(&lock, LockfileType::UvLock, status, None)],
            &pypi,
        )
        .await;
        assert_eq!(check.findings.len(), 1);
        assert_eq!(check.warnings.len(), 1, "{:?}", check.warnings);
        let warning = &check.warnings[0];
        assert!(warning.contains("uv.lock was refreshed without the 7d cooldown (the gated refresh failed: error: No solution found)"), "{warning}");
        assert!(!warning.contains("not checked"), "{warning}");
    }

    #[tokio::test]
    async fn a_lockfile_upd_cannot_read_is_named_as_unchecked() {
        let lock = project("pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
        let pypi = MockRegistry::new("pypi");

        let check = run(
            vec![report(&lock, LockfileType::PnpmLock, unenforced(), None)],
            &pypi,
        )
        .await;
        assert!(check.findings.is_empty());
        assert_eq!(check.warnings.len(), 1);
        assert!(
            check.warnings[0].ends_with("pnpm-lock.yaml was refreshed without the 7d cooldown (the tool has no release-age setting); its new entries were not checked"),
            "{:?}",
            check.warnings
        );
    }

    #[tokio::test]
    async fn entries_whose_age_is_unknown_are_warned_about() {
        let lock = project(
            "uv.lock",
            &uv_lock(&[
                ("down", "1.0"),
                ("undated", "1.0"),
                ("unlisted", "0.1"),
                ("ancient", "0.1"),
            ]),
        );
        let pypi = MockRegistry::new("pypi")
            .with_unavailable_versions("down")
            .with_version_meta("undated", "1.0", None, false, false)
            .with_version_meta("unlisted", "5.0", days(1), false, false)
            .with_version_meta("ancient", "5.0", days(100), false, false);

        let check = run(
            vec![report(&lock, LockfileType::UvLock, unenforced(), None)],
            &pypi,
        )
        .await;

        assert!(check.findings.is_empty(), "{:?}", check.findings);
        let mut warned: Vec<&str> = check
            .warnings
            .iter()
            .map(|w| w.split(": ").nth(1).unwrap().split(' ').next().unwrap())
            .collect();
        warned.sort_unstable();
        assert_eq!(
            warned,
            ["ancient", "down", "undated", "unlisted"],
            "a version the registry does not list has no known age, however old the listed ones are: {:?}",
            check.warnings
        );
    }

    #[tokio::test]
    async fn two_refreshes_of_one_lockfile_are_checked_once_from_the_first_bytes() {
        let lock = project("uv.lock", &uv_lock(&[("a", "2.0"), ("b", "2.0")]));
        let pypi = MockRegistry::new("pypi")
            .with_version_meta("a", "2.0", days(1), false, false)
            .with_version_meta("b", "2.0", days(1), false, false);

        let check = run(
            vec![
                report(
                    &lock,
                    LockfileType::UvLock,
                    unenforced(),
                    Some(uv_lock(&[("a", "1.0"), ("b", "1.0")])),
                ),
                report(
                    &lock,
                    LockfileType::UvLock,
                    unenforced(),
                    Some(uv_lock(&[("a", "2.0"), ("b", "1.0")])),
                ),
            ],
            &pypi,
        )
        .await;

        let mut packages: Vec<&str> = check.findings.iter().map(|f| f.package.as_str()).collect();
        packages.sort_unstable();
        assert_eq!(packages, ["a", "b"]);
    }
}
