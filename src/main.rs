use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use colored::Colorize;
use futures::stream::{self, StreamExt};

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use upd::align::{PackageAlignment, PackageOccurrence, find_alignments, scan_packages};
use upd::annotation::{self, AnnotationSource};
use upd::audit::cache::AuditCache;
use upd::audit::{
    AuditResult, Ecosystem, OsvClient, Package as AuditPackage, PackageAuditResult, Vulnerability,
};
use upd::cache::{Cache, CachedRegistry};
use upd::cli::{BumpLevel, Cli, Command, OutputMode, REVERT_TIP};
use upd::config::UpdConfig;
use upd::cooldown::CooldownPolicy;
use upd::fix::apply::{
    AppliedFix, FixApplyOptions, FixStatus, apply_fix_targets, probe_floor_target,
};
use upd::fix::{
    FixKind, FixTarget, FloorResolution, NpmOverrideForm, UnfixableTarget, resolve_floor_version,
    route_fix_targets,
};
use upd::interactive::{PendingUpdate, prompt_all};
use upd::lockfile::{
    LockfileType, RegenOutcome, RestoreFailure, Snapshot, containing_dir, detect_lockfiles,
    regenerate_lockfile,
};
use upd::lockscan;
use upd::normalize::pep503_normalize;
use upd::output::LockWriteStatus;
use upd::package_filter::PackageFilter;
use upd::path_display::display_path;
use upd::registry::{
    CratesIoRegistry, DockerRegistry, GitHubReleasesRegistry, GoProxyRegistry, GradleRegistry,
    MultiPyPiRegistry, NpmRegistry, NuGetRegistry, PyPiRegistry, RubyGemsRegistry,
    TerraformRegistry,
};
use upd::updater::{
    ActionShaUpdate, AnnotatedUpdater, BumpFilter, BumpKind, CargoTomlUpdater, CsprojUpdater,
    DEFAULT_UPDATE_ACTION_SHAS, DiscoverOptions, DockerUpdater, FileType, GemfileUpdater,
    GithubActionsUpdater, GoModUpdater, GradleUpdater, Lang, MiseUpdater, PackageJsonUpdater,
    ParseWarnings, PreCommitUpdater, PyProjectUpdater, RegistrySet, RequirementsUpdater,
    SkipStatus, SkippedUpdate, TerraformUpdater, UpdateOptions, UpdateResult, Updater,
    classify_bump, discover_files_with, ecosystem_key, read_file_safe, update_with_annotations,
    write_file_atomic,
};
use upd::version::{compare_versions, match_version_precision};

