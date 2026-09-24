//! Explicit, broad lockfile maintenance. Ordinary `--lock` stays targeted to
//! manifests changed by the update command.

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use upd::cli::{BumpLevel, Cli, OutputFormat};
use upd::config::UpdConfig;
use upd::lockfile::Snapshot;
use upd::lockscan::{LockScan, LockedPackage, cargo, npm, uv};
use upd::updater::{BumpKind, Lang, classify_bump, classify_bump_for};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Uv,
    Npm,
    Cargo,
}

impl Kind {
    fn for_path(path: &Path) -> Option<Self> {
        match path.file_name()?.to_str()? {
            "uv.lock" => Some(Self::Uv),
            "package-lock.json" | "npm-shrinkwrap.json" => Some(Self::Npm),
            "Cargo.lock" => Some(Self::Cargo),
            _ => None,
        }
    }

    fn manifest(self) -> &'static str {
        match self {
            Self::Uv => "pyproject.toml",
            Self::Npm => "package.json",
            Self::Cargo => "Cargo.toml",
        }
    }

    fn ecosystem(self) -> (&'static str, &'static str) {
        match self {
            Self::Uv => ("pypi", "python"),
            Self::Npm => ("npm", "node"),
            Self::Cargo => ("crates.io", "rust"),
        }
    }

    fn lang(self) -> Lang {
        match self {
            Self::Uv => Lang::Python,
            Self::Npm => Lang::Node,
            Self::Cargo => Lang::Rust,
        }
    }

    fn scan(self, path: &Path) -> Result<LockScan> {
        match self {
            Self::Uv => uv::scan_uv_lock(path),
            Self::Npm => npm::scan_npm_lock(path),
            Self::Cargo => cargo::scan_cargo_lock(path),
        }
    }

    fn command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Uv => ("uv", &["lock", "--upgrade"]),
            Self::Npm => (
                "npm",
                &[
                    "update",
                    "--package-lock-only",
                    "--ignore-scripts",
                    "--no-audit",
                    "--no-fund",
                ],
            ),
            Self::Cargo => ("cargo", &["update"]),
        }
    }
}

#[derive(Serialize)]
struct Change {
    package: String,
    from: Vec<String>,
    to: Vec<String>,
    bump: Option<&'static str>,
}

#[derive(Serialize)]
struct Entry {
    lockfile: String,
    status: &'static str,
    changes: Vec<Change>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn discover(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut found = BTreeSet::new();
    for path in paths {
        if !path.exists() {
            bail!("{} does not exist", path.display());
        }
        if path.is_file() {
            Kind::for_path(path)
                .with_context(|| format!("{} is not a supported lockfile", path.display()))?;
            found.insert(path.clone());
            continue;
        }
        for result in WalkBuilder::new(path).standard_filters(true).build() {
            let entry = result.with_context(|| format!("scanning {}", path.display()))?;
            let candidate = entry.path();
            if candidate.is_file() && Kind::for_path(candidate).is_some() {
                found.insert(candidate.to_path_buf());
            }
        }
    }
    Ok(found.into_iter().collect())
}

fn versions(packages: &[LockedPackage]) -> BTreeMap<String, Vec<String>> {
    let mut result: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for package in packages {
        result
            .entry(package.name.clone())
            .or_default()
            .push(package.version.clone());
    }
    for versions in result.values_mut() {
        versions.sort();
    }
    result
}

fn sources(packages: &[LockedPackage]) -> BTreeMap<String, Vec<(String, Option<String>)>> {
    let mut result: BTreeMap<String, Vec<(String, Option<String>)>> = BTreeMap::new();
    for package in packages {
        result
            .entry(package.name.clone())
            .or_default()
            .push((package.version.clone(), package.index.clone()));
    }
    for entries in result.values_mut() {
        entries.sort();
    }
    result
}

fn bump(kind: Kind, from: &[String], to: &[String]) -> Option<&'static str> {
    if from.len() != 1 || to.len() != 1 {
        return None;
    }
    let comparable = match kind {
        Kind::Uv => upd::version::pep440::compare_versions(&from[0], &to[0]),
        _ => upd::version::semver_util::compare_versions(&from[0], &to[0]),
    };
    comparable?;
    let level = match kind {
        Kind::Uv => classify_bump_for(Lang::Python, &from[0], &to[0]),
        _ => classify_bump(&from[0], &to[0]),
    };
    Some(match level {
        BumpKind::Major => "major",
        BumpKind::Minor => "minor",
        BumpKind::Patch => "patch",
    })
}