/// Walk up from `start` to find the nearest ancestor directory that contains a
/// `.git` entry (file or directory). Returns the path to that ancestor.
///
/// Handles both regular git repositories (`.git` is a directory) and
/// submodules/worktrees (`.git` is a file containing a `gitdir:` pointer).
fn find_vcs_root(start: &Path) -> Option<PathBuf> {
    let start = if start.is_file() {
        start.parent()?
    } else {
        start
    };

    let mut current = start;
    loop {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

/// Resolve the paths to scan.
///
/// If the CLI provided explicit paths, use them as-is. Otherwise, find the
/// nearest VCS root from the current working directory. If no VCS root is
/// found, return an `Err` with a user-facing message; the caller should exit 2.
fn resolve_scan_paths(cli: &Cli) -> Result<Vec<PathBuf>, String> {
    let explicit = cli.get_paths();
    if !explicit.is_empty() {
        return Ok(explicit);
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match find_vcs_root(&cwd) {
        Some(root) => Ok(vec![root]),
        None => Err("no paths given and not inside a git repository. \
Pass an explicit path, or run from inside a git repo."
            .to_string()),
    }
}

/// Classify an update as major, minor, or patch.
///
/// Delegates to the library classifier that the write-time `--max-bump` gate
/// uses, so a change can never be labelled one thing and gated as another.
fn classify_update(old: &str, new: &str) -> UpdateType {
    update_type(classify_bump(old, new))
}

fn update_type(bump: BumpKind) -> UpdateType {
    match bump {
        BumpKind::Major => UpdateType::Major,
        BumpKind::Minor => UpdateType::Minor,
        BumpKind::Patch => UpdateType::Patch,
    }
}

fn classify_path_update(path: &Path, old: &str, new: &str) -> UpdateType {
    let python = FileType::detect(path).is_some_and(|kind| kind.lang() == Lang::Python)
        || path
            .file_name()
            .is_some_and(|name| name == "uv.lock" || name == "poetry.lock");
    if python {
        update_type(upd::updater::classify_bump_for(Lang::Python, old, new))
    } else {
        classify_update(old, new)
    }
}

fn humanize_age(age: Duration) -> String {
    let seconds = age.num_seconds().max(0);
    if seconds < 60 {
        return format!("{}s ago", seconds);
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{}m ago", minutes);
    }
    let hours = minutes / 60;
    if hours < 48 {
        return format!("{}h ago", hours);
    }
    let days = hours / 24;
    if days < 14 {
        return format!("{}d ago", days);
    }
    let weeks = days / 7;
    format!("{}w ago", weeks)
}

fn humanize_cooldown(d: Duration) -> String {
    if d.num_seconds() == 0 {
        return "disabled".to_string();
    }
    if d.num_days() > 0 && d.num_days() * 86_400 == d.num_seconds() {
        return format!("{}d", d.num_days());
    }
    if d.num_hours() * 3600 == d.num_seconds() {
        return format!("{}h", d.num_hours());
    }
    format!("{}s", d.num_seconds())
}

fn init_tls(cli: &Cli) -> anyhow::Result<()> {
    upd::http::init(cli.insecure).context("Failed to initialize TLS options")?;
    if cli.insecure {
        eprintln!(
            "{}: TLS certificate verification disabled - connections are not authenticated",
            "warning".yellow().bold()
        );
    }
    Ok(())
}

fn format_held_back_line(
    name: &str,
    old: &str,
    new: &str,
    skipped_latest: &str,
    skipped_published_at: DateTime<Utc>,
    cooldown: Duration,
    now: DateTime<Utc>,
) -> String {
    let age = now - skipped_published_at;
    format!(
        "Held back {name} {old} → {new} ({skipped_latest} released {}, cooldown {})",
        humanize_age(age),
        humanize_cooldown(cooldown),
    )
}

fn format_skipped_by_cooldown_line(
    name: &str,
    skipped_latest: &str,
    skipped_published_at: Option<DateTime<Utc>>,
    cooldown: Duration,
    now: DateTime<Utc>,
) -> String {
    // No publish date means no age to state. Saying so keeps the line honest;
    // any stand-in date would render as a concrete, checkable-looking claim
    // about when the release happened.
    let released = match skipped_published_at {
        Some(published_at) => format!("released {}", humanize_age(now - published_at)),
        None => "release date unknown".to_string(),
    };
    format!(
        "Skipped {name} (only newer version {skipped_latest} {released}, cooldown {})",
        humanize_cooldown(cooldown),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateType {
    Major,
    Minor,
    Patch,
}

impl UpdateType {
    /// The wire token for this bump level, shared by the JSON `bump` field and
    /// the text report so the two always agree.
    fn as_str(self) -> &'static str {
        match self {
            UpdateType::Major => "major",
            UpdateType::Minor => "minor",
            UpdateType::Patch => "patch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ChangeKind {
    RegistryUpdate,
    ConfigPin,
    Normalization,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PlannedChange {
    kind: ChangeKind,
    path: PathBuf,
    file_type: FileType,
    package: String,
    old_version: String,
    new_version: String,
    line_num: Option<usize>,
    section: Option<String>,
}

impl PlannedChange {
    fn from_update(
        path: PathBuf,
        file_type: FileType,
        update: &(String, String, String, Option<usize>),
    ) -> Self {
        Self {
            kind: ChangeKind::RegistryUpdate,
            path,
            file_type,
            package: update.0.clone(),
            old_version: update.1.clone(),
            new_version: update.2.clone(),
            line_num: update.3,
            section: None,
        }
    }

    fn from_pinned(
        path: PathBuf,
        file_type: FileType,
        pinned: &(String, String, String, Option<usize>),
    ) -> Self {
        Self {
            kind: ChangeKind::ConfigPin,
            path,
            file_type,
            package: pinned.0.clone(),
            old_version: pinned.1.clone(),
            new_version: pinned.2.clone(),
            line_num: pinned.3,
            section: None,
        }
    }

    fn from_normalized(
        path: PathBuf,
        file_type: FileType,
        normalized: &upd::updater::NormalizedSpec,
    ) -> Self {
        Self {
            kind: ChangeKind::Normalization,
            path,
            file_type,
            package: normalized.package.clone(),
            old_version: normalized
                .previous_spec
                .clone()
                .unwrap_or_else(|| "(no specifier)".to_string()),
            new_version: normalized.new_spec.clone(),
            line_num: normalized.line_number,
            section: Some(normalized.section.clone()),
        }
    }
}

#[derive(Debug)]
struct ScannedFileResult {
    path: PathBuf,
    file_type: FileType,
    result: UpdateResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditStatus {
    Clean,
    Vulnerable,
    Incomplete,
}

#[derive(Debug, Clone)]
struct ResolvedUpdateConfig {
    config: Arc<UpdConfig>,
    path: PathBuf,
    explicit: bool,
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn log_update_config_usage(resolved: &ResolvedUpdateConfig) {
    if !resolved.explicit && !resolved.config.has_config() {
        return;
    }

    println!(
        "{}",
        format!("Using config from: {}", display_path(&resolved.path)).cyan()
    );

    if !resolved.config.ignore.is_empty() {
        println!(
            "{}",
            format!("  Ignoring {} package(s)", resolved.config.ignore.len()).dimmed()
        );
    }

    if !resolved.config.pin.is_empty() {
        println!(
            "{}",
            format!("  Pinning {} package(s)", resolved.config.pin.len()).dimmed()
        );
    }

    if !resolved.config.exclude.is_empty() {
        println!(
            "{}",
            format!(
                "  Excluding {} path pattern(s)",
                resolved.config.exclude.len()
            )
            .dimmed()
        );
    }

    if !resolved.config.include.is_empty() {
        println!(
            "{}",
            format!(
                "  Including {} annotated path pattern(s)",
                resolved.config.include.len()
            )
            .dimmed()
        );
    }
}

fn discover_update_config(start_dir: &Path) -> Result<Option<ResolvedUpdateConfig>, String> {
    Ok(
        UpdConfig::discover(start_dir)?.map(|(config, path)| ResolvedUpdateConfig {
            config: Arc::new(config),
            path,
            explicit: false,
        }),
    )
}

/// Resolve the single config that governs discovery-level settings.
///
/// Discovery-level settings (the `include`/`exclude` path globs, and the `ignore` list
/// for `align`) describe the whole scan rather than an individual file, so they
/// are resolved once from a single config: an explicit `--config` when given,
/// otherwise the nearest config discovered upward from the first scan path.
/// Falls back to an empty config when none is found.
fn resolve_root_config(cli: &Cli, paths: &[PathBuf]) -> Result<ResolvedUpdateConfig> {
    if let Some(config_path) = &cli.config {
        return Ok(ResolvedUpdateConfig {
            config: Arc::new(
                UpdConfig::load_from_path_with_error(config_path).map_err(anyhow::Error::msg)?,
            ),
            path: config_path.clone(),
            explicit: true,
        });
    }

    let start_dir = paths
        .first()
        .map(|p| {
            if p.is_dir() {
                p.clone()
            } else {
                p.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."))
            }
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    Ok(discover_update_config(&start_dir)
        .map_err(anyhow::Error::msg)?
        .unwrap_or_else(|| ResolvedUpdateConfig {
            config: Arc::new(UpdConfig::default()),
            path: start_dir,
            explicit: false,
        }))
}

fn load_update_configs(
    cli: &Cli,
    files: &[(PathBuf, FileType)],
) -> Result<HashMap<PathBuf, Option<Arc<UpdConfig>>>> {
    let explicit_config = if let Some(config_path) = &cli.config {
        Some(ResolvedUpdateConfig {
            config: Arc::new(
                UpdConfig::load_from_path_with_error(config_path).map_err(anyhow::Error::msg)?,
            ),
            path: config_path.clone(),
            explicit: true,
        })
    } else {
        None
    };

    let mut file_configs = HashMap::new();
    let mut discovered_by_dir: HashMap<PathBuf, Option<ResolvedUpdateConfig>> = HashMap::new();
    let mut logged_config_paths = HashSet::new();

    if let Some(resolved) = explicit_config.as_ref()
        && cli.verbose
        && logged_config_paths.insert(resolved.path.clone())
    {
        log_update_config_usage(resolved);
    }

    for (path, _) in files {
        let resolved = if let Some(explicit) = explicit_config.as_ref() {
            Some(explicit.clone())
        } else {
            let start_dir = path.parent().unwrap_or(path.as_path()).to_path_buf();
            if let Some(cached) = discovered_by_dir.get(&start_dir) {
                cached.clone()
            } else {
                let discovered = discover_update_config(&start_dir).map_err(anyhow::Error::msg)?;
                discovered_by_dir.insert(start_dir, discovered.clone());
                discovered
            }
        };

        if let Some(config) = resolved.as_ref()
            && cli.verbose
            && logged_config_paths.insert(config.path.clone())
        {
            log_update_config_usage(config);
        }

        file_configs.insert(path.clone(), resolved.map(|config| config.config));
    }

    Ok(file_configs)
}

#[allow(clippy::too_many_arguments)]
fn build_update_options(
    dry_run: bool,
    full_precision: bool,
    update_action_shas: Option<bool>,
    config: Option<Arc<UpdConfig>>,
    package_filter: &PackageFilter,
    langs: &[Lang],
    annotation_langs: Option<&[Lang]>,
    cooldown_policy: Option<&CooldownPolicy>,
    cooldown_notes: Arc<Mutex<BTreeMap<String, String>>>,
    bump_filter: BumpFilter,
) -> UpdateOptions {
    // Command line first, then the config file nearest this file, then the
    // built-in default.
    let action_shas = update_action_shas
        .or_else(|| config.as_ref().and_then(|c| c.update_action_shas))
        .unwrap_or(DEFAULT_UPDATE_ACTION_SHAS);

    let mut options = UpdateOptions::new(dry_run, full_precision);
    options = options.with_action_sha_updates(action_shas);
    if let Some(config) = config {
        options = options.with_config(config);
    }
    options = options.with_package_filter(package_filter.clone());
    options = options.with_langs(langs.to_vec());
    options.annotation_langs = annotation_langs.map(<[Lang]>::to_vec);
    options = options.with_bump_filter(bump_filter);
    if let Some(policy) = cooldown_policy {
        options = options.with_cooldown_policy(policy.clone(), Utc::now());
    }
    options.cooldown_unavailable_notes = cooldown_notes;
    options
}

/// The `Lang` an OSV `Ecosystem` corresponds to (mirrors the private
/// `fix::ecosystem_lang`; duplicated here since `main.rs` is a separate
/// crate from the `upd` library and cannot see its private items). Drives
/// `scan_packages`'s occurrence-map lookups and `resolve_floor_version`'s
/// per-lang version comparison for `--package` lock-only floors.
fn ecosystem_to_lang(ecosystem: Ecosystem) -> Lang {
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

/// Normalize a package name per ecosystem convention: PEP 503 for PyPI,
/// lowercase otherwise (mirrors the private `fix::normalized_name`).
fn normalized_package_name(name: &str, ecosystem: Ecosystem) -> String {
    if ecosystem == Ecosystem::PyPI {
        pep503_normalize(name)
    } else {
        name.to_lowercase()
    }
}

/// Whether `norm` (already normalized per `ecosystem`) matches any manifest
/// occurrence scanned by `scan_packages`. A `--package` name that matches a
/// manifest occurrence keeps today's update path untouched (rule 2); a name
/// matching only a scanned lockfile is a version-floor candidate.
fn matches_manifest_occurrence(
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
    norm: &str,
    ecosystem: Ecosystem,
) -> bool {
    let lang = ecosystem_to_lang(ecosystem);
    packages.iter().any(|((name, l), occs)| {
        if *l != lang || occs.is_empty() {
            return false;
        }
        let candidate = if ecosystem == Ecosystem::PyPI {
            pep503_normalize(name)
        } else {
            name.clone()
        };
        candidate == norm
    })
}

/// Whether normalization's broader PEP 508 parser recognized a declaration
/// that the ordinary occurrence scanner cannot represent (notably a bare name
/// or a parenthesized specifier).
fn recognized_in_manifest(scanned: &[ScannedFileResult], norm: &str, ecosystem: Ecosystem) -> bool {
    let lang = ecosystem_to_lang(ecosystem);
    let matches = |name: &str| normalized_package_name(name, ecosystem) == norm;
    scanned.iter().any(|file| {
        file.file_type.lang() == lang
            && (file
                .result
                .normalized
                .iter()
                .any(|entry| matches(&entry.package))
                || file.result.updated.iter().any(|(name, ..)| matches(name))
                || file.result.pinned.iter().any(|(name, ..)| matches(name))
                || file
                    .result
                    .normalize_recognized
                    .iter()
                    .any(|name| matches(name)))
    })
}

/// Whether `name` (as requested via `--package`) resolves only through a
/// scanned lockfile: no manifest occurrence, but at least one lock-scanned
/// package under the same normalized name (rule 2). Shared between the
/// non-interactive floor branch and the `--interactive` early-return note
/// (rule 9).
fn is_lock_only_package(
    locked: &upd::lockscan::LockedPackage,
    manifest_packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
) -> bool {
    let norm = normalized_package_name(&locked.name, locked.ecosystem);
    !matches_manifest_occurrence(manifest_packages, &norm, locked.ecosystem)
}

/// Match a concrete lockfile name while preserving the ecosystem-normalized
/// exact-name behaviour the lock-only path had before package globs existed.
/// Glob matching itself remains case-sensitive and operates on the name as it
/// appears in the lockfile.
fn package_filter_matches_locked(
    package_filter: &PackageFilter,
    locked: &upd::lockscan::LockedPackage,
) -> bool {
    package_filter.matches(&locked.name)
        || package_filter.patterns().iter().any(|pattern| {
            normalized_package_name(pattern, locked.ecosystem)
                == normalized_package_name(&locked.name, locked.ecosystem)
        })
}

/// The report/apply path a version floor targets for a given lock, mirroring
/// `fix::route_lock_only`'s path assignment: the host manifest for Uv/Npm,
/// the lockfile itself for Cargo (a pure lockfile mutation) and Poetry (no
/// floor mechanism exists, so the diagnostic is anchored on the lock).
fn floor_report_path(lockfile: &Path, kind: lockscan::discover::LockKind) -> PathBuf {
    let dir = lockfile.parent().unwrap_or_else(|| Path::new("."));
    match kind {
        lockscan::discover::LockKind::Uv => dir.join("pyproject.toml"),
        lockscan::discover::LockKind::Npm => dir.join("package.json"),
        lockscan::discover::LockKind::Cargo
        | lockscan::discover::LockKind::Poetry
        | lockscan::discover::LockKind::Gradle => lockfile.to_path_buf(),
    }
}

/// The manifest path used to resolve `--package` floor config (ignore/pin):
/// always the ecosystem's manifest file, even for Cargo/Poetry where the
/// floor's own report path is the lockfile.
fn floor_config_lookup_path(lockfile: &Path, kind: lockscan::discover::LockKind) -> PathBuf {
    let dir = lockfile.parent().unwrap_or_else(|| Path::new("."));
    match kind {
        lockscan::discover::LockKind::Uv | lockscan::discover::LockKind::Poetry => {
            dir.join("pyproject.toml")
        }
        lockscan::discover::LockKind::Npm => dir.join("package.json"),
        lockscan::discover::LockKind::Cargo => dir.join("Cargo.toml"),
        lockscan::discover::LockKind::Gradle => {
            if dir.join("build.gradle.kts").exists() {
                dir.join("build.gradle.kts")
            } else {
                dir.join("build.gradle")
            }
        }
    }
}

/// Grouping key for routed floor targets, mirroring the merge
/// `fix::route_fix_targets` applies to uv/npm floors: `(method, manifest,
/// normalized package)`. npm range overrides also retain their compatibility
/// branch. A uv constraint or npm $-reference covers every copy. A
/// `cargo-precise` floor lifts a single locked copy, so its locked version
/// joins the key and the copies stay apart, exactly as routing keeps them.
type FloorMergeKey = (&'static str, PathBuf, String, Option<String>);

fn floor_merge_key(target: &FixTarget) -> FloorMergeKey {
    let normalized = if target.kind == FixKind::UvConstraint {
        pep503_normalize(&target.package)
    } else {
        target.package.to_lowercase()
    };
    let copy = if target.kind == FixKind::CargoPrecise {
        Some(target.from_version.clone())
    } else if target.kind == FixKind::NpmOverride
        && target.npm_form == Some(NpmOverrideForm::CompatibleRange)
    {
        Some(
            upd::fix::npm::compatibility_range(&target.vulnerable_version)
                .unwrap_or_else(|| target.vulnerable_version.clone()),
        )
    } else {
        None
    };
    (target.kind.method(), target.path.clone(), normalized, copy)
}

/// Merge one routed target into `pool`, the way `fix::merge_floor_group` merges
/// the targets it routes in a single pass: keep the highest candidate, since
/// the floor has to clear every locked copy, and the highest locked version,
/// since that is the copy the floor is reported against.
fn merge_capped_target(pool: &mut BTreeMap<FloorMergeKey, FixTarget>, target: &FixTarget) {
    pool.entry(floor_merge_key(target))
        .and_modify(|existing| {
            if compare_versions(&target.to_version, &existing.to_version) == Ordering::Greater {
                existing.to_version = target.to_version.clone();
            }
            if compare_versions(&target.from_version, &existing.from_version) == Ordering::Greater {
                existing.from_version = target.from_version.clone();
            }
            // `$name` wins a mix the way routing's merge makes it win: it marks
            // a package npm refuses to override without one, which is a
            // property of the manifest, not of the copy that routed here.
            if target.npm_form == Some(NpmOverrideForm::DollarName) {
                existing.npm_form = target.npm_form;
            }
        })
        .or_insert_with(|| target.clone());
}

/// `(file_type, lang)` for a floor report grouped under `path`, derived from
/// the path's filename since a floor's report path can be a lockfile (Cargo,
/// Poetry) that has no dedicated `FileType` variant.
fn floor_file_type_and_lang(path: &Path) -> (&'static str, &'static str) {
    match path.file_name().and_then(|n| n.to_str()) {
        Some("package.json") => (FileType::PackageJson.as_str(), Lang::Node.as_str()),
        Some("Cargo.toml") => (FileType::CargoToml.as_str(), Lang::Rust.as_str()),
        Some("Cargo.lock") => ("cargo_lock", Lang::Rust.as_str()),
        Some("poetry.lock") => ("poetry_lock", Lang::Python.as_str()),
        _ => (FileType::PyProject.as_str(), Lang::Python.as_str()),
    }
}

/// An empty `UpdateFileReport` anchored on `path`, ready for a floor loop to
/// push into one of its vecs. Shared by every floor grouping site (outcomes,
/// unfixables, ignored, resolution errors) so the field list lives in one
/// place.
fn empty_floor_report(path: &Path) -> upd::output::UpdateFileReport {
    let (file_type, lang) = floor_file_type_and_lang(path);
    upd::output::UpdateFileReport {
        path: display_path(path),
        file_type,
        lang,
        updates: Vec::new(),
        pinned: Vec::new(),
        ignored: Vec::new(),
        held_back: Vec::new(),
        skipped_by_cooldown: Vec::new(),
        skipped: Vec::new(),
        capped: Vec::new(),
        annotations: Vec::new(),
        normalized: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
    }
}

/// Resolve the `UpdConfig` governing a `--package` floor target's
/// ignore/pin rules. Reuses an already-discovered file config when the
/// floor's config lookup path was itself a discovered manifest (the common
/// case); falls back to a fresh discovery walk for lockfile-only hosts
/// (Cargo, Poetry) that `load_update_configs` never saw because it only
/// walks from discovered manifest files.
fn resolve_floor_config(
    cli: &Cli,
    file_configs: &HashMap<PathBuf, Option<Arc<UpdConfig>>>,
    lookup_path: &Path,
) -> Result<Option<Arc<UpdConfig>>> {
    if let Some(existing) = file_configs.get(lookup_path) {
        return Ok(existing.clone());
    }
    if let Some(config_path) = &cli.config {
        return Ok(Some(Arc::new(
            UpdConfig::load_from_path_with_error(config_path).map_err(anyhow::Error::msg)?,
        )));
    }
    let start_dir = lookup_path.parent().unwrap_or(lookup_path).to_path_buf();
    Ok(discover_update_config(&start_dir)
        .map_err(anyhow::Error::msg)?
        .map(|resolved| resolved.config))
}

/// Message printed to stderr when `--interactive --package <name>` names a
/// lock-only dependency (rule 9): interactive floor prompting is out of
/// scope, so the note tells the user to rerun without `--interactive`.
fn lock_only_interactive_note(package: &str) -> String {
    format!(
        "note: {package} is a lock-only dependency; version floors are not offered interactively - rerun without --interactive"
    )
}

fn unmatched_package_pattern_warnings(package_filter: &PackageFilter) -> Vec<String> {
    package_filter
        .unmatched_globs()
        .into_iter()
        .map(|pattern| format!("package pattern '{pattern}' matched no packages"))
        .collect()
}

/// Render one file's warning the way `print_file_result` does, so the
/// interactive path and the aborted-scan flush cannot drift from the
/// non-interactive renderer.
fn format_warning_line(path: &Path, warning: &str) -> String {
    let location = format!("{}:", display_path(path));
    format!(
        "{} {} {}",
        location.blue().underline(),
        "Warning:".yellow(),
        warning
    )
}

fn format_error_line(path: &Path, error: &str) -> String {
    let location = format!("{}:", display_path(path));
    format!(
        "{} {} {}",
        location.blue().underline(),
        "Error:".red(),
        error
    )
}

/// Every diagnostic one scanned file produced, errors first, ready for stderr.
/// The interactive path reads `updated` and `pinned` only, so without this the
/// structured channel is discarded and a refusal is silent.
fn format_scan_diagnostics(path: &Path, result: &UpdateResult) -> Vec<String> {
    let mut lines = Vec::new();
    for error in &result.errors {
        lines.push(format_error_line(path, error));
    }
    for warning in &result.warnings {
        lines.push(format_warning_line(path, warning));
    }
    lines
}

fn build_approved_change_counts(
    updates_with_decisions: &[PendingUpdate],
    planned_changes: &[PlannedChange],
) -> HashMap<PlannedChange, usize> {
    debug_assert_eq!(updates_with_decisions.len(), planned_changes.len());

    let mut approved_counts = HashMap::new();

    for (update, change) in updates_with_decisions.iter().zip(planned_changes.iter()) {
        if update.approved {
            *approved_counts.entry(change.clone()).or_insert(0) += 1;
        }
    }

    approved_counts
}

fn take_approved_changes_for_file(
    path: &Path,
    file_type: FileType,
    updates: &[(String, String, String, Option<usize>)],
    approved_change_counts: &mut HashMap<PlannedChange, usize>,
) -> Vec<PlannedChange> {
    let mut selected = Vec::new();

    for update in updates {
        let candidate = PlannedChange::from_update(path.to_path_buf(), file_type, update);
        if let Some(count) = approved_change_counts.get_mut(&candidate)
            && *count > 0
        {
            *count -= 1;
            selected.push(candidate);
        }
    }

    approved_change_counts.retain(|_, count| *count > 0);
    selected
}

fn take_pinned_changes_for_file(
    path: &Path,
    file_type: FileType,
    pinned: &[(String, String, String, Option<usize>)],
) -> Vec<PlannedChange> {
    pinned
        .iter()
        .map(|pin| PlannedChange::from_pinned(path.to_path_buf(), file_type, pin))
        .collect()
}

fn take_approved_normalizations_for_file(
    scanned_file: &ScannedFileResult,
    approved_change_counts: &mut HashMap<PlannedChange, usize>,
) -> Vec<upd::updater::NormalizedSpec> {
    let mut selected = Vec::new();
    for normalized in &scanned_file.result.normalized {
        let candidate = PlannedChange::from_normalized(
            scanned_file.path.clone(),
            scanned_file.file_type,
            normalized,
        );
        if let Some(count) = approved_change_counts.get_mut(&candidate)
            && *count > 0
        {
            *count -= 1;
            selected.push(normalized.clone());
        }
    }
    approved_change_counts.retain(|_, count| *count > 0);
    selected
}

fn collect_selected_changes_for_file(
    scanned_file: &ScannedFileResult,
    approved_change_counts: &mut HashMap<PlannedChange, usize>,
) -> Vec<PlannedChange> {
    let mut selected = take_approved_changes_for_file(
        &scanned_file.path,
        scanned_file.file_type,
        &scanned_file.result.updated,
        approved_change_counts,
    );
    selected.extend(take_pinned_changes_for_file(
        &scanned_file.path,
        scanned_file.file_type,
        &scanned_file.result.pinned,
    ));
    selected
}

fn file_has_manifest_changes(result: &UpdateResult) -> bool {
    !result.updated.is_empty()
        || !result.pinned.is_empty()
        || !result.annotations.is_empty()
        || !result.normalized.is_empty()
}

type ChangedByLockfile = HashMap<(PathBuf, LockfileType), Vec<String>>;

/// Record changed package names against only the lockfiles owned by their
/// manifest. Several ecosystems can coexist in one directory (for example a
/// Rust/Python extension with a Dockerfile), so directory-level grouping would
/// feed image names into `cargo update -p` or skip a sibling ecosystem.
fn record_lockfile_changes(
    changed_by_lockfile: &mut ChangedByLockfile,
    manifest_path: &Path,
    package_names: impl IntoIterator<Item = String>,
) {
    let Some(dir) = manifest_path.parent() else {
        return;
    };
    let lockfiles = detect_lockfiles(manifest_path);
    if lockfiles.is_empty() {
        return;
    }
    let names: Vec<String> = package_names.into_iter().collect();
    for lockfile in lockfiles {
        let entry = changed_by_lockfile
            .entry((dir.to_path_buf(), lockfile))
            .or_default();
        for name in &names {
            if !entry.contains(name) {
                entry.push(name.clone());
            }
        }
    }
}

fn lockfile_changes_for(
    changed_by_lockfile: &ChangedByLockfile,
    manifest_path: &Path,
) -> Vec<String> {
    let Some(dir) = manifest_path.parent() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for lockfile in detect_lockfiles(manifest_path) {
        if let Some(changed) = changed_by_lockfile.get(&(dir.to_path_buf(), lockfile)) {
            for name in changed {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
    }
    names
}

/// Guidance printed beside a manifest that a failed lockfile refresh put back.
const LOCK_ROLLBACK_HINT: &str =
    "rerun without --lock to keep the manifest edits and refresh the lockfile yourself";

/// The manifests that share lockfiles (including a declared uv workspace), with the bytes of every
/// one of those files as the run found them. A refresh failure restores the
/// whole group, so a manifest is never left ahead of the lockfile it owns.
struct LockGroup {
    dir: PathBuf,
    lockfiles: Vec<LockfileType>,
    manifests: Vec<PathBuf>,
    snapshot: Snapshot,
    /// Workspace root used to invoke a shared resolver.
    refresh_manifest: Option<PathBuf>,
}

impl LockGroup {
    fn lockfile_paths(&self) -> Vec<PathBuf> {
        self.lockfiles
            .iter()
            .map(|lockfile| self.dir.join(lockfile.filename()))
            .collect()
    }
}

/// Group every manifest that owns a lockfile with the others sharing it and
/// capture manifest and lockfile bytes. Runs before any updater writes, so a
/// restore puts back what the run found rather than what it wrote.
fn plan_lock_groups(files: &[(PathBuf, FileType)]) -> Result<Vec<LockGroup>> {
    let mut groups: Vec<LockGroup> = Vec::new();
    for (path, file_type) in files {
        if *file_type == FileType::Annotated {
            continue;
        }
        let workspace = upd::lockfile::uv_workspace_root(path)?;
        let owner = workspace.as_ref().map(|root| root.join("pyproject.toml"));
        let lockfiles = match &owner {
            Some(owner) if owner.with_file_name("uv.lock").exists() => detect_lockfiles(owner),
            Some(_) => Vec::new(),
            None => detect_lockfiles(path),
        };
        if lockfiles.is_empty() {
            continue;
        }
        let owner = owner.filter(|owner| owner.with_file_name("uv.lock").exists());
        let dir = containing_dir(owner.as_deref().unwrap_or(path)).to_path_buf();
        let index = groups
            .iter()
            .position(|group| group.dir == dir && group.lockfiles == lockfiles)
            .unwrap_or_else(|| {
                let group = LockGroup {
                    dir,
                    lockfiles,
                    manifests: Vec::new(),
                    snapshot: Snapshot::default(),
                    refresh_manifest: owner.clone(),
                };
                let lockfile_paths = group.lockfile_paths();
                groups.push(group);
                let index = groups.len() - 1;
                groups[index].snapshot.extend(&lockfile_paths);
                index
            });
        let group = &mut groups[index];
        if group.manifests.is_empty()
            && let Some(owner) = &owner
        {
            let mut members: Vec<_> = upd::lockscan::discover::uv_workspace_manifests(owner)
                .map_err(anyhow::Error::msg)?
                .into_iter()
                .collect();
            members.sort();
            for member in &members {
                let member_root = upd::lockfile::uv_workspace_root(member)?;
                if member_root.as_deref() != Some(group.dir.as_path()) {
                    anyhow::bail!(
                        "Cannot safely group workspace member {}: its lockfile owner differs from {}",
                        member.display(),
                        group.dir.display()
                    );
                }
            }
            group.snapshot.extend(&members);
        }
        group.snapshot.extend(std::slice::from_ref(path));
        group.manifests.push(path.clone());
    }
    for group in &groups {
        group.snapshot.ensure_restorable()?;
    }
    Ok(groups)
}

/// What one group's refresh came to.
struct LockRefresh {
    /// The group's manifests that the run rewrote.
    manifests: Vec<PathBuf>,
    outcomes: Vec<RegenOutcome>,
    /// Present when a refresh failed and the group was put back.
    rollback: Option<Rollback>,
}

struct Rollback {
    /// The files back at their pre-run bytes.
    restored: Vec<PathBuf>,
    failures: Vec<RestoreFailure>,
}

/// Refresh the lockfiles of every group with a rewritten manifest, one command
/// per lockfile. A group whose refresh fails is restored from its snapshot,
/// manifests and lockfiles alike; a group without a rewritten manifest is left
/// alone.
fn refresh_lock_groups(
    groups: Vec<LockGroup>,
    updated_files: &[PathBuf],
    changed_by_lockfile: &ChangedByLockfile,
    verbose: bool,
) -> Vec<LockRefresh> {
    let mut refreshes = Vec::new();
    for group in groups {
        let manifests: Vec<PathBuf> = group
            .manifests
            .iter()
            .filter(|manifest| updated_files.contains(manifest))
            .cloned()
            .collect();
        let Some(anchor) = manifests.first() else {
            continue;
        };
        let changed = manifests
            .iter()
            .flat_map(|manifest| lockfile_changes_for(changed_by_lockfile, manifest))
            .collect::<Vec<_>>();
        let anchor = group.refresh_manifest.as_ref().unwrap_or(anchor);
        let outcomes: Vec<RegenOutcome> = group
            .lockfiles
            .iter()
            .map(|lockfile| regenerate_lockfile(anchor, *lockfile, &changed, verbose))
            .collect();
        let rollback = outcomes
            .iter()
            .any(|outcome| !matches!(outcome, RegenOutcome::Ok(_)))
            .then(|| {
                let failures = group.snapshot.restore();
                let restored = manifests
                    .iter()
                    .cloned()
                    .chain(group.lockfile_paths())
                    .chain(
                        group
                            .snapshot
                            .paths()
                            .filter(|path| {
                                !manifests.contains(path) && !group.lockfile_paths().contains(path)
                            })
                            .cloned(),
                    )
                    .filter(|path| !failures.iter().any(|failure| failure.path == *path))
                    .collect();
                Rollback { restored, failures }
            });
        refreshes.push(LockRefresh {
            manifests,
            outcomes,
            rollback,
        });
    }
    refreshes
}

/// How a failed lockfile refresh left one rewritten manifest.
struct LockFailure {
    /// The refresh errors, what was put back and what was not, for the
    /// file's error entry.
    message: String,
    /// `RolledBack` when the whole directory is back at its pre-run bytes,
    /// `Failed` when a file in it could not be put back. Either way the
    /// writes the scan reported for the manifest are not applied updates.
    status: LockWriteStatus,
}

type LockFailures = HashMap<PathBuf, LockFailure>;

/// Whether a failed lockfile refresh hit `path`, so the writes the scan
/// reported for it are not counted as applied.
fn refresh_failed(lock_failures: &LockFailures, path: &Path) -> bool {
    lock_failures.contains_key(path)
}

/// Print every refresh outcome and describe each failure against the
/// manifests it hit. Returns the failures by manifest and one error message
/// per manifest hit, in the order the refreshes ran.
fn report_lock_refreshes(
    refreshes: Vec<LockRefresh>,
    print_progress: bool,
) -> (LockFailures, Vec<String>) {
    let mut failures = LockFailures::new();
    let mut errors = Vec::new();
    for refresh in refreshes {
        let mut causes = Vec::new();
        for outcome in &refresh.outcomes {
            match outcome {
                RegenOutcome::Ok(lockfile) => {
                    // A lockfile regenerated before a sibling's refresh failed
                    // was put back with the rest of its group, so announcing
                    // it would describe bytes that are no longer on disk.
                    if print_progress && refresh.rollback.is_none() {
                        println!("{} Regenerated {}", "✓".green(), lockfile.filename().bold());
                    }
                }
                other => {
                    if let Some(msg) = other.error_message() {
                        eprintln!("{}", format!("error: {msg}").red());
                        causes.push(msg);
                    }
                }
            }
        }
        let Some(rollback) = refresh.rollback else {
            continue;
        };
        let mut message = causes.join("; ");
        if !rollback.restored.is_empty() {
            let restored = join_names(rollback.restored.iter().map(|path| display_path(path)));
            eprintln!("rolled back {restored}");
            message.push_str(&format!("\nrolled back {restored}"));
        }
        for failure in &rollback.failures {
            eprintln!("{}", format!("error: {failure}").red());
            message.push_str(&format!("\n{failure}"));
        }
        // `rolled_back` promises the directory is back at its pre-run bytes.
        // One file that stayed changed breaks that promise for every manifest
        // sharing the lockfile, so the whole group is `failed`, and the hint
        // to rerun without --lock is withheld: the directory first needs the
        // named file put back by hand.
        let status = if rollback.failures.is_empty() {
            eprintln!("hint: {LOCK_ROLLBACK_HINT}");
            message.push_str(&format!("\nhint: {LOCK_ROLLBACK_HINT}"));
            LockWriteStatus::RolledBack
        } else {
            LockWriteStatus::Failed
        };
        for manifest in refresh.manifests {
            errors.push(message.clone());
            failures.insert(
                manifest,
                LockFailure {
                    message: message.clone(),
                    status,
                },
            );
        }
    }
    (failures, errors)
}

/// Join names as prose: "a", "a and b", "a, b and c".
fn join_names(names: impl IntoIterator<Item = String>) -> String {
    let names: Vec<String> = names.into_iter().collect();
    match names.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// The `note:` printed for a rewritten manifest that owns no lockfile.
fn print_no_lockfile_note(manifest_path: &Path) {
    let manifest_name = manifest_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    eprintln!("note: no lockfile found for {manifest_name} - skipping (nothing to regenerate)");
}

fn has_checkable_manifest_changes(result: &UpdateResult, filter: UpdateFilter) -> bool {
    // A cooldown-only "Skipped" outcome is expected steady state: we
    // deliberately chose to hold the current version. It must NOT trip
    // `--check` into signaling "pending work". Held-back entries do count:
    // they mean we are already writing a different version than the registry
    // latest, so there is something actionable (the safer pin).
    //
    // Annotations count for the plainest reason: `--apply` writes them. What
    // `--check` reports is what an apply would do, so a check that stayed quiet
    // about them would call a tree up to date that the next apply rewrites. The
    // bump filter does not reach them - an annotation moves no version, so
    // there is no bump level for `--major`/`--minor`/`--patch` to select on.
    let (_, _, _, filtered_total) = count_result_updates(result, filter);
    filtered_total > 0
        || !result.pinned.is_empty()
        || !result.held_back.is_empty()
        || !result.annotations.is_empty()
        || !result.normalized.is_empty()
}

fn has_interactive_changes(
    pending_updates: &[PendingUpdate],
    scanned_results: &[ScannedFileResult],
) -> bool {
    !pending_updates.is_empty()
        || scanned_results.iter().any(|scanned| {
            !scanned.result.pinned.is_empty()
                || !scanned.result.normalized.is_empty()
                || !scanned.result.errors.is_empty()
                || !scanned.result.warnings.is_empty()
        })
}

fn audit_status(audit_result: &AuditResult) -> AuditStatus {
    if !audit_result.errors.is_empty() || !audit_result.warnings.is_empty() {
        AuditStatus::Incomplete
    } else if audit_result.vulnerable.is_empty() {
        AuditStatus::Clean
    } else {
        AuditStatus::Vulnerable
    }
}

/// Coverage warnings for go.mod files whose `go` directive predates 1.17.
/// Such files do not list the full transitive module set (the go tool only
/// records the complete graph from 1.17 on; a missing directive means 1.16
/// semantics), so audit findings for them may be incomplete.
fn go_mod_coverage_warnings(files: &[(std::path::PathBuf, FileType)]) -> Vec<String> {
    let mut warnings = Vec::new();
    for (path, file_type) in files {
        if *file_type != FileType::GoMod {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let version = content.lines().find_map(|line| {
            // Tokenize on any whitespace: `go 1.22` and `go\t1.22` are both
            // valid directives (matching go_mod.rs's whitespace-tolerant style).
            let mut tokens = line.split_whitespace();
            if tokens.next() != Some("go") {
                return None;
            }
            let mut parts = tokens.next()?.split('.');
            let major: u32 = parts.next()?.parse().ok()?;
            let minor_txt = parts.next()?;
            let digits: String = minor_txt
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            let minor: u32 = digits.parse().ok()?;
            Some((major, minor))
        });
        let modern = matches!(version, Some((maj, min)) if maj > 1 || (maj == 1 && min >= 17));
        if !modern {
            warnings.push(format!(
                "{}: go.mod predates go 1.17 module graph pruning: transitive coverage may be incomplete; run 'go mod tidy' with a modern toolchain",
                display_path(path)
            ));
        }
    }
    warnings
}

/// Classify an anyhow error into a clispec structured error kind and exit code.
/// Levenshtein edit distance between two ASCII-ish strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Suggest the closest known subcommand for a mistyped positional argument,
/// when one is within a small edit distance (a typo, not an arbitrary word).
fn suggest_subcommand(input: &str) -> Option<&'static str> {
    const SUBCOMMANDS: [&str; 6] = [
        "update",
        "align",
        "audit",
        "clean-cache",
        "self-update",
        "schema",
    ];
    SUBCOMMANDS
        .iter()
        .copied()
        .map(|cmd| (levenshtein(input, cmd), cmd))
        .filter(|(dist, _)| *dist <= 2)
        .min_by_key(|(dist, _)| *dist)
        .map(|(_, cmd)| cmd)
}

fn classify_error(e: &anyhow::Error) -> serde_json::Value {
    let msg = e.to_string();
    let (kind, exit_code) = if msg.contains("No such file")
        || msg.contains("Permission denied")
        || msg.contains("os error")
        || msg.contains("does not exist")
    {
        ("io_error", 2)
    } else if msg.contains("network")
        || msg.contains("connection")
        || msg.contains("timeout")
        || msg.contains("HTTP")
        || msg.contains("reqwest")
    {
        ("network_error", 3)
    } else if msg.contains("parse") || msg.contains("invalid") || msg.contains("malformed") {
        ("parse_error", 4)
    } else {
        ("io_error", 2)
    };
    serde_json::json!({
        "error": {
            "kind": kind,
            "message": msg,
            "exit_code": exit_code
        }
    })
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        let error_json = classify_error(&e);
        eprintln!("{}", serde_json::to_string(&error_json).unwrap());
        std::process::exit(error_json["error"]["exit_code"].as_i64().unwrap_or(2) as i32);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::try_parse().unwrap_or_else(|e| {
        // Clap uses Err for help/version display too; those are not real errors.
        // Only emit the structured envelope for genuine parse failures.
        // Help and version display are not errors; let clap handle them with its
        // own exit code (0 for help/version, which is correct).
        let is_display = e.kind() == clap::error::ErrorKind::DisplayHelp
            || e.kind() == clap::error::ErrorKind::DisplayVersion
            || e.kind() == clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand;
        if is_display {
            e.exit();
        }
        // Genuine parse errors: emit the structured envelope, then exit with
        // the code the envelope declares (4), not the code clap would use (2).
        eprintln!(
            "{}",
            serde_json::json!({"error": {"kind": "parse_error", "message": e.to_string(), "exit_code": 4}})
        );
        std::process::exit(4)
    });

    // Handle no-color flag
    if cli.no_color {
        colored::control::set_override(false);
    }

    // Schema subcommand: works offline with no config or auth required.
    if matches!(cli.command, Some(Command::Schema)) {
        upd::schema::print_schema();
        return Ok(());
    }
    if matches!(cli.command, Some(Command::Capabilities)) {
        let capabilities = serde_json::json!({
            "name": "upd",
            "version": env!("CARGO_PKG_VERSION"),
            "clispec": "0.3",
            "output": ["text", "json", "sarif"],
            "features": ["schema", "dry-run", "dependency updates", "security audit", "verified action SHA updates"]
        });
        if effective_json_mode(&cli) {
            println!("{}", serde_json::to_string_pretty(&capabilities)?);
        } else {
            println!(
                "upd {} - clispec 0.3; text/json output, dry-run, dependency audit",
                env!("CARGO_PKG_VERSION")
            );
        }
        return Ok(());
    }

    // --show-config: print the settings this run resolved to, and exit.
    if cli.show_config {
        // A named --config that cannot be read is an error rather than a
        // silent fall back to discovery: the whole point of the flag is to ask
        // what THAT file resolves to.
        let (loaded_config, source, explicit) = match &cli.config {
            Some(path) => (
                upd::config::UpdConfig::load_from_path_with_error(path)
                    .map_err(anyhow::Error::msg)?,
                Some(path.clone()),
                true,
            ),
            None => {
                let cwd = std::env::current_dir()?;
                match upd::config::UpdConfig::discover(&cwd).map_err(anyhow::Error::msg)? {
                    Some((config, path)) => (config, Some(path), false),
                    None => (upd::config::UpdConfig::default(), None, false),
                }
            }
        };

        let policy = loaded_config.to_cooldown_policy(cli.min_age.as_deref())?;
        let effective = upd::config::EffectiveConfig {
            source: source.as_deref(),
            explicit,
            config: &loaded_config,
            cooldown: &policy,
            update_action_shas: cli
                .action_sha_override()
                .or(loaded_config.update_action_shas)
                .unwrap_or(upd::updater::DEFAULT_UPDATE_ACTION_SHAS),
        };

        if effective_json_mode(&cli) {
            println!("{}", serde_json::to_string_pretty(&effective.to_json())?);
        } else {
            print!("{}", effective.render_text());
            // The schema follows the resolved values because a config-parse
            // warning points here to see the accepted keys, and because the
            // reader needs the answer to "what did this run use" before the
            // answer to "what could I write".
            println!();
            println!("# --- accepted configuration schema ---");
            print!("{}", upd::config::UpdConfig::schema_toml());
        }

        return Ok(());
    }

    // Reject non-existent paths before any I/O; known subcommands are routed by clap before this point.
    let invalid: Vec<_> = cli.paths.iter().filter(|p| !p.exists()).collect();
    if !invalid.is_empty() {
        for path in &invalid {
            let arg = display_path(path);
            let mut msg = format!("'{arg}' is not a known subcommand or existing path");
            if let Some(suggestion) = suggest_subcommand(&arg) {
                msg.push_str(&format!(". Did you mean '{suggestion}'?"));
            }
            eprintln!(
                "{}",
                serde_json::json!({"error": {"kind": "io_error", "message": msg, "exit_code": 2}})
            );
        }
        std::process::exit(2);
    }

    match &cli.command {
        Some(Command::CleanCache) => {
            clean_cache()?;
        }
        Some(Command::SelfUpdate) => {
            self_update(&cli).await?;
        }
        Some(Command::Align { .. }) => {
            run_align(&cli).await?;
        }
        Some(Command::Audit { .. }) => {
            run_audit(&cli).await?;
        }
        Some(Command::Schema) => {
            // Already handled above before show_config check.
            unreachable!("Schema handled earlier");
        }
        Some(Command::Capabilities) => {
            unreachable!("Capabilities handled earlier");
        }
        Some(Command::Update { .. }) | None => {
            run_update(&cli).await?;
        }
    }

    Ok(())
}

/// Returns true when JSON output should be emitted, honoring both --output/-o
/// (three-valued, clispec P1 compliant) and --format (legacy).
///
/// --output/-o takes precedence when set explicitly. When --output is auto
/// (the default), --format wins if set explicitly. Auto-detection (TTY check)
/// fires only when both are at their defaults.
///
/// --format sarif overrides everything: SARIF is never treated as plain JSON.
fn effective_json_mode(cli: &Cli) -> bool {
    use upd::cli::OutputFormat;
    // SARIF is its own mode; never treat it as JSON.
    if cli.format == Some(OutputFormat::Sarif) {
        return false;
    }
    match cli.output {
        // Explicit --output/-o wins unconditionally (three-valued clispec P1 rule).
        OutputMode::Json => true,
        OutputMode::Text => false,
        // Auto: an explicit --format value always wins over TTY detection.
        // None means --format was not passed, so fall through to TTY detection.
        OutputMode::Auto => match cli.format {
            Some(OutputFormat::Json) => true,
            Some(OutputFormat::Text) => false,
            Some(OutputFormat::Sarif) => false,
            None => cli.is_json_output(),
        },
    }
}

fn with_ecosystem_config(cli: &Cli, config: &UpdConfig) -> Result<(Cli, bool)> {
    let selection = config
        .selected_ecosystems(&cli.langs)
        .map_err(anyhow::Error::msg)?;
    let none_enabled = selection.as_ref().is_some_and(Vec::is_empty);
    let mut resolved = cli.clone();
    if cli.langs.is_empty() && selection.is_some() {
        use clap::ValueEnum;
        let mut sources = selection.clone().unwrap();
        if sources.contains(&Lang::Annotated) {
            sources = Lang::value_variants().to_vec();
        }
        let disabled: Vec<_> = config
            .ecosystems
            .disable
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|name| {
                <Lang as ValueEnum>::from_str(name, false).expect("config already validated")
            })
            .collect();
        sources.retain(|lang| *lang != Lang::Annotated && !disabled.contains(lang));
        resolved.annotation_langs = Some(sources);
    }
    resolved.langs = selection.unwrap_or_default();
    Ok((resolved, none_enabled))
}

async fn run_update(cli: &Cli) -> Result<()> {
    let json_mode = effective_json_mode(cli);
    let package_filter = PackageFilter::new(cli.packages.clone()).map_err(anyhow::Error::msg)?;

    // Reject --interactive with an explicit JSON output request. When output is
    // auto-detected as JSON (stdout piped), the TTY check inside
    // run_interactive_update fires first and gives a clearer error message.
    let explicit_json =
        matches!(cli.output, OutputMode::Json) || cli.format == Some(upd::cli::OutputFormat::Json);
    if cli.interactive && explicit_json {
        anyhow::bail!("--interactive cannot be combined with --format json or --output json");
    }

    // Resolve paths: explicit > VCS root > error
    let paths = match resolve_scan_paths(cli) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!(
                "{}",
                serde_json::json!({"error": {"kind": "io_error", "message": msg, "exit_code": 2}})
            );
            std::process::exit(2);
        }
    };

    // Mutations are opt-in. Without --apply/--yes (and not --interactive,
    // --check, or --dry-run), the run behaves as dry-run.
    let effective_dry_run = cli.is_effective_dry_run();

    // `include`/`exclude` are discovery-level settings resolved once from the root config;
    // per-file `ignore`/`pin` are loaded separately by `load_update_configs`.
    let root_config = resolve_root_config(cli, &paths)?;
    let (resolved_cli, no_ecosystems) = with_ecosystem_config(cli, &root_config.config)?;
    let cli = &resolved_cli;

    let files = if no_ecosystems {
        Vec::new()
    } else {
        discover_files_with(
            &paths,
            &cli.langs,
            DiscoverOptions {
                no_ignore: cli.no_ignore,
                verbose: cli.verbose,
                include: &root_config.config.include,
                exclude: &root_config.config.exclude,
            },
        )
    };
    let file_count = files.len();

    let text_mode_early = !json_mode;

    if files.is_empty() {
        if text_mode_early {
            if !cli.quiet {
                println!("{}", "No dependency files found.".yellow());
            }
        } else {
            emit_update_json(
                UpdateReportInput {
                    scanned: &[],
                    total_result: &UpdateResult::default(),
                    lock_failures: &LockFailures::new(),
                    file_count: 0,
                    dry_run: effective_dry_run,
                    filter: UpdateFilter::from_cli(&cli.only_bump, cli.max_bump),
                    file_cooldowns: &HashMap::new(),
                    cooldown_notes: Vec::new(),
                    floor_reports: Vec::new(),
                    run_warnings: Vec::new(),
                },
                &BoundedOutputParams::from_cli(cli),
            )?;
        }
        return Ok(());
    }

    // Init TLS only after we know we're going to network. The empty-files
    // early return above must not be killed by a malformed CA bundle env var.
    init_tls(cli)?;

    let file_configs = load_update_configs(cli, &files)?;

    // Resolve a cooldown policy per file so configs attached to one manifest
    // cannot silently apply to another (e.g. a `.updrc.toml` in a subtree).
    // CLI --min-age always wins over per-file config values.
    let file_cooldowns: HashMap<PathBuf, Option<CooldownPolicy>> = file_configs
        .iter()
        .map(|(path, config)| {
            let raw = match config.as_ref() {
                Some(cfg) => cfg.to_cooldown_policy(cli.min_age.as_deref())?,
                None => UpdConfig::default().to_cooldown_policy(cli.min_age.as_deref())?,
            };
            let is_noop = raw.force_override.is_none()
                && raw.default <= Duration::zero()
                && raw.per_ecosystem.is_empty();
            Ok::<_, anyhow::Error>((path.clone(), if is_noop { None } else { Some(raw) }))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let cooldown_notes: Arc<Mutex<BTreeMap<String, String>>> =
        Arc::new(Mutex::new(BTreeMap::new()));

    if cli.verbose {
        eprintln!(
            "{}",
            format!("Found {} dependency file(s)", file_count).cyan()
        );
    }

    // Create filter from CLI flags
    let filter = UpdateFilter::from_cli(&cli.only_bump, cli.max_bump);

    // Create shared cache and wrap registries with caching layer
    let cache = Cache::new_shared();
    let cache_enabled = !cli.no_cache;

    // Create PyPI registry with optional credentials and extra index URLs
    let pypi_registry = {
        let index_url =
            PyPiRegistry::detect_index_url().unwrap_or_else(|| "https://pypi.org".to_string());
        let credentials = PyPiRegistry::detect_credentials(&index_url);
        if cli.verbose && credentials.is_some() {
            eprintln!("{}", "Using authenticated PyPI access".cyan());
        }
        let primary = PyPiRegistry::with_index_url_and_credentials(index_url, credentials);

        // Check for extra index URLs (UV_EXTRA_INDEX_URL, PIP_EXTRA_INDEX_URL)
        let extra_urls = PyPiRegistry::detect_extra_index_urls();
        if cli.verbose && !extra_urls.is_empty() {
            eprintln!(
                "{}",
                format!("Using {} extra PyPI index(es)", extra_urls.len()).cyan()
            );
        }

        MultiPyPiRegistry::from_primary_and_extras(primary, extra_urls)
    };

    let pypi_cache_namespace = pypi_registry.cache_namespace();
    let pypi = CachedRegistry::with_namespace(
        pypi_registry,
        Arc::clone(&cache),
        cache_enabled,
        pypi_cache_namespace,
    );

    // Create npm registry with optional credentials
    let npm_registry = {
        let registry_url = NpmRegistry::detect_registry_url()
            .unwrap_or_else(|| "https://registry.npmjs.org".to_string());
        let credentials = NpmRegistry::detect_credentials(&registry_url);
        if cli.verbose && credentials.is_some() {
            eprintln!("{}", "Using authenticated npm access".cyan());
        }
        NpmRegistry::with_registry_url_and_credentials(registry_url, credentials)
    };

    let npm = CachedRegistry::new(npm_registry, Arc::clone(&cache), cache_enabled);

    // Create Cargo registry with optional credentials
    let crates_io_registry = {
        let registry_url = CratesIoRegistry::detect_registry_url()
            .unwrap_or_else(|| "https://crates.io/api/v1/crates".to_string());
        let credentials = CratesIoRegistry::detect_credentials("crates-io");
        let has_cargo_files = files.iter().any(|(_, ft)| *ft == FileType::CargoToml);
        if cli.verbose && credentials.is_some() && has_cargo_files {
            eprintln!("{}", "Using authenticated crates.io access".cyan());
        }
        CratesIoRegistry::with_registry_url_and_credentials(registry_url, credentials)
    };

    let crates_io = CachedRegistry::new(crates_io_registry, Arc::clone(&cache), cache_enabled);

    // Create Go proxy registry with optional credentials
    let go_proxy_registry = {
        let proxy_url = GoProxyRegistry::detect_proxy_url()
            .unwrap_or_else(|| "https://proxy.golang.org".to_string());
        let credentials = GoProxyRegistry::detect_credentials(&proxy_url);
        if cli.verbose && credentials.is_some() {
            eprintln!("{}", "Using authenticated Go proxy access".cyan());
        }
        GoProxyRegistry::with_proxy_url_and_credentials(proxy_url, credentials)
    };

    let go_proxy = CachedRegistry::new(go_proxy_registry, Arc::clone(&cache), cache_enabled);

    // Create RubyGems registry
    let rubygems_registry = RubyGemsRegistry::new();
    let rubygems = CachedRegistry::new(rubygems_registry, Arc::clone(&cache), cache_enabled);

    // Create Terraform registry
    let terraform_registry = TerraformRegistry::new();
    let terraform = CachedRegistry::new(terraform_registry, Arc::clone(&cache), cache_enabled);

    // Gradle libraries and plugin markers use distinct Maven repositories.
    let gradle = Arc::new(CachedRegistry::new(
        GradleRegistry::new(),
        Arc::clone(&cache),
        cache_enabled,
    ));
    // Create NuGet registry
    let nuget_registry = NuGetRegistry::new();
    let nuget = CachedRegistry::new(nuget_registry, Arc::clone(&cache), cache_enabled);

    // Create GitHub releases registry with optional token
    let github_releases_registry = GitHubReleasesRegistry::new();
    if cli.verbose && GitHubReleasesRegistry::detect_token().is_some() {
        eprintln!("{}", "Using authenticated GitHub access".cyan());
    }
    let github_releases =
        CachedRegistry::new(github_releases_registry, Arc::clone(&cache), cache_enabled);

    let docker_registry = DockerRegistry::new();
    let docker = CachedRegistry::new(docker_registry, Arc::clone(&cache), cache_enabled);

    // Create updaters wrapped in Arc for parallel processing
    let requirements_updater = Arc::new(RequirementsUpdater::new());
    let pyproject_updater = Arc::new(PyProjectUpdater::new());
    let package_json_updater = Arc::new(PackageJsonUpdater::new());
    let cargo_toml_updater = Arc::new(CargoTomlUpdater::new());
    let go_mod_updater = Arc::new(GoModUpdater::new());
    let github_actions_updater = Arc::new(GithubActionsUpdater::new());
    let gemfile_updater = Arc::new(GemfileUpdater::new());
    let terraform_updater = Arc::new(TerraformUpdater::new());
    let csproj_updater = Arc::new(CsprojUpdater::new());
    let docker_updater = Arc::new(DockerUpdater::new().with_verbose(cli.verbose));

    // Wrap registries in Arc for parallel processing
    let pypi = Arc::new(pypi);
    let npm = Arc::new(npm);
    let crates_io = Arc::new(crates_io);
    let go_proxy = Arc::new(go_proxy);
    let rubygems = Arc::new(rubygems);
    let terraform = Arc::new(terraform);
    let nuget = Arc::new(nuget);
    let github_releases = Arc::new(github_releases);
    let docker = Arc::new(docker);

    // Built last: they hold the cached registries, so they cannot exist before
    // them. Terraform is absent because no v1 annotation source names it, and a
    // mise entry cannot name it either.
    let registry_set = || {
        RegistrySet::resolving(
            &pypi,
            &npm,
            &crates_io,
            &go_proxy,
            &rubygems,
            &nuget,
            &github_releases,
        )
    };
    let annotated_updater = Arc::new(AnnotatedUpdater::new(registry_set()));
    let mise_updater = Arc::new(MiseUpdater::new(registry_set()));
    let pre_commit_updater = Arc::new(PreCommitUpdater::with_registries(registry_set()));

    // Interactive mode: first discover updates, then prompt, then apply approved ones
    if cli.interactive {
        return run_interactive_update(
            cli,
            &package_filter,
            &files,
            &paths,
            &file_configs,
            filter,
            &pypi,
            &npm,
            &crates_io,
            &go_proxy,
            &rubygems,
            &terraform,
            &nuget,
            &gradle,
            &github_releases,
            &docker,
            &requirements_updater,
            &pyproject_updater,
            &package_json_updater,
            &cargo_toml_updater,
            &go_mod_updater,
            &gemfile_updater,
            &github_actions_updater,
            &pre_commit_updater,
            &mise_updater,
            &terraform_updater,
            &csproj_updater,
            &docker_updater,
            &annotated_updater,
            &cache,
            cache_enabled,
            &file_cooldowns,
            Arc::clone(&cooldown_notes),
        )
        .await;
    }

    // Non-interactive mode: process files in parallel
    let dry_run = effective_dry_run;
    // Only cloned when --package narrows to specific names: the version-floor
    // branch below needs the discovered (path, FileType) list again after
    // `files` is consumed by `.into_iter()` just below, but every other run
    // must not pay for a clone it never uses.
    let discovered_files: Vec<(PathBuf, FileType)> = if package_filter.is_empty() {
        Vec::new()
    } else {
        files.clone()
    };
    // Under --lock, capture every lockfile-owning manifest and its lockfiles
    // before any updater writes, so a failed refresh can put them back.
    let lock_groups = if cli.lock && !dry_run {
        plan_lock_groups(&files)?
    } else {
        Vec::new()
    };
    let file_jobs: Vec<_> = files
        .into_iter()
        .map(|(path, file_type)| {
            let config = file_configs.get(&path).cloned().flatten();
            let cooldown_policy = file_cooldowns.get(&path).and_then(|p| p.as_ref());
            (
                path,
                file_type,
                build_update_options(
                    dry_run,
                    cli.full_precision,
                    cli.action_sha_override(),
                    config,
                    &package_filter,
                    &cli.langs,
                    cli.annotation_langs.as_deref(),
                    cooldown_policy,
                    Arc::clone(&cooldown_notes),
                    filter.to_bump_filter(),
                ),
            )
        })
        .collect();

    let verbose = cli.verbose;

    // Process files in parallel with a concurrency limit
    let concurrency_limit = 8; // Process up to 8 files concurrently

    let results: Vec<(PathBuf, FileType, Result<UpdateResult, String>)> = stream::iter(file_jobs)
        .map(|(path, file_type, update_options)| {
            let pypi = Arc::clone(&pypi);
            let npm = Arc::clone(&npm);
            let crates_io = Arc::clone(&crates_io);
            let go_proxy = Arc::clone(&go_proxy);
            let rubygems = Arc::clone(&rubygems);
            let terraform = Arc::clone(&terraform);
            let nuget = Arc::clone(&nuget);
            let github_releases = Arc::clone(&github_releases);
            let docker = Arc::clone(&docker);
            let requirements_updater = Arc::clone(&requirements_updater);
            let pyproject_updater = Arc::clone(&pyproject_updater);
            let package_json_updater = Arc::clone(&package_json_updater);
            let cargo_toml_updater = Arc::clone(&cargo_toml_updater);
            let go_mod_updater = Arc::clone(&go_mod_updater);
            let gemfile_updater = Arc::clone(&gemfile_updater);
            let github_actions_updater = Arc::clone(&github_actions_updater);
            let pre_commit_updater = Arc::clone(&pre_commit_updater);
            let mise_updater = Arc::clone(&mise_updater);
            let gradle = Arc::clone(&gradle);
            let csproj_updater = Arc::clone(&csproj_updater);
            let terraform_updater = Arc::clone(&terraform_updater);
            let docker_updater = Arc::clone(&docker_updater);
            let annotated_updater = Arc::clone(&annotated_updater);

            async move {
                let result = match file_type {
                    FileType::Requirements => {
                        requirements_updater
                            .update(&path, pypi.as_ref(), update_options.clone())
                            .await
                    }
                    FileType::PyProject => {
                        pyproject_updater
                            .update(&path, pypi.as_ref(), update_options.clone())
                            .await
                    }
                    FileType::PackageJson => {
                        package_json_updater
                            .update(&path, npm.as_ref(), update_options.clone())
                            .await
                    }
                    FileType::CargoToml => {
                        cargo_toml_updater
                            .update(&path, crates_io.as_ref(), update_options.clone())
                            .await
                    }
                    FileType::GoMod => {
                        go_mod_updater
                            .update(&path, go_proxy.as_ref(), update_options.clone())
                            .await
                    }
                    FileType::Gemfile => {
                        gemfile_updater
                            .update(&path, rubygems.as_ref(), update_options.clone())
                            .await
                    }
                    FileType::GithubActions => {
                        update_with_annotations(
                            github_actions_updater.as_ref(),
                            annotated_updater.as_ref(),
                            &path,
                            github_releases.as_ref(),
                            update_options.clone(),
                        )
                        .await
                    }
                    FileType::PreCommitConfig => {
                        pre_commit_updater
                            .update(&path, github_releases.as_ref(), update_options.clone())
                            .await
                    }
                    FileType::MiseToml | FileType::ToolVersions => {
                        mise_updater
                            .update(&path, github_releases.as_ref(), update_options.clone())
                            .await
                    }
                    FileType::GradleCatalog | FileType::GradleScript | FileType::GradleWrapper => {
                        GradleUpdater::new()
                            .update(&path, gradle.as_ref(), update_options.clone())
                            .await
                    }
                    FileType::Csproj => {
                        csproj_updater
                            .update(&path, nuget.as_ref(), update_options.clone())
                            .await
                    }
                    FileType::TerraformTf => {
                        terraform_updater
                            .update(&path, terraform.as_ref(), update_options.clone())
                            .await
                    }
                    FileType::Dockerfile => {
                        update_with_annotations(
                            docker_updater.as_ref(),
                            annotated_updater.as_ref(),
                            &path,
                            docker.as_ref(),
                            update_options.clone(),
                        )
                        .await
                    }
                    FileType::DockerCompose => {
                        docker_updater
                            .update(&path, docker.as_ref(), update_options.clone())
                            .await
                    }
                    FileType::Annotated => {
                        annotated_updater
                            .update(&path, pypi.as_ref(), update_options.clone())
                            .await
                    }
                };
                (path, file_type, result.map_err(|e| e.to_string()))
            }
        })
        .buffer_unordered(concurrency_limit)
        .collect()
        .await;

    // Process results, preserving per-file attribution for both text and JSON output.
    let text_mode = !json_mode;
    let mut total_result = UpdateResult::default();
    let mut updated_files: Vec<PathBuf> = Vec::new();
    let mut scanned: Vec<ScannedFileResult> = Vec::new();

    for (path, file_type, result) in results {
        if verbose && text_mode {
            println!("{}", format!("Processed: {}", display_path(&path)).cyan());
        }

        match result {
            Ok(file_result) => {
                if !dry_run
                    && file_type != FileType::Annotated
                    && file_has_manifest_changes(&file_result)
                {
                    updated_files.push(path.clone());
                }
                if text_mode && !cli.quiet {
                    let cooldown_policy = file_cooldowns.get(&path).and_then(|p| p.as_ref());
                    print_file_result(
                        &display_path(&path),
                        file_type,
                        &file_result,
                        dry_run,
                        filter,
                        verbose,
                        cooldown_policy,
                    );
                }
                scanned.push(ScannedFileResult {
                    path: path.clone(),
                    file_type,
                    result: file_result,
                });
            }
            Err(e) => {
                let msg = format!("Error processing {}: {}", display_path(&path), e);
                eprintln!("{}", msg.red());
                // Surface the outer error in the per-file record, from where
                // the aggregate picks it up, so JSON output captures it and
                // the exit-code logic can detect that errors occurred.
                let error_result = UpdateResult {
                    errors: vec![e],
                    ..Default::default()
                };
                scanned.push(ScannedFileResult {
                    path: path.clone(),
                    file_type,
                    result: error_result,
                });
            }
        }
    }

    // Refresh lockfiles if requested and at least one manifest changed. This
    // transaction settles before the floor branch below opens its own: the
    // lock groups were snapshotted before any manifest was rewritten, so a
    // rollback here restores the directory to its pre-run state and would
    // also erase a floor already written and reported as applied. Running
    // first also lets the floor branch scan the lockfile the refresh wrote.
    let (lock_failures, lock_errors) = if cli.lock && !dry_run && !updated_files.is_empty() {
        // Group changed package names by the lockfile their manifest owns.
        // This keeps unrelated ecosystems in the same directory isolated.
        let mut changed_by_lockfile = ChangedByLockfile::new();
        for scanned_file in &scanned {
            if scanned_file.file_type == FileType::Annotated {
                continue;
            }
            if scanned_file.result.updated.is_empty()
                && scanned_file.result.pinned.is_empty()
                && scanned_file.result.normalized.is_empty()
            {
                continue;
            }
            record_lockfile_changes(
                &mut changed_by_lockfile,
                &scanned_file.path,
                scanned_file
                    .result
                    .updated
                    .iter()
                    .map(|(name, _, _, _)| name.clone())
                    .chain(
                        scanned_file
                            .result
                            .pinned
                            .iter()
                            .map(|(name, _, _, _)| name.clone()),
                    )
                    .chain(
                        scanned_file
                            .result
                            .normalized
                            .iter()
                            .map(|entry| entry.package.clone()),
                    ),
            );
        }

        for path in &updated_files {
            if !lock_groups
                .iter()
                .any(|group| group.manifests.contains(path))
            {
                print_no_lockfile_note(path);
            }
        }
        let refreshes = refresh_lock_groups(
            lock_groups,
            &updated_files,
            &changed_by_lockfile,
            verbose && text_mode,
        );
        // The header is only printed when there is real work to do.
        if text_mode && !refreshes.is_empty() && !cli.quiet {
            println!();
            println!("{}", "Regenerating lockfiles...".cyan());
        }
        report_lock_refreshes(refreshes, text_mode && !cli.quiet)
    } else {
        (LockFailures::new(), Vec::new())
    };

    // Version-floor branch for --package lock-only dependencies:
    // lockfiles are parsed only when --package narrows to specific names. A
    // requested name with no manifest occurrence but a hit in a scanned
    // lockfile is floored to the registry latest (or a config pin) through
    // the lock's own mechanism (uv constraint, npm override, cargo
    // --precise); poetry has no floor mechanism and is reported unfixable.
    let mut floor_reports: Vec<upd::output::UpdateFileReport> = Vec::new();
    let mut run_warnings: Vec<String> = Vec::new();
    let mut floor_has_planned = false;

    if !package_filter.is_empty() {
        let lock_scan = lockscan::scan_locks(&discovered_files, &paths);
        run_warnings = lock_scan.warnings.clone();
        if text_mode {
            for warning in &run_warnings {
                eprintln!("{} {}", "Warning:".yellow(), warning);
            }
        }

        let manifest_packages =
            match scan_packages(&discovered_files, &cli.langs, ParseWarnings::Suppress) {
                Ok(p) => p,
                Err(e) => {
                    // The JSON report is emitted after this point, so returning here
                    // would discard every warning the updater loop already merged.
                    // Text mode already printed them per file inside that loop.
                    if json_mode {
                        for scanned_file in &scanned {
                            for warning in &scanned_file.result.warnings {
                                eprintln!("{}", format_warning_line(&scanned_file.path, warning));
                            }
                        }
                    }
                    eprintln!("{}", format!("Error scanning files: {}", e).red());
                    return Err(e);
                }
            };

        let lock_kind_by_path: HashMap<PathBuf, lockscan::discover::LockKind> = lock_scan
            .locks
            .iter()
            .map(|l| (l.path.clone(), l.kind))
            .collect();

        // Rule 2: a requested name is lock-only when it matches no manifest
        // occurrence but resolves via at least one scanned lockfile.
        let mut lock_only: Vec<&upd::lockscan::LockedPackage> = Vec::new();
        for locked in &lock_scan.packages {
            let norm = normalized_package_name(&locked.name, locked.ecosystem);
            let requested = package_filter_matches_locked(&package_filter, locked);
            if !requested
                || !is_lock_only_package(locked, &manifest_packages)
                || recognized_in_manifest(&scanned, &norm, locked.ecosystem)
            {
                continue;
            }
            lock_only.push(locked);
        }

        // Group identical locked packages so every holder is discovered, then
        // select and route candidates separately for each project's policy.
        let mut distinct: Vec<&upd::lockscan::LockedPackage> = Vec::new();
        for lp in &lock_only {
            if !distinct.iter().any(|d: &&upd::lockscan::LockedPackage| {
                d.name == lp.name && d.version == lp.version && d.ecosystem == lp.ecosystem
            }) {
                distinct.push(lp);
            }
        }

        let mut synthetic_vulnerable: BTreeMap<PathBuf, Vec<PackageAuditResult>> = BTreeMap::new();
        // Candidates the ceiling refused, held until routing has said whether
        // this lock has a floor mechanism at all. Reported as held back only
        // if it does.
        let mut capped_pending: Vec<(&upd::lockscan::LockedPackage, String, PathBuf)> = Vec::new();
        let mut floor_ignored: HashMap<PathBuf, Vec<upd::output::IgnoredEntry>> = HashMap::new();
        let mut floor_errors: HashMap<PathBuf, Vec<upd::output::ErrorEntry>> = HashMap::new();
        let mut floor_capped: HashMap<PathBuf, Vec<upd::output::CappedEntry>> = HashMap::new();

        // Projects whose own config ignores a package, so the fan-out below can
        // be trimmed back to the projects that actually want the floor. Keyed
        // by the path the floor is reported against, which is what both a fix
        // target and an unfixable target carry; the ecosystem rides along
        // because the name alone is shared across ecosystems and neither kind
        // of target carries one.
        let mut ignoring_reports: HashMap<PathBuf, (Ecosystem, HashSet<String>)> = HashMap::new();

        for locked in &distinct {
            // Resolve each holder against its own pins, cooldown and Python
            // upload policy. Routing below is restricted to that holder.
            let mut holders = Vec::new();
            for lp in &lock_only {
                if lp.name != locked.name
                    || lp.version != locked.version
                    || lp.ecosystem != locked.ecosystem
                {
                    continue;
                }
                let Some(kind) = lock_kind_by_path.get(&lp.lockfile_path).copied() else {
                    continue;
                };
                let report_path = floor_report_path(&lp.lockfile_path, kind);
                let lookup_path = floor_config_lookup_path(&lp.lockfile_path, kind);
                let config = resolve_floor_config(cli, &file_configs, &lookup_path)?;

                let raw_policy = match config.as_ref() {
                    Some(cfg) => cfg.to_cooldown_policy(cli.min_age.as_deref())?,
                    None => UpdConfig::default().to_cooldown_policy(cli.min_age.as_deref())?,
                };
                let is_noop_cooldown = raw_policy.force_override.is_none()
                    && raw_policy.default <= Duration::zero()
                    && raw_policy.per_ecosystem.is_empty();
                let cooldown_policy = if is_noop_cooldown {
                    None
                } else {
                    Some(raw_policy)
                };

                let options = build_update_options(
                    dry_run,
                    cli.full_precision,
                    cli.action_sha_override(),
                    config,
                    &package_filter,
                    &cli.langs,
                    cli.annotation_langs.as_deref(),
                    cooldown_policy.as_ref(),
                    Arc::clone(&cooldown_notes),
                    filter.to_bump_filter(),
                );

                let ignored = options.should_ignore(&lp.name);
                holders.push((*lp, report_path, options, ignored));
            }

            for (lp, report_path, _, ignored) in &holders {
                if !ignored {
                    continue;
                }
                floor_ignored.entry(report_path.clone()).or_default().push(
                    upd::output::IgnoredEntry {
                        package: lp.name.clone(),
                        current: lp.version.clone(),
                        line: None,
                        source: None,
                    },
                );
                ignoring_reports
                    .entry(report_path.clone())
                    .or_insert_with(|| (lp.ecosystem, HashSet::new()))
                    .1
                    .insert(normalized_package_name(&lp.name, lp.ecosystem));
            }

            for (locked, report_path, options, _) in holders.iter().filter(|h| !h.3) {
                let lang = ecosystem_to_lang(locked.ecosystem);

                let registry: &dyn upd::registry::Registry = match locked.ecosystem {
                    Ecosystem::PyPI => pypi.as_ref(),
                    Ecosystem::Npm => npm.as_ref(),
                    Ecosystem::CratesIo => crates_io.as_ref(),
                    Ecosystem::Go | Ecosystem::RubyGems | Ecosystem::NuGet | Ecosystem::Maven => {
                        continue;
                    }
                };

                let resolution = if lang == Lang::Python {
                    PyProjectUpdater::resolve_floor_version(
                        report_path,
                        registry,
                        &locked.name,
                        &locked.version,
                        options,
                    )
                    .await
                } else {
                    resolve_floor_version(registry, &locked.name, &locked.version, lang, options)
                        .await
                };
                match resolution {
                    Ok(FloorResolution::Capped(candidate)) => {
                        // Above the ceiling: nothing is written, but a newer
                        // release is waiting and has to be visible. Held until the
                        // router has been asked whether a floor could be written
                        // here at all, since "held back by the ceiling" promises
                        // that raising the ceiling releases the update.
                        capped_pending.push((*locked, candidate, report_path.clone()));
                    }
                    Ok(FloorResolution::Floor(candidate)) => {
                        synthetic_vulnerable
                            .entry(report_path.clone())
                            .or_default()
                            .push(PackageAuditResult {
                                package: AuditPackage {
                                    name: locked.name.clone(),
                                    version: locked.version.clone(),
                                    ecosystem: locked.ecosystem,
                                },
                                vulnerabilities: vec![Vulnerability {
                                    id: "floor".to_string(),
                                    summary: None,
                                    severity: None,
                                    url: None,
                                    fixed_version: Some(candidate),
                                    aliases: Vec::new(),
                                    source: String::new(),
                                }],
                            });
                    }
                    Ok(FloorResolution::NotNeeded) => {}
                    Err(e) => {
                        let msg = format!("Error resolving floor for {}: {}", locked.name, e);
                        eprintln!("{}", msg.red());
                        total_result.errors.push(msg.clone());
                        // Registry-resolution failures never become a FixTarget, so
                        // they need their own file-anchored entry: without this the
                        // error would only bump summary.errors, invisible in files[].
                        floor_errors.entry(report_path.clone()).or_default().push(
                            upd::output::ErrorEntry::with_file(
                                display_path(report_path),
                                "registry",
                                msg,
                            ),
                        );
                    }
                }
            }
        }

        // Routing fans one triple out to every project that resolves it,
        // including the ones whose own config ignores the package. Those
        // projects have already been reported as `ignored`, and ignoring is an
        // answer in itself: they must not be written to, told the update is
        // held back, or told the floor cannot be written.
        let floor_is_ignored = |path: &Path, package: &str| -> bool {
            ignoring_reports
                .get(path)
                .is_some_and(|(ecosystem, names)| {
                    names.contains(&normalized_package_name(package, *ecosystem))
                })
        };

        // Shared by the real routing pass and the capped classification pass
        // below, both of which ask the same question of the same locks.
        let prov = if synthetic_vulnerable.is_empty() && capped_pending.is_empty() {
            None
        } else {
            Some(lockscan::provenance::classify(
                &lock_scan.locks,
                &lock_scan.packages,
                &manifest_packages,
            ))
        };

        let (mut outcomes, mut unfixable, apply_notes): (
            Vec<AppliedFix>,
            Vec<UnfixableTarget>,
            Vec<String>,
        ) = if synthetic_vulnerable.is_empty() {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            let mut targets = Vec::new();
            let mut unfixable = Vec::new();
            for (report_path, vulnerable) in synthetic_vulnerable {
                let synthetic_audit = AuditResult {
                    vulnerable,
                    safe_count: 0,
                    errors: Vec::new(),
                    warnings: Vec::new(),
                };
                let routing = upd::fix::route_update_targets(
                    &synthetic_audit,
                    prov.as_ref().expect("prov built for a non-empty floor set"),
                    &manifest_packages,
                );
                targets.extend(
                    routing
                        .targets
                        .into_iter()
                        .filter(|t| t.path == report_path),
                );
                unfixable.extend(
                    routing
                        .unfixable
                        .into_iter()
                        .filter(|u| u.path.as_ref() == Some(&report_path)),
                );
            }

            let opts = FixApplyOptions {
                dry_run,
                relock_manifests: cli.lock && !cli.no_lock,
                relock_floors: !cli.no_lock,
                verbose: verbose && text_mode,
            };
            let (outcomes, notes) = apply_fix_targets(targets, &opts, &|_, _, _| Ok(false));
            (outcomes, unfixable, notes)
        };

        // Classify the candidates the ceiling refused, discarding the fix
        // targets' edits: this pass only asks WHERE a floor could have been
        // written and where it could not. A poetry.lock has no floor mechanism
        // at all, and an npm direct dependency whose spec upd cannot bump fails
        // the override guard, so those are unfixable whatever the ceiling says.
        // Reporting them as held back would promise that raising the ceiling
        // releases them, when raising it changes nothing; they get the same
        // unfixable diagnostic, with its actionable guidance, that an in-cap
        // candidate gets.
        //
        // The verdict is per PATH, not per package: one triple can resolve in a
        // uv.lock that takes a floor and a poetry.lock that cannot, and the uv
        // project is then genuinely waiting on the ceiling alone. Routing is
        // asked one triple at a time so each answer stays attached to the triple
        // that produced it, since a shared pass is matchable back only by name
        // and version, which two ecosystems can share.
        //
        // What is written is one floor per manifest and package, whatever the
        // lock holds: the in-cap path merges every locked copy of a package into
        // that single floor. The per-triple pass here sees the copies one at a
        // time, so it collects the routed targets and merges them the same way
        // before classifying any of them. Classifying first would ask the same
        // question of the same manifest once per locked copy and answer it once
        // per copy, so a package locked twice would report its floor twice above
        // the ceiling and once below it. A BTreeMap also fixes the order, which
        // routing does not.
        let mut capped_targets: BTreeMap<FloorMergeKey, FixTarget> = BTreeMap::new();

        for (locked, candidate, report_path) in &capped_pending {
            let capped_audit = AuditResult {
                vulnerable: vec![PackageAuditResult {
                    package: AuditPackage {
                        name: locked.name.clone(),
                        version: locked.version.clone(),
                        ecosystem: locked.ecosystem,
                    },
                    vulnerabilities: vec![Vulnerability {
                        id: "floor".to_string(),
                        summary: None,
                        severity: None,
                        url: None,
                        fixed_version: Some(candidate.clone()),
                        aliases: Vec::new(),
                        source: String::new(),
                    }],
                }],
                safe_count: 0,
                errors: Vec::new(),
                warnings: Vec::new(),
            };
            let routing = upd::fix::route_update_targets(
                &capped_audit,
                prov.as_ref()
                    .expect("prov built for a non-empty capped set"),
                &manifest_packages,
            );

            // Routing fans a floor out over every manifest it would be written
            // to, which is exactly the set of projects waiting on the ceiling.
            // Anything reported outside that fan-out would land only on the
            // representative's own manifest, leaving every other project
            // holding the same package looking up to date.
            for target in &routing.targets {
                // A project whose own config ignores the package is not waiting
                // on the ceiling, and has been reported as `ignored` already.
                if target.path != *report_path || floor_is_ignored(&target.path, &target.package) {
                    continue;
                }
                merge_capped_target(&mut capped_targets, target);
            }
            unfixable.extend(routing.unfixable.into_iter().filter(|u| {
                u.path
                    .as_ref()
                    .is_some_and(|path| path == report_path && !floor_is_ignored(path, &u.package))
            }));
        }

        let mut capped_reports: BTreeMap<(PathBuf, String), (String, String)> = BTreeMap::new();

        for target in capped_targets.values() {
            // Routing places a target without reading the manifest or the apply
            // options, so a target alone does not mean the floor can be
            // written: an existing uv constraint upd will not rewrite, an
            // `overrides` entry that is not an object, or `--no-lock` against a
            // floor that mutates only `Cargo.lock` all refuse it whatever the
            // ceiling says. Only a floor that would really have been written
            // leaves the ceiling as the reason the update is waiting; every
            // other answer is the one this candidate gets in cap, so the
            // diagnostic a reader sees stops depending on where the ceiling
            // happens to sit.
            if let Some((status, error)) = probe_floor_target(target, !cli.no_lock) {
                outcomes.push(AppliedFix {
                    target: target.clone(),
                    status,
                    error,
                });
                continue;
            }

            // A floor already being written to this manifest at or above the
            // candidate leaves the ceiling holding nothing back: an in-cap copy
            // of the same package is floored to a version that lifts this copy
            // too. Reporting it as held back would ask for a --max-bump that
            // changes what is written not at all.
            if outcomes.iter().any(|o| {
                floor_entry_counts_as_update(Some(o.status.as_str()))
                    && o.target.path == target.path
                    && o.target.package == target.package
                    && compare_versions(&o.target.to_version, &target.to_version) != Ordering::Less
            }) {
                continue;
            }

            // Cargo floors stay one per locked copy above, since
            // `cargo update --precise` lifts one copy at a time; the capped
            // channel still reports one entry per manifest and package,
            // carrying the highest locked version.
            let entry = capped_reports
                .entry((target.path.clone(), target.package.clone()))
                .or_insert_with(|| (target.from_version.clone(), target.to_version.clone()));
            if compare_versions(&target.from_version, &entry.0) == Ordering::Greater {
                entry.0 = target.from_version.clone();
            }
            if compare_versions(&target.to_version, &entry.1) == Ordering::Greater {
                entry.1 = target.to_version.clone();
            }
        }

        for ((path, package), (current, available)) in capped_reports {
            total_result.record_capped(&package, &current, &available, None);
            if text_mode && !cli.quiet {
                println!(
                    "{}",
                    format_capped_line(
                        &display_path(&path),
                        None,
                        &package,
                        &current,
                        &available,
                        classify_path_update(&path, &current, &available)
                    )
                );
            }
            floor_capped
                .entry(path.clone())
                .or_default()
                .push(upd::output::CappedEntry {
                    package,
                    current: current.clone(),
                    available: available.clone(),
                    bump: classify_path_update(&path, &current, &available).as_str(),
                    line: None,
                    source: None,
                });
        }

        if !cli.quiet {
            for u in &unfixable {
                eprintln!(
                    "{} Cannot auto-fix {}: {}",
                    "⚠".yellow().bold(),
                    u.package.bold(),
                    u.reason
                );
            }
        }

        for note in &apply_notes {
            eprintln!("note: {note}");
        }

        if text_mode && !cli.quiet {
            for outcome in &outcomes {
                print_fix_outcome(outcome);
            }
        }

        let mut grouped: std::collections::BTreeMap<PathBuf, upd::output::UpdateFileReport> =
            std::collections::BTreeMap::new();

        for message in fix_failure_messages(&outcomes) {
            eprintln!("{}", message.red());
        }

        for outcome in &outcomes {
            let target = &outcome.target;
            if matches!(outcome.status, FixStatus::Failed | FixStatus::RolledBack)
                && let Some(err) = &outcome.error
            {
                let msg = format!("{}: {}", target.package, err);
                total_result.errors.push(msg);
            }
            if outcome.status == FixStatus::Planned {
                floor_has_planned = true;
            }
            let report = grouped
                .entry(target.path.clone())
                .or_insert_with(|| empty_floor_report(&target.path));
            report.updates.push(upd::output::UpdateEntry {
                section: None,
                previous_spec: None,
                new_spec: None,
                package: target.package.clone(),
                current: target.from_version.clone(),
                latest: target.to_version.clone(),
                bump: classify_path_update(&target.path, &target.from_version, &target.to_version)
                    .as_str(),
                line: None,
                method: Some(target.kind.method()),
                status: Some(outcome.status.as_str()),
                error: outcome.error.clone(),
                source: None,
                reference_kind: None,
                current_commit: None,
                latest_commit: None,
            });
        }

        for u in &unfixable {
            let Some(path) = u.path.clone() else {
                continue;
            };
            let current = u.from_version.clone();
            let latest = u.to_version.clone().unwrap_or_else(|| current.clone());
            let report = grouped
                .entry(path.clone())
                .or_insert_with(|| empty_floor_report(&path));
            report.updates.push(upd::output::UpdateEntry {
                section: None,
                previous_spec: None,
                new_spec: None,
                package: u.package.clone(),
                current: current.clone(),
                latest: latest.clone(),
                bump: classify_path_update(&path, &current, &latest).as_str(),
                line: None,
                method: u.method,
                status: Some("unfixable"),
                error: Some(u.reason.clone()),
                source: None,
                reference_kind: None,
                current_commit: None,
                latest_commit: None,
            });
        }

        for (path, ignored) in floor_ignored {
            let report = grouped
                .entry(path.clone())
                .or_insert_with(|| empty_floor_report(&path));
            report.ignored.extend(ignored);
        }

        for (path, errors) in floor_errors {
            let report = grouped
                .entry(path.clone())
                .or_insert_with(|| empty_floor_report(&path));
            report.errors.extend(errors);
        }

        for (path, capped) in floor_capped {
            let report = grouped
                .entry(path.clone())
                .or_insert_with(|| empty_floor_report(&path));
            report.capped.extend(capped);
        }

        floor_reports = grouped.into_values().collect();
    }

    let package_pattern_warnings = unmatched_package_pattern_warnings(&package_filter);
    if text_mode {
        for warning in &package_pattern_warnings {
            eprintln!("{} {}", "Warning:".yellow(), warning);
        }
    }
    run_warnings.extend(package_pattern_warnings);

    // A manifest whose lockfile refresh failed was either put back, carrying
    // none of the writes its scan reported, or left in a directory that could
    // not be put back, where the writes are not applied updates either. The
    // totals are gathered only now, once the lockfile step has settled which
    // files kept their edits.
    for scanned_file in &scanned {
        let mut file_result = scanned_file.result.clone();
        if refresh_failed(&lock_failures, &scanned_file.path) {
            file_result.updated.clear();
            file_result.update_context.clear();
            file_result.pinned.clear();
            file_result.normalized.clear();
            file_result.annotations.clear();
            file_result.action_sha_updates.clear();
        }
        total_result.merge(file_result);
    }
    total_result.errors.extend(lock_errors);

    // Save cache to disk
    if cache_enabled {
        let _ = Cache::save_shared(&cache);
    }

    // Emit cooldown unavailability notes, one per condition across all files.
    if let Ok(notes) = cooldown_notes.lock() {
        for note in notes.values() {
            eprintln!("note: {}", note);
        }
    }

    if text_mode {
        if !cli.quiet {
            println!();
            let applied = print_summary(
                &total_result,
                file_count,
                dry_run,
                filter,
                count_unfixable_floors(&floor_reports),
                count_skipped_floors(&floor_reports),
            );
            // Print the revert tip after a mutating run that applied at least one update.
            if !dry_run && applied > 0 {
                println!("{}", REVERT_TIP);
            }
            let implicit_dry_run = effective_dry_run && !cli.check && !cli.dry_run;
            if implicit_dry_run && applied > 0 {
                println!(
                    "{}",
                    "Run with --apply to write changes, or --interactive to approve individually."
                        .yellow()
                );
            }
        }
    } else {
        let notes_vec: Vec<String> = cooldown_notes
            .lock()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default();
        emit_update_json(
            UpdateReportInput {
                scanned: &scanned,
                total_result: &total_result,
                lock_failures: &lock_failures,
                file_count,
                dry_run,
                filter,
                file_cooldowns: &file_cooldowns,
                cooldown_notes: notes_vec,
                floor_reports,
                run_warnings,
            },
            &BoundedOutputParams::from_cli(cli),
        )?;
    }

    let has_errors = !total_result.errors.is_empty();
    let has_pending = has_checkable_manifest_changes(&total_result, filter) || floor_has_planned;
    let exit_code = upd::decide_exit_code(dry_run, has_pending, has_errors);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Parameters controlling bounded JSON output (--limit, --offset, --fields).
struct BoundedOutputParams<'a> {
    limit: Option<usize>,
    offset: usize,
    fields: &'a Option<String>,
}

impl<'a> BoundedOutputParams<'a> {
    fn from_cli(cli: &'a Cli) -> Self {
        Self {
            limit: cli.limit,
            offset: cli.offset,
            fields: &cli.fields,
        }
    }
}

/// Inputs needed to build the update JSON report.
struct UpdateReportInput<'a> {
    scanned: &'a [ScannedFileResult],
    total_result: &'a UpdateResult,
    /// Manifests whose lockfile refresh failed, keyed by scanned path.
    lock_failures: &'a LockFailures,
    file_count: usize,
    dry_run: bool,
    filter: UpdateFilter,
    file_cooldowns: &'a HashMap<PathBuf, Option<CooldownPolicy>>,
    cooldown_notes: Vec<String>,
    /// Version-floor file reports for lock-only `--package` targets (rule 6);
    /// empty when `--package` matched no lock-only names or was not given.
    floor_reports: Vec<upd::output::UpdateFileReport>,
    /// Run-level warnings that do not belong to one dependency file, including
    /// lock discovery guards and unmatched package patterns.
    run_warnings: Vec<String>,
}

/// Apply --limit, --offset, and --fields to a JSON document for bounded output.
///
/// `list_key` is the name of the top-level array field that is paginated
/// (e.g. "files" for update, "packages" for align, "vulnerabilities" for audit).
/// When limit/offset truncate the list, truncation metadata is injected at the
/// top level so consumers know they received a partial result.
fn apply_bounded_output(
    mut doc: serde_json::Value,
    list_key: &str,
    params: &BoundedOutputParams<'_>,
) -> serde_json::Value {
    let limit = params.limit;
    let offset = params.offset;
    let fields = params.fields;
    // Apply limit/offset to the list-shaped field.
    if let Some(arr) = doc.get_mut(list_key).and_then(|v| v.as_array_mut()) {
        let total = arr.len();
        let start = offset.min(total);
        let sliced: Vec<serde_json::Value> = arr[start..]
            .iter()
            .take(limit.unwrap_or(usize::MAX))
            .cloned()
            .collect();
        let returned = sliced.len();
        *arr = sliced;

        // Inject truncation metadata when the output is bounded.
        let is_truncated =
            offset > 0 || limit.is_some_and(|l| returned < total - start || l < total);
        if is_truncated && let Some(obj) = doc.as_object_mut() {
            obj.insert("total".to_string(), serde_json::json!(total));
            obj.insert("limit".to_string(), serde_json::json!(limit));
            obj.insert("offset".to_string(), serde_json::json!(offset));
        }
    }

    // Apply --fields to filter top-level keys.
    if let Some(fields_str) = fields {
        let keep: std::collections::HashSet<&str> = fields_str.split(',').map(str::trim).collect();
        if let Some(obj) = doc.as_object_mut() {
            obj.retain(|k, _| keep.contains(k.as_str()));
        }
    }

    doc
}

/// A floor's `UpdateEntry.status` counts toward the update summary
/// (`updates_total`, the bump buckets, `files_with_changes`) only when it
/// reflects an actual or would-be manifest write. `unfixable`, `failed`,
/// `rolled_back`, `skipped`, and `already_satisfied` are zero-change
/// diagnostics: the entry stays visible in `files[].updates[]`, but must not
/// be counted as a completed update.
fn floor_entry_counts_as_update(status: Option<&str>) -> bool {
    matches!(status, Some("planned" | "applied" | "pending_relock"))
}

/// Floor entries naming a newer release `upd` has no mechanism to write.
///
/// These make no manifest change, so `floor_entry_counts_as_update` keeps them
/// out of `updates_total`. They are still a discovered release waiting on a
/// human, so they are counted in their own right: a run that found one must not
/// close with "all dependencies up to date".
fn count_unfixable_floors(floor_reports: &[upd::output::UpdateFileReport]) -> usize {
    count_floor_status(floor_reports, "unfixable")
}

/// Floor entries naming a newer release `upd` could write but was told not to,
/// today only a `cargo-precise` floor under `--no-lock`. Nothing is written, so
/// these stay out of `updates_total` as well; a release is still waiting, and
/// unlike an unfixable one it needs no human judgement, only the run again
/// without the flag. Counting it is what keeps the run from closing on "all
/// dependencies up to date" one line under the warning saying otherwise.
fn count_skipped_floors(floor_reports: &[upd::output::UpdateFileReport]) -> usize {
    count_floor_status(floor_reports, "skipped")
}

fn count_floor_status(floor_reports: &[upd::output::UpdateFileReport], status: &str) -> usize {
    floor_reports
        .iter()
        .flat_map(|r| &r.updates)
        .filter(|entry| entry.status == Some(status))
        .count()
}

fn emit_update_json(input: UpdateReportInput<'_>, bounded: &BoundedOutputParams<'_>) -> Result<()> {
    use upd::output::{UpdateReport, UpdateSummary, build_update_file_report};

    let UpdateReportInput {
        scanned,
        total_result,
        lock_failures,
        file_count,
        dry_run,
        filter,
        file_cooldowns,
        cooldown_notes,
        floor_reports,
        run_warnings,
    } = input;

    let mut files: Vec<_> = scanned
        .iter()
        .map(|sf| {
            let cooldown_policy = file_cooldowns.get(&sf.path).and_then(|p| p.as_ref());
            let mut report = build_update_file_report(
                &sf.path,
                sf.file_type,
                &sf.result,
                cooldown_policy,
                |old, new| classify_update(old, new).as_str(),
            );
            if let Some(failure) = lock_failures.get(&sf.path) {
                report.record_lock_failure(&failure.message, failure.status);
            }
            report
        })
        .collect();

    let (major, minor, patch, total) = count_result_updates(total_result, filter);

    // Floor entries are gated by allows_bump/cooldown inside resolve_floor_version
    // already, so they count toward the summary unconditionally (rule 7) rather
    // than through count_updates_by_type's own filter re-application. But a
    // floor entry can also carry a diagnostic status (unfixable, failed,
    // rolled_back, skipped, already_satisfied) that made no manifest change,
    // so only entries whose status reflects an actual or would-be write count
    // toward the summary; every entry stays visible in files[].updates[].
    let (floor_major, floor_minor, floor_patch, floor_total) = floor_reports
        .iter()
        .flat_map(|r| &r.updates)
        .filter(|entry| floor_entry_counts_as_update(entry.status))
        .fold(
            (0usize, 0usize, 0usize, 0usize),
            |(maj, min, pat, tot), entry| match entry.bump {
                "major" => (maj + 1, min, pat, tot + 1),
                "minor" => (maj, min + 1, pat, tot + 1),
                _ => (maj, min, pat + 1, tot + 1),
            },
        );
    let floor_ignored: usize = floor_reports.iter().map(|r| r.ignored.len()).sum();
    let floor_files_with_changes = floor_reports
        .iter()
        .filter(|r| {
            r.updates
                .iter()
                .any(|entry| floor_entry_counts_as_update(entry.status))
        })
        .count();

    let summary = UpdateSummary {
        files_scanned: file_count,
        files_with_changes: scanned
            .iter()
            .filter(|sf| {
                file_has_manifest_changes(&sf.result) && !refresh_failed(lock_failures, &sf.path)
            })
            .count()
            + floor_files_with_changes,
        updates_total: total + floor_total,
        updates_major: major + floor_major,
        updates_minor: minor + floor_minor,
        updates_patch: patch + floor_patch,
        pinned: total_result.pinned.len(),
        ignored: total_result.ignored.len() + floor_ignored,
        errors: total_result.errors.len(),
        warnings: total_result.warnings.len() + run_warnings.len(),
        held_back: total_result.held_back.len(),
        skipped_by_cooldown: total_result.skipped_by_cooldown.len(),
        skipped: total_result
            .skipped
            .iter()
            .filter(|s| s.status == SkipStatus::Blocked)
            .count(),
        not_examined: total_result
            .skipped
            .iter()
            .filter(|s| s.status == SkipStatus::NotExamined)
            .count(),
        capped: total_result.capped.len(),
        annotations: total_result.annotations.len(),
        normalized: total_result.normalized.len(),
        unfixable: count_unfixable_floors(&floor_reports),
        skipped_floors: count_skipped_floors(&floor_reports),
    };

    files.extend(floor_reports);

    let report = UpdateReport {
        command: "update",
        mode: if dry_run { "dry-run" } else { "applied" },
        files,
        summary,
        cooldown_notes,
        warnings: run_warnings,
    };

    let doc = serde_json::to_value(&report)?;
    let doc = apply_bounded_output(doc, "files", bounded);
    println!("{}", serde_json::to_string_pretty(&doc)?);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_interactive_update(
    cli: &Cli,
    package_filter: &PackageFilter,
    files: &[(std::path::PathBuf, FileType)],
    paths: &[PathBuf],
    file_configs: &HashMap<PathBuf, Option<Arc<UpdConfig>>>,
    filter: UpdateFilter,
    pypi: &Arc<CachedRegistry<MultiPyPiRegistry>>,
    npm: &Arc<CachedRegistry<NpmRegistry>>,
    crates_io: &Arc<CachedRegistry<CratesIoRegistry>>,
    go_proxy: &Arc<CachedRegistry<GoProxyRegistry>>,
    rubygems: &Arc<CachedRegistry<RubyGemsRegistry>>,
    terraform: &Arc<CachedRegistry<TerraformRegistry>>,
    nuget: &Arc<CachedRegistry<NuGetRegistry>>,
    gradle: &Arc<CachedRegistry<GradleRegistry>>,
    github_releases: &Arc<CachedRegistry<GitHubReleasesRegistry>>,
    docker: &Arc<CachedRegistry<DockerRegistry>>,
    requirements_updater: &Arc<RequirementsUpdater>,
    pyproject_updater: &Arc<PyProjectUpdater>,
    package_json_updater: &Arc<PackageJsonUpdater>,
    cargo_toml_updater: &Arc<CargoTomlUpdater>,
    go_mod_updater: &Arc<GoModUpdater>,
    gemfile_updater: &Arc<GemfileUpdater>,
    github_actions_updater: &Arc<GithubActionsUpdater>,
    pre_commit_updater: &Arc<PreCommitUpdater>,
    mise_updater: &Arc<MiseUpdater>,
    terraform_updater: &Arc<TerraformUpdater>,
    csproj_updater: &Arc<CsprojUpdater>,
    docker_updater: &Arc<DockerUpdater>,
    annotated_updater: &Arc<AnnotatedUpdater>,
    cache: &Arc<std::sync::Mutex<Cache>>,
    cache_enabled: bool,
    file_cooldowns: &HashMap<PathBuf, Option<CooldownPolicy>>,
    cooldown_notes: Arc<Mutex<BTreeMap<String, String>>>,
) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "{}",
            serde_json::json!({
                "error": {
                    "kind": "confirmation_required",
                    "message": "--interactive requires a terminal on stdin",
                    "hint": "Use --check to preview updates, or --dry-run to print proposed changes.",
                    "exit_code": 2
                }
            })
        );
        std::process::exit(2);
    }

    // Rule 9: version floors write constraint hosts and relock, which does not
    // fit the per-package accept/reject prompt, so interactive floor
    // prompting is out of scope for v1. When a requested --package name is
    // lock-only (no manifest occurrence, but a hit in a scanned lockfile),
    // tell the user instead of silently doing nothing for that name. This
    // scan is best-effort: unlike the non-interactive floor branch, a
    // failure here must not abort the interactive session.
    if !package_filter.is_empty()
        && let Ok(manifest_packages) = scan_packages(files, &cli.langs, ParseWarnings::Suppress)
    {
        let lock_scan = lockscan::scan_locks(files, paths);
        let mut noted = HashSet::new();
        for locked in &lock_scan.packages {
            if package_filter_matches_locked(package_filter, locked)
                && is_lock_only_package(locked, &manifest_packages)
                && noted.insert(locked.name.clone())
            {
                eprintln!("{}", lock_only_interactive_note(&locked.name));
            }
        }
    }

    let mut pending_updates: Vec<PendingUpdate> = Vec::new();
    let mut planned_changes: Vec<PlannedChange> = Vec::new();
    let mut scanned_results: Vec<ScannedFileResult> = Vec::new();
    // A file the scan could not process at all. Counted here because it never
    // reaches `scanned_results`, so nothing downstream would otherwise know it
    // happened.
    let mut unscannable_files: usize = 0;

    for (path, file_type) in files {
        let cooldown_policy = file_cooldowns.get(path).and_then(|p| p.as_ref());
        let dry_run_options = build_update_options(
            true,
            cli.full_precision,
            cli.action_sha_override(),
            file_configs.get(path).cloned().flatten(),
            package_filter,
            &cli.langs,
            cli.annotation_langs.as_deref(),
            cooldown_policy,
            Arc::clone(&cooldown_notes),
            filter.to_bump_filter(),
        );

        if cli.verbose {
            eprintln!("{}", format!("Scanning: {}", display_path(path)).cyan());
        }

        let result = match file_type {
            FileType::Requirements => {
                requirements_updater
                    .update(path, pypi.as_ref(), dry_run_options.clone())
                    .await
            }
            FileType::PyProject => {
                pyproject_updater
                    .update(path, pypi.as_ref(), dry_run_options.clone())
                    .await
            }
            FileType::PackageJson => {
                package_json_updater
                    .update(path, npm.as_ref(), dry_run_options.clone())
                    .await
            }
            FileType::CargoToml => {
                cargo_toml_updater
                    .update(path, crates_io.as_ref(), dry_run_options.clone())
                    .await
            }
            FileType::GoMod => {
                go_mod_updater
                    .update(path, go_proxy.as_ref(), dry_run_options.clone())
                    .await
            }
            FileType::Gemfile => {
                gemfile_updater
                    .update(path, rubygems.as_ref(), dry_run_options.clone())
                    .await
            }
            FileType::GithubActions => {
                update_with_annotations(
                    github_actions_updater.as_ref(),
                    annotated_updater.as_ref(),
                    path,
                    github_releases.as_ref(),
                    dry_run_options.clone(),
                )
                .await
            }
            FileType::PreCommitConfig => {
                pre_commit_updater
                    .update(path, github_releases.as_ref(), dry_run_options.clone())
                    .await
            }
            FileType::MiseToml | FileType::ToolVersions => {
                mise_updater
                    .update(path, github_releases.as_ref(), dry_run_options.clone())
                    .await
            }
            FileType::GradleCatalog | FileType::GradleScript | FileType::GradleWrapper => {
                GradleUpdater::new()
                    .update(path, gradle.as_ref(), dry_run_options.clone())
                    .await
            }
            FileType::Csproj => {
                csproj_updater
                    .update(path, nuget.as_ref(), dry_run_options.clone())
                    .await
            }
            FileType::TerraformTf => {
                terraform_updater
                    .update(path, terraform.as_ref(), dry_run_options.clone())
                    .await
            }
            FileType::Dockerfile => {
                update_with_annotations(
                    docker_updater.as_ref(),
                    annotated_updater.as_ref(),
                    path,
                    docker.as_ref(),
                    dry_run_options.clone(),
                )
                .await
            }
            FileType::DockerCompose => {
                docker_updater
                    .update(path, docker.as_ref(), dry_run_options.clone())
                    .await
            }
            FileType::Annotated => {
                annotated_updater
                    .update(path, pypi.as_ref(), dry_run_options.clone())
                    .await
            }
        };

        match result {
            Ok(file_result) => {
                for line in format_scan_diagnostics(path, &file_result) {
                    eprintln!("{}", line);
                }

                // A capped update has nothing to prompt for, since the ceiling
                // already decided it, so it is reported here as the file is
                // scanned rather than through the accept/reject flow. Printing
                // it here also means it survives a run that HAS changes to
                // prompt for: gating it on "nothing else to do" would hide it
                // in exactly the busy repository that needs it most.
                if !cli.quiet {
                    let displayed_path = display_path(path);
                    for line in format_capped_lines(&displayed_path, &file_result) {
                        println!("{}", line);
                    }
                    // Reported here for the same reason, and left unwritten:
                    // the accept/reject flow carries version transitions, and
                    // an annotation is not one. A run without --interactive
                    // writes them.
                    for line in format_annotation_lines(&displayed_path, &file_result, true) {
                        println!("{}", line);
                    }
                    // And again for the same reason: a blocked pin is a
                    // dependency that could not be checked at all, so there is
                    // nothing to accept or reject, but it must not go unsaid.
                    for line in format_skipped_lines(&displayed_path, &file_result, cli.verbose) {
                        println!("{}", line);
                    }
                }

                for (index, update) in file_result.updated.iter().enumerate() {
                    let (package, old_version, new_version, line_num) = update;
                    let update_type = update_type(file_result.update_bump(index));

                    // Apply filter
                    if !filter.matches(update_type) {
                        continue;
                    }

                    pending_updates.push(PendingUpdate::new(
                        display_path(path),
                        *line_num,
                        package.clone(),
                        old_version.clone(),
                        new_version.clone(),
                        update_type == UpdateType::Major,
                    ));
                    pending_updates.last_mut().unwrap().context =
                        file_result.update_context.get(&index).cloned();
                    planned_changes.push(PlannedChange::from_update(
                        path.clone(),
                        *file_type,
                        update,
                    ));
                }

                for normalized in &file_result.normalized {
                    let previous = normalized
                        .previous_spec
                        .clone()
                        .unwrap_or_else(|| "(no specifier)".to_string());
                    let is_major = normalized.previous_version.as_deref().is_some_and(|old| {
                        upd::updater::classify_bump_for(Lang::Python, old, &normalized.version)
                            == BumpKind::Major
                    });
                    pending_updates.push(PendingUpdate::new(
                        display_path(path),
                        normalized.line_number,
                        normalized.package.clone(),
                        previous,
                        normalized.new_spec.clone(),
                        is_major,
                    ));
                    planned_changes.push(PlannedChange::from_normalized(
                        path.clone(),
                        *file_type,
                        normalized,
                    ));
                }

                scanned_results.push(ScannedFileResult {
                    path: path.clone(),
                    file_type: *file_type,
                    result: file_result,
                });
            }
            Err(e) => {
                eprintln!(
                    "{}",
                    format!("Error processing {}: {}", display_path(path), e).red()
                );
                unscannable_files += 1;
            }
        }
    }

    // A dependency that could not be read, or a configured pin that could not
    // be written, is a failed run however the session ends. The non-interactive
    // path reports these through `decide_exit_code`; interactive mode has three
    // ways out and has to say the same thing at each of them, or the same
    // manifest that exits 2 under `--check` exits 0 here and a script reads the
    // run as clean.
    let scan_errors: usize = unscannable_files
        + scanned_results
            .iter()
            .map(|scanned| scanned.result.errors.len())
            .sum::<usize>();

    for warning in unmatched_package_pattern_warnings(package_filter) {
        eprintln!("{} {}", "Warning:".yellow(), warning);
    }

    let annotation_total: usize = scanned_results
        .iter()
        .map(|scanned| scanned.result.annotations.len())
        .sum();
    // Both statuses withhold the tick, and both are counted here rather than
    // only the ones the scan loop named: a not-examined pin prints per-line
    // only under --verbose, so without this its count would be the sole thing
    // saying the run left SHA pins alone. Matches the non-interactive summary.
    let blocked_total: usize = scanned_results
        .iter()
        .map(|scanned| {
            scanned
                .result
                .skipped
                .iter()
                .filter(|skipped| skipped.status == SkipStatus::Blocked)
                .count()
        })
        .sum();
    let not_examined_groups = not_examined_groups(
        scanned_results
            .iter()
            .flat_map(|scanned| scanned.result.skipped.iter()),
    );
    let not_examined_total: usize = not_examined_groups
        .iter()
        .map(|(_, count)| count)
        .sum::<usize>();
    if !cli.quiet {
        if blocked_total > 0 {
            println!(
                "{} {} package(s) blocked by safety checks",
                "Blocked".yellow(),
                blocked_total.to_string().yellow().bold()
            );
        }
        print_not_examined_lines(&not_examined_groups);
    }
    if annotation_total > 0 && !cli.quiet {
        println!(
            "{} {} SHA pin(s) can have the release they name written beside them; run without {} to write them",
            "!".yellow().bold(),
            annotation_total.to_string().cyan().bold(),
            "--interactive".bold()
        );
    }

    if !has_interactive_changes(&pending_updates, &scanned_results) {
        if !cli.quiet {
            let capped: usize = scanned_results
                .iter()
                .map(|scanned| scanned.result.capped.len())
                .sum();
            let warning_total: usize = scanned_results
                .iter()
                .map(|scanned| scanned.result.warnings.len())
                .sum();
            if let Some(line) = format_interactive_closing_line(
                files.len(),
                capped,
                annotation_total,
                blocked_total + not_examined_total,
                warning_total,
            ) {
                println!("{line}");
            }
        }
        return finish_interactive(scan_errors);
    }

    let configured_pin_count: usize = scanned_results
        .iter()
        .map(|scanned| scanned.result.pinned.len())
        .sum();
    // Phase 2: Prompt user for each update
    let updates_with_decisions = if pending_updates.is_empty() {
        Vec::new()
    } else {
        prompt_all(pending_updates)?
    };

    let mut approved_change_counts =
        build_approved_change_counts(&updates_with_decisions, &planned_changes);
    let approved_count = updates_with_decisions.iter().filter(|u| u.approved).count();
    let approved_normalization_count = updates_with_decisions
        .iter()
        .zip(&planned_changes)
        .filter(|(update, change)| update.approved && change.kind == ChangeKind::Normalization)
        .count();
    let approved_version_count = approved_count - approved_normalization_count;

    if approved_count == 0 && configured_pin_count == 0 {
        if !cli.quiet {
            println!("\n{}", "No updates applied.".yellow());
        }
        return finish_interactive(scan_errors);
    }

    let mut apply_parts = Vec::new();
    if approved_version_count > 0 {
        apply_parts.push(format!("{} selected update(s)", approved_version_count));
    }
    if configured_pin_count > 0 {
        apply_parts.push(format!("{} configured pin(s)", configured_pin_count));
    }
    if approved_normalization_count > 0 {
        apply_parts.push(format!(
            "{} selected normalization(s)",
            approved_normalization_count
        ));
    }
    if !cli.quiet {
        println!(
            "\n{}",
            format!("Applying {}...", apply_parts.join(" and ")).cyan()
        );
    }

    let mut applied_updates = 0;
    let mut applied_pins = 0;
    let mut applied_normalizations = 0;
    let mut updated_files: Vec<std::path::PathBuf> = Vec::new();
    let mut changed_by_lockfile = ChangedByLockfile::new();
    // Under --lock, capture every lockfile-owning manifest and its lockfiles
    // before the first write, so a failed refresh can put them back.
    let lock_groups = if cli.lock {
        let files: Vec<(PathBuf, FileType)> = scanned_results
            .iter()
            .map(|scanned_file| (scanned_file.path.clone(), scanned_file.file_type))
            .collect();
        plan_lock_groups(&files)?
    } else {
        Vec::new()
    };
    // What each rewritten file received, so a rollback can take it back out
    // of the totals: (updates, pins, normalizations).
    let mut applied_by_file: HashMap<PathBuf, (usize, usize, usize)> = HashMap::new();

    for scanned_file in scanned_results {
        let selected_changes =
            collect_selected_changes_for_file(&scanned_file, &mut approved_change_counts);
        let selected_normalizations =
            take_approved_normalizations_for_file(&scanned_file, &mut approved_change_counts);
        if selected_changes.is_empty() && selected_normalizations.is_empty() {
            continue;
        }

        let content = read_file_safe(&scanned_file.path)?;
        let updates: Vec<_> = selected_changes
            .iter()
            .map(|change| VersionEdit {
                package: change.package.as_str(),
                old_version: change.old_version.as_str(),
                new_version: change.new_version.as_str(),
                line_num: change.line_num,
                expected_source: scanned_file
                    .result
                    .entry_ecosystem
                    .get(&change.package)
                    .copied(),
                // A workflow scan records the resolved commit transition beside
                // the version one.
                sha_pin: sha_pin_for(
                    &scanned_file.result.action_sha_updates,
                    &change.package,
                    change.line_num,
                ),
            })
            .collect();
        let rewritten = if scanned_file.file_type == FileType::PreCommitConfig {
            apply_selected_pre_commit_edits(
                &content,
                &updates,
                &scanned_file.result.pre_commit_edits,
            )
        } else if scanned_file.file_type == FileType::GradleWrapper {
            if updates.len() != 1 {
                anyhow::bail!("expected one Gradle wrapper update");
            }
            let edit = &updates[0];
            GradleUpdater::rewrite_wrapper(
                &content,
                edit.old_version,
                edit.new_version,
                gradle.as_ref(),
            )
            .await
            .map(|content| AppliedVersionUpdates {
                content,
                applied: vec![true],
            })
        } else {
            apply_version_updates(
                &content,
                &updates,
                scanned_file.file_type,
                cli.full_precision,
            )
        }
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to rewrite {}: {}",
                display_path(&scanned_file.path),
                e
            )
        })?;
        let rewritten_content = if selected_normalizations.is_empty() {
            rewritten.content
        } else {
            upd::updater::apply_normalized_specs(&rewritten.content, &selected_normalizations)?
        };

        if rewritten_content == content {
            continue;
        }

        write_file_atomic(&scanned_file.path, &rewritten_content)?;
        if scanned_file.file_type != FileType::Annotated {
            updated_files.push(scanned_file.path.clone());
        }
        applied_by_file.insert(
            scanned_file.path.clone(),
            (
                selected_changes
                    .iter()
                    .filter(|change| change.kind == ChangeKind::RegistryUpdate)
                    .count(),
                selected_changes
                    .iter()
                    .filter(|change| change.kind == ChangeKind::ConfigPin)
                    .count(),
                selected_normalizations.len(),
            ),
        );

        let file_str = display_path(&scanned_file.path);
        for normalized in &selected_normalizations {
            if scanned_file.file_type != FileType::Annotated {
                record_lockfile_changes(
                    &mut changed_by_lockfile,
                    &scanned_file.path,
                    [normalized.package.clone()],
                );
            }
            applied_normalizations += 1;
        }
        if !cli.quiet {
            for line in format_normalized_lines(&file_str, &selected_normalizations, false) {
                println!("{line}");
            }
        }
        for change in selected_changes {
            let location = match change.line_num {
                Some(n) => format!("{}:{}:", file_str, n),
                None => format!("{}:", file_str),
            };

            // Registry updates and config pins both contribute to the targeted
            // lockfile refresh, but only for the lockfile this manifest owns.
            if scanned_file.file_type != FileType::Annotated {
                record_lockfile_changes(
                    &mut changed_by_lockfile,
                    &scanned_file.path,
                    [change.package.clone()],
                );
            }

            match change.kind {
                ChangeKind::RegistryUpdate => {
                    applied_updates += 1;
                    if !cli.quiet {
                        println!(
                            "{} {} {} {} → {}",
                            location.blue().underline(),
                            "Updated".green(),
                            change.package.bold(),
                            change.old_version.dimmed(),
                            change.new_version.green(),
                        );
                    }
                }
                ChangeKind::ConfigPin => {
                    applied_pins += 1;
                    if !cli.quiet {
                        println!(
                            "{} {} {} {} → {} {}",
                            location.blue().underline(),
                            "Pinned".cyan(),
                            change.package.bold(),
                            change.old_version.dimmed(),
                            change.new_version.cyan(),
                            "(pinned)".dimmed(),
                        );
                    }
                }
                ChangeKind::Normalization => unreachable!("normalizations use their own writer"),
            }
        }
    }

    // Refresh lockfiles if requested and files were updated. A file whose
    // refresh failed is put back, and its writes leave the totals with it.
    let mut lock_errors = Vec::new();
    if cli.lock && !updated_files.is_empty() {
        for path in &updated_files {
            if !lock_groups
                .iter()
                .any(|group| group.manifests.contains(path))
            {
                print_no_lockfile_note(path);
            }
        }
        let refreshes = refresh_lock_groups(
            lock_groups,
            &updated_files,
            &changed_by_lockfile,
            cli.verbose,
        );
        if !refreshes.is_empty() && !cli.quiet {
            println!();
            println!("{}", "Regenerating lockfiles...".cyan());
        }
        let (lock_failures, errors) = report_lock_refreshes(refreshes, !cli.quiet);
        for (path, (updates, pins, normalizations)) in &applied_by_file {
            if refresh_failed(&lock_failures, path) {
                applied_updates -= updates;
                applied_pins -= pins;
                applied_normalizations -= normalizations;
            }
        }
        lock_errors = errors;
    }

    // Save cache to disk
    if cache_enabled {
        let _ = Cache::save_shared(cache);
    }

    if !cli.quiet {
        println!();
        if applied_updates > 0 {
            println!(
                "{} {} package(s)",
                "Updated".green(),
                applied_updates.to_string().green().bold()
            );
        }
        if applied_pins > 0 {
            println!(
                "{} {} package(s) to configured versions",
                "Pinned".cyan(),
                applied_pins.to_string().cyan().bold()
            );
        }
        if applied_normalizations > 0 {
            println!(
                "{} {} package(s) to the configured specifier shape",
                "Normalized".cyan(),
                applied_normalizations.to_string().cyan().bold()
            );
        }
    }

    if !lock_errors.is_empty() {
        eprintln!(
            "{}",
            serde_json::json!({
                "error": {
                    "kind": "io_error",
                    "message": lock_errors.join("; "),
                    "exit_code": 2
                }
            })
        );
        std::process::exit(2);
    }

    finish_interactive(scan_errors)
}

/// Close an interactive session, reporting a scan error the way every other
/// command reports one. Applying whatever the session did approve is still the
/// right outcome, so this runs after the writes rather than instead of them:
/// the exit code says a dependency was left unresolved, not that nothing
/// happened.
fn finish_interactive(scan_errors: usize) -> Result<()> {
    if scan_errors > 0 {
        std::process::exit(2);
    }
    Ok(())
}

async fn run_align(cli: &Cli) -> Result<()> {
    let text_mode = !effective_json_mode(cli);

    // Resolve paths: explicit > VCS root > error
    let paths = match resolve_scan_paths(cli) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!(
                "{}",
                serde_json::json!({"error": {"kind": "io_error", "message": msg, "exit_code": 2}})
            );
            std::process::exit(2);
        }
    };

    // Resolve config before discovery: `include` and `exclude` decide which files to scan
    // and `ignore` decides which packages to align, so both must be known up
    // front. Precedence mirrors `update`: explicit `--config` wins, else the
    // nearest discovered config.
    let resolved_config = resolve_root_config(cli, &paths)?;
    let (resolved_cli, no_ecosystems) = with_ecosystem_config(cli, &resolved_config.config)?;
    let cli = &resolved_cli;
    let config = Arc::clone(&resolved_config.config);

    if cli.verbose && text_mode {
        log_update_config_usage(&resolved_config);
    }

    let files = if no_ecosystems {
        Vec::new()
    } else {
        discover_files_with(
            &paths,
            &cli.langs,
            DiscoverOptions {
                no_ignore: cli.no_ignore,
                verbose: cli.verbose,
                include: &config.include,
                exclude: &config.exclude,
            },
        )
    };
    let file_count = files.len();

    if files.is_empty() {
        if text_mode {
            if !cli.quiet {
                println!("{}", "No dependency files found.".yellow());
            }
        } else {
            emit_align_json(&[], 0, &BoundedOutputParams::from_cli(cli))?;
        }
        return Ok(());
    }

    // Init TLS only after we know we're going to network. The empty-files
    // early return above must not be killed by a malformed CA bundle env var.
    init_tls(cli)?;

    if cli.verbose && text_mode {
        println!(
            "{}",
            format!("Scanning {} dependency file(s) for alignment", file_count).cyan()
        );
    }

    // Scan all files for packages
    let packages = match scan_packages(&files, &cli.langs, ParseWarnings::Print) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", format!("Error scanning files: {}", e).red());
            return Err(e);
        }
    };

    // Find alignments
    let align_result = find_alignments(packages);

    // Surface packages the config ignores so a green `--check` is explainable.
    // Goes to stderr (like the discovery "skipping <path>" lines) so it never
    // pollutes JSON on stdout.
    if cli.verbose {
        for p in &align_result.packages {
            if p.has_misalignment() && config.should_ignore(&p.package_name) {
                eprintln!("skipping {}: ignored by config", p.package_name);
            }
        }
    }

    // Filter to only misaligned packages the config does not ignore. The same
    // filter is applied to the JSON path so both output modes agree.
    let misaligned: Vec<&PackageAlignment> = align_result
        .packages
        .iter()
        .filter(|p| !config.should_ignore(&p.package_name))
        .filter(|p| p.has_misalignment())
        .collect();

    if !text_mode {
        let to_report: Vec<PackageAlignment> = align_result
            .packages
            .iter()
            .filter(|p| !config.should_ignore(&p.package_name))
            .filter(|p| p.has_misalignment())
            .cloned()
            .collect();
        emit_align_json(&to_report, file_count, &BoundedOutputParams::from_cli(cli))?;
    }

    if misaligned.is_empty() {
        if text_mode && !cli.quiet {
            println!(
                "{} Scanned {} file(s), all packages are aligned",
                "✓".green(),
                file_count
            );
        }
        return Ok(());
    }

    // Mutations are opt-in for align as well. --yes is an alias for --apply.
    let dry_run = cli.is_effective_dry_run();

    if text_mode && !cli.quiet {
        let action_prefix = if dry_run { "Would align" } else { "Aligning" };

        println!(
            "\n{} {} misaligned package(s) across {} file(s):\n",
            action_prefix,
            misaligned.len().to_string().yellow().bold(),
            file_count
        );

        for alignment in &misaligned {
            print_alignment(alignment, dry_run);
        }
    }

    // Apply alignments if not dry-run
    if !dry_run {
        let updated_count = apply_alignments(&misaligned, cli.full_precision)?;
        if text_mode && !cli.quiet {
            println!(
                "\n{} {} package occurrence(s)",
                "Aligned".green(),
                updated_count.to_string().green().bold()
            );
        }
    } else if text_mode && !cli.quiet {
        let total_misaligned: usize = misaligned
            .iter()
            .map(|a| a.misaligned_occurrences().len())
            .sum();
        println!(
            "\n{} {} package occurrence(s) to align",
            "Found".yellow(),
            total_misaligned.to_string().yellow().bold()
        );
        println!("Run with --apply to write changes.");
    }

    // Dry-run (including --check) signals pending misalignments with exit 1,
    // the same "changes available" signal `update` uses. Applying exits 0.
    if dry_run && !misaligned.is_empty() {
        std::process::exit(1);
    }

    Ok(())
}

fn emit_align_json(
    packages: &[PackageAlignment],
    file_count: usize,
    bounded: &BoundedOutputParams<'_>,
) -> Result<()> {
    use upd::output::{AlignReport, AlignSummary, build_align_package};

    let pkgs: Vec<_> = packages.iter().map(build_align_package).collect();
    let misaligned_packages = pkgs.iter().filter(|p| p.is_misaligned).count();
    let misaligned_occurrences = pkgs
        .iter()
        .flat_map(|p| p.occurrences.iter())
        .filter(|o| o.is_misaligned)
        .count();

    let report = AlignReport {
        command: "align",
        summary: AlignSummary {
            files_scanned: file_count,
            packages: pkgs.len(),
            misaligned_packages,
            misaligned_occurrences,
        },
        packages: pkgs,
    };

    let doc = serde_json::to_value(&report)?;
    let doc = apply_bounded_output(doc, "packages", bounded);
    println!("{}", serde_json::to_string_pretty(&doc)?);
    Ok(())
}

/// Build the deduplicated list of packages to submit to OSV.
///
/// The HashMap key is lowercased for case-insensitive alignment deduplication,
/// but OSV's NuGet ecosystem is case-sensitive. Each `PackageOccurrence` carries
/// `original_name` with the casing from the dependency file; that value is used
/// as `AuditPackage::name` so OSV queries reach the correct advisory.
pub(crate) fn build_audit_packages(
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
    lock_packages: &[lockscan::LockedPackage],
) -> Vec<AuditPackage> {
    let mut audit_packages: Vec<AuditPackage> = Vec::new();
    let mut seen: HashSet<(String, String, String)> = HashSet::new();

    // LOCK-IS-GROUND-TRUTH SUPPRESSION: manifest-derived versions are often
    // range fragments with the operator stripped (`pkg>=1.0` parses as
    // version "1.0"), i.e. versions that may not exist or aren't what is
    // installed. When a scanned lock resolves a package, the lock version is
    // authoritative, so the package is audited ONLY at its locked version(s):
    // no fabricated range-fragment query, no duplicate findings. Manifests
    // with no scanned lock (unlocked subprojects, partial-workspace skips)
    // keep today's manifest-derived behavior unchanged. Accepted imprecision:
    // in a multi-subproject scan where one subproject's lock resolves `pkg`
    // and another, unlocked subproject also declares `pkg`, that unlocked
    // entry is suppressed too - strictly no worse than today's
    // fabricated-version query.
    let locked_names: HashSet<(String, String)> = lock_packages
        .iter()
        .map(|lp| {
            let name = if lp.ecosystem == Ecosystem::PyPI {
                pep503_normalize(&lp.name)
            } else {
                lp.name.to_lowercase()
            };
            (name, lp.ecosystem.as_str().to_string())
        })
        .collect();

    for ((name, lang), occurrences) in packages {
        // OSV doesn't cover GitHub Actions, pre-commit hooks, mise tools, Docker,
        // Terraform, GitHub releases, or annotated pins; skip
        if *lang == Lang::Actions
            || *lang == Lang::PreCommit
            || *lang == Lang::Mise
            || *lang == Lang::Terraform
            || *lang == Lang::Docker
            || *lang == Lang::GithubReleases
            || *lang == Lang::Gradle
            || *lang == Lang::Annotated
        {
            continue;
        }

        let ecosystem = match lang {
            Lang::Python => Ecosystem::PyPI,
            Lang::Node => Ecosystem::Npm,
            Lang::Rust => Ecosystem::CratesIo,
            Lang::Go => Ecosystem::Go,
            Lang::Ruby => Ecosystem::RubyGems,
            Lang::DotNet => Ecosystem::NuGet,
            Lang::Actions
            | Lang::PreCommit
            | Lang::Mise
            | Lang::Terraform
            | Lang::Docker
            | Lang::GithubReleases
            | Lang::Gradle
            | Lang::Annotated => {
                unreachable!("filtered above")
            }
        };

        // PyPI names are PEP 503-normalized so `typing_extensions` (manifest)
        // and `typing-extensions` (lock) dedup and suppress as the same name.
        let key_name = if ecosystem == Ecosystem::PyPI {
            pep503_normalize(name)
        } else {
            name.clone()
        };
        if locked_names.contains(&(key_name.clone(), ecosystem.as_str().to_string())) {
            continue;
        }

        for occurrence in occurrences {
            let key = (
                key_name.clone(),
                occurrence.version.clone(),
                ecosystem.as_str().to_string(),
            );
            if seen.insert(key) {
                audit_packages.push(AuditPackage {
                    name: occurrence.original_name.clone(),
                    version: occurrence.version.clone(),
                    ecosystem,
                });
            }
        }
    }

    for lp in lock_packages {
        let key_name = if lp.ecosystem == Ecosystem::PyPI {
            pep503_normalize(&lp.name)
        } else {
            lp.name.to_lowercase()
        };
        let key = (
            key_name,
            lp.version.clone(),
            lp.ecosystem.as_str().to_string(),
        );
        if seen.insert(key) {
            audit_packages.push(AuditPackage {
                name: lp.name.clone(),
                version: lp.version.clone(),
                ecosystem: lp.ecosystem,
            });
        }
    }

    audit_packages
}

async fn run_audit(cli: &Cli) -> Result<()> {
    let no_fail = matches!(&cli.command, Some(Command::Audit { no_fail, .. }) if *no_fail);
    let fix_audit = matches!(&cli.command, Some(Command::Audit { fix_audit, .. }) if *fix_audit);
    let offline = matches!(&cli.command, Some(Command::Audit { offline, .. }) if *offline);
    let json_mode = effective_json_mode(cli);
    let text_mode = !json_mode && cli.format != Some(upd::cli::OutputFormat::Sarif);
    let sarif_mode = cli.format == Some(upd::cli::OutputFormat::Sarif) && !json_mode;
    // Audit never mutates files, so no VCS check is needed. Fall back to CWD.
    let paths = {
        let explicit = cli.get_paths();
        if explicit.is_empty() {
            vec![PathBuf::from(".")]
        } else {
            explicit
        }
    };
    // `include`/`exclude` path globs are honored uniformly across subcommands; resolve the
    // root config so audit drops the same files `update`/`align` would.
    let root_config = resolve_root_config(cli, &paths)?;
    let (resolved_cli, no_ecosystems) = with_ecosystem_config(cli, &root_config.config)?;
    let cli = &resolved_cli;
    let files = if no_ecosystems {
        Vec::new()
    } else {
        discover_files_with(
            &paths,
            &cli.langs,
            DiscoverOptions {
                no_ignore: cli.no_ignore,
                verbose: cli.verbose,
                include: &root_config.config.include,
                exclude: &root_config.config.exclude,
            },
        )
    };
    let file_count = files.len();
    let mut coverage_warnings = go_mod_coverage_warnings(&files);
    let lock_scan = lockscan::scan_locks(&files, &paths);
    coverage_warnings.extend(lock_scan.warnings.iter().cloned());

    if files.is_empty() {
        if text_mode {
            if !cli.quiet {
                println!("{}", "No dependency files found.".yellow());
            }
        } else if sarif_mode {
            emit_audit_sarif(&AuditResult::default(), &HashMap::new())?;
        } else {
            emit_audit_json(
                &AuditResult::default(),
                "complete",
                Vec::new(),
                &BoundedOutputParams::from_cli(cli),
            )?;
        }
        return Ok(());
    }

    if cli.verbose && text_mode {
        println!(
            "{}",
            format!(
                "Scanning {} dependency file(s) for vulnerabilities",
                file_count
            )
            .cyan()
        );
    }

    // Scan all files for packages
    let packages = match scan_packages(&files, &cli.langs, ParseWarnings::Print) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", format!("Error scanning files: {}", e).red());
            return Err(e);
        }
    };

    // Convert to audit packages (deduplicate by name+version+ecosystem)
    let package_filter = PackageFilter::new(cli.packages.clone()).map_err(anyhow::Error::msg)?;
    let mut audit_packages = build_audit_packages(&packages, &lock_scan.packages);
    audit_packages.retain(|package| {
        package_filter.matches(&package.name)
            || package_filter.patterns().iter().any(|pattern| {
                normalized_package_name(pattern, package.ecosystem)
                    == normalized_package_name(&package.name, package.ecosystem)
            })
    });
    coverage_warnings.extend(unmatched_package_pattern_warnings(&package_filter));

    if audit_packages.is_empty() {
        let empty_result = AuditResult {
            warnings: coverage_warnings.clone(),
            ..Default::default()
        };
        let status_str = if empty_result.warnings.is_empty() {
            "complete"
        } else {
            "incomplete"
        };
        for warning in &empty_result.warnings {
            eprintln!("{} {}", "Warning:".yellow(), warning);
        }
        if text_mode {
            if !cli.quiet {
                println!(
                    "{} Scanned {} file(s), no packages found",
                    "✓".green(),
                    file_count
                );
            }
        } else if sarif_mode {
            emit_audit_sarif(&empty_result, &HashMap::new())?;
        } else {
            emit_audit_json(
                &empty_result,
                status_str,
                Vec::new(),
                &BoundedOutputParams::from_cli(cli),
            )?;
        }
        return Ok(());
    }

    // Init TLS only when we're actually going to contact OSV. `--offline` reads
    // exclusively from the on-disk cache and must not be killed by a malformed
    // CA bundle env var; empty-files / empty-packages early returns above are
    // similarly network-free. Initialized before the user-visible "Checking…"
    // line so the `--insecure` warning lands on stderr first.
    if !offline {
        init_tls(cli)?;
    }

    if text_mode && !cli.quiet {
        println!(
            "{}",
            format!(
                "Checking {} unique package(s) for vulnerabilities...",
                audit_packages.len()
            )
            .cyan()
        );
    }

    // Query OSV API (with optional disk-backed cache)
    let osv_client = OsvClient::new();
    let audit_cache = if cli.no_cache {
        None
    } else {
        Some(AuditCache::new_shared())
    };
    let mut audit_result = osv_client
        .check_packages_cached(&audit_packages, audit_cache.as_ref(), offline)
        .await?;
    audit_result.warnings.extend(coverage_warnings);

    // Persist cache to disk after a successful query.
    if let Some(ref c) = audit_cache {
        // Non-fatal: a save failure should not abort the audit report.
        let _ = AuditCache::save_shared(c);
    }

    let status = audit_status(&audit_result);
    let status_str = match status {
        AuditStatus::Clean | AuditStatus::Vulnerable => "complete",
        AuditStatus::Incomplete => "incomplete",
    };

    if text_mode {
        if !cli.quiet {
            match status {
                AuditStatus::Clean => {
                    println!(
                        "\n{} No vulnerabilities found in {} package(s)",
                        "✓".green(),
                        audit_packages.len()
                    );
                }
                AuditStatus::Vulnerable => {
                    print_audit_vulnerabilities(&audit_result);
                }
                AuditStatus::Incomplete => {
                    let issue_count = audit_result.errors.len() + audit_result.warnings.len();
                    if audit_result.vulnerable.is_empty() {
                        println!(
                            "\n{} Audit incomplete: {} issue(s) occurred while checking {} package(s)",
                            "⚠".yellow().bold(),
                            issue_count.to_string().yellow().bold(),
                            audit_packages.len()
                        );
                    } else {
                        print_audit_vulnerabilities(&audit_result);
                        println!(
                            "\n{} Audit incomplete: {} issue(s) occurred while checking dependencies",
                            "⚠".yellow().bold(),
                            issue_count.to_string().yellow().bold()
                        );
                    }
                }
            }
        }

        for error in &audit_result.errors {
            eprintln!("{} {}", "Error:".red(), error);
        }
        for warning in &audit_result.warnings {
            eprintln!("{} {}", "Warning:".yellow(), warning);
        }
    } else if sarif_mode {
        // Errors always go to stderr; structured data to stdout.
        for error in &audit_result.errors {
            eprintln!("{} {}", "Error:".red(), error);
        }
        for warning in &audit_result.warnings {
            eprintln!("{} {}", "Warning:".yellow(), warning);
        }
        // Build the per-package occurrence map from already-scanned data.
        let sarif_occurrences = build_sarif_occurrences(&packages, &lock_scan.packages);
        emit_audit_sarif(&audit_result, &sarif_occurrences)?;
    } else {
        // JSON mode: errors to stderr, structured data to stdout.
        for error in &audit_result.errors {
            eprintln!("{} {}", "Error:".red(), error);
        }
        for warning in &audit_result.warnings {
            eprintln!("{} {}", "Warning:".yellow(), warning);
        }
        if !fix_audit {
            emit_audit_json(
                &audit_result,
                status_str,
                Vec::new(),
                &BoundedOutputParams::from_cli(cli),
            )?;
        }
    }

    // --fix-audit: route every vulnerable (name, version) pair to a concrete
    // fix action (manifest edit or version floor) and apply the routing in
    // transactional per-file/per-lockfile groups. Runs even when nothing is
    // vulnerable so the deferred JSON emission below always fires exactly
    // once under `--fix-audit && json_mode` (rule 2).
    if fix_audit {
        let prov =
            upd::lockscan::provenance::classify(&lock_scan.locks, &lock_scan.packages, &packages);
        let routing = route_fix_targets(&audit_result, &prov, &packages);

        // Diagnostics always go to stderr regardless of output mode, so
        // agents can detect them even in JSON/SARIF mode.
        if !cli.quiet {
            for u in &routing.unfixable {
                eprintln!(
                    "{} Cannot auto-fix {}: {}",
                    "⚠".yellow().bold(),
                    u.package.bold(),
                    u.reason
                );
            }
        }

        let all_blocked =
            routing.targets.is_empty() && routing.unfixable.iter().all(|u| u.no_fixed_version);
        let effective_dry_run = cli.is_effective_dry_run();

        let (outcomes, notes): (Vec<AppliedFix>, Vec<String>) = if all_blocked {
            (Vec::new(), Vec::new())
        } else {
            let opts = FixApplyOptions {
                dry_run: effective_dry_run,
                relock_manifests: !cli.no_lock,
                relock_floors: !cli.no_lock,
                verbose: cli.verbose && text_mode,
            };
            let manifest_editor =
                |path: &Path, file_type: FileType, targets: &[&FixTarget]| -> Result<bool> {
                    let content = read_file_safe(path)?;
                    let updates: Vec<VersionEdit<'_>> = targets
                        .iter()
                        .map(|t| VersionEdit {
                            package: t.dependency_key.as_deref().unwrap_or(t.package.as_str()),
                            old_version: t.from_version.as_str(),
                            new_version: t.to_version.as_str(),
                            line_num: t.line_number,
                            expected_source: None,
                            // Fix targets name a vulnerable version in a
                            // manifest, never an action's commit pin.
                            sha_pin: None,
                        })
                        .collect();
                    let applied =
                        apply_version_updates(&content, &updates, file_type, cli.full_precision)?;
                    if applied.content != content {
                        write_file_atomic(path, &applied.content)?;
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                };
            apply_fix_targets(routing.targets, &opts, &manifest_editor)
        };

        if !all_blocked {
            for note in &notes {
                eprintln!("note: {note}");
            }

            if text_mode && !cli.quiet {
                for outcome in &outcomes {
                    print_fix_outcome(outcome);
                }

                let planned = outcomes
                    .iter()
                    .filter(|o| o.status == FixStatus::Planned)
                    .count();
                let applied = outcomes
                    .iter()
                    .filter(|o| o.status == FixStatus::Applied)
                    .count();
                if effective_dry_run {
                    if planned > 0 {
                        println!(
                            "\n{} Would fix {} vulnerable package occurrence(s). Run with --apply to write changes.",
                            "→".yellow(),
                            planned.to_string().yellow().bold()
                        );
                    }
                } else {
                    println!(
                        "\n{} Fixed {} vulnerable package occurrence(s)",
                        "✓".green(),
                        applied.to_string().green().bold()
                    );
                }
            }
        }

        let fix_entries = upd::output::build_fix_entries(&outcomes, &routing.unfixable);
        if json_mode {
            emit_audit_json(
                &audit_result,
                status_str,
                fix_entries,
                &BoundedOutputParams::from_cli(cli),
            )?;
        }

        if !all_blocked {
            // Exit-code contract for --fix-audit:
            // - errors during fix (any Failed/RolledBack outcome) → 2
            // - dry-run with pending fixes and !no_fail → 1
            // - applied successfully (or no_fail) → 0
            let fix_errors: Vec<String> = outcomes
                .iter()
                .filter(|o| matches!(o.status, FixStatus::Failed | FixStatus::RolledBack))
                .filter_map(|o| o.error.clone())
                .collect();
            let has_planned = outcomes.iter().any(|o| o.status == FixStatus::Planned);
            let fix_exit_code = if !fix_errors.is_empty() {
                2
            } else if effective_dry_run && has_planned && !no_fail {
                1
            } else {
                0
            };

            if fix_exit_code == 2 {
                let combined = fix_errors.join("; ");
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "error": {
                            "kind": "io_error",
                            "message": combined,
                            "exit_code": 2
                        }
                    })
                );
                std::process::exit(2);
            } else if fix_exit_code != 0 {
                std::process::exit(fix_exit_code);
            }
            return Ok(());
        }
    }

    let exit_code = upd::decide_audit_exit_code(
        audit_result.total_vulnerabilities(),
        audit_result.errors.len(),
        no_fail,
    );
    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

fn emit_audit_json(
    audit: &AuditResult,
    status: &'static str,
    fixes: Vec<upd::output::FixEntry>,
    bounded: &BoundedOutputParams<'_>,
) -> Result<()> {
    use upd::output::build_audit_report;
    let report = build_audit_report(audit, 0, status, fixes);
    let doc = serde_json::to_value(&report)?;
    let doc = apply_bounded_output(doc, "vulnerabilities", bounded);
    println!("{}", serde_json::to_string_pretty(&doc)?);
    Ok(())
}

/// Report a shared transaction failure once, retaining project attribution and
/// all affected package names. Structured results remain per target.
fn fix_failure_messages(outcomes: &[AppliedFix]) -> Vec<String> {
    let mut groups: BTreeMap<_, Vec<&str>> = BTreeMap::new();
    for outcome in outcomes {
        if matches!(outcome.status, FixStatus::Failed | FixStatus::RolledBack)
            && let Some(error) = outcome.error.as_deref()
        {
            let names = groups
                .entry((
                    &outcome.target.path,
                    &outcome.target.lockfile,
                    outcome.status == FixStatus::RolledBack,
                    error,
                ))
                .or_default();
            if !names.contains(&outcome.target.package.as_str()) {
                names.push(&outcome.target.package);
            }
        }
    }
    groups
        .into_iter()
        .map(|((path, lockfile, rolled_back, error), mut names)| {
            names.sort_unstable();
            let mut message = format!("{} ({}): {error}", display_path(path), names.join(", "));
            if rolled_back {
                message.push_str(&format!("\nrolled back {}", display_path(path)));
                if let Some(lockfile) = lockfile
                    && lockfile != path
                {
                    message.push_str(&format!(" and {}", display_path(lockfile)));
                }
            }
            message
        })
        .collect()
}

/// Print a single `--fix-audit` outcome line in text mode.
///
/// Manifest edits keep the pre-existing "Would fix"/"Fixed" wording; version
/// floors (uv constraints, npm overrides, cargo-precise) get their own
/// "Would floor"/"Floored" wording naming the write target and method, since
/// they don't touch a version pin at a known line the way manifest edits do.
fn print_fix_outcome(outcome: &AppliedFix) {
    let target = &outcome.target;
    let path = display_path(&target.path);
    match (target.kind, outcome.status) {
        (FixKind::ManifestEdit, FixStatus::Planned) => {
            println!(
                "{} {} {} {} → {} {}",
                format!("{path}:").blue().underline(),
                "Would fix".yellow(),
                target.package.bold(),
                target.from_version.dimmed(),
                target.to_version.yellow(),
                "(security fix)".dimmed(),
            );
        }
        (FixKind::ManifestEdit, FixStatus::Applied) => {
            println!(
                "{} {} {} {} → {} {}",
                format!("{path}:").blue().underline(),
                "Fixed".green(),
                target.package.bold(),
                target.from_version.dimmed(),
                target.to_version.green(),
                "(security fix)".dimmed(),
            );
        }
        (_, FixStatus::Planned) => {
            println!(
                "{path}: Would floor {} {} → {} ({})",
                target.package.bold(),
                target.from_version.dimmed(),
                target.to_version.yellow(),
                target.kind.method(),
            );
        }
        (_, FixStatus::Applied) => {
            println!(
                "{path}: Floored {} {} → {} ({})",
                target.package.bold(),
                target.from_version.dimmed(),
                target.to_version.green(),
                target.kind.method(),
            );
        }
        (FixKind::ManifestEdit, FixStatus::PendingRelock) => {
            println!(
                "{} {}: edit written to {path} but lockfile not regenerated (--no-lock)",
                "⚠".yellow().bold(),
                target.package.bold(),
            );
        }
        (_, FixStatus::PendingRelock) => {
            println!(
                "{} {}: floor written to {path} but lockfile not regenerated (--no-lock)",
                "⚠".yellow().bold(),
                target.package.bold(),
            );
        }
        (_, FixStatus::Skipped) => {
            println!(
                "{} {}: {}",
                "⚠".yellow().bold(),
                target.package.bold(),
                outcome.error.as_deref().unwrap_or("skipped"),
            );
        }
        (_, FixStatus::Unfixable) => {
            // Apply-time unfixable: routing placed this target as fixable,
            // but the floor writer refused once it inspected the existing
            // entry (e.g. a non-simple uv constraint, an object-valued npm
            // override, or another unexpected shape). This is distinct from
            // a routing-time unfixable target (`routing.unfixable`, printed
            // before this loop runs) and must get its own report here or a
            // default text-mode run prints nothing and exits 0.
            eprintln!(
                "{} Cannot auto-fix {}: {}",
                "⚠".yellow().bold(),
                target.package.bold(),
                outcome.error.as_deref().unwrap_or("unfixable"),
            );
        }
        (_, FixStatus::AlreadySatisfied) => {
            // Silent in text mode by design: nothing changed.
        }
        (_, FixStatus::Failed) | (_, FixStatus::RolledBack) => {
            // Silent here; these reach the combined fix-errors stderr dump
            // emitted after this loop (see `fix_errors` below), which exits
            // the process with code 2.
        }
    }
}

/// Emit a SARIF 2.1.0 document for the audit result.
///
/// The `occurrences` map is keyed by `(package_name, version, ecosystem)` and
/// maps to the list of `(file_path, line_number)` pairs where that pin appears.
fn emit_audit_sarif(
    audit: &AuditResult,
    occurrences: &upd::output::SarifOccurrenceMap,
) -> Result<()> {
    use upd::output::build_sarif_audit_report;
    let log = build_sarif_audit_report(audit, occurrences);
    println!("{}", serde_json::to_string_pretty(&log)?);
    Ok(())
}

/// Build the occurrence map required by SARIF output from the scanned package data.
///
/// Returns a map keyed by `(package_name, version, ecosystem_str)` with values
/// being the list of `(file_path, line_number)` where that pin appears.
fn build_sarif_occurrences(
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
    lock_packages: &[lockscan::LockedPackage],
) -> upd::output::SarifOccurrenceMap {
    let mut map: upd::output::SarifOccurrenceMap = HashMap::new();
    for ((_, lang), occurrences) in packages {
        // Only ecosystems that OSV covers and that will appear in the audit result.
        if *lang == Lang::Actions
            || *lang == Lang::PreCommit
            || *lang == Lang::Mise
            || *lang == Lang::Terraform
            || *lang == Lang::Docker
            || *lang == Lang::GithubReleases
            || *lang == Lang::Gradle
            || *lang == Lang::Annotated
        {
            continue;
        }

        let ecosystem = match lang {
            Lang::Python => Ecosystem::PyPI,
            Lang::Node => Ecosystem::Npm,
            Lang::Rust => Ecosystem::CratesIo,
            Lang::Go => Ecosystem::Go,
            Lang::Ruby => Ecosystem::RubyGems,
            Lang::DotNet => Ecosystem::NuGet,
            Lang::Actions
            | Lang::PreCommit
            | Lang::Mise
            | Lang::Terraform
            | Lang::Docker
            | Lang::GithubReleases
            | Lang::Gradle
            | Lang::Annotated => {
                unreachable!("filtered above")
            }
        };

        for occ in occurrences {
            let key = (
                occ.original_name.clone(),
                occ.version.clone(),
                ecosystem.as_str().to_string(),
            );
            let uri = display_path(&occ.file_path);
            map.entry(key).or_default().push((uri, occ.line_number));
        }
    }

    // Lockfile anchors: a lock-only or lock-suppressed package's finding
    // is anchored to its lockfile entry instead of (or in addition to) any
    // manifest occurrence.
    for lp in lock_packages {
        let key = (
            lp.name.clone(),
            lp.version.clone(),
            lp.ecosystem.as_str().to_string(),
        );
        let uri = display_path(&lp.lockfile_path);
        map.entry(key).or_default().push((uri, lp.line_number));
    }

    map
}

fn print_audit_vulnerabilities(audit_result: &AuditResult) {
    println!(
        "\n{} Found {} vulnerability/ies in {} package(s):\n",
        "⚠".yellow().bold(),
        audit_result
            .total_vulnerabilities()
            .to_string()
            .red()
            .bold(),
        audit_result
            .vulnerable_packages()
            .to_string()
            .yellow()
            .bold()
    );

    for pkg_result in &audit_result.vulnerable {
        let ecosystem_str = match pkg_result.package.ecosystem {
            Ecosystem::PyPI => "(PyPI)",
            Ecosystem::Npm => "(npm)",
            Ecosystem::CratesIo => "(crates.io)",
            Ecosystem::Go => "(Go)",
            Ecosystem::RubyGems => "(RubyGems)",
            Ecosystem::NuGet => "(NuGet)",
            Ecosystem::Maven => "(Maven)",
        };

        println!(
            "  {} {}@{} {}",
            "●".red(),
            pkg_result.package.name.bold(),
            pkg_result.package.version.dimmed(),
            ecosystem_str.dimmed()
        );

        for vuln in &pkg_result.vulnerabilities {
            let severity_str = vuln
                .severity
                .as_ref()
                .map(|s| format!("[{}]", s).red().to_string())
                .unwrap_or_default();

            let summary = vuln
                .summary
                .as_ref()
                .map(|s| {
                    if s.len() > 60 {
                        format!("{}...", &s[..57])
                    } else {
                        s.clone()
                    }
                })
                .unwrap_or_else(|| "No description".to_string());

            let cve_aliases: Vec<&str> = vuln
                .aliases
                .iter()
                .filter(|a| a.starts_with("CVE-"))
                .map(String::as_str)
                .collect();
            let id_display = if cve_aliases.is_empty() {
                vuln.id.clone()
            } else {
                format!("{} ({})", vuln.id, cve_aliases.join(", "))
            };

            println!(
                "    {} {} {} {}",
                "├──".dimmed(),
                id_display.yellow(),
                severity_str,
                summary.dimmed()
            );

            if let Some(fixed) = &vuln.fixed_version {
                println!(
                    "    {}   {} {}",
                    "│".dimmed(),
                    "Fixed in:".dimmed(),
                    fixed.green()
                );
            }

            if let Some(url) = &vuln.url {
                println!("    {}   {}", "│".dimmed(), url.blue().underline());
            }
        }
        println!();
    }

    println!(
        "{} {} vulnerable package(s), {} total vulnerability/ies",
        "Summary:".bold(),
        audit_result.vulnerable_packages().to_string().yellow(),
        audit_result.total_vulnerabilities().to_string().red()
    );
}

fn print_alignment(alignment: &PackageAlignment, _dry_run: bool) {
    let lang_indicator = match alignment.lang {
        Lang::Python => "",
        Lang::Node => " (npm)",
        Lang::Rust => " (cargo)",
        Lang::Go => " (go)",
        Lang::Ruby => " (rubygems)",
        Lang::DotNet => " (nuget)",
        Lang::Gradle => " (gradle)",
        Lang::Actions => " (actions)",
        Lang::PreCommit => " (pre-commit)",
        Lang::Mise => " (mise)",
        Lang::Terraform => " (terraform)",
        Lang::Docker => " (docker)",
        Lang::GithubReleases => " (github-releases)",
        Lang::Annotated => " (annotated)",
    };

    println!(
        "  {}{}",
        alignment.package_name.bold(),
        lang_indicator.dimmed()
    );
    println!("    → {} (highest)", alignment.highest_version.green());

    for occurrence in &alignment.occurrences {
        let path = display_path(&occurrence.file_path);
        let location = match occurrence.line_number {
            Some(n) => format!("{path}:{n}"),
            None => path,
        };

        if occurrence.has_upper_bound {
            println!(
                "    {} {} {} {}",
                "├──".dimmed(),
                location.blue(),
                occurrence.version.dimmed(),
                "(constrained, skipped)".yellow()
            );
        } else if occurrence.version == alignment.highest_version {
            println!(
                "    {} {} {} {}",
                "├──".dimmed(),
                location.blue(),
                occurrence.version.green(),
                "(already aligned)".dimmed()
            );
        } else {
            println!(
                "    {} {} {} → {}",
                "├──".dimmed(),
                location.blue(),
                occurrence.version.red(),
                alignment.highest_version.green()
            );
        }
    }

    println!();
}

fn apply_alignments(alignments: &[&PackageAlignment], full_precision: bool) -> Result<usize> {
    use std::collections::HashMap;

    // Group updates by file path, carrying the FileType from the occurrence
    // (FileType::detect() doesn't handle path-based types like GithubActions)
    type FileUpdates<'a> = (FileType, Vec<VersionEdit<'a>>);
    let mut updates_by_file: HashMap<&std::path::Path, FileUpdates<'_>> = HashMap::new();

    for alignment in alignments {
        for occurrence in alignment.misaligned_occurrences() {
            updates_by_file
                .entry(occurrence.file_path.as_path())
                .or_insert_with(|| (occurrence.file_type, Vec::new()))
                .1
                .push(VersionEdit {
                    package: &alignment.package_name,
                    old_version: &occurrence.version,
                    new_version: &alignment.highest_version,
                    line_num: occurrence.line_number,
                    expected_source: None,
                    // Alignment equalises declared version strings across
                    // manifests; it never touches a workflow's commit pin.
                    sha_pin: None,
                });
        }
    }

    let mut total_updated = 0;

    for (path, (file_type, updates)) in updates_by_file {
        let content = read_file_safe(path)?;
        let applied_updates = apply_version_updates(&content, &updates, file_type, full_precision)
            .map_err(|e| anyhow::anyhow!("Failed to rewrite {}: {}", display_path(path), e))?;

        if applied_updates.content != content {
            write_file_atomic(path, &applied_updates.content)?;
            total_updated += applied_updates.applied_count();
        }
    }

    Ok(total_updated)
}

#[derive(Debug, Clone, Copy)]
struct VersionEdit<'a> {
    package: &'a str,
    old_version: &'a str,
    new_version: &'a str,
    line_num: Option<usize>,
    /// The source the interactive scan recorded for this package, when the file
    /// was an annotated one. `None` means the edit did not come from an
    /// annotation, and `apply_annotated_version` skips its source check.
    expected_source: Option<AnnotationSource>,
    /// The commit transition the scan resolved for this line, when the action is
    /// pinned to a SHA rather than a tag. `None` means the line carries a tag and
    /// is rewritten as one. Without this the tag-shaped rewrite would be applied
    /// to a commit pin, which is why SHA updates were once barred from the
    /// interactive path entirely.
    sha_pin: Option<&'a ActionShaUpdate>,
}

/// Find the commit transition a workflow scan resolved for `package` on
/// `line_num`.
///
/// Both sides must name a line. A workflow referencing the same action from two
/// jobs is ordinary, and matching on name alone would hand one line's commit to
/// the other, so a pin whose line is unknown matches nothing rather than the
/// first reference that happens to share its name. An edit left without its pin
/// fails to apply and is reported; it is never rewritten as a tag.
fn sha_pin_for<'a>(
    pins: &'a [ActionShaUpdate],
    package: &str,
    line_num: Option<usize>,
) -> Option<&'a ActionShaUpdate> {
    let line_num = line_num?;
    pins.iter()
        .find(|pin| pin.package == package && pin.line_number == Some(line_num))
}

#[derive(Debug)]
struct AppliedVersionUpdates {
    content: String,
    applied: Vec<bool>,
}

impl AppliedVersionUpdates {
    fn applied_count(&self) -> usize {
        self.applied.iter().filter(|applied| **applied).count()
    }
}

#[derive(Debug, Clone)]
struct TextDocument {
    lines: Vec<String>,
    line_endings: Vec<&'static str>,
}

impl TextDocument {
    fn from_content(content: &str) -> Self {
        let mut lines = Vec::new();
        let mut line_endings = Vec::new();
        for segment in content.split_inclusive('\n') {
            let (line, ending) = if let Some(line) = segment.strip_suffix("\r\n") {
                (line, "\r\n")
            } else if let Some(line) = segment.strip_suffix('\n') {
                (line, "\n")
            } else {
                (segment, "")
            };
            lines.push(line.to_string());
            line_endings.push(ending);
        }
        Self {
            lines,
            line_endings,
        }
    }