fn changed(kind: Kind, before: &LockScan, after: &LockScan) -> Vec<Change> {
    let old = versions(&before.packages);
    let new = versions(&after.packages);
    let names: BTreeSet<_> = old.keys().chain(new.keys()).cloned().collect();
    names
        .into_iter()
        .filter_map(|package| {
            let from = old.get(&package).cloned().unwrap_or_default();
            let to = new.get(&package).cloned().unwrap_or_default();
            (from != to).then(|| Change {
                bump: bump(kind, &from, &to),
                package,
                from,
                to,
            })
        })
        .collect()
}

fn protected(name: &str, config: &UpdConfig) -> bool {
    config.should_ignore(name) || config.get_pinned_version(name).is_some()
}

fn exceeds_limit(level: Option<&str>, limit: BumpLevel) -> bool {
    matches!(
        (level, limit),
        (Some("major"), BumpLevel::Minor | BumpLevel::Patch)
            | (Some("minor"), BumpLevel::Patch)
            | (None, BumpLevel::Minor | BumpLevel::Patch)
    )
}

fn refresh_one(path: &Path, cli: &Cli) -> Result<Entry> {
    let kind = Kind::for_path(path).context("unsupported lockfile")?;
    let dir = path.parent().context("lockfile has no parent directory")?;
    let manifest = dir.join(kind.manifest());
    if !manifest.is_file() {
        bail!("{} has no adjacent {}", path.display(), kind.manifest());
    }
    if kind == Kind::Npm
        && dir.join("package-lock.json").is_file()
        && dir.join("npm-shrinkwrap.json").is_file()
    {
        bail!(
            "{} contains both npm lockfile names; choose one before maintenance",
            dir.display()
        );
    }
    let config = match &cli.config {
        Some(path) => UpdConfig::load_from_path_with_error(path).map_err(anyhow::Error::msg)?,
        None => UpdConfig::discover(dir)
            .map_err(anyhow::Error::msg)?
            .map(|(config, _)| config)
            .unwrap_or_default(),
    };
    if !config
        .selected_ecosystems(&cli.langs)
        .map_err(anyhow::Error::msg)?
        .is_none_or(|selected| selected.contains(&kind.lang()))
    {
        return Ok(Entry {
            lockfile: path.display().to_string(),
            status: "skipped",
            changes: vec![],
            error: None,
        });
    }
    let (ecosystem, lang) = kind.ecosystem();
    let cooldown = config.to_cooldown_policy(cli.min_age.as_deref())?;
    if cooldown.is_enabled_for(ecosystem, Some(lang)) {
        bail!(
            "{}: lock maintenance under a cooldown is not yet supported for {}; no files changed",
            path.display(),
            ecosystem
        );
    }
    let before = kind.scan(path)?;
    if !before.warnings.is_empty() {
        bail!("{}: {}", path.display(), before.warnings.join("; "));
    }
    if cli.is_effective_dry_run() {
        return Ok(Entry {
            lockfile: path.display().to_string(),
            status: "planned",
            changes: vec![],
            error: None,
        });
    }

    let manifest_before = std::fs::read(&manifest)?;
    let lock_before = std::fs::read(path)?;
    let snapshot = Snapshot::capture(&[path.to_path_buf(), manifest.clone()]);
    snapshot.ensure_restorable()?;
    let (tool, default_args) = kind.command();
    let mut args: Vec<String> = default_args.iter().map(|arg| (*arg).to_string()).collect();
    if kind == Kind::Uv && before.packages.iter().any(|p| protected(&p.name, &config)) {
        let candidates: BTreeSet<_> = before
            .packages
            .iter()
            .filter(|p| !protected(&p.name, &config))
            .map(|p| p.name.as_str())
            .collect();
        if candidates.is_empty() {
            return Ok(Entry {
                lockfile: path.display().to_string(),
                status: "unchanged",
                changes: vec![],
                error: None,
            });
        }
        args = vec!["lock".into()];
        for package in candidates {
            args.push("--upgrade-package".into());
            args.push(package.into());
        }
    }
    let output = Command::new(tool)
        .args(&args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("could not start {tool} for {}", path.display()));
    let result = (|| -> Result<Entry> {
        let output = output?;
        if !output.status.success() {
            bail!(
                "{} failed: {}",
                tool,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if std::fs::read(&manifest)? != manifest_before {
            bail!("{} changed the manifest during lockfile maintenance", tool);
        }
        let after = kind.scan(path)?;
        if !after.warnings.is_empty() {
            bail!("{}", after.warnings.join("; "));
        }
        let old_sources = sources(&before.packages);
        let new_sources = sources(&after.packages);
        for package in old_sources
            .keys()
            .chain(new_sources.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
        {
            if protected(&package, &config)
                && old_sources.get(&package) != new_sources.get(&package)
            {
                bail!("protected package {package} moved or changed source");
            }
        }
        let changes = changed(kind, &before, &after);
        if let Some(limit) = cli.max_bump
            && let Some(change) = changes
                .iter()
                .find(|change| exceeds_limit(change.bump, limit))
        {
            bail!("{} exceeds --max-bump", change.package);
        }
        for change in &changes {
            if change.from.len() == 1 && change.to.len() == 1 {
                let comparison = match kind {
                    Kind::Uv => {
                        upd::version::pep440::compare_versions(&change.from[0], &change.to[0])
                    }
                    _ => {
                        upd::version::semver_util::compare_versions(&change.from[0], &change.to[0])
                    }
                };
                if comparison.is_some_and(|order| order.is_gt()) {
                    bail!("{} was downgraded", change.package);
                }
            }
        }
        Ok(Entry {
            lockfile: path.display().to_string(),
            status: if std::fs::read(path)? == lock_before {
                "unchanged"
            } else {
                "refreshed"
            },
            changes,
            error: None,
        })
    })();
    if let Err(error) = &result {
        let failures = snapshot.restore();
        if !failures.is_empty() {
            bail!(
                "{}; rollback failed: {}",
                error,
                failures
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
    }
    result
}

pub fn run(cli: &Cli, paths: &[PathBuf], json: bool) -> Result<()> {
    if cli.no_cache
        || cli.full_precision
        || cli.update_action_shas
        || cli.no_update_action_shas
        || cli.insecure
    {
        bail!(
            "lock-refresh does not support --no-cache, --full-precision, --update-action-shas, --no-update-action-shas, or --insecure"
        );
    }
    if cli.check || cli.interactive || cli.no_ignore {
        bail!("lock-refresh does not support --check, --interactive, or --no-ignore");
    }
    if !cli.only_bump.is_empty() || !cli.packages.is_empty() {
        bail!("lock-refresh supports --max-bump, but not --only-bump or --package");
    }
    if cli.lock || cli.no_lock {
        bail!("lock-refresh controls its own lockfile writes; omit --lock and --no-lock");
    }
    if cli.fields.is_some() && !json {
        bail!("--fields requires JSON output");
    }
    if cli.format == Some(OutputFormat::Sarif) {
        bail!("lock-refresh does not support SARIF output");
    }
    let selected_fields = cli
        .fields
        .as_ref()
        .map(|fields| fields.split(',').map(str::trim).collect::<BTreeSet<_>>());
    if let Some(selected) = &selected_fields {
        let valid = ["lockfile", "status", "changes", "error"];
        if selected.is_empty() || selected.iter().any(|field| !valid.contains(field)) {
            bail!("--fields accepts only: {}", valid.join(", "));
        }
    }
    let lockfiles = discover(paths)?;
    let mut entries = Vec::new();
    let mut failures = 0;
    for path in lockfiles
        .into_iter()
        .skip(cli.offset)
        .take(cli.limit.unwrap_or(usize::MAX))
    {
        match refresh_one(&path, cli) {
            Ok(entry) => entries.push(entry),
            Err(error) => {
                failures += 1;
                entries.push(Entry {
                    lockfile: path.display().to_string(),
                    status: "failed",
                    changes: vec![],
                    error: Some(format!("{error:#}")),
                });
            }
        }
    }
    if cli.quiet {
        for entry in &entries {
            if let Some(error) = &entry.error {
                eprintln!("{}: {error}", entry.lockfile);
            }
        }
    } else if json {
        let mut report = serde_json::to_value(&entries)?;
        if let Some(selected) = &selected_fields {
            for entry in report
                .as_array_mut()
                .expect("entries serialize as an array")
            {
                entry
                    .as_object_mut()
                    .expect("entries serialize as objects")
                    .retain(|field, _| selected.contains(field.as_str()));
            }
        }
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if entries.is_empty() {
        println!("No supported lockfiles found.");
    } else {
        for entry in &entries {
            println!("{}: {}", entry.lockfile, entry.status);
            for change in &entry.changes {
                println!(
                    "  {}: {} → {}{}",
                    change.package,
                    change.from.join(", "),
                    change.to.join(", "),
                    change
                        .bump
                        .map(|level| format!(" ({level})"))
                        .unwrap_or_default()
                );
            }
            if let Some(error) = &entry.error {
                eprintln!("  {error}");
            }
        }
    }
    if failures > 0 {
        bail!("{failures} lockfile refresh(es) failed");
    }
    Ok(())
}