    fn into_content(self) -> String {
        let mut content = String::new();
        for (line, ending) in self.lines.into_iter().zip(self.line_endings) {
            content.push_str(&line);
            content.push_str(ending);
        }
        content
    }
}

fn apply_selected_pre_commit_edits(
    content: &str,
    updates: &[VersionEdit<'_>],
    planned: &[upd::updater::PreCommitEdit],
) -> Result<AppliedVersionUpdates> {
    let mut used = std::collections::HashSet::new();
    let mut edits = Vec::new();
    for update in updates {
        let Some((index, edit)) = planned.iter().enumerate().find(|(index, edit)| {
            !used.contains(index)
                && edit.package == update.package
                && edit.current == update.old_version
                && edit.new == update.new_version
                && edit.line == update.line_num
        }) else {
            anyhow::bail!("Cannot locate selected hook update for {}", update.package);
        };
        if content.get(edit.span.clone()) != Some(edit.original.as_str()) {
            anyhow::bail!("Hook configuration changed since preview; rerun upd");
        }
        used.insert(index);
        edits.push(edit);
    }
    for edit in &edits {
        if let Some((span, revision)) = &edit.required_revision
            && !edits
                .iter()
                .any(|e| e.span == *span && e.replacement == *revision)
        {
            anyhow::bail!(
                "Select the repository revision update together with {}: its hook language was resolved at {revision}",
                edit.package
            );
        }
    }
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.span.start));
    let mut rewritten = content.to_string();
    for edit in edits {
        rewritten.replace_range(edit.span.clone(), &edit.replacement);
    }
    Ok(AppliedVersionUpdates {
        content: rewritten,
        applied: vec![true; updates.len()],
    })
}

fn apply_version_updates(
    content: &str,
    updates: &[VersionEdit<'_>],
    file_type: FileType,
    full_precision: bool,
) -> Result<AppliedVersionUpdates> {
    if matches!(
        file_type,
        FileType::GradleCatalog | FileType::GradleScript | FileType::GradleWrapper
    ) {
        let edits: Vec<_> = updates
            .iter()
            .map(|u| (u.package, u.old_version, u.new_version, u.line_num))
            .collect();
        let content = GradleUpdater::apply_approved_updates(content, file_type, &edits)?;
        return Ok(AppliedVersionUpdates {
            content,
            applied: vec![true; updates.len()],
        });
    }
    let mut document = TextDocument::from_content(content);
    let mut applied = vec![false; updates.len()];

    for (idx, update) in updates.iter().enumerate() {
        let target_version = if full_precision {
            update.new_version.to_string()
        } else {
            match_version_precision(update.old_version, update.new_version)
        };

        applied[idx] = match file_type {
            FileType::Requirements => {
                apply_requirements_version(&mut document, update, &target_version)
            }
            FileType::PyProject => apply_pyproject_version(&mut document, update, &target_version),
            FileType::PackageJson => {
                apply_package_json_version(&mut document, update, &target_version)
            }
            FileType::CargoToml => apply_cargo_toml_version(&mut document, update, &target_version),
            FileType::GoMod => apply_go_mod_version(&mut document, update, &target_version),
            FileType::Gemfile => apply_gemfile_version(&mut document, update, &target_version),
            FileType::GithubActions => {
                apply_github_actions_version(&mut document, update, &target_version)
            }
            FileType::PreCommitConfig => {
                apply_pre_commit_version(&mut document, update, &target_version)
            }
            FileType::MiseToml => apply_mise_toml_version(&mut document, update, &target_version),
            FileType::ToolVersions => {
                apply_tool_versions_version(&mut document, update, &target_version)
            }
            FileType::GradleCatalog | FileType::GradleScript | FileType::GradleWrapper => {
                unreachable!("handled as a group")
            }
            FileType::Csproj => apply_csproj_version(&mut document, update, &target_version),
            FileType::TerraformTf => {
                apply_terraform_version(&mut document, update, &target_version)
            }
            FileType::Dockerfile | FileType::DockerCompose => {
                let current = document.clone().into_content();
                let updater = DockerUpdater::new();
                if file_type == FileType::Dockerfile && update.expected_source.is_some() {
                    let annotation = line_index(update.line_num).and_then(|idx| {
                        upd::updater::OwnsLines::annotations(&updater, &current)
                            .into_iter()
                            .nth(idx)
                    });
                    apply_annotated_version_with_outcome(
                        &mut document,
                        update,
                        &target_version,
                        annotation,
                    )
                } else if let Some(updated) = updater.apply_approved_update(
                    &current,
                    file_type,
                    update.package,
                    update.old_version,
                    &target_version,
                    update.line_num,
                ) {
                    document = TextDocument::from_content(&updated);
                    true
                } else {
                    false
                }
            }
            FileType::Annotated => apply_annotated_version(&mut document, update, &target_version),
        };
    }

    let unapplied: Vec<String> = updates
        .iter()
        .zip(applied.iter())
        .filter(|(_, applied)| !**applied)
        .map(|(update, _)| {
            let location = update
                .line_num
                .map(|line| format!(":{}", line))
                .unwrap_or_default();
            format!(
                "{}{} {} -> {}",
                update.package, location, update.old_version, update.new_version
            )
        })
        .collect();

    if !unapplied.is_empty() {
        anyhow::bail!(
            "Failed to apply {} version edit(s): {}",
            unapplied.len(),
            unapplied.join("; ")
        );
    }

    Ok(AppliedVersionUpdates {
        content: document.into_content(),
        applied,
    })
}

fn line_index(line_num: Option<usize>) -> Option<usize> {
    line_num.and_then(|line| line.checked_sub(1))
}

fn apply_unique_line_replacement<F>(
    document: &mut TextDocument,
    skip_idx: Option<usize>,
    replacer: &F,
) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    let mut candidate: Option<(usize, String)> = None;

    for idx in 0..document.lines.len() {
        if Some(idx) == skip_idx {
            continue;
        }

        if let Some(updated) = replacer(&document.lines[idx]) {
            if candidate.is_some() {
                return false;
            }

            candidate = Some((idx, updated));
        }
    }

    if let Some((idx, updated)) = candidate {
        document.lines[idx] = updated;
        return true;
    }

    false
}

fn replace_first_match(line: &str, re: &regex::Regex, replacement: &str) -> Option<String> {
    let updated = re.replacen(line, 1, replacement).to_string();
    (updated != line).then_some(updated)
}

fn apply_line_replacement<F>(
    document: &mut TextDocument,
    line_num: Option<usize>,
    replacer: F,
) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(idx) = line_index(line_num) {
        if idx < document.lines.len()
            && let Some(updated) = replacer(&document.lines[idx])
        {
            document.lines[idx] = updated;
            return true;
        }

        return apply_unique_line_replacement(
            document,
            (idx < document.lines.len()).then_some(idx),
            &replacer,
        );
    }

    for idx in 0..document.lines.len() {
        if let Some(updated) = replacer(&document.lines[idx]) {
            document.lines[idx] = updated;
            return true;
        }
    }

    false
}

fn apply_requirements_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    let pattern = format!(
        r"^(\s*{}(?:\[[^\]]*\])?\s*(?:==|>=|<=|~=|!=|>|<)\s*){}",
        regex::escape(update.package),
        regex::escape(update.old_version)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    let replacement = format!("${{1}}{}", target_version);

    apply_line_replacement(document, update.line_num, |line| {
        replace_first_match(line, &re, &replacement)
    })
}

fn apply_pyproject_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    let pep621_pattern = format!(
        r#"({}(?:\[[^\]]*\])?\s*(?:==|>=|<=|~=|!=|>|<)\s*){}"#,
        regex::escape(update.package),
        regex::escape(update.old_version)
    );
    let pep621_re = regex::Regex::new(&pep621_pattern).unwrap();
    let pep621_replacement = format!("${{1}}{}", target_version);
    if apply_line_replacement(document, update.line_num, |line| {
        replace_first_match(line, &pep621_re, &pep621_replacement)
    }) {
        return true;
    }

    let poetry_string_pattern = format!(
        r#"(^\s*{}\s*=\s*["'][~^>=<!]*){}"#,
        regex::escape(update.package),
        regex::escape(update.old_version)
    );
    let poetry_string_re = regex::Regex::new(&poetry_string_pattern).unwrap();
    let poetry_string_replacement = format!("${{1}}{}", target_version);
    if apply_line_replacement(document, update.line_num, |line| {
        replace_first_match(line, &poetry_string_re, &poetry_string_replacement)
    }) {
        return true;
    }

    let poetry_inline_pattern = format!(
        r#"(^\s*{}\s*=\s*\{{[^}}]*version\s*=\s*["'][~^>=<!]*){}"#,
        regex::escape(update.package),
        regex::escape(update.old_version)
    );
    let poetry_inline_re = regex::Regex::new(&poetry_inline_pattern).unwrap();
    let poetry_inline_replacement = format!("${{1}}{}", target_version);
    apply_line_replacement(document, update.line_num, |line| {
        replace_first_match(line, &poetry_inline_re, &poetry_inline_replacement)
    })
}

fn apply_package_json_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    let pattern = format!(
        r#"("{}"\s*:\s*"[\^~>=<]*){}(")"#,
        regex::escape(update.package),
        regex::escape(update.old_version)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    let replacement = format!(r#"${{1}}{}${{2}}"#, target_version);

    apply_line_replacement(document, update.line_num, |line| {
        replace_first_match(line, &re, &replacement)
    })
}

fn replace_cargo_inline_dependency_version(
    line: &str,
    package: &str,
    old_version: &str,
    new_version: &str,
) -> Option<String> {
    let simple_pattern = format!(
        r#"(^\s*{}\s*=\s*["'][~^>=<]*){}(["'])"#,
        regex::escape(package),
        regex::escape(old_version)
    );
    let simple_re = regex::Regex::new(&simple_pattern).unwrap();
    let simple_replacement = format!(r#"${{1}}{}${{2}}"#, new_version);
    if let Some(updated) = replace_first_match(line, &simple_re, &simple_replacement) {
        return Some(updated);
    }

    let inline_pattern = format!(
        r#"(^\s*{}\s*=\s*\{{[^}}]*version\s*=\s*["'][~^>=<]*){}(["'])"#,
        regex::escape(package),
        regex::escape(old_version)
    );
    let inline_re = regex::Regex::new(&inline_pattern).unwrap();
    let inline_replacement = format!(r#"${{1}}{}${{2}}"#, new_version);
    replace_first_match(line, &inline_re, &inline_replacement)
}

fn replace_cargo_table_version_assignment(
    line: &str,
    old_version: &str,
    new_version: &str,
) -> Option<String> {
    let pattern = format!(
        r#"(^\s*version\s*=\s*["'][~^>=<]*){}(["'])"#,
        regex::escape(old_version)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    let replacement = format!(r#"${{1}}{}${{2}}"#, new_version);
    replace_first_match(line, &re, &replacement)
}

fn is_cargo_dependency_header(line: &str, package: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return false;
    }

    let section = &trimmed[1..trimmed.len() - 1];
    section.contains("dependencies") && section.rsplit('.').next() == Some(package)
}

fn replace_cargo_version_in_following_table(
    document: &TextDocument,
    start_idx: usize,
    old_version: &str,
    new_version: &str,
) -> Option<(usize, String)> {
    for idx in start_idx + 1..document.lines.len() {
        if document.lines[idx].trim().starts_with('[') {
            break;
        }

        if let Some(updated) =
            replace_cargo_table_version_assignment(&document.lines[idx], old_version, new_version)
        {
            return Some((idx, updated));
        }
    }

    None
}

fn cargo_replacement_candidate(
    document: &TextDocument,
    start_idx: usize,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> Option<(usize, String)> {
    if start_idx >= document.lines.len() {
        return None;
    }

    if let Some(updated) = replace_cargo_inline_dependency_version(
        &document.lines[start_idx],
        update.package,
        update.old_version,
        target_version,
    ) {
        return Some((start_idx, updated));
    }

    if is_cargo_dependency_header(&document.lines[start_idx], update.package) {
        return replace_cargo_version_in_following_table(
            document,
            start_idx,
            update.old_version,
            target_version,
        );
    }

    None
}

fn apply_unique_cargo_replacement(
    document: &mut TextDocument,
    skip_idx: Option<usize>,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    let mut candidate: Option<(usize, String)> = None;

    for start_idx in 0..document.lines.len() {
        if Some(start_idx) == skip_idx {
            continue;
        }

        if let Some(found) =
            cargo_replacement_candidate(document, start_idx, update, target_version)
        {
            if candidate.is_some() {
                return false;
            }

            candidate = Some(found);
        }
    }

    if let Some((line_idx, updated)) = candidate {
        document.lines[line_idx] = updated;
        return true;
    }

    false
}

fn apply_cargo_toml_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    if let Some(idx) = line_index(update.line_num) {
        if let Some((line_idx, updated)) =
            cargo_replacement_candidate(document, idx, update, target_version)
        {
            document.lines[line_idx] = updated;
            return true;
        }

        return apply_unique_cargo_replacement(
            document,
            (idx < document.lines.len()).then_some(idx),
            update,
            target_version,
        );
    }

    for idx in 0..document.lines.len() {
        if let Some((line_idx, updated)) =
            cargo_replacement_candidate(document, idx, update, target_version)
        {
            document.lines[line_idx] = updated;
            return true;
        }
    }

    false
}

fn apply_go_mod_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    let pattern = format!(
        r"({}\s+){}(\s|$)",
        regex::escape(update.package),
        regex::escape(update.old_version)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    let replacement = format!("${{1}}{}${{2}}", target_version);

    apply_line_replacement(document, update.line_num, |line| {
        replace_first_match(line, &re, &replacement)
    })
}

fn apply_gemfile_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    let updater = GemfileUpdater::new();
    apply_line_replacement(document, update.line_num, |line| {
        updater.rewrite_floor(line, update.package, update.old_version, target_version)
    })
}

fn replace_csproj_inline_version(
    line: &str,
    package: &str,
    old_version: &str,
    new_version: &str,
) -> Option<String> {
    let pattern = format!(
        r#"(<(?:PackageReference|PackageVersion)\s+Include="{}"[^>]*Version="){}"#,
        regex::escape(package),
        regex::escape(old_version)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    let replacement = format!(r#"${{1}}{}"#, new_version);
    replace_first_match(line, &re, &replacement)
}

fn is_csproj_package_line(line: &str, package: &str) -> bool {
    let pattern = format!(
        r#"<(?:PackageReference|PackageVersion)\s+Include="{}""#,
        regex::escape(package)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    re.is_match(line)
}

fn replace_csproj_version_element(
    line: &str,
    old_version: &str,
    new_version: &str,
) -> Option<String> {
    let pattern = format!(
        r#"(<Version>\s*){}(\s*</Version>)"#,
        regex::escape(old_version)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    let replacement = format!("${{1}}{}${{2}}", new_version);
    replace_first_match(line, &re, &replacement)
}

fn apply_csproj_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    let candidate_indices: Vec<usize> = if let Some(idx) = line_index(update.line_num) {
        vec![idx]
    } else {
        document
            .lines
            .iter()
            .enumerate()
            .filter_map(|(idx, line)| is_csproj_package_line(line, update.package).then_some(idx))
            .collect()
    };

    for start_idx in candidate_indices {
        if start_idx >= document.lines.len() {
            continue;
        }

        if let Some(updated) = replace_csproj_inline_version(
            &document.lines[start_idx],
            update.package,
            update.old_version,
            target_version,
        ) {
            document.lines[start_idx] = updated;
            return true;
        }

        if !is_csproj_package_line(&document.lines[start_idx], update.package) {
            continue;
        }

        for idx in start_idx + 1..document.lines.len() {
            let trimmed = document.lines[idx].trim();

            if let Some(updated) = replace_csproj_version_element(
                &document.lines[idx],
                update.old_version,
                target_version,
            ) {
                document.lines[idx] = updated;
                return true;
            }

            if trimmed.starts_with("</PackageReference")
                || trimmed.starts_with("</PackageVersion")
                || trimmed.starts_with("<PackageReference")
                || trimmed.starts_with("<PackageVersion")
            {
                break;
            }
        }
    }

    false
}

fn apply_github_actions_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    // A commit pin is rewritten through the updater's own routine so both the
    // SHA and its version comment move together. `target_version` is deliberately
    // unused here: it has been through precision matching, while the pin's
    // new_version names the release that resolved to new_commit, and writing any
    // other string would leave the comment describing a different commit.
    if let Some(pin) = update.sha_pin {
        return apply_line_replacement(document, update.line_num, |line| {
            GithubActionsUpdater::new().replace_sha_pin(
                line,
                &pin.current_commit,
                &pin.current_version,
                &pin.new_commit,
                &pin.new_version,
            )
        });
    }

    let pattern = format!(
        r#"({}@){}(\s|$|#|")"#,
        regex::escape(update.package),
        regex::escape(update.old_version)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    let replacement = format!("${{1}}{}${{2}}", target_version);

    apply_line_replacement(document, update.line_num, |line| {
        replace_first_match(line, &re, &replacement)
    })
}

fn apply_pre_commit_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    let content = document.clone().into_content();
    if let Some(updated) = PreCommitUpdater::rewrite_revision(
        &content,
        update.package,
        update.old_version,
        target_version,
        update.line_num,
    ) {
        *document = TextDocument::from_content(&updated);
        true
    } else {
        false
    }
}

fn apply_mise_toml_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    let pattern = format!(
        r#"^("?{}?"?\s*=\s*"){}(")"#,
        regex::escape(update.package),
        regex::escape(update.old_version)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    let replacement = format!(r#"${{1}}{}${{2}}"#, target_version);

    apply_line_replacement(document, update.line_num, |line| {
        replace_first_match(line, &re, &replacement)
    })
}

fn apply_tool_versions_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    let pattern = format!(
        r"(?m)^({}\s+){}(\s|$)",
        regex::escape(update.package),
        regex::escape(update.old_version)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    let replacement = format!("${{1}}{}${{2}}", target_version);

    apply_line_replacement(document, update.line_num, |line| {
        replace_first_match(line, &re, &replacement)
    })
}

fn apply_terraform_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    let updater = TerraformUpdater::new();
    apply_line_replacement(document, update.line_num, |line| {
        updater.rewrite_floor(line, update.old_version, target_version)
    })
}

/// Re-derive an annotated line's meaning from the file on disk before writing
/// it. The interactive path scans, prompts, and only then writes, so the line
/// can have changed in between; every check here answers "is this still the
/// line the user approved?". Returning `false` makes the caller bail.
fn apply_annotated_version(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
) -> bool {
    let outcome = line_index(update.line_num)
        .and_then(|idx| document.lines.get(idx))
        .map(|line| annotation::parse_line(line));
    apply_annotated_version_with_outcome(document, update, target_version, outcome)
}

fn apply_annotated_version_with_outcome(
    document: &mut TextDocument,
    update: &VersionEdit<'_>,
    target_version: &str,
    outcome: Option<annotation::ParseOutcome>,
) -> bool {
    let Some(idx) = line_index(update.line_num) else {
        return false;
    };
    let Some(line) = document.lines.get(idx).cloned() else {
        return false;
    };
    let Some(annotation::ParseOutcome::Found(annotation)) = outcome else {
        return false;
    };
    // Byte for byte: the annotation names the registry's own spelling, and upd
    // must not normalise a name a user wrote deliberately.
    if annotation.package != update.package {
        return false;
    }
    if let Some(expected) = update.expected_source
        && annotation.source != expected
    {
        return false;
    }

    let spans = annotation::version_spans(&line, annotation.comment_start);
    let distinct = annotation::distinct_values(&line, &spans);
    if distinct.len() != 1 || distinct[0] != update.old_version {
        return false;
    }

    let target = annotation::reapply_v_prefix(update.old_version, target_version);
    document.lines[idx] = annotation::rewrite_spans(&line, &spans, &target);
    true
}

/// Filter configuration for update types
#[derive(Clone, Copy)]
struct UpdateFilter {
    major: bool,
    minor: bool,
    patch: bool,
}

impl UpdateFilter {
    /// Build a filter from CLI flags.
    ///
    /// `only_bump` is a list of exact levels to include (empty = all).
    /// `max_bump` is a ceiling level: only updates at or below that level are included.
    /// The two are mutually exclusive; clap enforces this at parse time.
    fn from_cli(only_bump: &[BumpLevel], max_bump: Option<BumpLevel>) -> Self {
        if let Some(max) = max_bump {
            return Self {
                major: matches!(max, BumpLevel::Major),
                minor: matches!(max, BumpLevel::Major | BumpLevel::Minor),
                patch: true,
            };
        }
        if only_bump.is_empty() {
            return Self {
                major: true,
                minor: true,
                patch: true,
            };
        }
        Self {
            major: only_bump.contains(&BumpLevel::Major),
            minor: only_bump.contains(&BumpLevel::Minor),
            patch: only_bump.contains(&BumpLevel::Patch),
        }
    }

    fn matches(&self, update_type: UpdateType) -> bool {
        match update_type {
            UpdateType::Major => self.major,
            UpdateType::Minor => self.minor,
            UpdateType::Patch => self.patch,
        }
    }

    /// Convert to the library-side filter that updaters consult at write time,
    /// so the bump ceiling gates the actual file writes (not just reporting).
    fn to_bump_filter(self) -> BumpFilter {
        BumpFilter {
            major: self.major,
            minor: self.minor,
            patch: self.patch,
        }
    }
}

fn count_result_updates(
    result: &UpdateResult,
    filter: UpdateFilter,
) -> (usize, usize, usize, usize) {
    let mut counts = (0, 0, 0, 0);
    for index in 0..result.updated.len() {
        let kind = update_type(result.update_bump(index));
        if !filter.matches(kind) {
            continue;
        }
        match kind {
            UpdateType::Major => counts.0 += 1,
            UpdateType::Minor => counts.1 += 1,
            UpdateType::Patch => counts.2 += 1,
        }
        counts.3 += 1;
    }
    counts
}

/// Counts updates by type, respecting the filter.
/// Returns (major_count, minor_count, patch_count, filtered_total)
#[cfg(test)]
fn count_updates_by_type(
    updates: &[(String, String, String, Option<usize>)],
    filter: UpdateFilter,
) -> (usize, usize, usize, usize) {
    updates.iter().fold(
        (0, 0, 0, 0),
        |(major, minor, patch, total), (_, old, new, _)| {
            let update_type = classify_update(old, new);
            if filter.matches(update_type) {
                match update_type {
                    UpdateType::Major => (major + 1, minor, patch, total + 1),
                    UpdateType::Minor => (major, minor + 1, patch, total + 1),
                    UpdateType::Patch => (major, minor, patch + 1, total + 1),
                }
            } else {
                (major, minor, patch, total)
            }
        },
    )
}

/// The one rendering of a held-back update, so the non-interactive report,
/// the interactive scan and the lock-only floor path never drift apart.
/// Naming the bump that exceeded the ceiling says what raising it would let
/// through.
fn format_capped_line(
    path: &str,
    line_number: Option<usize>,
    package: &str,
    current: &str,
    available: &str,
    bump: UpdateType,
) -> String {
    let location = match line_number {
        Some(n) => format!("{}:{}:", path, n),
        None => format!("{}:", path),
    };
    format!(
        "{} {} {} {} {} {} ({} bump)",
        location.blue().underline(),
        "Held back".yellow(),
        package.bold(),
        current.dimmed(),
        "→".dimmed(),
        available.cyan(),
        bump.as_str()
    )
}

/// The one rendering of an annotation, naming the release that was written and
/// the commit it describes.
///
/// The version is shown without an arrow: nothing moved, and rendering it as
/// `current → new` would read as an update that happened to keep its version.
fn format_annotation_line(
    path: &str,
    line_number: Option<usize>,
    package: &str,
    version: &str,
    commit: &str,
    dry_run: bool,
) -> String {
    let location = match line_number {
        Some(n) => format!("{}:{}:", path, n),
        None => format!("{}:", path),
    };
    let action = if dry_run {
        "Would annotate"
    } else {
        "Annotated"
    };
    format!(
        "{} {} {} {} {}",
        location.blue().underline(),
        action.cyan(),
        package.bold(),
        version.cyan(),
        format!("({})", short_commit(commit)).dimmed()
    )
}

/// A commit rendered the way Git and GitHub render one in prose: enough to
/// recognise, short enough to read beside a version.
fn short_commit(commit: &str) -> &str {
    let end = commit
        .char_indices()
        .nth(7)
        .map_or(commit.len(), |(idx, _)| idx);
    &commit[..end]
}

/// Every annotation for one file, rendered. A helper for the same reason
/// `format_capped_lines` is one: `--interactive` reports them without going
/// through the accept/reject flow, since there is no version choice to make.
fn format_annotation_lines(path: &str, result: &UpdateResult, dry_run: bool) -> Vec<String> {
    result
        .annotations
        .iter()
        .map(|annotation| {
            format_annotation_line(
                path,
                annotation.line_number,
                &annotation.package,
                &annotation.version,
                &annotation.commit,
                dry_run,
            )
        })
        .collect()
}

fn format_normalized_lines(
    path: &str,
    normalized: &[upd::updater::NormalizedSpec],
    dry_run: bool,
) -> Vec<String> {
    let action = if dry_run {
        "Would normalize"
    } else {
        "Normalized"
    };
    normalized
        .iter()
        .map(|entry| {
            let location = match entry.line_number {
                Some(n) => format!("{}:{}:", path, n),
                None => format!("{}:", path),
            };
            let previous = entry.previous_spec.as_deref().unwrap_or("(no specifier)");
            let mut line = format!(
                "{} {} {} {} → {}",
                location.blue().underline(),
                action.cyan(),
                entry.package.bold(),
                previous.dimmed(),
                entry.new_spec.cyan()
            );
            if entry.pinned {
                line.push_str(&format!(" {}", "(pinned)".dimmed()));
            }
            if let Some((skipped, _)) = &entry.held_back_from {
                line.push_str(&format!(
                    " {}",
                    format!("(held back from {skipped})").dimmed()
                ));
            }
            line
        })
        .collect()
}

/// The summary phrase for a group of dependencies upd did not examine.
///
/// Keyed by the reason token the updater recorded, so a reason with no phrase
/// here still gets an honest line rather than borrowing another reason's
/// wording. `--verbose` names each dependency with its own message.
fn not_examined_phrase(reason: &str) -> &'static str {
    match reason {
        "exact-pins-disabled" => "exact pin(s) preserved by configuration",
        "action-sha-updates-off" => "SHA-pinned action(s), not checked while SHA updates are off",
        "unsupported-backend" => "tool(s) on a backend upd cannot query",
        "unknown-tool" => "tool(s) upd knows no registry for",
        "symbolic-version" => "tool version(s) mise resolves at install time",
        _ => "dependency(ies) upd did not check",
    }
}

/// How many dependencies went unexamined for each reason, in the order the
/// reasons first appear, so a run that skips for two reasons reports both.
fn not_examined_groups<'a>(
    skipped: impl Iterator<Item = &'a SkippedUpdate>,
) -> Vec<(&'static str, usize)> {
    let mut groups: Vec<(&'static str, usize)> = Vec::new();
    for skip in skipped.filter(|skip| skip.status == SkipStatus::NotExamined) {
        let phrase = not_examined_phrase(skip.reason);
        match groups.iter_mut().find(|(known, _)| *known == phrase) {
            Some((_, count)) => *count += 1,
            None => groups.push((phrase, 1)),
        }
    }
    groups
}

/// Print one `Skipped` line per reason a dependency went unexamined.
fn print_not_examined_lines(groups: &[(&'static str, usize)]) {
    for (phrase, count) in groups {
        println!(
            "{} {} {}",
            "Skipped".dimmed(),
            count.to_string().dimmed(),
            phrase
        );
    }
}

/// Every skipped pin for one file, rendered.
///
/// A blocked pin needs attention on the line it is on, so it always prints. A
/// not-examined pin is the steady state for anyone who leaves SHA-pin updates
/// off, and a repo that pins every action would otherwise emit a line per
/// action on every run; the summary always reports the count, and `--verbose`
/// names them.
fn format_skipped_lines(path: &str, result: &UpdateResult, verbose: bool) -> Vec<String> {
    result
        .skipped
        .iter()
        .filter(|skipped| verbose || skipped.status != SkipStatus::NotExamined)
        .map(|skipped| {
            let location = match skipped.line_number {
                Some(n) => format!("{}:{}:", path, n),
                None => format!("{}:", path),
            };
            let label = match skipped.status {
                SkipStatus::Blocked => skipped.status.label().yellow(),
                SkipStatus::NotExamined => skipped.status.label().dimmed(),
            };
            format!(
                "{} {} {} {} ({})",
                location.blue().underline(),
                label,
                skipped.package.bold(),
                skipped.message,
                skipped.reason.dimmed()
            )
        })
        .collect()
}

/// Every held-back update for one file, rendered. Produced by a helper rather
/// than printed in place so `--interactive` can report them without a
/// terminal-driven test, the same reason `format_scan_diagnostics` exists.
fn format_capped_lines(path: &str, result: &UpdateResult) -> Vec<String> {
    result
        .capped
        .iter()
        .map(|capped| {
            format_capped_line(
                path,
                capped.line_number,
                &capped.package,
                &capped.current,
                &capped.available,
                update_type(capped.lang.map_or_else(
                    || classify_bump(&capped.current, &capped.available),
                    |lang| {
                        upd::updater::classify_bump_for(lang, &capped.current, &capped.available)
                    },
                )),
            )
        })
        .collect()
}

fn print_file_result(
    path: &str,
    file_type: FileType,
    result: &UpdateResult,
    dry_run: bool,
    filter: UpdateFilter,
    verbose: bool,
    cooldown_policy: Option<&CooldownPolicy>,
) {
    if result.updated.is_empty()
        && result.pinned.is_empty()
        && result.ignored.is_empty()
        && result.errors.is_empty()
        && result.warnings.is_empty()
        && result.held_back.is_empty()
        && result.skipped_by_cooldown.is_empty()
        && result.skipped.is_empty()
        && result.capped.is_empty()
        && result.annotations.is_empty()
        && result.normalized.is_empty()
    {
        return;
    }

    let action = if dry_run { "Would update" } else { "Updated" };

    for (index, (package, old, new, line_num)) in result.updated.iter().enumerate() {
        let update_type = update_type(result.update_bump(index));

        // Skip if filtered out
        if !filter.matches(update_type) {
            continue;
        }

        // Format location as "file:line:" (blue + underline for clickability)
        let location = match line_num {
            Some(n) => format!("{}:{}:", path, n),
            None => format!("{}:", path),
        };

        let type_indicator = match update_type {
            UpdateType::Major => " (MAJOR)".yellow().bold().to_string(),
            UpdateType::Minor => String::new(),
            UpdateType::Patch => String::new(),
        };

        let context = result.update_context.get(&index);
        let previous = context
            .and_then(|c| c.previous_spec.as_deref())
            .unwrap_or(old);
        let next = context.and_then(|c| c.new_spec.as_deref()).unwrap_or(new);
        let section = context
            .and_then(|c| c.section.as_deref())
            .map(|s| format!(" [{s}]"))
            .unwrap_or_default();
        println!(
            "{} {} {} {} → {}{}{}",
            location.blue().underline(),
            action.green(),
            package.bold(),
            previous.dimmed(),
            next.green(),
            type_indicator,
            section
        );
    }

    // Show pinned packages (always shown)
    let pinned_action = if dry_run { "Would pin" } else { "Pinned" };
    for (package, old, new, line_num) in &result.pinned {
        let location = match line_num {
            Some(n) => format!("{}:{}:", path, n),
            None => format!("{}:", path),
        };

        println!(
            "{} {} {} {} → {} {}",
            location.blue().underline(),
            pinned_action.cyan(),
            package.bold(),
            old.dimmed(),
            new.cyan(),
            "(pinned)".dimmed()
        );
    }

    // An annotation always prints, whatever the --filter: filters select a bump
    // level to write, and an annotation has no bump level to select on.
    for line in format_annotation_lines(path, result, dry_run) {
        println!("{line}");
    }

    for line in format_normalized_lines(path, &result.normalized, dry_run) {
        println!("{line}");
    }

    // Cooldown-related lines share a per-file location, but not a cooldown
    // duration: an annotated file's entries each carry their own ecosystem.
    if !result.held_back.is_empty() || !result.skipped_by_cooldown.is_empty() {
        let file_location = format!("{}:", path);
        let file_ecosystem = ecosystem_key(file_type);
        let now = Utc::now();

        for (index, (package, old, chosen, skipped_latest, skipped_pub_at)) in
            result.held_back.iter().enumerate()
        {
            let cooldown = upd::output::entry_cooldown(
                cooldown_policy,
                result
                    .held_back_sources
                    .get(&index)
                    .copied()
                    .or_else(|| result.entry_ecosystem.get(package).copied()),
                file_ecosystem,
            );
            let line = format_held_back_line(
                package,
                old,
                chosen,
                skipped_latest,
                *skipped_pub_at,
                cooldown,
                now,
            );
            println!("{} {}", file_location.blue().underline(), line.yellow());
        }

        for (index, (package, _current, skipped_latest, skipped_pub_at)) in
            result.skipped_by_cooldown.iter().enumerate()
        {
            let cooldown = upd::output::entry_cooldown(
                cooldown_policy,
                result
                    .cooldown_skip_sources
                    .get(&index)
                    .copied()
                    .or_else(|| result.entry_ecosystem.get(package).copied()),
                file_ecosystem,
            );
            let line = format_skipped_by_cooldown_line(
                package,
                skipped_latest,
                *skipped_pub_at,
                cooldown,
                now,
            );
            println!("{} {}", file_location.blue().underline(), line.dimmed());
        }
    }

    // Show ignored packages (only in verbose mode)
    if verbose {
        for (package, version, line_num) in &result.ignored {
            let location = match line_num {
                Some(n) => format!("{}:{}:", path, n),
                None => format!("{}:", path),
            };

            println!(
                "{} {} {} {} {}",
                location.blue().underline(),
                "Skipped".dimmed(),
                package.bold(),
                version.dimmed(),
                "(ignored)".dimmed()
            );
        }
    }

    // An update the ceiling held back always prints, whatever the --filter:
    // filters narrow which updates get written, and this one was not written.
    for line in format_capped_lines(path, result) {
        println!("{line}");
    }

    for line in format_skipped_lines(path, result, verbose) {
        println!("{line}");
    }

    for error in &result.errors {
        let location = format!("{}:", path);
        eprintln!(
            "{} {} {}",
            location.blue().underline(),
            "Error:".red(),
            error
        );
    }

    for warning in &result.warnings {
        let location = format!("{}:", path);
        eprintln!(
            "{} {} {}",
            location.blue().underline(),
            "Warning:".yellow(),
            warning
        );
    }
}

/// The closing line for an interactive run with nothing left to prompt for.
///
/// The green tick claims every dependency was checked, so it is withheld
/// whenever something was not: a skipped pin is one whose version was never
/// read, an annotation is a write still waiting to happen, and a warning is a
/// dependency that did not end where the run meant to leave it. All three have
/// already printed their own lines, so `None` means those lines have said
/// everything there is to say and a closing summary would only repeat them.
///
/// A capped update outranks the tick for the same reason it does in the
/// non-interactive summary: the ceiling, not the registry, decided it, and the
/// user is the one who set the ceiling.
///
/// Produced by a helper rather than printed in place so it can be asserted
/// without a terminal, the same reason `format_capped_lines` is one.
fn format_interactive_closing_line(
    file_count: usize,
    capped: usize,
    annotations: usize,
    skipped: usize,
    warnings: usize,
) -> Option<String> {
    if capped > 0 {
        return Some(format!(
            "{} Scanned {} file(s), {} update(s) held back by the bump ceiling (--max-bump/--only-bump)",
            "!".yellow().bold(),
            file_count,
            capped.to_string().yellow().bold()
        ));
    }
    if annotations > 0 || skipped > 0 || warnings > 0 {
        return None;
    }
    Some(format!(
        "{} Scanned {} file(s), all dependencies up to date",
        "✓".green(),
        file_count
    ))
}

/// The closing line for a run that found nothing to update.
///
/// "All dependencies up to date" is a claim that every dependency was checked,
/// so it is only printed when every lookup succeeded. When lookups failed the
/// line says how many could not be checked instead of showing a green tick
/// next to a run that may have checked nothing at all.
fn print_nothing_to_update_line(file_count: usize, up_to_date: usize, failed_lookups: usize) {
    if failed_lookups == 0 {
        println!(
            "{} Scanned {} file(s), all dependencies up to date",
            "✓".green(),
            file_count
        );
    } else {
        println!(
            "{} Scanned {} file(s), {} dependency(ies) up to date, {} could not be checked",
            "⚠".yellow().bold(),
            file_count,
            up_to_date,
            failed_lookups.to_string().red().bold()
        );
    }
}

/// Whether the run left nothing for the user to act on, so the closing tick can
/// claim every dependency is current.
///
/// The tick is a claim about the whole run, which makes every channel carrying
/// unfinished business a veto on it: updates the filter still shows, floors that
/// could not be raised, dependencies something declined to touch, and warnings
/// about ones upd looked at but would not rewrite. A run that prints one of
/// those and then a green tick has told the user two different things, and the
/// tick is the one they read.
///
/// A pure function rather than a condition written in place so each veto can be
/// asserted without a terminal, the same reason [`format_interactive_closing_line`]
/// is one.
fn nothing_outstanding(
    result: &UpdateResult,
    filtered_total: usize,
    unfixable_floors: usize,
    skipped_floors: usize,
) -> bool {
    filtered_total == 0
        && result.pinned.is_empty()
        && result.held_back.is_empty()
        && result.skipped_by_cooldown.is_empty()
        && result.skipped.is_empty()
        && result.capped.is_empty()
        && result.annotations.is_empty()
        && result.normalized.is_empty()
        && result.warnings.is_empty()
        && unfixable_floors == 0
        && skipped_floors == 0
}

fn print_summary(
    result: &UpdateResult,
    file_count: usize,
    dry_run: bool,
    filter: UpdateFilter,
    unfixable_floors: usize,
    skipped_floors: usize,
) -> usize {
    let action = if dry_run { "Would update" } else { "Updated" };

    // Count by update type, respecting filter
    let (major_count, minor_count, patch_count, filtered_total) =
        count_result_updates(result, filter);

    let pinned_count = result.pinned.len();
    let ignored_count = result.ignored.len();
    let held_back_count = result.held_back.len();
    let skipped_cooldown_count = result.skipped_by_cooldown.len();
    let blocked_count = result
        .skipped
        .iter()
        .filter(|s| s.status == SkipStatus::Blocked)
        .count();
    let capped_count = result.capped.len();
    let annotation_count = result.annotations.len();
    let normalized_count = result.normalized.len();
    // A warning is something that did not go the way the run intended: a
    // dependency upd found newer but will not rewrite, a version already ahead
    // of its registry.
    let warning_count = result.warnings.len();

    if nothing_outstanding(result, filtered_total, unfixable_floors, skipped_floors) {
        print_nothing_to_update_line(file_count, result.unchanged, result.errors.len());
    } else {
        // Build breakdown string for updates
        let mut parts = Vec::new();
        if major_count > 0 {
            parts.push(format!(
                "{} {}",
                major_count.to_string().yellow().bold(),
                "major".yellow()
            ));
        }
        if minor_count > 0 {
            parts.push(format!("{} minor", minor_count));
        }
        if patch_count > 0 {
            parts.push(format!("{} patch", patch_count));
        }
        let breakdown = if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        };

        if filtered_total > 0 {
            println!(
                "{} {} package(s){} in {} file(s), {} up to date",
                action,
                filtered_total.to_string().green().bold(),
                breakdown,
                file_count,
                result.unchanged
            );
        }

        // Show pinned count
        if pinned_count > 0 {
            let pinned_action = if dry_run { "Would pin" } else { "Pinned" };
            println!(
                "{} {} package(s) to configured versions",
                pinned_action,
                pinned_count.to_string().cyan().bold()
            );
        }

        // Show the annotation count. Nothing moved, so this cannot ride along
        // on the update line, and the file was rewritten, so it cannot be
        // folded into the up-to-date tally either.
        if annotation_count > 0 {
            let annotate_action = if dry_run {
                "Would annotate"
            } else {
                "Annotated"
            };
            println!(
                "{} {} package(s) with the release their pinned commit belongs to",
                annotate_action,
                annotation_count.to_string().cyan().bold()
            );
        }

        if normalized_count > 0 {
            let normalize_action = if dry_run {
                "Would normalize"
            } else {
                "Normalized"
            };
            println!(
                "{} {} package(s) to the configured specifier shape",
                normalize_action,
                normalized_count.to_string().cyan().bold()
            );
        }

        // Show the count of newer releases upd found but has no mechanism to
        // write. The reason went to stderr per package; this line exists so
        // stdout never closes on a tick claiming everything is current.
        if unfixable_floors > 0 {
            println!(
                "{} {} package(s) with a newer release available",
                "Cannot auto-fix".yellow(),
                unfixable_floors.to_string().yellow().bold()
            );
        }

        // Show the count of floors upd was told not to write. The flag that
        // blocked each one went to stdout per package; this line exists for the
        // same reason as the one above, and it says "not written" rather than
        // "skipped" because something is still waiting on this run.
        if skipped_floors > 0 {
            println!(
                "{} {} package(s) with a newer release available",
                "Not written".yellow(),
                skipped_floors.to_string().yellow().bold()
            );
        }

        // Show capped count. These are real waiting updates, so the line has to
        // appear even when nothing was written, or the run reads as "all up to
        // date" while a major release sits there.
        if capped_count > 0 {
            println!(
                "{} {} package(s) held back by the bump ceiling (--max-bump/--only-bump)",
                "Held back".yellow(),
                capped_count.to_string().yellow().bold()
            );
        }

        // Show held-back count (cooldown caused selection of older safe version)
        if held_back_count > 0 {
            println!(
                "{} {} package(s) held back by cooldown",
                "Held back".yellow(),
                held_back_count.to_string().yellow().bold()
            );
        }

        // Show skipped-by-cooldown count (no version old enough)
        if skipped_cooldown_count > 0 {
            println!(
                "{} {} package(s) skipped (cooldown)",
                "Skipped".dimmed(),
                skipped_cooldown_count.to_string().dimmed()
            );
        }

        if blocked_count > 0 {
            println!(
                "{} {} package(s) blocked by safety checks",
                "Blocked".yellow(),
                blocked_count.to_string().yellow().bold()
            );
        }

        print_not_examined_lines(&not_examined_groups(result.skipped.iter()));

        // Each warning already named its package on stderr. This line exists so
        // that stdout, which is where the tick would otherwise be, carries the
        // count too.
        if warning_count > 0 {
            println!(
                "{} {} warning(s), see above",
                "Warning".yellow(),
                warning_count.to_string().yellow().bold()
            );
        }
    }

    // Show ignored count (informational)
    if ignored_count > 0 {
        println!(
            "{} {} package(s) per config",
            "Skipped".dimmed(),
            ignored_count.to_string().dimmed()
        );
    }

    if !result.errors.is_empty() {
        eprintln!(
            "{} error(s) occurred",
            result.errors.len().to_string().red().bold()
        );
    }

    filtered_total + normalized_count
}

fn clean_cache() -> Result<()> {
    Cache::clean()?;
    println!("{}", "Cache cleaned successfully.".green());
    Ok(())
}

async fn self_update(cli: &Cli) -> Result<()> {
    init_tls(cli)?;
    println!("Checking for updates...");

    let url = "https://api.github.com/repos/rvben/upd/releases/latest";
    let client =
        upd::http::apply(reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)))
            .build()?;
    let response = client
        .get(url)
        .header("User-Agent", "upd")
        .send()
        .await
        .map_err(|e| upd::http::wrap_send_err(e, url))?;

    if !response.status().is_success() {
        anyhow::bail!("Failed to check for updates: HTTP {}", response.status());
    }

    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }

    let release: Release = response.json().await?;
    let latest = release.tag_name.trim_start_matches('v');

    if latest == VERSION {
        println!(
            "{}",
            format!("Already at latest version ({})", VERSION).green()
        );
        return Ok(());
    }

    println!(
        "{}",
        format!("New version available: {} → {}", VERSION, latest).yellow()
    );
    println!("To update, run: cargo install upd");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use tempfile::tempdir;
    use upd::align::PackageOccurrence;

    /// A name with no manifest occurrence but a hit in a scanned lockfile is
    /// lock-only (rule 2); a name occurring in the manifest is not, even
    /// when the same name also appears in the lockfile.
    #[test]
    fn is_lock_only_package_detects_lock_only_and_manifest_matched() {
        let locked = upd::lockscan::LockedPackage {
            name: "lockonly".to_string(),
            version: "0.40.0".to_string(),
            ecosystem: Ecosystem::PyPI,
            lockfile_path: PathBuf::from("uv.lock"),
            line_number: None,
            locator: None,
        };
        let empty_manifest: HashMap<(String, Lang), Vec<PackageOccurrence>> = HashMap::new();
        assert!(is_lock_only_package(&locked, &empty_manifest));

        let mut manifest_with_occurrence: HashMap<(String, Lang), Vec<PackageOccurrence>> =
            HashMap::new();
        manifest_with_occurrence.insert(
            ("lockonly".to_string(), Lang::Python),
            vec![PackageOccurrence {
                file_path: PathBuf::from("pyproject.toml"),
                file_type: FileType::PyProject,
                version: "0.40.0".to_string(),
                line_number: None,
                has_upper_bound: false,
                original_name: "lockonly".to_string(),
                is_bumpable: true,
            }],
        );
        assert!(!is_lock_only_package(&locked, &manifest_with_occurrence));
    }

    #[test]
    fn lock_filter_preserves_normalized_exact_names_but_not_glob_case() {
        let locked = upd::lockscan::LockedPackage {
            name: "typing-extensions".to_string(),
            version: "4.0.0".to_string(),
            ecosystem: Ecosystem::PyPI,
            lockfile_path: PathBuf::from("uv.lock"),
            line_number: None,
            locator: None,
        };

        let exact = PackageFilter::new(vec!["Typing_Extensions".to_string()]).unwrap();
        assert!(package_filter_matches_locked(&exact, &locked));

        let glob = PackageFilter::new(vec!["Typing*".to_string()]).unwrap();
        assert!(!package_filter_matches_locked(&glob, &locked));
    }

    /// Rule 9: the interactive early path's note names the package and
    /// tells the user to rerun without `--interactive`, since a version
    /// floor cannot be offered through the per-package accept/reject prompt.
    #[test]
    fn interactive_lock_only_package_gets_note() {
        let note = lock_only_interactive_note("lockonly");
        assert!(
            note.contains("lockonly is a lock-only dependency"),
            "{note}"
        );
        assert!(note.contains("rerun without --interactive"), "{note}");
    }

    #[test]
    fn test_classify_update_major() {
        assert_eq!(classify_update("1.0.0", "2.0.0"), UpdateType::Major);
        assert_eq!(classify_update("1.5.3", "2.0.0"), UpdateType::Major);
        assert_eq!(classify_update("0.9.0", "1.0.0"), UpdateType::Major);
    }

    /// The reported label has to agree with the ceiling that gates the write,
    /// so a breaking zero-major step reads as major here too.
    #[test]
    fn test_classify_update_zero_major_minor_step_is_major() {
        assert_eq!(classify_update("0.12", "0.13"), UpdateType::Major);
        assert_eq!(classify_update("0.12.1", "0.13.0"), UpdateType::Major);
    }

    #[test]
    fn test_classify_update_minor() {
        assert_eq!(classify_update("1.0.0", "1.1.0"), UpdateType::Minor);
        assert_eq!(classify_update("1.5.3", "1.6.0"), UpdateType::Minor);
        assert_eq!(classify_update("2.0.0", "2.5.0"), UpdateType::Minor);
    }

    #[test]
    fn test_classify_update_patch() {
        assert_eq!(classify_update("1.0.0", "1.0.1"), UpdateType::Patch);
        assert_eq!(classify_update("1.5.3", "1.5.4"), UpdateType::Patch);
        assert_eq!(classify_update("2.0.0", "2.0.10"), UpdateType::Patch);
        // Still inside `^0.12`, so still compatible.
        assert_eq!(classify_update("0.12.1", "0.12.4"), UpdateType::Patch);
    }

    #[test]
    fn test_classify_update_invalid_versions() {
        // Invalid versions default to Patch
        assert_eq!(classify_update("abc", "1.0.0"), UpdateType::Patch);
        assert_eq!(classify_update("1.0.0", "abc"), UpdateType::Patch);
    }

    #[test]
    fn test_update_filter_defaults_to_all() {
        let filter = UpdateFilter::from_cli(&[], None);
        assert!(filter.major);
        assert!(filter.minor);
        assert!(filter.patch);
    }

    #[test]
    fn test_update_filter_major_only() {
        let filter = UpdateFilter::from_cli(&[BumpLevel::Major], None);
        assert!(filter.major);
        assert!(!filter.minor);
        assert!(!filter.patch);
    }

    #[test]
    fn test_update_filter_minor_only() {
        let filter = UpdateFilter::from_cli(&[BumpLevel::Minor], None);
        assert!(!filter.major);
        assert!(filter.minor);
        assert!(!filter.patch);
    }

    #[test]
    fn test_update_filter_patch_only() {
        let filter = UpdateFilter::from_cli(&[BumpLevel::Patch], None);
        assert!(!filter.major);
        assert!(!filter.minor);
        assert!(filter.patch);
    }

    #[test]
    fn test_update_filter_combined() {
        let filter = UpdateFilter::from_cli(&[BumpLevel::Major, BumpLevel::Minor], None);
        assert!(filter.major);
        assert!(filter.minor);
        assert!(!filter.patch);
    }

    #[test]
    fn test_update_filter_matches() {
        let filter = UpdateFilter::from_cli(&[BumpLevel::Major], None);
        assert!(filter.matches(UpdateType::Major));
        assert!(!filter.matches(UpdateType::Minor));
        assert!(!filter.matches(UpdateType::Patch));

        let filter = UpdateFilter::from_cli(&[BumpLevel::Minor, BumpLevel::Patch], None);
        assert!(!filter.matches(UpdateType::Major));
        assert!(filter.matches(UpdateType::Minor));
        assert!(filter.matches(UpdateType::Patch));
    }

    #[test]
    fn test_update_filter_max_bump_major_allows_all() {
        let filter = UpdateFilter::from_cli(&[], Some(BumpLevel::Major));
        assert!(filter.major);
        assert!(filter.minor);
        assert!(filter.patch);
    }

    #[test]
    fn test_update_filter_max_bump_minor_excludes_major() {
        let filter = UpdateFilter::from_cli(&[], Some(BumpLevel::Minor));
        assert!(!filter.major);
        assert!(filter.minor);
        assert!(filter.patch);
    }

    #[test]
    fn test_update_filter_max_bump_patch_allows_only_patch() {
        let filter = UpdateFilter::from_cli(&[], Some(BumpLevel::Patch));
        assert!(!filter.major);
        assert!(!filter.minor);
        assert!(filter.patch);
    }

    #[test]
    fn test_count_updates_by_type_empty() {
        let updates: Vec<(String, String, String, Option<usize>)> = vec![];
        let filter = UpdateFilter::from_cli(&[], None); // show all

        let (major, minor, patch, total) = count_updates_by_type(&updates, filter);
        assert_eq!(major, 0);
        assert_eq!(minor, 0);
        assert_eq!(patch, 0);
        assert_eq!(total, 0);
    }

    #[test]
    fn test_count_updates_by_type_mixed() {
        let updates = vec![
            ("pkg1".into(), "1.0.0".into(), "2.0.0".into(), Some(1)), // major
            ("pkg2".into(), "1.0.0".into(), "1.1.0".into(), Some(2)), // minor
            ("pkg3".into(), "1.0.0".into(), "1.0.1".into(), Some(3)), // patch
            ("pkg4".into(), "2.0.0".into(), "3.0.0".into(), Some(4)), // major
            ("pkg5".into(), "1.5.0".into(), "1.5.1".into(), Some(5)), // patch
        ];
        let filter = UpdateFilter::from_cli(&[], None); // show all

        let (major, minor, patch, total) = count_updates_by_type(&updates, filter);
        assert_eq!(major, 2);
        assert_eq!(minor, 1);
        assert_eq!(patch, 2);
        assert_eq!(total, 5);
    }

    #[test]
    fn test_count_updates_by_type_with_filter_major_only() {
        let updates = vec![
            ("pkg1".into(), "1.0.0".into(), "2.0.0".into(), Some(1)), // major
            ("pkg2".into(), "1.0.0".into(), "1.1.0".into(), Some(2)), // minor (filtered out)
            ("pkg3".into(), "1.0.0".into(), "1.0.1".into(), Some(3)), // patch (filtered out)
        ];
        let filter = UpdateFilter::from_cli(&[BumpLevel::Major], None);

        let (major, minor, patch, total) = count_updates_by_type(&updates, filter);
        assert_eq!(major, 1);
        assert_eq!(minor, 0);
        assert_eq!(patch, 0);
        assert_eq!(total, 1);
    }

    #[test]
    fn test_count_updates_by_type_with_filter_minor_and_patch() {
        let updates = vec![
            ("pkg1".into(), "1.0.0".into(), "2.0.0".into(), Some(1)), // major (filtered out)
            ("pkg2".into(), "1.0.0".into(), "1.1.0".into(), Some(2)), // minor
            ("pkg3".into(), "1.0.0".into(), "1.0.1".into(), Some(3)), // patch
        ];
        let filter = UpdateFilter::from_cli(&[BumpLevel::Minor, BumpLevel::Patch], None);

        let (major, minor, patch, total) = count_updates_by_type(&updates, filter);
        assert_eq!(major, 0);
        assert_eq!(minor, 1);
        assert_eq!(patch, 1);
        assert_eq!(total, 2);
    }

    #[test]
    fn test_count_updates_by_type_no_line_numbers() {
        let updates = vec![
            ("pkg1".into(), "1.0.0".into(), "2.0.0".into(), None), // major, no line
            ("pkg2".into(), "1.0.0".into(), "1.1.0".into(), None), // minor, no line
        ];
        let filter = UpdateFilter::from_cli(&[], None); // show all

        let (major, minor, patch, total) = count_updates_by_type(&updates, filter);
        assert_eq!(major, 1);
        assert_eq!(minor, 1);
        assert_eq!(patch, 0);
        assert_eq!(total, 2);
    }

    #[test]
    fn test_has_checkable_manifest_changes_counts_pin_only_results() {
        let result = UpdateResult {
            pinned: vec![("react".into(), "18.2.0".into(), "19.0.0".into(), Some(4))],
            ..Default::default()
        };
        let filter = UpdateFilter::from_cli(&[], None);

        assert!(has_checkable_manifest_changes(&result, filter));
    }

    #[test]
    fn test_has_checkable_manifest_changes_respects_update_filter_without_pins() {
        let result = UpdateResult {
            updated: vec![("react".into(), "18.2.0".into(), "19.0.0".into(), Some(4))],
            ..Default::default()
        };
        let filter = UpdateFilter::from_cli(&[BumpLevel::Minor, BumpLevel::Patch], None);

        assert!(!has_checkable_manifest_changes(&result, filter));
    }

    #[test]
    fn test_take_approved_changes_for_file_only_returns_selected_updates() {
        let path = PathBuf::from("package.json");
        let file_type = FileType::PackageJson;
        let updates = vec![
            ("react".into(), "18.2.0".into(), "19.0.0".into(), Some(2)),
            ("vue".into(), "3.4.0".into(), "3.5.0".into(), Some(3)),
        ];

        let mut approved = PendingUpdate::new(
            "package.json".into(),
            Some(2),
            "react".into(),
            "18.2.0".into(),
            "19.0.0".into(),
            true,
        );
        approved.approved = true;

        let rejected = PendingUpdate::new(
            "package.json".into(),
            Some(3),
            "vue".into(),
            "3.4.0".into(),
            "3.5.0".into(),
            false,
        );

        let planned_changes: Vec<_> = updates
            .iter()
            .map(|update| PlannedChange::from_update(path.clone(), file_type, update))
            .collect();
        let mut approved_counts =
            build_approved_change_counts(&[approved, rejected], &planned_changes);

        let selected =
            take_approved_changes_for_file(&path, file_type, &updates, &mut approved_counts);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].package, "react");
        assert_eq!(selected[0].kind, ChangeKind::RegistryUpdate);
        assert!(approved_counts.is_empty());
    }

    #[test]
    fn test_build_approved_change_counts_tracks_duplicate_identical_updates() {
        let path = PathBuf::from("package.json");
        let file_type = FileType::PackageJson;
        let updates = vec![
            ("react".into(), "18.2.0".into(), "19.0.0".into(), Some(2)),
            ("react".into(), "18.2.0".into(), "19.0.0".into(), Some(2)),
        ];

        let mut first = PendingUpdate::new(
            "package.json".into(),
            Some(2),
            "react".into(),
            "18.2.0".into(),
            "19.0.0".into(),
            true,
        );
        first.approved = true;

        let mut second = PendingUpdate::new(
            "package.json".into(),
            Some(2),
            "react".into(),
            "18.2.0".into(),
            "19.0.0".into(),
            true,
        );
        second.approved = true;

        let planned_changes: Vec<_> = updates
            .iter()
            .map(|update| PlannedChange::from_update(path.clone(), file_type, update))
            .collect();
        let mut approved_counts = build_approved_change_counts(&[first, second], &planned_changes);

        let selected =
            take_approved_changes_for_file(&path, file_type, &updates, &mut approved_counts);

        assert_eq!(selected.len(), 2);
        assert!(approved_counts.is_empty());
    }

    #[test]
    fn test_build_approved_change_counts_distinguishes_duplicate_updates_by_line_number() {
        let path = PathBuf::from("package.json");
        let file_type = FileType::PackageJson;
        let updates = vec![
            ("react".into(), "18.2.0".into(), "19.0.0".into(), Some(4)),
            ("react".into(), "18.2.0".into(), "19.0.0".into(), Some(8)),
        ];

        let mut approved = PendingUpdate::new(
            "package.json".into(),
            Some(4),
            "react".into(),
            "18.2.0".into(),
            "19.0.0".into(),
            true,
        );
        approved.approved = true;

        let rejected = PendingUpdate::new(
            "package.json".into(),
            Some(8),
            "react".into(),
            "18.2.0".into(),
            "19.0.0".into(),
            true,
        );

        let planned_changes: Vec<_> = updates
            .iter()
            .map(|update| PlannedChange::from_update(path.clone(), file_type, update))
            .collect();
        let mut approved_counts =
            build_approved_change_counts(&[approved, rejected], &planned_changes);

        let selected =
            take_approved_changes_for_file(&path, file_type, &updates, &mut approved_counts);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].line_num, Some(4));
        assert!(approved_counts.is_empty());
    }

    #[test]
    fn test_collect_selected_changes_for_file_includes_config_pins() {
        let scanned_file = ScannedFileResult {
            path: PathBuf::from("package.json"),
            file_type: FileType::PackageJson,
            result: UpdateResult {
                updated: vec![("react".into(), "18.2.0".into(), "19.0.0".into(), Some(2))],
                pinned: vec![("lodash".into(), "4.17.20".into(), "4.17.21".into(), Some(3))],
                ..Default::default()
            },
        };

        let mut approved = PendingUpdate::new(
            "package.json".into(),
            Some(2),
            "react".into(),
            "18.2.0".into(),
            "19.0.0".into(),
            true,
        );
        approved.approved = true;

        let planned_changes = vec![PlannedChange::from_update(
            scanned_file.path.clone(),
            scanned_file.file_type,
            &scanned_file.result.updated[0],
        )];
        let mut approved_counts = build_approved_change_counts(&[approved], &planned_changes);

        let selected = collect_selected_changes_for_file(&scanned_file, &mut approved_counts);

        assert_eq!(selected.len(), 2);
        assert!(
            selected.iter().any(
                |change| change.kind == ChangeKind::RegistryUpdate && change.package == "react"
            )
        );
        assert!(
            selected
                .iter()
                .any(|change| change.kind == ChangeKind::ConfigPin && change.package == "lodash")
        );
        assert!(approved_counts.is_empty());
    }

    #[test]
    fn test_file_has_manifest_changes_for_pin_only_results() {
        let result = UpdateResult {
            pinned: vec![("react".into(), "18.2.0".into(), "18.3.0".into(), Some(2))],
            ..Default::default()
        };

        assert!(file_has_manifest_changes(&result));
    }

    #[test]
    fn test_has_interactive_changes_for_pin_only_results() {
        let scanned_results = vec![ScannedFileResult {
            path: PathBuf::from("package.json"),
            file_type: FileType::PackageJson,
            result: UpdateResult {
                pinned: vec![("react".into(), "18.2.0".into(), "18.3.0".into(), Some(2))],
                ..Default::default()
            },
        }];

        assert!(has_interactive_changes(&[], &scanned_results));
    }

    #[test]
    fn normalized_specs_are_manifest_changes_and_interactive_work() {
        let normalized = upd::updater::NormalizedSpec {
            package: "click".into(),
            section: "project.dependencies".into(),
            previous_spec: None,
            new_spec: ">=8.2.1".into(),
            version: "8.2.1".into(),
            previous_version: None,
            pinned: false,
            held_back_from: None,
            line_number: Some(4),
        };
        let result = UpdateResult {
            normalized: vec![normalized],
            ..Default::default()
        };
        assert!(file_has_manifest_changes(&result));
        assert!(has_checkable_manifest_changes(
            &result,
            UpdateFilter::from_cli(&[], None)
        ));
        assert!(has_interactive_changes(
            &[],
            &[ScannedFileResult {
                path: PathBuf::from("pyproject.toml"),
                file_type: FileType::PyProject,
                result,
            }]
        ));
    }

    #[test]
    fn interactive_normalization_selection_is_scoped_to_its_section() {
        let make_spec = |section: &str| upd::updater::NormalizedSpec {
            package: "click".into(),
            section: section.into(),
            previous_spec: None,
            new_spec: "==8.2.1".into(),
            version: "8.2.1".into(),
            previous_version: None,
            pinned: false,
            held_back_from: None,
            line_number: None,
        };
        let scanned = ScannedFileResult {
            path: PathBuf::from("pyproject.toml"),
            file_type: FileType::PyProject,
            result: UpdateResult {
                normalized: vec![
                    make_spec("project.dependencies"),
                    make_spec("dependency-groups.dev"),
                ],
                ..Default::default()
            },
        };
        let plans: Vec<_> = scanned
            .result
            .normalized
            .iter()
            .map(|spec| {
                PlannedChange::from_normalized(scanned.path.clone(), scanned.file_type, spec)
            })
            .collect();
        let mut rejected = PendingUpdate::new(
            "pyproject.toml".into(),
            None,
            "click".into(),
            "(no specifier)".into(),
            "==8.2.1".into(),
            false,
        );
        let mut approved = rejected.clone();
        rejected.approved = false;
        approved.approved = true;
        let mut counts = build_approved_change_counts(&[rejected, approved], &plans);

        let selected = take_approved_normalizations_for_file(&scanned, &mut counts);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].section, "dependency-groups.dev");
        assert!(counts.is_empty());
    }

    /// A file that produced only diagnostics is not "up to date". Without this,
    /// `upd update --interactive` over a repository whose
    /// every lookup failed prints `✓ Scanned 1 file(s), all dependencies up to
    /// date` and returns Ok(()). The fixtures are `requirements.txt` results,
    /// not annotated ones: an annotated fixture passes against an
    /// implementation that special-cases `FileType::Annotated`.
    #[test]
    fn interactive_changes_covers_warnings_and_errors() {
        let quiet = vec![ScannedFileResult {
            path: PathBuf::from("requirements.txt"),
            file_type: FileType::Requirements,
            result: UpdateResult::default(),
        }];

        let warned = vec![ScannedFileResult {
            path: PathBuf::from("requirements.txt"),
            file_type: FileType::Requirements,
            result: UpdateResult {
                warnings: vec![
                    "skipping foo: current version \"1.0++\" is not a valid PEP 440 version".into(),
                ],
                ..Default::default()
            },
        }];

        let errored = vec![ScannedFileResult {
            path: PathBuf::from("requirements.txt"),
            file_type: FileType::Requirements,
            result: UpdateResult {
                errors: vec!["foo: registry request failed".into()],
                ..Default::default()
            },
        }];

        let pending = vec![PendingUpdate::new(
            "requirements.txt".to_string(),
            Some(1),
            "foo".to_string(),
            "1.0.0".to_string(),
            "1.1.0".to_string(),
            false,
        )];

        assert!(
            has_interactive_changes(&[], &warned),
            "a warnings-only result must not be reported as up to date"
        );
        assert!(
            has_interactive_changes(&[], &errored),
            "an errors-only result must not be reported as up to date"
        );
        assert!(
            !has_interactive_changes(&[], &quiet),
            "a result with nothing to report is the up-to-date case"
        );
        assert!(
            has_interactive_changes(&pending, &quiet),
            "a pending update is still a change"
        );
    }

    /// Held-back updates render through the same helper in interactive mode as
    /// in the report, and `run_interactive_update` calls it inside the per-file
    /// scan loop rather than in the "nothing to do" branch: a repository with
    /// both an in-cap update to prompt for and an above-cap one held back must
    /// show the second, which is precisely the busy repository where losing it
    /// matters. Asserted here rather than end-to-end because
    /// `run_interactive_update` rejects non-TTY stdin before any of this runs
    /// (see tests/interactive_tty.rs), the same reason
    /// `interactive_lock_only_package_gets_note` lives here.
    #[test]
    fn capped_lines_name_the_package_the_versions_and_the_bump() {
        let result = UpdateResult {
            capped: vec![
                upd::updater::CappedUpdate {
                    lang: None,
                    package: "reqwest".into(),
                    current: "0.12.1".into(),
                    available: "0.13.0".into(),
                    line_number: Some(7),
                },
                upd::updater::CappedUpdate {
                    lang: None,
                    package: "serde".into(),
                    current: "1.0.1".into(),
                    available: "1.1.0".into(),
                    line_number: None,
                },
            ],
            ..Default::default()
        };

        let lines = format_capped_lines("Cargo.toml", &result);

        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("Cargo.toml:7:"), "{}", lines[0]);
        assert!(lines[0].contains("reqwest"), "{}", lines[0]);
        assert!(lines[0].contains("0.12.1"), "{}", lines[0]);
        assert!(lines[0].contains("0.13.0"), "{}", lines[0]);
        assert!(
            lines[0].contains("(major bump)"),
            "a step between zero-major versions is breaking, and the line has to \
             say so or it reads as reachable under a minor ceiling: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("Cargo.toml:") && !lines[1].contains("Cargo.toml:0:"),
            "an entry with no line number must not invent one: {}",
            lines[1]
        );
        assert!(lines[1].contains("(minor bump)"), "{}", lines[1]);
    }

    /// The tick is a claim that every dependency was checked. These cover what
    /// has to be true for the interactive run to make it, since the run itself
    /// needs a terminal (see tests/interactive_tty.rs).
    #[test]
    fn the_tick_is_withheld_from_anything_left_unchecked_or_unwritten() {
        let ticked = format_interactive_closing_line(2, 0, 0, 0, 0)
            .expect("a run that checked everything closes by saying so");
        assert!(ticked.contains("all dependencies up to date"), "{ticked}");
        assert!(ticked.contains("Scanned 2 file(s)"), "{ticked}");

        assert_eq!(
            format_interactive_closing_line(1, 0, 3, 0, 0),
            None,
            "an annotation is a write still waiting to happen, so the run has \
             not finished and must not claim it has"
        );
        assert_eq!(
            format_interactive_closing_line(1, 0, 0, 2, 0),
            None,
            "a skipped pin is a dependency whose version was never read, so \
             calling it up to date claims a check that never happened"
        );
        assert_eq!(
            format_interactive_closing_line(1, 0, 0, 0, 1),
            None,
            "a warning is a dependency that did not end where the run meant to \
             leave it, so the tick would be claiming the opposite about it"
        );
        assert_eq!(
            format_interactive_closing_line(1, 0, 3, 2, 1),
            None,
            "{:?}",
            format_interactive_closing_line(1, 0, 3, 2, 1)
        );
    }

    /// The same claim in the non-interactive summary, where it decides between
    /// the green tick and a breakdown. Every channel is exercised one at a time:
    /// a veto that only works alongside another one is not a veto.
    #[test]
    fn every_unfinished_channel_withholds_the_non_interactive_tick() {
        let clean = UpdateResult {
            unchanged: 7,
            ..Default::default()
        };
        assert!(
            nothing_outstanding(&clean, 0, 0, 0),
            "a run whose every dependency was current has nothing to report"
        );

        // An error is deliberately not a veto here: the tick line itself counts
        // them and says how many could not be checked (see
        // print_nothing_to_update_line), so vetoing would suppress that count.
        let failed = UpdateResult {
            errors: vec!["boom".to_string()],
            ..clean.clone()
        };
        assert!(
            nothing_outstanding(&failed, 0, 0, 0),
            "a failed lookup is reported by the closing line, not instead of it"
        );

        let vetoes: Vec<(&str, UpdateResult)> = vec![
            (
                "a dependency upd found newer but will not rewrite",
                UpdateResult {
                    warnings: vec!["lodash: 5.0.0 is available".to_string()],
                    ..clean.clone()
                },
            ),
            (
                "a version the config pinned",
                UpdateResult {
                    pinned: vec![("a".into(), "1.0".into(), "1.0".into(), None)],
                    ..clean.clone()
                },
            ),
            (
                "an update cooldown kept the run away from",
                UpdateResult {
                    skipped_by_cooldown: vec![("a".into(), "1.0".into(), "2.0".into(), None)],
                    ..clean.clone()
                },
            ),
            (
                "an update cooldown settled for an older release than the latest",
                UpdateResult {
                    held_back: vec![(
                        "a".into(),
                        "1.0".into(),
                        "1.5".into(),
                        "2.0".into(),
                        chrono::Utc::now(),
                    )],
                    ..clean.clone()
                },
            ),
            (
                "a dependency something declined to touch",
                UpdateResult {
                    skipped: vec![upd::updater::SkippedUpdate {
                        package: "a".into(),
                        current: "1.0".into(),
                        status: SkipStatus::Blocked,
                        reason: "pinned",
                        message: "pinned by config".into(),
                        line_number: None,
                    }],
                    ..clean.clone()
                },
            ),
            (
                "an update the bump ceiling held back",
                UpdateResult {
                    capped: vec![upd::updater::CappedUpdate {
                        lang: None,
                        package: "a".into(),
                        current: "1.0".into(),
                        available: "2.0".into(),
                        line_number: None,
                    }],
                    ..clean.clone()
                },
            ),
            (
                "a pin whose release upd wrote down without moving it",
                UpdateResult {
                    annotations: vec![upd::updater::Annotation {
                        package: "a".into(),
                        version: "1.0".into(),
                        commit: "deadbeef".into(),
                        line_number: None,
                    }],
                    ..clean.clone()
                },
            ),
        ];
        for (what, result) in vetoes {
            assert!(
                !nothing_outstanding(&result, 0, 0, 0),
                "the tick claims every dependency is current, over {what}"
            );
        }

        // The counts that reach the function as plain numbers.
        assert!(
            !nothing_outstanding(&clean, 1, 0, 0),
            "an update is pending"
        );
        assert!(
            !nothing_outstanding(&clean, 0, 1, 0),
            "a floor that could not be raised is unfinished work"
        );
        assert!(
            !nothing_outstanding(&clean, 0, 0, 1),
            "a floor left alone is unfinished work"
        );
    }

    /// The ceiling is the user's own setting, so a run it held back reports
    /// that in preference to anything else, exactly as the non-interactive
    /// summary does.
    #[test]
    fn a_capped_update_outranks_the_tick_and_the_silence() {
        let capped = format_interactive_closing_line(1, 4, 0, 0, 0)
            .expect("a held-back update is not nothing to report");
        assert!(capped.contains("held back by the bump ceiling"), "{capped}");
        assert!(capped.contains('4'), "{capped}");
        assert!(
            !capped.contains("up to date"),
            "an update the ceiling refused is not up to date: {capped}"
        );

        let still_capped = format_interactive_closing_line(1, 4, 2, 3, 1)
            .expect("the ceiling outranks findings that print their own lines");
        assert!(
            still_capped.contains("held back by the bump ceiling"),
            "{still_capped}"
        );
    }

    /// A blocked pin is the one kind of finding that needs attention on its own
    /// line whatever the verbosity: it says a dependency could not be checked
    /// at all. Rendered by a helper so `--interactive` reports it without a
    /// terminal, the `format_capped_lines` precedent above.
    #[test]
    fn a_blocked_pin_names_its_line_its_package_and_why() {
        let result = UpdateResult {
            skipped: vec![upd::updater::SkippedUpdate {
                package: "actions/checkout".into(),
                current: "f548e57e544e1ff5a4c46bf1e1b8685f8e4a348a".into(),
                status: SkipStatus::Blocked,
                reason: "unreleased-commit",
                message: "no tag names this commit".into(),
                line_number: Some(8),
            }],
            ..Default::default()
        };

        let lines = format_skipped_lines(".github/workflows/ci.yml", &result, false);

        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains(".github/workflows/ci.yml:8:"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("Blocked"), "{}", lines[0]);
        assert!(lines[0].contains("actions/checkout"), "{}", lines[0]);
        assert!(
            lines[0].contains("no tag names this commit"),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].contains("unreleased-commit"),
            "the reason is the machine-readable half of the line and has to \
             survive into the human one: {}",
            lines[0]
        );
    }

    /// A repo that SHA-pins every action would emit a line per action on every
    /// run, so leaving SHA updates off stays quiet until asked. The count still
    /// reaches the summary, which is what stops the run claiming it checked
    /// them.
    #[test]
    fn a_not_examined_pin_is_named_only_when_asked() {
        let result = UpdateResult {
            skipped: vec![upd::updater::SkippedUpdate {
                package: "actions/cache".into(),
                current: "55cc8345863c7cc4c66a329aec7e433d2d1c52a9".into(),
                status: SkipStatus::NotExamined,
                reason: "sha-updates-disabled",
                message: "SHA-pinned action updates are off".into(),
                line_number: Some(12),
            }],
            ..Default::default()
        };

        assert!(
            format_skipped_lines("ci.yml", &result, false).is_empty(),
            "a settled configuration choice is not a per-line finding"
        );

        let verbose = format_skipped_lines("ci.yml", &result, true);
        assert_eq!(verbose.len(), 1, "{verbose:?}");
        assert!(verbose[0].contains("Not checked"), "{}", verbose[0]);
        assert!(
            !verbose[0].contains("Blocked"),
            "a configuration choice must not be dressed as a safety problem: {}",
            verbose[0]
        );
        assert!(verbose[0].contains("actions/cache"), "{}", verbose[0]);
    }

    /// The interactive diagnostic is produced by a helper so the fourth CLI
    /// invocation can assert it without a terminal, following the
    /// `interactive_lock_only_package_gets_note` precedent. Errors render the
    /// way the non-interactive renderer does (`src/main.rs:4385`), warnings the
    /// way `print_file_result` does (`:4395`), errors first.
    #[test]
    fn scan_diagnostics_render_errors_then_warnings() {
        let result = UpdateResult {
            errors: vec!["foo: registry request failed".into()],
            warnings: vec!["line 3: unsupported source 'cocoapods'".into()],
            ..Default::default()
        };

        let lines = format_scan_diagnostics(Path::new("sub/Makefile"), &result);

        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("Error:"), "{}", lines[0]);
        assert!(lines[0].contains("sub/Makefile"), "{}", lines[0]);
        assert!(
            lines[0].contains("foo: registry request failed"),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("Warning:"), "{}", lines[1]);
        assert!(lines[1].contains("sub/Makefile"), "{}", lines[1]);
        assert!(
            lines[1].contains("line 3: unsupported source 'cocoapods'"),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn scan_diagnostics_are_empty_for_a_clean_result() {
        let clean = UpdateResult {
            updated: vec![("foo".into(), "1.0.0".into(), "1.1.0".into(), Some(1))],
            ..Default::default()
        };

        assert!(
            format_scan_diagnostics(Path::new("requirements.txt"), &clean).is_empty(),
            "an ordinary update is not a diagnostic"
        );
    }

    #[test]
    fn test_audit_status_clean() {
        let result = AuditResult {
            safe_count: 1,
            ..Default::default()
        };

        assert_eq!(audit_status(&result), AuditStatus::Clean);
    }

    #[test]
    fn test_audit_status_vulnerable() {
        let mut result = AuditResult::default();
        result.vulnerable.push(upd::audit::PackageAuditResult {
            package: upd::audit::Package {
                name: "serde".into(),
                version: "1.0.0".into(),
                ecosystem: Ecosystem::CratesIo,
            },
            vulnerabilities: vec![upd::audit::Vulnerability {
                id: "RUSTSEC-0000-0000".into(),
                summary: Some("Test".into()),
                severity: None,
                url: None,
                fixed_version: None,
                aliases: Vec::new(),
                source: String::new(),
            }],
        });

        assert_eq!(audit_status(&result), AuditStatus::Vulnerable);
    }

    #[test]
    fn test_audit_status_incomplete_takes_precedence() {
        let mut result = AuditResult::default();
        result.vulnerable.push(upd::audit::PackageAuditResult {
            package: upd::audit::Package {
                name: "serde".into(),
                version: "1.0.0".into(),
                ecosystem: Ecosystem::CratesIo,
            },
            vulnerabilities: vec![upd::audit::Vulnerability {
                id: "RUSTSEC-0000-0000".into(),
                summary: Some("Test".into()),
                severity: None,
                url: None,
                fixed_version: None,
                aliases: Vec::new(),
                source: String::new(),
            }],
        });
        result.errors.push("network timeout".into());

        assert_eq!(audit_status(&result), AuditStatus::Incomplete);
    }

    #[test]
    fn test_load_update_configs_explicit_missing_errors() {
        let cli = Cli::try_parse_from(["upd", "--config", "/definitely/missing/upd.toml"]).unwrap();
        let files = vec![(PathBuf::from("requirements.txt"), FileType::Requirements)];

        let error = load_update_configs(&cli, &files).unwrap_err();

        assert!(error.to_string().contains("Config file not found"));
    }

    #[test]
    fn test_load_update_configs_explicit_valid() {
        let temp = tempdir().unwrap();
        let config_path = temp.path().join("upd.toml");
        std::fs::write(&config_path, "ignore = [\"requests\"]").unwrap();

        let cli = Cli::try_parse_from(["upd", "--config", config_path.to_str().unwrap()]).unwrap();
        let file = temp.path().join("requirements.txt");
        std::fs::write(&file, "requests==2.0.0").unwrap();
        let files = vec![(file.clone(), FileType::Requirements)];

        let configs = load_update_configs(&cli, &files).unwrap();
        let config = configs.get(&file).cloned().flatten().unwrap();

        assert!(config.should_ignore("requests"));
    }

    #[test]
    fn test_load_update_configs_resolves_config_per_file() {
        let temp = tempdir().unwrap();
        let service_a = temp.path().join("service-a");
        let service_b = temp.path().join("service-b");
        std::fs::create_dir_all(&service_a).unwrap();
        std::fs::create_dir_all(&service_b).unwrap();

        std::fs::write(service_a.join(".updrc.toml"), "ignore = [\"react\"]").unwrap();
        std::fs::write(service_b.join(".updrc.toml"), "ignore = [\"vue\"]").unwrap();

        let file_a = service_a.join("package.json");
        let file_b = service_b.join("package.json");
        std::fs::write(&file_a, "{}").unwrap();
        std::fs::write(&file_b, "{}").unwrap();

        let cli = Cli::try_parse_from(["upd"]).unwrap();
        let files = vec![
            (file_a.clone(), FileType::PackageJson),
            (file_b.clone(), FileType::PackageJson),
        ];

        let configs = load_update_configs(&cli, &files).unwrap();
        let config_a = configs.get(&file_a).cloned().flatten().unwrap();
        let config_b = configs.get(&file_b).cloned().flatten().unwrap();

        assert!(config_a.should_ignore("react"));
        assert!(!config_a.should_ignore("vue"));
        assert!(config_b.should_ignore("vue"));
        assert!(!config_b.should_ignore("react"));
    }

    #[test]
    fn test_apply_version_updates_pyproject_preserves_additional_constraints() {
        let content = "[project]\ndependencies = [\"django>=3.2,<4\"]\n";
        let updates = [VersionEdit {
            package: "django",
            old_version: "3.2",
            new_version: "3.10.0",
            line_num: Some(2),
            expected_source: None,
            sha_pin: None,
        }];

        let applied = apply_version_updates(content, &updates, FileType::PyProject, false).unwrap();

        assert_eq!(applied.applied_count(), 1);
        assert_eq!(
            applied.content,
            "[project]\ndependencies = [\"django>=3.10,<4\"]\n"
        );
    }

    /// A constraint set carries no order, so the floor an interactive session
    /// approved may be written after a ceiling. The write has to find it where
    /// it is: reading the first version on the line instead matched nothing, so
    /// the session reported "Failed to apply 1 version edit(s)", exited 2, and
    /// wrote nothing, for a file every other mode of the same binary updates.
    #[test]
    fn test_apply_version_updates_writes_a_floor_that_follows_a_ceiling() {
        for (file_type, content, expected) in [
            (
                FileType::Gemfile,
                "source 'https://rubygems.org'\ngem 'rails', '< 9.0', '>= 6.0'\n",
                "source 'https://rubygems.org'\ngem 'rails', '< 9.0', '>= 8.1'\n",
            ),
            (
                FileType::TerraformTf,
                "terraform {\n  required_providers {\n    aws = {\n      source  = \"hashicorp/aws\"\n      version = \"< 9.0, >= 6.0\"\n    }\n  }\n}\n",
                "terraform {\n  required_providers {\n    aws = {\n      source  = \"hashicorp/aws\"\n      version = \"< 9.0, >= 8.1\"\n    }\n  }\n}\n",
            ),
        ] {
            let line_num = content
                .lines()
                .position(|line| line.contains(">= 6.0"))
                .map(|index| index + 1);
            let updates = [VersionEdit {
                package: if file_type == FileType::Gemfile {
                    "rails"
                } else {
                    "hashicorp/aws"
                },
                old_version: "6.0",
                new_version: "8.1",
                line_num,
                expected_source: None,
                sha_pin: None,
            }];

            let applied = apply_version_updates(content, &updates, file_type, false).unwrap();

            assert_eq!(applied.applied_count(), 1, "file type {file_type:?}");
            assert_eq!(applied.content, expected, "file type {file_type:?}");
        }
    }

    #[test]
    fn test_apply_version_updates_uses_unique_fallback_when_target_line_does_not_match() {
        let content = "[project]\ndependencies = [\"django>=3.2,<4\"]\n";
        let updates = [VersionEdit {
            package: "django",
            old_version: "3.2",
            new_version: "3.10.0",
            line_num: Some(1),
            expected_source: None,
            sha_pin: None,
        }];

        let applied = apply_version_updates(content, &updates, FileType::PyProject, false).unwrap();

        assert_eq!(applied.applied_count(), 1);
        assert_eq!(
            applied.content,
            "[project]\ndependencies = [\"django>=3.10,<4\"]\n"
        );
    }

    /// A SHA pin approved interactively must move the commit and its comment
    /// together. The tag-shaped rewrite cannot do this: it looks for
    /// `actions/checkout@v4.2.2`, which a commit-pinned line does not contain.
    #[test]
    fn test_apply_version_updates_rewrites_an_approved_sha_pin() {
        const OLD_SHA: &str = "11bd71901bbe5b1630ceea73d27597364c9af683";
        const NEW_SHA: &str = "08c6903cd8c0fde910a37f88322edcfb5dd907a8";
        let content = format!(
            "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@{OLD_SHA} # v4.2.2\n"
        );
        let pin = ActionShaUpdate {
            package: "actions/checkout".to_string(),
            current_version: "v4.2.2".to_string(),
            new_version: "v4.3.0".to_string(),
            current_commit: OLD_SHA.to_string(),
            new_commit: NEW_SHA.to_string(),
            line_number: Some(4),
        };
        let updates = [VersionEdit {
            package: "actions/checkout",
            old_version: "v4.2.2",
            new_version: "v4.3.0",
            line_num: Some(4),
            expected_source: None,
            sha_pin: Some(&pin),
        }];

        let applied =
            apply_version_updates(&content, &updates, FileType::GithubActions, false).unwrap();

        assert_eq!(applied.applied_count(), 1);
        assert!(
            applied
                .content
                .contains(&format!("actions/checkout@{NEW_SHA} # v4.3.0")),
            "commit and comment must move together:\n{}",
            applied.content
        );
        assert!(
            !applied.content.contains(OLD_SHA),
            "the old commit must not survive:\n{}",
            applied.content
        );
        assert!(
            !applied.content.contains("actions/checkout@v4.3.0"),
            "the pin must stay a commit pin, not become a mutable tag:\n{}",
            applied.content
        );
    }

    /// The pin is only rewritten from the state the scan verified. A line that
    /// changed underneath leaves the SHA alone rather than being rewritten from
    /// stale input.
    #[test]
    fn test_apply_version_updates_refuses_a_sha_pin_that_moved_since_the_scan() {
        const SCANNED_SHA: &str = "11bd71901bbe5b1630ceea73d27597364c9af683";
        const ON_DISK_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let content = format!(
            "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@{ON_DISK_SHA} # v4.2.2\n"
        );
        let pin = ActionShaUpdate {
            package: "actions/checkout".to_string(),
            current_version: "v4.2.2".to_string(),
            new_version: "v4.3.0".to_string(),
            current_commit: SCANNED_SHA.to_string(),
            new_commit: "08c6903cd8c0fde910a37f88322edcfb5dd907a8".to_string(),
            line_number: Some(4),
        };
        let updates = [VersionEdit {
            package: "actions/checkout",
            old_version: "v4.2.2",
            new_version: "v4.3.0",
            line_num: Some(4),
            expected_source: None,
            sha_pin: Some(&pin),
        }];

        let error = apply_version_updates(&content, &updates, FileType::GithubActions, false)
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("Failed to apply 1 version edit"),
            "a drifted pin must fail loudly, not be rewritten: {error}"
        );
    }

    /// Build a pin for `package` on `line`, with commits distinguishable by the
    /// line they came from.
    fn pin_on_line(package: &str, line: Option<usize>) -> ActionShaUpdate {
        let n = line.unwrap_or(0);
        ActionShaUpdate {
            package: package.to_string(),
            current_version: "v4.2.2".to_string(),
            new_version: "v4.3.0".to_string(),
            current_commit: format!("old{n}"),
            new_commit: format!("new{n}"),
            line_number: line,
        }
    }

    /// A workflow may use one action from two jobs, and each reference resolves
    /// its own commit. Matching on name alone would rewrite one line with the
    /// other's commit.
    #[test]
    fn test_sha_pin_for_keeps_two_references_to_one_action_apart() {
        let pins = [
            pin_on_line("actions/checkout", Some(4)),
            pin_on_line("actions/checkout", Some(11)),
        ];

        let found = sha_pin_for(&pins, "actions/checkout", Some(11)).expect("pin on line 11");
        assert_eq!(found.new_commit, "new11");
        assert!(sha_pin_for(&pins, "actions/checkout", Some(7)).is_none());
        assert!(sha_pin_for(&pins, "actions/setup-node", Some(4)).is_none());
    }

    /// An edit or a pin without a line cannot be shown to be the same reference,
    /// so it matches nothing. The edit then fails to apply and is reported,
    /// which is what a wrong commit would not be.
    #[test]
    fn test_sha_pin_for_will_not_match_without_a_line_on_both_sides() {
        let unlocated = [pin_on_line("actions/checkout", None)];
        assert!(sha_pin_for(&unlocated, "actions/checkout", Some(4)).is_none());
        assert!(sha_pin_for(&unlocated, "actions/checkout", None).is_none());

        let located = [pin_on_line("actions/checkout", Some(4))];
        assert!(sha_pin_for(&located, "actions/checkout", None).is_none());
    }

    #[test]
    fn test_apply_version_updates_errors_when_fallback_is_ambiguous() {
        let content = r#"[project]
dependencies = [
  "django>=3.2,<4",
]

[project.optional-dependencies]
dev = [
  "django>=3.2,<4",
]
"#;
        let updates = [VersionEdit {
            package: "django",
            old_version: "3.2",
            new_version: "3.10.0",
            line_num: Some(1),
            expected_source: None,
            sha_pin: None,
        }];

        let error =
            apply_version_updates(content, &updates, FileType::PyProject, false).unwrap_err();

        assert!(error.to_string().contains("Failed to apply 1 version edit"));
    }

    #[test]
    fn test_apply_version_updates_csproj_targets_selected_multiline_package_only() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="PackageA">
      <Version>1.0.0</Version>
    </PackageReference>
    <PackageReference Include="PackageB">
      <Version>1.0.0</Version>
    </PackageReference>
  </ItemGroup>
</Project>
"#;
        let updates = [VersionEdit {
            package: "PackageB",
            old_version: "1.0.0",
            new_version: "2.0.0",
            line_num: Some(6),
            expected_source: None,
            sha_pin: None,
        }];

        let applied = apply_version_updates(content, &updates, FileType::Csproj, false).unwrap();

        assert_eq!(applied.applied_count(), 1);
        assert!(
            applied.content.contains(
                "<PackageReference Include=\"PackageA\">\n      <Version>1.0.0</Version>"
            )
        );
        assert!(
            applied.content.contains(
                "<PackageReference Include=\"PackageB\">\n      <Version>2.0.0</Version>"
            )
        );
    }

    #[test]
    fn test_apply_version_updates_cargo_named_dependency_table() {
        let content = r#"[package]
name = "demo"
version = "0.1.0"

[dependencies.my-crate]
version = "1.0.0"
"#;
        let updates = [VersionEdit {
            package: "my-crate",
            old_version: "1.0.0",
            new_version: "1.2.3",
            line_num: Some(5),
            expected_source: None,
            sha_pin: None,
        }];

        let applied = apply_version_updates(content, &updates, FileType::CargoToml, false).unwrap();

        assert_eq!(applied.applied_count(), 1);
        assert!(
            applied
                .content
                .contains("[dependencies.my-crate]\nversion = \"1.2.3\"")
        );
    }

    #[test]
    fn test_apply_version_updates_package_json_uses_unique_fallback_for_duplicate_targets() {
        let content = r#"{
  "dependencies": {
    "react": "^18.2.0"
  },
  "devDependencies": {
    "react": "^18.1.0"
  }
}
"#;
        let updates = [
            VersionEdit {
                package: "react",
                old_version: "18.2.0",
                new_version: "19.0.0",
                line_num: Some(3),
                expected_source: None,
                sha_pin: None,
            },
            VersionEdit {
                package: "react",
                old_version: "18.1.0",
                new_version: "19.0.0",
                line_num: Some(3),
                expected_source: None,
                sha_pin: None,
            },
        ];

        let applied =
            apply_version_updates(content, &updates, FileType::PackageJson, false).unwrap();

        assert_eq!(applied.applied_count(), 2);
        assert!(
            applied
                .content
                .contains("\"dependencies\": {\n    \"react\": \"^19.0.0\"")
        );
        assert!(
            applied
                .content
                .contains("\"devDependencies\": {\n    \"react\": \"^19.0.0\"")
        );
    }

    #[test]
    fn test_apply_version_updates_cargo_uses_unique_fallback_for_duplicate_targets() {
        let content = r#"[package]
name = "demo"
version = "0.1.0"

[dependencies]
serde = "1.0.0"

[dev-dependencies]
serde = "1.0.1"
"#;
        let updates = [
            VersionEdit {
                package: "serde",
                old_version: "1.0.0",
                new_version: "1.0.2",
                line_num: Some(6),
                expected_source: None,
                sha_pin: None,
            },
            VersionEdit {
                package: "serde",
                old_version: "1.0.1",
                new_version: "1.0.2",
                line_num: Some(6),
                expected_source: None,
                sha_pin: None,
            },
        ];

        let applied = apply_version_updates(content, &updates, FileType::CargoToml, false).unwrap();

        assert_eq!(applied.applied_count(), 2);
        assert!(
            applied
                .content
                .contains("[dependencies]\nserde = \"1.0.2\"")
        );
        assert!(
            applied
                .content
                .contains("[dev-dependencies]\nserde = \"1.0.2\"")
        );
    }

    #[test]
    fn test_apply_alignments_csproj_multiline_uses_occurrence_line_numbers() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("Test.csproj");
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="PackageA">
      <Version>1.0.0</Version>
    </PackageReference>
    <PackageReference Include="PackageB">
      <Version>1.0.0</Version>
    </PackageReference>
  </ItemGroup>
</Project>
"#;
        std::fs::write(&file, content).unwrap();

        let alignment = PackageAlignment {
            package_name: "PackageB".into(),
            highest_version: "2.0.0".into(),
            occurrences: vec![PackageOccurrence {
                file_path: file.clone(),
                file_type: FileType::Csproj,
                version: "1.0.0".into(),
                line_number: Some(6),
                has_upper_bound: false,
                original_name: "PackageB".into(),
                is_bumpable: true,
            }],
            lang: Lang::DotNet,
        };

        let updated_count = apply_alignments(&[&alignment], false).unwrap();
        let updated = std::fs::read_to_string(&file).unwrap();

        assert_eq!(updated_count, 1);
        assert!(
            updated.contains(
                "<PackageReference Include=\"PackageA\">\n      <Version>1.0.0</Version>"
            )
        );
        assert!(
            updated.contains(
                "<PackageReference Include=\"PackageB\">\n      <Version>2.0.0</Version>"
            )
        );
    }

    /// Asserts `AuditPackage.name` preserves original casing for OSV queries
    /// (NuGet is case-sensitive).
    #[test]
    fn test_build_audit_packages_preserves_original_name_casing() {
        use std::path::PathBuf;

        // Simulate what scan_packages produces for a .csproj: the HashMap key is
        // lowercased for deduplication, but the occurrence records the original casing.
        let key = ("newtonsoft.json".to_string(), Lang::DotNet);
        let occurrences = vec![PackageOccurrence {
            file_path: PathBuf::from("MyApp.csproj"),
            file_type: FileType::Csproj,
            version: "12.0.1".to_string(),
            line_number: Some(5),
            has_upper_bound: false,
            original_name: "Newtonsoft.Json".to_string(),
            is_bumpable: true,
        }];

        let mut packages = HashMap::new();
        packages.insert(key, occurrences);

        let audit_pkgs = build_audit_packages(&packages, &[]);

        assert_eq!(audit_pkgs.len(), 1);
        assert_eq!(
            audit_pkgs[0].name, "Newtonsoft.Json",
            "AuditPackage.name must use original casing; lowercased name fails OSV NuGet lookups"
        );
        assert_eq!(audit_pkgs[0].version, "12.0.1");
    }

    /// `build_audit_packages` must include Go pseudo-version entries so the OSV
    /// query can find CVEs for the specific commit snapshot. Pseudo-versions are
    /// valid OSV query inputs for the Go ecosystem.
    #[test]
    fn test_build_audit_packages_includes_go_pseudo_version() {
        use std::path::PathBuf;

        let pseudo_version = "v0.0.0-20200115085410-6d4e4cb37c7d";

        // Simulate a scan_packages result containing a pseudo-version occurrence.
        let key = ("golang.org/x/crypto".to_string(), Lang::Go);
        let occurrences = vec![PackageOccurrence {
            file_path: PathBuf::from("go.mod"),
            file_type: FileType::GoMod,
            version: pseudo_version.to_string(),
            line_number: Some(5),
            has_upper_bound: false,
            original_name: "golang.org/x/crypto".to_string(),
            is_bumpable: false,
        }];

        let mut packages = HashMap::new();
        packages.insert(key, occurrences);

        let audit_pkgs = build_audit_packages(&packages, &[]);

        assert_eq!(
            audit_pkgs.len(),
            1,
            "pseudo-version must produce an AuditPackage"
        );
        assert_eq!(audit_pkgs[0].name, "golang.org/x/crypto");
        assert_eq!(
            audit_pkgs[0].version, pseudo_version,
            "exact pseudo-version string must be forwarded to OSV"
        );
        assert_eq!(
            audit_pkgs[0].ecosystem,
            upd::audit::Ecosystem::Go,
            "ecosystem must be Go"
        );
    }

    // ── find_vcs_root unit tests ──────────────────────────────────────────────

    #[test]
    fn test_find_vcs_root_returns_none_in_plain_tempdir() {
        // A freshly created tempdir has no .git ancestor.
        let tmp = tempdir().unwrap();
        let result = find_vcs_root(tmp.path());
        assert!(
            result.is_none(),
            "find_vcs_root must return None outside a git repo"
        );
    }

    #[test]
    fn test_find_vcs_root_finds_git_directory() {
        let tmp = tempdir().unwrap();
        // Create a fake .git directory at the root.
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let result = find_vcs_root(tmp.path());
        assert_eq!(
            result,
            Some(tmp.path().to_path_buf()),
            "find_vcs_root must return the directory containing .git"
        );
    }

    #[test]
    fn test_find_vcs_root_finds_git_file_for_submodules() {
        let tmp = tempdir().unwrap();
        // A .git *file* (not directory) is used by submodules and worktrees.
        std::fs::write(tmp.path().join(".git"), "gitdir: ../.git/worktrees/foo").unwrap();
        let result = find_vcs_root(tmp.path());
        assert_eq!(
            result,
            Some(tmp.path().to_path_buf()),
            "find_vcs_root must handle .git as a file (submodule/worktree)"
        );
    }

    #[test]
    fn test_find_vcs_root_walks_up_to_parent() {
        let tmp = tempdir().unwrap();
        // .git is in the root; subdir should still find it.
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let subdir = tmp.path().join("deep").join("nested");
        std::fs::create_dir_all(&subdir).unwrap();
        let result = find_vcs_root(&subdir);
        assert_eq!(
            result,
            Some(tmp.path().to_path_buf()),
            "find_vcs_root must walk up to find the .git ancestor"
        );
    }

    #[test]
    fn test_find_vcs_root_with_file_input_checks_parent() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        // Pass a file path; should scan the parent.
        let file = tmp.path().join("requirements.txt");
        std::fs::write(&file, "requests==1.0.0\n").unwrap();
        let result = find_vcs_root(&file);
        assert_eq!(
            result,
            Some(tmp.path().to_path_buf()),
            "find_vcs_root with a file path must check the file's parent"
        );
    }

    #[test]
    fn lockfile_changes_do_not_cross_ecosystems_in_one_directory() {
        let tmp = tempdir().unwrap();
        let cargo_manifest = tmp.path().join("Cargo.toml");
        let dockerfile = tmp.path().join("Dockerfile");
        std::fs::write(&cargo_manifest, "[dependencies]\nserde = \"1\"\n").unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), "version = 4\n").unwrap();
        std::fs::write(&dockerfile, "FROM rust:1.90-alpine\n").unwrap();

        let mut changed = ChangedByLockfile::new();
        record_lockfile_changes(&mut changed, &cargo_manifest, ["serde".to_string()]);
        record_lockfile_changes(&mut changed, &dockerfile, ["rust".to_string()]);

        assert_eq!(lockfile_changes_for(&changed, &cargo_manifest), ["serde"]);
        assert!(lockfile_changes_for(&changed, &dockerfile).is_empty());
    }

    /// Three manifests with lockfiles, in two directories, and two files
    /// that own no lockfile: one group per manifest that owns a lockfile,
    /// keyed on its directory and lockfile set, in discovery order.
    #[test]
    fn plan_lock_groups_makes_one_group_per_directory_and_lockfile_set() {
        let tmp = tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let c = tmp.path().join("c");
        for dir in [&a, &b, &c] {
            std::fs::create_dir(dir).unwrap();
        }
        let pyproject = a.join("pyproject.toml");
        let package_json = a.join("package.json");
        let dockerfile = a.join("Dockerfile");
        let cargo_toml = b.join("Cargo.toml");
        let requirements = c.join("requirements.txt");
        std::fs::write(&pyproject, "[project]\n").unwrap();
        std::fs::write(a.join("uv.lock"), "version = 1\n").unwrap();
        std::fs::write(&package_json, "{}\n").unwrap();
        std::fs::write(a.join("package-lock.json"), "{}\n").unwrap();
        std::fs::write(&dockerfile, "FROM rust:1.90-alpine\n").unwrap();
        std::fs::write(&cargo_toml, "[package]\n").unwrap();
        std::fs::write(b.join("Cargo.lock"), "version = 4\n").unwrap();
        std::fs::write(&requirements, "requests==2.31.0\n").unwrap();

        let groups = plan_lock_groups(&[
            (pyproject.clone(), FileType::PyProject),
            (dockerfile, FileType::Annotated),
            (package_json.clone(), FileType::PackageJson),
            (requirements, FileType::Requirements),
            (cargo_toml.clone(), FileType::CargoToml),
        ])
        .unwrap();

        let summary: Vec<(PathBuf, Vec<LockfileType>, Vec<PathBuf>)> = groups
            .iter()
            .map(|group| {
                (
                    group.dir.clone(),
                    group.lockfiles.clone(),
                    group.manifests.clone(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    a.clone(),
                    vec![LockfileType::UvLock],
                    vec![pyproject.clone()]
                ),
                (
                    a.clone(),
                    vec![LockfileType::PackageLockJson],
                    vec![package_json.clone()]
                ),
                (b.clone(), vec![LockfileType::CargoLock], vec![cargo_toml]),
            ]
        );
        assert_eq!(groups[0].lockfile_paths(), vec![a.join("uv.lock")]);
    }

    /// The snapshot a group is planned with holds the bytes the run found, so
    /// a restore after the updaters and the lockfile tool have written puts
    /// both the manifest and the lockfile back.
    #[test]
    fn a_planned_group_restores_the_bytes_the_run_found() {
        let tmp = tempdir().unwrap();
        let manifest = tmp.path().join("pyproject.toml");
        let lockfile = tmp.path().join("uv.lock");
        std::fs::write(&manifest, "[project]\ndependencies=['requests==2.31.0']\n").unwrap();
        std::fs::write(&lockfile, "version = 1\n").unwrap();
        let groups = plan_lock_groups(&[(manifest.clone(), FileType::PyProject)]).unwrap();
        std::fs::write(&manifest, "[project]\ndependencies=['requests==2.32.0']\n").unwrap();
        std::fs::write(&lockfile, "version = 2\n").unwrap();

        let failures = groups[0].snapshot.restore();

        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(
            std::fs::read_to_string(&manifest).unwrap(),
            "[project]\ndependencies=['requests==2.31.0']\n"
        );
        assert_eq!(std::fs::read_to_string(&lockfile).unwrap(), "version = 1\n");
    }

    fn failed_refresh(
        manifests: &[&Path],
        restored: &[&Path],
        failures: Vec<RestoreFailure>,
    ) -> LockRefresh {
        LockRefresh {
            manifests: manifests.iter().map(|path| path.to_path_buf()).collect(),
            outcomes: vec![RegenOutcome::Failed {
                lockfile: LockfileType::UvLock,
                message: "Failed to regenerate uv.lock: resolver blew up".to_string(),
            }],
            rollback: Some(Rollback {
                restored: restored.iter().map(|path| path.to_path_buf()).collect(),
                failures,
            }),
        }
    }

    /// A refresh that succeeded records nothing against its manifest.
    #[test]
    fn a_successful_refresh_records_no_failure() {
        let manifest = PathBuf::from("pyproject.toml");
        let refresh = LockRefresh {
            manifests: vec![manifest.clone()],
            outcomes: vec![RegenOutcome::Ok(LockfileType::UvLock)],
            rollback: None,
        };

        let (failures, errors) = report_lock_refreshes(vec![refresh], false);

        assert!(failures.is_empty());
        assert!(errors.is_empty());
        assert!(!refresh_failed(&failures, &manifest));
    }

    /// Every file of the group back at its pre-run bytes: the manifest is
    /// `rolled_back`, its error names the refresh failure and what was put
    /// back, and carries the hint to rerun without --lock.
    #[test]
    fn a_fully_restored_group_is_rolled_back_with_the_hint() {
        let manifest = PathBuf::from("pyproject.toml");
        let lockfile = PathBuf::from("uv.lock");
        let refresh = failed_refresh(&[&manifest], &[&manifest, &lockfile], Vec::new());

        let (failures, errors) = report_lock_refreshes(vec![refresh], false);

        let failure = &failures[&manifest];
        assert_eq!(failure.status, LockWriteStatus::RolledBack);
        assert!(
            failure
                .message
                .contains("Failed to regenerate uv.lock: resolver blew up"),
            "{}",
            failure.message
        );
        assert!(
            failure
                .message
                .contains("rolled back pyproject.toml and uv.lock"),
            "{}",
            failure.message
        );
        assert!(
            failure
                .message
                .ends_with(&format!("hint: {LOCK_ROLLBACK_HINT}")),
            "{}",
            failure.message
        );
        assert_eq!(errors, vec![failure.message.clone()]);
        assert!(refresh_failed(&failures, &manifest));
        assert!(!refresh_failed(
            &failures,
            Path::new("other/pyproject.toml")
        ));
    }

    /// One file that could not be put back makes every manifest sharing the
    /// lockfile `failed`, with the unrestored file named in the error and no
    /// hint, since the directory first needs that file put back by hand.
    #[test]
    fn a_group_with_an_unrestored_file_is_failed_for_every_manifest() {
        let manifest = PathBuf::from("pyproject.toml");
        let sibling = PathBuf::from("other.toml");
        let lockfile = PathBuf::from("uv.lock");
        let refresh = failed_refresh(
            &[&manifest, &sibling],
            &[&sibling, &lockfile],
            vec![RestoreFailure {
                path: manifest.clone(),
                reason: "Permission denied (os error 13)".to_string(),
            }],
        );

        let (failures, errors) = report_lock_refreshes(vec![refresh], false);

        for path in [&manifest, &sibling] {
            let failure = &failures[path];
            assert_eq!(
                failure.status,
                LockWriteStatus::Failed,
                "{}",
                path.display()
            );
            assert!(
                failure
                    .message
                    .contains("pyproject.toml was not restored: Permission denied (os error 13)"),
                "{}",
                failure.message
            );
            assert!(
                failure
                    .message
                    .contains("rolled back other.toml and uv.lock"),
                "{}",
                failure.message
            );
            assert!(
                !failure.message.contains(LOCK_ROLLBACK_HINT),
                "{}",
                failure.message
            );
            assert!(refresh_failed(&failures, path));
        }
        assert_eq!(errors.len(), 2);
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap()
    }

    #[test]
    fn test_format_held_back_line() {
        let pub_at = fixed_now() - Duration::days(2);
        let line = format_held_back_line(
            "lodash",
            "4.17.20",
            "4.17.21",
            "4.17.22",
            pub_at,
            Duration::days(7),
            fixed_now(),
        );
        assert!(line.contains("Held back"), "line: {line}");
        assert!(line.contains("lodash"), "line: {line}");
        assert!(line.contains("4.17.20"), "line: {line}");
        assert!(line.contains("4.17.21"), "line: {line}");
        assert!(line.contains("4.17.22"), "line: {line}");
        assert!(line.contains("2d ago"), "line: {line}");
        assert!(line.contains("cooldown 7d"), "line: {line}");
    }

    #[test]
    fn test_format_skipped_by_cooldown_line() {
        let pub_at = fixed_now() - Duration::hours(6);
        let line = format_skipped_by_cooldown_line(
            "express",
            "4.19.0",
            Some(pub_at),
            Duration::days(7),
            fixed_now(),
        );
        assert!(line.contains("Skipped"), "line: {line}");
        assert!(line.contains("express"), "line: {line}");
        assert!(line.contains("4.19.0"), "line: {line}");
        assert!(line.contains("released 6h ago"), "line: {line}");
        assert!(line.contains("cooldown 7d"), "line: {line}");
    }

    /// Without a publish date the line says so. The failure this guards against
    /// is not a missing word but a plausible one: substituting the run time
    /// renders "released 0s ago", which looks like a real, very recent release
    /// and is consistent with the cooldown that skipped it.
    #[test]
    fn test_format_skipped_by_cooldown_line_without_a_publish_date() {
        let line = format_skipped_by_cooldown_line(
            "express",
            "4.19.0",
            None,
            Duration::days(7),
            fixed_now(),
        );
        assert!(line.contains("Skipped"), "line: {line}");
        assert!(line.contains("4.19.0"), "line: {line}");
        assert!(
            line.contains("release date unknown"),
            "an unknown date must be stated, line: {line}"
        );
        assert!(
            !line.contains("released"),
            "no age may be claimed without a date, line: {line}"
        );
        assert!(
            !line.contains("0s ago"),
            "the run time must not stand in for the publish date, line: {line}"
        );
        assert!(line.contains("cooldown 7d"), "line: {line}");
    }

    #[test]
    fn test_humanize_age_seconds() {
        assert_eq!(humanize_age(Duration::seconds(30)), "30s ago");
    }

    #[test]
    fn test_humanize_age_minutes() {
        assert_eq!(humanize_age(Duration::minutes(45)), "45m ago");
    }

    #[test]
    fn test_humanize_age_hours() {
        assert_eq!(humanize_age(Duration::hours(5)), "5h ago");
    }

    #[test]
    fn test_humanize_age_days() {
        assert_eq!(humanize_age(Duration::days(3)), "3d ago");
    }

    #[test]
    fn test_humanize_age_weeks() {
        assert_eq!(humanize_age(Duration::days(21)), "3w ago");
    }

    #[test]
    fn test_humanize_age_negative_clamps_to_zero() {
        assert_eq!(humanize_age(Duration::seconds(-5)), "0s ago");
    }

    #[test]
    fn test_humanize_age_boundary_47h_stays_hours() {
        assert_eq!(humanize_age(Duration::hours(47)), "47h ago");
    }

    #[test]
    fn test_humanize_age_boundary_48h_switches_to_days() {
        assert_eq!(humanize_age(Duration::hours(48)), "2d ago");
    }

    #[test]
    fn test_humanize_age_boundary_13d_stays_days() {
        assert_eq!(humanize_age(Duration::days(13)), "13d ago");
    }

    #[test]
    fn test_humanize_age_boundary_14d_switches_to_weeks() {
        assert_eq!(humanize_age(Duration::days(14)), "2w ago");
    }

    #[test]
    fn test_humanize_cooldown_days() {
        assert_eq!(humanize_cooldown(Duration::days(7)), "7d");
    }

    #[test]
    fn test_humanize_cooldown_hours() {
        assert_eq!(humanize_cooldown(Duration::hours(6)), "6h");
    }

    #[test]
    fn test_humanize_cooldown_disabled() {
        assert_eq!(humanize_cooldown(Duration::zero()), "disabled");
    }

    #[test]
    fn test_has_checkable_manifest_changes_held_back() {
        let result = UpdateResult {
            held_back: vec![(
                "pkg".to_string(),
                "1.0.0".to_string(),
                "1.0.1".to_string(),
                "1.0.2".to_string(),
                Utc::now(),
            )],
            ..Default::default()
        };
        assert!(
            has_checkable_manifest_changes(&result, UpdateFilter::from_cli(&[], None)),
            "held_back entries must count as pending changes for --check"
        );
    }

    #[test]
    fn test_has_checkable_manifest_changes_skipped_by_cooldown_is_not_pending() {
        // A cooldown-only Skipped outcome is steady state, not pending work.
        // The tool intentionally chose to stay on the current version; that
        // must not flip `--check` to a non-zero exit.
        let result = UpdateResult {
            skipped_by_cooldown: vec![(
                "pkg".to_string(),
                "1.0.0".to_string(),
                "1.0.1".to_string(),
                Some(Utc::now()),
            )],
            ..Default::default()
        };
        assert!(
            !has_checkable_manifest_changes(&result, UpdateFilter::from_cli(&[], None)),
            "skipped_by_cooldown entries are steady state and must not count as pending changes"
        );
    }

    /// An unchecked SHA pin is not pending work either: nobody knows whether an
    /// update exists, and failing `--check` on it would break every CI job in a
    /// repo that pins its actions and leaves the feature off.
    #[test]
    fn test_has_checkable_manifest_changes_not_examined_is_not_pending() {
        let result = UpdateResult {
            skipped: vec![upd::updater::SkippedUpdate {
                package: "rvben/clispec".to_string(),
                current: "1111111111111111111111111111111111111111".to_string(),
                status: SkipStatus::NotExamined,
                reason: "action-sha-updates-off",
                message: "off".to_string(),
                line_number: Some(17),
            }],
            ..Default::default()
        };
        assert!(
            !has_checkable_manifest_changes(&result, UpdateFilter::from_cli(&[], None)),
            "an unchecked pin is an unknown, not a pending change"
        );
    }

    fn options_for(cli_flag: Option<bool>, config_key: Option<bool>) -> UpdateOptions {
        let package_filter = PackageFilter::default();
        build_update_options(
            true,
            false,
            cli_flag,
            Some(Arc::new(UpdConfig {
                update_action_shas: config_key,
                ..Default::default()
            })),
            &package_filter,
            &[],
            None,
            None,
            Arc::default(),
            BumpFilter::default(),
        )
    }

    #[test]
    fn test_action_sha_resolution_prefers_flag_then_config_then_default() {
        assert_eq!(
            options_for(None, None).update_action_shas,
            DEFAULT_UPDATE_ACTION_SHAS,
            "silence everywhere leaves the built-in default"
        );
        assert!(
            options_for(None, Some(true)).update_action_shas,
            "the config file decides when no flag was passed"
        );
        assert!(
            !options_for(None, Some(false)).update_action_shas,
            "the config file can turn it off as well as on"
        );
        assert!(
            options_for(Some(true), Some(false)).update_action_shas,
            "--update-action-shas overrides the config file"
        );
        assert!(
            !options_for(Some(false), Some(true)).update_action_shas,
            "--no-update-action-shas overrides the config file"
        );
    }

    #[test]
    fn test_has_checkable_manifest_changes_empty() {
        let result = UpdateResult::default();
        assert!(
            !has_checkable_manifest_changes(&result, UpdateFilter::from_cli(&[], None)),
            "empty result must not count as pending"
        );
    }

    fn annotated_edit<'a>(
        package: &'a str,
        old: &'a str,
        new: &'a str,
        line: usize,
        source: Option<AnnotationSource>,
    ) -> VersionEdit<'a> {
        VersionEdit {
            package,
            old_version: old,
            new_version: new,
            line_num: Some(line),
            expected_source: source,
            sha_pin: None,
        }
    }

    #[test]
    fn dockerfile_annotated_apply_preserves_other_lines() {
        let content = "FROM alpine:3.22\r\n # upd: pypi uv\r\nARG UV_VERSION=0.9.30\r\n";
        let edits = [annotated_edit(
            "uv",
            "0.9.30",
            "0.10.0",
            3,
            Some(AnnotationSource::PyPi),
        )];
        let result = apply_version_updates(content, &edits, FileType::Dockerfile, false).unwrap();
        assert_eq!(result.content, content.replace("0.9.30", "0.10.0"));
        assert_eq!(result.applied_count(), 1);
    }

    #[test]
    fn dockerfile_interactive_apply_refuses_changed_annotation_or_value() {
        let edits = [annotated_edit(
            "uv",
            "0.9.30",
            "0.10.0",
            3,
            Some(AnnotationSource::PyPi),
        )];
        for content in [
            "FROM scratch\n# upd: npm uv\nARG UV_VERSION=0.9.30\n",
            "FROM scratch\n# upd: pypi other\nARG UV_VERSION=0.9.30\n",
            "FROM scratch\n# upd: pypi uv\nARG UV_VERSION=0.9.31\n",
            "FROM scratch\n# ordinary comment\nARG UV_VERSION=0.9.30\n",
            "FROM scratch\n# upd: pypi uv\nFROM uv:0.9.30\n",
        ] {
            assert!(
                apply_version_updates(content, &edits, FileType::Dockerfile, false).is_err(),
                "{content}"
            );
        }
    }

    #[test]
    fn annotated_apply_rewrites_the_named_line() {
        let content = "A ?= 1.0.0  # upd: pypi widget\nB ?= 1.0.0  # upd: pypi other\n";
        let edits = [annotated_edit(
            "widget",
            "1.0.0",
            "2.0.0",
            1,
            Some(AnnotationSource::PyPi),
        )];
        let result =
            apply_version_updates(content, &edits, FileType::Annotated, false).expect("apply");
        assert_eq!(
            result.content,
            "A ?= 2.0.0  # upd: pypi widget\nB ?= 1.0.0  # upd: pypi other\n"
        );
        assert_eq!(result.applied_count(), 1);
    }

    #[test]
    fn annotated_apply_preserves_mixed_line_endings_and_physical_line_numbers() {
        let content =
            "FIRST := unchanged\r\nWIDGET ?= 1.0.0  # upd: pypi widget\nLAST := unchanged";
        let edits = [annotated_edit(
            "widget",
            "1.0.0",
            "2.0.0",
            2,
            Some(AnnotationSource::PyPi),
        )];

        let result =
            apply_version_updates(content, &edits, FileType::Annotated, false).expect("apply");

        assert_eq!(
            result.content,
            "FIRST := unchanged\r\nWIDGET ?= 2.0.0  # upd: pypi widget\nLAST := unchanged"
        );
        assert_eq!(result.applied_count(), 1);
    }

    #[test]
    fn annotated_apply_matches_the_package_name_byte_for_byte() {
        // The scan recorded `Azure.Core`; a recased edit must not write the line.
        let content = "PKG ?= 1.0.0  # upd: nuget Azure.Core\n";
        let edits = [annotated_edit(
            "azure.core",
            "1.0.0",
            "2.0.0",
            1,
            Some(AnnotationSource::NuGet),
        )];
        let err = apply_version_updates(content, &edits, FileType::Annotated, false)
            .expect_err("recased name must not apply");
        assert!(err.to_string().contains("azure.core"), "{err}");
    }

    #[test]
    fn annotated_apply_refuses_when_the_line_changed_source_under_it() {
        let content = "PKG ?= 1.0.0  # upd: npm widget\n";
        let edits = [annotated_edit(
            "widget",
            "1.0.0",
            "2.0.0",
            1,
            Some(AnnotationSource::PyPi),
        )];
        let err = apply_version_updates(content, &edits, FileType::Annotated, false)
            .expect_err("source mismatch must not apply");
        assert!(err.to_string().contains("widget"), "{err}");
    }

    #[test]
    fn annotated_apply_refuses_when_the_version_changed_under_it() {
        let content = "PKG ?= 1.5.0  # upd: pypi widget\n";
        let edits = [annotated_edit(
            "widget",
            "1.0.0",
            "2.0.0",
            1,
            Some(AnnotationSource::PyPi),
        )];
        apply_version_updates(content, &edits, FileType::Annotated, false)
            .expect_err("stale old_version must not apply");
    }

    #[test]
    fn annotated_apply_keeps_the_lines_v_prefix() {
        let content = "GH ?= v2.60.0  # upd: github-releases cli/cli\n";
        let edits = [annotated_edit(
            "cli/cli",
            "v2.60.0",
            "2.65.4",
            1,
            Some(AnnotationSource::GitHubReleases),
        )];
        let result =
            apply_version_updates(content, &edits, FileType::Annotated, false).expect("apply");
        assert_eq!(
            result.content,
            "GH ?= v2.65.4  # upd: github-releases cli/cli\n"
        );
    }

    /// The non-interactive writer (`AnnotatedUpdater::update`) and the
    /// interactive/align writer (`apply_version_updates`' annotated arm) are two
    /// implementations of one rewrite. Nothing forces them to agree, so this
    /// runs both over one fixture and compares bytes.
    ///
    /// The fixture carries the two cases where they could plausibly diverge: a
    /// `v`-prefixed pin, which only agrees if both call `reapply_v_prefix`, and a
    /// two-segment pin, which only agrees if both call `match_version_precision`.
    /// A three-segment plain pin would pass under either implementation and
    /// prove nothing.
    #[tokio::test]
    async fn both_write_paths_produce_the_same_bytes_for_an_annotated_file() {
        use std::sync::Mutex;
        use upd::registry::{MultiPyPiRegistry, PyPiRegistry};
        use upd::updater::{AnnotatedUpdater, RegistrySet, UpdateOptions, Updater};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        for (package, latest) in [("alpha", "2.0.0"), ("beta", "2.0.0")] {
            Mock::given(method("GET"))
                .and(path(format!("/pypi/{package}/json")))
                .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                    r#"{{"releases":{{"{latest}":[{{"yanked":false,"upload_time_iso_8601":"2024-01-01T00:00:00Z"}}]}}}}"#
                )))
                .mount(&mock)
                .await;
            Mock::given(method("GET"))
                .and(path(format!("/simple/{package}/")))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock)
                .await;
        }

        let original =
            "ALPHA ?= v1.2.0  # upd: pypi alpha\r\nUNCHANGED := yes\nBETA ?= 1.2  # upd: pypi beta";

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Makefile");
        std::fs::write(&file, original).unwrap();

        // `enabled: false` on every CachedRegistry, so no lookup is answered from
        // a shared on-disk cache and the mock sees both requests.
        let cache = Arc::new(Mutex::new(Cache::default()));
        let pypi = Arc::new(CachedRegistry::new(
            MultiPyPiRegistry::from_primary_and_extras(
                PyPiRegistry::with_index_url(mock.uri()),
                Vec::new(),
            ),
            Arc::clone(&cache),
            false,
        ));
        let npm = Arc::new(CachedRegistry::new(
            NpmRegistry::new(),
            Arc::clone(&cache),
            false,
        ));
        let crates_io = Arc::new(CachedRegistry::new(
            CratesIoRegistry::new(),
            Arc::clone(&cache),
            false,
        ));
        let go_proxy = Arc::new(CachedRegistry::new(
            GoProxyRegistry::new(),
            Arc::clone(&cache),
            false,
        ));
        let rubygems = Arc::new(CachedRegistry::new(
            RubyGemsRegistry::new(),
            Arc::clone(&cache),
            false,
        ));
        let nuget = Arc::new(CachedRegistry::new(
            NuGetRegistry::new(),
            Arc::clone(&cache),
            false,
        ));
        let github_releases = Arc::new(CachedRegistry::new(
            GitHubReleasesRegistry::new(),
            Arc::clone(&cache),
            false,
        ));

        let updater = AnnotatedUpdater::new(RegistrySet::resolving(
            &pypi,
            &npm,
            &crates_io,
            &go_proxy,
            &rubygems,
            &nuget,
            &github_releases,
        ));
        let result = updater
            .update(&file, pypi.as_ref(), UpdateOptions::new(false, false))
            .await
            .expect("update");
        let path_a = std::fs::read_to_string(&file).expect("read back");

        assert_eq!(result.updated.len(), 2, "{result:?}");
        assert_ne!(path_a, original, "the fixture must actually change");

        // Rebuild the interactive path's edits from what the updater reported,
        // exactly as `src/main.rs:2243-2248` does: the source comes from
        // `entry_ecosystem`, keyed by package name.
        let edits: Vec<VersionEdit<'_>> = result
            .updated
            .iter()
            .map(|(package, old, new, line)| VersionEdit {
                package: package.as_str(),
                old_version: old.as_str(),
                new_version: new.as_str(),
                line_num: *line,
                expected_source: result.entry_ecosystem.get(package).copied(),
                sha_pin: None,
            })
            .collect();

        let applied = apply_version_updates(original, &edits, FileType::Annotated, false)
            .expect("apply interactive-path edits");

        assert_eq!(applied.applied_count(), 2, "both edits must apply");
        assert_eq!(
            applied.content, path_a,
            "the interactive writer and the updater disagree about the same rewrite"
        );
        assert_eq!(
            path_a,
            "ALPHA ?= v2.0.0  # upd: pypi alpha\r\nUNCHANGED := yes\nBETA ?= 2.0  # upd: pypi beta",
            "both must preserve exact line endings, final-newline state, v prefix, and precision"
        );
    }
}

#[cfg(test)]
mod pre_commit_selection_tests {
    use super::*;
    use upd::updater::PreCommitEdit;

    #[test]
    fn selecting_one_inline_dependency_only_changes_that_scalar() {
        let content = "repos = [{repo = 'local', hooks = [{id = 'demo', language = 'python', additional_dependencies = ['demo==1.0.0', 'other==1.0.0']}]}]";
        let start = content.find("demo==1.0.0").unwrap();
        let planned = vec![PreCommitEdit {
            package: "demo".into(),
            current: "1.0.0".into(),
            new: "2.0.0".into(),
            line: Some(1),
            span: start..start + "demo==1.0.0".len(),
            original: "demo==1.0.0".into(),
            replacement: "demo==2.0.0".into(),
            source: Some(AnnotationSource::PyPi),
            pinned: false,
            required_revision: None,
        }];
        let updates = [VersionEdit {
            package: "demo",
            old_version: "1.0.0",
            new_version: "2.0.0",
            line_num: Some(1),
            expected_source: Some(AnnotationSource::PyPi),
            sha_pin: None,
        }];
        let rewritten = apply_selected_pre_commit_edits(content, &updates, &planned).unwrap();
        assert_eq!(
            rewritten.content,
            content.replace("demo==1.0.0", "demo==2.0.0")
        );
        assert!(
            apply_selected_pre_commit_edits(
                &content.replace("demo==1.0.0", "demo==1.1.0"),
                &updates,
                &planned
            )
            .is_err()
        );
        let mut planned = planned;
        planned[0].required_revision = Some((0..1, "v2".into()));
        assert!(apply_selected_pre_commit_edits(content, &updates, &planned).is_err());
    }

    #[test]
    fn alignment_rewrites_toml_and_flow_yaml_revisions() {
        for content in [
            "repos = [{repo = 'https://github.com/owner/repo', rev = 'v1.0.0', hooks = []}]",
            "repos: [{repo: 'https://github.com/owner/repo', rev: 'v1.0.0', hooks: []}]",
        ] {
            let updates = [VersionEdit {
                package: "owner/repo",
                old_version: "v1.0.0",
                new_version: "v2.0.0",
                line_num: Some(1),
                expected_source: None,
                sha_pin: None,
            }];
            let rewritten =
                apply_version_updates(content, &updates, FileType::PreCommitConfig, false).unwrap();
            assert_eq!(rewritten.content, content.replace("v1.0.0", "v2.0.0"));
        }
    }
}
