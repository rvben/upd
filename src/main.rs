use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use colored::Colorize;
use futures::stream::{self, StreamExt};

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use upd::align::{PackageAlignment, PackageOccurrence, find_alignments, scan_packages};
use upd::audit::cache::AuditCache;
use upd::audit::{AuditResult, Ecosystem, OsvClient, Package as AuditPackage, compute_fix_plan};
use upd::cache::{Cache, CachedRegistry};
use upd::cli::{BumpLevel, Cli, Command, REVERT_TIP};
use upd::config::UpdConfig;
use upd::cooldown::CooldownPolicy;
use upd::interactive::{PendingUpdate, prompt_all};
use upd::lockfile::{LockfileRegenResult, regenerate_lockfiles};
use upd::registry::{
    CratesIoRegistry, GitHubReleasesRegistry, GoProxyRegistry, MultiPyPiRegistry, NpmRegistry,
    NuGetRegistry, PyPiRegistry, RubyGemsRegistry, TerraformRegistry,
};
use upd::updater::{
    CargoTomlUpdater, CsprojUpdater, FileType, GemfileUpdater, GithubActionsUpdater, GoModUpdater,
    Lang, MiseUpdater, PackageJsonUpdater, PreCommitUpdater, PyProjectUpdater, RequirementsUpdater,
    TerraformUpdater, UpdateOptions, UpdateResult, Updater, discover_files, read_file_safe,
    write_file_atomic,
};
use upd::version::match_version_precision;

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

/// Parse version components
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim_start_matches('v');
    let parts: Vec<&str> = v.split('.').collect();
    let major = parts.first()?.parse().ok()?;
    let minor = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// Classify an update as major, minor, or patch
fn classify_update(old: &str, new: &str) -> UpdateType {
    if let (Some((old_major, old_minor, _)), Some((new_major, new_minor, _))) =
        (parse_version(old), parse_version(new))
    {
        if new_major > old_major {
            return UpdateType::Major;
        }
        if new_minor > old_minor {
            return UpdateType::Minor;
        }
        UpdateType::Patch
    } else {
        UpdateType::Patch
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
    skipped_published_at: DateTime<Utc>,
    cooldown: Duration,
    now: DateTime<Utc>,
) -> String {
    let age = now - skipped_published_at;
    format!(
        "Skipped {name} (only newer version {skipped_latest} released {}, cooldown {})",
        humanize_age(age),
        humanize_cooldown(cooldown),
    )
}

/// Map a [`FileType`] to the registry ecosystem name used by cooldown policy keys.
fn ecosystem_for_file_type(file_type: FileType) -> &'static str {
    match file_type {
        FileType::Requirements | FileType::PyProject => "pypi",
        FileType::PackageJson => "npm",
        FileType::CargoToml => "crates.io",
        FileType::GoMod => "go-proxy",
        FileType::Gemfile => "rubygems",
        FileType::GithubActions
        | FileType::PreCommitConfig
        | FileType::MiseToml
        | FileType::ToolVersions => "github-releases",
        FileType::Csproj => "nuget",
        FileType::TerraformTf => "terraform",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateType {
    Major,
    Minor,
    Patch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ChangeKind {
    RegistryUpdate,
    ConfigPin,
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
        format!("Using config from: {}", resolved.path.display()).cyan()
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
}

fn discover_update_config(start_dir: &Path) -> Option<ResolvedUpdateConfig> {
    UpdConfig::discover(start_dir).map(|(config, path)| ResolvedUpdateConfig {
        config: Arc::new(config),
        path,
        explicit: false,
    })
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
                let discovered = discover_update_config(&start_dir);
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

fn build_update_options(
    dry_run: bool,
    full_precision: bool,
    config: Option<Arc<UpdConfig>>,
    packages: &[String],
    cooldown_policy: Option<&CooldownPolicy>,
    cooldown_notes: Arc<Mutex<BTreeSet<String>>>,
) -> UpdateOptions {
    let mut options = UpdateOptions::new(dry_run, full_precision);
    if let Some(config) = config {
        options = options.with_config(config);
    }
    options = options.with_packages(packages.to_vec());
    if let Some(policy) = cooldown_policy {
        options = options.with_cooldown_policy(policy.clone(), Utc::now());
    }
    options.cooldown_unavailable_notes = cooldown_notes;
    options
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
    !result.updated.is_empty() || !result.pinned.is_empty()
}

fn has_checkable_manifest_changes(result: &UpdateResult, filter: UpdateFilter) -> bool {
    // A cooldown-only "Skipped" outcome is expected steady state: we
    // deliberately chose to hold the current version. It must NOT trip
    // `--check` into signaling "pending work". Held-back entries do count:
    // they mean we are already writing a different version than the registry
    // latest, so there is something actionable (the safer pin).
    let (_, _, _, filtered_total) = count_updates_by_type(&result.updated, filter);
    filtered_total > 0 || !result.pinned.is_empty() || !result.held_back.is_empty()
}

fn has_interactive_changes(
    pending_updates: &[PendingUpdate],
    scanned_results: &[ScannedFileResult],
) -> bool {
    !pending_updates.is_empty()
        || scanned_results
            .iter()
            .any(|scanned| !scanned.result.pinned.is_empty())
}

fn audit_status(audit_result: &AuditResult) -> AuditStatus {
    if !audit_result.errors.is_empty() {
        AuditStatus::Incomplete
    } else if audit_result.vulnerable.is_empty() {
        AuditStatus::Clean
    } else {
        AuditStatus::Vulnerable
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle no-color flag
    if cli.no_color {
        colored::control::set_override(false);
    }

    // --show-config: print the canonical config schema and exit
    if cli.show_config {
        print!("{}", upd::config::UpdConfig::schema_toml());

        // Also render the active cooldown policy (if any).
        let cwd = std::env::current_dir()?;
        let (loaded_config, _) = upd::config::UpdConfig::discover(&cwd)
            .unwrap_or_else(|| (upd::config::UpdConfig::default(), cwd.clone()));
        let policy = loaded_config.to_cooldown_policy(cli.min_age.as_deref())?;
        println!();
        print!("{}", upd::config::render_cooldown_for_show_config(&policy));

        return Ok(());
    }

    // Reject non-existent paths before any I/O; known subcommands are routed by clap before this point.
    let invalid: Vec<_> = cli.paths.iter().filter(|p| !p.exists()).collect();
    for path in &invalid {
        eprintln!(
            "{} '{}' is not a known subcommand or existing path",
            "error:".red(),
            path.display()
        );
    }
    if !invalid.is_empty() {
        std::process::exit(2);
    }

    match &cli.command {
        Some(Command::CleanCache) => {
            clean_cache()?;
        }
        Some(Command::SelfUpdate) => {
            self_update().await?;
        }
        Some(Command::Align { .. }) => {
            run_align(&cli).await?;
        }
        Some(Command::Audit { .. }) => {
            run_audit(&cli).await?;
        }
        Some(Command::Update { .. }) | None => {
            run_update(&cli).await?;
        }
    }

    Ok(())
}

async fn run_update(cli: &Cli) -> Result<()> {
    if cli.interactive && cli.format == upd::cli::OutputFormat::Json {
        anyhow::bail!("--interactive cannot be combined with --format json");
    }

    // Resolve paths: explicit > VCS root > error
    let paths = match resolve_scan_paths(cli) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{} {}", "error:".red(), msg);
            std::process::exit(2);
        }
    };

    // Mutations are opt-in. Without --apply (and not --interactive, --check,
    // or --dry-run), the run behaves as dry-run and tells the user how to apply.
    let effective_dry_run = cli.is_effective_dry_run();

    let files = discover_files(&paths, &cli.langs);
    let file_count = files.len();

    let text_mode_early = cli.format == upd::cli::OutputFormat::Text;

    if files.is_empty() {
        if text_mode_early {
            if !cli.quiet {
                println!("{}", "No dependency files found.".yellow());
            }
        } else {
            emit_update_json(
                &[],
                &UpdateResult::default(),
                0,
                effective_dry_run,
                UpdateFilter::from_cli(&cli.only_bump, cli.max_bump),
                &HashMap::new(),
                Vec::new(),
            )?;
        }
        return Ok(());
    }

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
    let cooldown_notes: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));

    if cli.verbose {
        println!(
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
            println!("{}", "Using authenticated PyPI access".cyan());
        }
        let primary = PyPiRegistry::with_index_url_and_credentials(index_url, credentials);

        // Check for extra index URLs (UV_EXTRA_INDEX_URL, PIP_EXTRA_INDEX_URL)
        let extra_urls = PyPiRegistry::detect_extra_index_urls();
        if cli.verbose && !extra_urls.is_empty() {
            println!(
                "{}",
                format!("Using {} extra PyPI index(es)", extra_urls.len()).cyan()
            );
        }

        MultiPyPiRegistry::from_primary_and_extras(primary, extra_urls)
    };

    let pypi = CachedRegistry::new(pypi_registry, Arc::clone(&cache), cache_enabled);

    // Create npm registry with optional credentials
    let npm_registry = {
        let registry_url = NpmRegistry::detect_registry_url()
            .unwrap_or_else(|| "https://registry.npmjs.org".to_string());
        let credentials = NpmRegistry::detect_credentials(&registry_url);
        if cli.verbose && credentials.is_some() {
            println!("{}", "Using authenticated npm access".cyan());
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
            println!("{}", "Using authenticated crates.io access".cyan());
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
            println!("{}", "Using authenticated Go proxy access".cyan());
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

    // Create NuGet registry
    let nuget_registry = NuGetRegistry::new();
    let nuget = CachedRegistry::new(nuget_registry, Arc::clone(&cache), cache_enabled);

    // Create GitHub releases registry with optional token
    let github_releases_registry = GitHubReleasesRegistry::new();
    if cli.verbose && GitHubReleasesRegistry::detect_token().is_some() {
        println!("{}", "Using authenticated GitHub access".cyan());
    }
    let github_releases =
        CachedRegistry::new(github_releases_registry, Arc::clone(&cache), cache_enabled);

    // Create updaters wrapped in Arc for parallel processing
    let requirements_updater = Arc::new(RequirementsUpdater::new());
    let pyproject_updater = Arc::new(PyProjectUpdater::new());
    let package_json_updater = Arc::new(PackageJsonUpdater::new());
    let cargo_toml_updater = Arc::new(CargoTomlUpdater::new());
    let go_mod_updater = Arc::new(GoModUpdater::new());
    let github_actions_updater = Arc::new(GithubActionsUpdater::new());
    let pre_commit_updater = Arc::new(PreCommitUpdater::new());
    let gemfile_updater = Arc::new(GemfileUpdater::new());
    let mise_updater = Arc::new(MiseUpdater::new());
    let terraform_updater = Arc::new(TerraformUpdater::new());
    let csproj_updater = Arc::new(CsprojUpdater::new());

    // Wrap registries in Arc for parallel processing
    let pypi = Arc::new(pypi);
    let npm = Arc::new(npm);
    let crates_io = Arc::new(crates_io);
    let go_proxy = Arc::new(go_proxy);
    let rubygems = Arc::new(rubygems);
    let terraform = Arc::new(terraform);
    let nuget = Arc::new(nuget);
    let github_releases = Arc::new(github_releases);

    // Interactive mode: first discover updates, then prompt, then apply approved ones
    if cli.interactive {
        return run_interactive_update(
            cli,
            &files,
            &file_configs,
            filter,
            &pypi,
            &npm,
            &crates_io,
            &go_proxy,
            &rubygems,
            &terraform,
            &nuget,
            &github_releases,
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
            &cache,
            cache_enabled,
            &file_cooldowns,
            Arc::clone(&cooldown_notes),
        )
        .await;
    }

    // Non-interactive mode: process files in parallel
    let dry_run = effective_dry_run;
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
                    config,
                    &cli.packages,
                    cooldown_policy,
                    Arc::clone(&cooldown_notes),
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
            let requirements_updater = Arc::clone(&requirements_updater);
            let pyproject_updater = Arc::clone(&pyproject_updater);
            let package_json_updater = Arc::clone(&package_json_updater);
            let cargo_toml_updater = Arc::clone(&cargo_toml_updater);
            let go_mod_updater = Arc::clone(&go_mod_updater);
            let gemfile_updater = Arc::clone(&gemfile_updater);
            let github_actions_updater = Arc::clone(&github_actions_updater);
            let pre_commit_updater = Arc::clone(&pre_commit_updater);
            let mise_updater = Arc::clone(&mise_updater);
            let csproj_updater = Arc::clone(&csproj_updater);
            let terraform_updater = Arc::clone(&terraform_updater);

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
                        github_actions_updater
                            .update(&path, github_releases.as_ref(), update_options.clone())
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
                };
                (path, file_type, result.map_err(|e| e.to_string()))
            }
        })
        .buffer_unordered(concurrency_limit)
        .collect()
        .await;

    // Process results, preserving per-file attribution for both text and JSON output.
    let text_mode = cli.format == upd::cli::OutputFormat::Text;
    let mut total_result = UpdateResult::default();
    let mut updated_files: Vec<PathBuf> = Vec::new();
    let mut scanned: Vec<ScannedFileResult> = Vec::new();

    for (path, file_type, result) in results {
        if verbose && text_mode {
            println!("{}", format!("Processed: {}", path.display()).cyan());
        }

        match result {
            Ok(file_result) => {
                if !dry_run && file_has_manifest_changes(&file_result) {
                    updated_files.push(path.clone());
                }
                if text_mode && !cli.quiet {
                    let cooldown_policy = file_cooldowns.get(&path).and_then(|p| p.as_ref());
                    print_file_result(
                        &path.display().to_string(),
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
                    result: file_result.clone(),
                });
                total_result.merge(file_result);
            }
            Err(e) => {
                let msg = format!("Error processing {}: {}", path.display(), e);
                eprintln!("{}", msg.red());
                // Surface the outer error in both the aggregate and the per-file
                // record so JSON output captures it and the exit-code logic can
                // detect that errors occurred.
                let error_result = UpdateResult {
                    errors: vec![e.clone()],
                    ..Default::default()
                };
                total_result.errors.push(e);
                scanned.push(ScannedFileResult {
                    path: path.clone(),
                    file_type,
                    result: error_result,
                });
            }
        }
    }

    // Regenerate lockfiles if requested and files were updated
    if cli.lock && !dry_run && !updated_files.is_empty() {
        let mut processed_dirs: HashSet<PathBuf> = HashSet::new();
        let mut regen_results: Vec<(PathBuf, LockfileRegenResult)> = Vec::new();

        for path in &updated_files {
            if let Some(dir) = path.parent() {
                let dir_path = dir.to_path_buf();
                if processed_dirs.insert(dir_path) {
                    let result = regenerate_lockfiles(path, verbose && text_mode);
                    regen_results.push((path.clone(), result));
                }
            }
        }

        // Determine whether any lockfiles will actually be regenerated so
        // the header is only printed when there is real work to do.
        let has_work = regen_results.iter().any(|(_, r)| !r.no_lockfiles);

        if text_mode && has_work && !cli.quiet {
            println!();
            println!("{}", "Regenerating lockfiles...".cyan());
        }

        for (path, result) in regen_results {
            if result.no_lockfiles {
                let manifest_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                eprintln!(
                    "note: no lockfile found for {} — skipping (nothing to regenerate)",
                    manifest_name
                );
                continue;
            }
            for outcome in result.outcomes {
                if let Some(msg) = outcome.error_message() {
                    eprintln!("{}", format!("error: {msg}").red());
                    total_result.errors.push(msg);
                }
            }
        }
    }

    // Save cache to disk
    if cache_enabled {
        let _ = Cache::save_shared(&cache);
    }

    // Emit cooldown unavailability notes (deduplicated across all files).
    if let Ok(notes) = cooldown_notes.lock() {
        for note in notes.iter() {
            eprintln!("note: {}", note);
        }
    }

    if text_mode {
        if !cli.quiet {
            println!();
            let applied = print_summary(&total_result, file_count, dry_run, filter);
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
            .map(|g| g.iter().cloned().collect())
            .unwrap_or_default();
        emit_update_json(
            &scanned,
            &total_result,
            file_count,
            dry_run,
            filter,
            &file_cooldowns,
            notes_vec,
        )?;
    }

    let has_errors = !total_result.errors.is_empty();
    let has_pending = has_checkable_manifest_changes(&total_result, filter);
    let exit_code = upd::decide_exit_code(dry_run, has_pending, has_errors);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

fn emit_update_json(
    scanned: &[ScannedFileResult],
    total_result: &UpdateResult,
    file_count: usize,
    dry_run: bool,
    filter: UpdateFilter,
    file_cooldowns: &HashMap<PathBuf, Option<CooldownPolicy>>,
    cooldown_notes: Vec<String>,
) -> Result<()> {
    use upd::output::{UpdateReport, UpdateSummary, build_update_file_report};

    let files: Vec<_> = scanned
        .iter()
        .map(|sf| {
            let cooldown_seconds = file_cooldowns
                .get(&sf.path)
                .and_then(|p| p.as_ref())
                .map(|p| {
                    p.effective_for(ecosystem_for_file_type(sf.file_type))
                        .num_seconds()
                })
                .unwrap_or(0);
            build_update_file_report(
                &sf.path,
                sf.file_type,
                &sf.result,
                cooldown_seconds,
                |old, new| match classify_update(old, new) {
                    UpdateType::Major => "major",
                    UpdateType::Minor => "minor",
                    UpdateType::Patch => "patch",
                },
            )
        })
        .collect();

    let (major, minor, patch, total) = count_updates_by_type(&total_result.updated, filter);
    let summary = UpdateSummary {
        files_scanned: file_count,
        files_with_changes: scanned
            .iter()
            .filter(|sf| file_has_manifest_changes(&sf.result))
            .count(),
        updates_total: total,
        updates_major: major,
        updates_minor: minor,
        updates_patch: patch,
        pinned: total_result.pinned.len(),
        ignored: total_result.ignored.len(),
        errors: total_result.errors.len(),
        warnings: total_result.warnings.len(),
        held_back: total_result.held_back.len(),
        skipped_by_cooldown: total_result.skipped_by_cooldown.len(),
    };

    let report = UpdateReport {
        command: "update",
        mode: if dry_run { "dry-run" } else { "applied" },
        files,
        summary,
        cooldown_notes,
    };

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_interactive_update(
    cli: &Cli,
    files: &[(std::path::PathBuf, FileType)],
    file_configs: &HashMap<PathBuf, Option<Arc<UpdConfig>>>,
    filter: UpdateFilter,
    pypi: &Arc<CachedRegistry<MultiPyPiRegistry>>,
    npm: &Arc<CachedRegistry<NpmRegistry>>,
    crates_io: &Arc<CachedRegistry<CratesIoRegistry>>,
    go_proxy: &Arc<CachedRegistry<GoProxyRegistry>>,
    rubygems: &Arc<CachedRegistry<RubyGemsRegistry>>,
    terraform: &Arc<CachedRegistry<TerraformRegistry>>,
    nuget: &Arc<CachedRegistry<NuGetRegistry>>,
    github_releases: &Arc<CachedRegistry<GitHubReleasesRegistry>>,
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
    cache: &Arc<std::sync::Mutex<Cache>>,
    cache_enabled: bool,
    file_cooldowns: &HashMap<PathBuf, Option<CooldownPolicy>>,
    cooldown_notes: Arc<Mutex<BTreeSet<String>>>,
) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "{} --interactive requires a terminal on stdin",
            "error:".red()
        );
        eprintln!(
            "{} use --check to preview updates, or --dry-run to print proposed changes",
            "hint:".dimmed()
        );
        std::process::exit(2);
    }

    let mut pending_updates: Vec<PendingUpdate> = Vec::new();
    let mut planned_changes: Vec<PlannedChange> = Vec::new();
    let mut scanned_results: Vec<ScannedFileResult> = Vec::new();

    for (path, file_type) in files {
        let cooldown_policy = file_cooldowns.get(path).and_then(|p| p.as_ref());
        let dry_run_options = build_update_options(
            true,
            cli.full_precision,
            file_configs.get(path).cloned().flatten(),
            &cli.packages,
            cooldown_policy,
            Arc::clone(&cooldown_notes),
        );

        if cli.verbose {
            println!("{}", format!("Scanning: {}", path.display()).cyan());
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
                github_actions_updater
                    .update(path, github_releases.as_ref(), dry_run_options.clone())
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
        };

        match result {
            Ok(file_result) => {
                for update in &file_result.updated {
                    let (package, old_version, new_version, line_num) = update;
                    let update_type = classify_update(old_version, new_version);

                    // Apply filter
                    if !filter.matches(update_type) {
                        continue;
                    }

                    pending_updates.push(PendingUpdate::new(
                        path.display().to_string(),
                        *line_num,
                        package.clone(),
                        old_version.clone(),
                        new_version.clone(),
                        update_type == UpdateType::Major,
                    ));
                    planned_changes.push(PlannedChange::from_update(
                        path.clone(),
                        *file_type,
                        update,
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
                    format!("Error processing {}: {}", path.display(), e).red()
                );
            }
        }
    }

    if !has_interactive_changes(&pending_updates, &scanned_results) {
        if !cli.quiet {
            println!(
                "{} Scanned {} file(s), all dependencies up to date",
                "✓".green(),
                files.len()
            );
        }
        return Ok(());
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

    if approved_count == 0 && configured_pin_count == 0 {
        if !cli.quiet {
            println!("\n{}", "No updates applied.".yellow());
        }
        return Ok(());
    }

    let mut apply_parts = Vec::new();
    if approved_count > 0 {
        apply_parts.push(format!("{} selected update(s)", approved_count));
    }
    if configured_pin_count > 0 {
        apply_parts.push(format!("{} configured pin(s)", configured_pin_count));
    }
    if !cli.quiet {
        println!(
            "\n{}",
            format!("Applying {}...", apply_parts.join(" and ")).cyan()
        );
    }

    let mut applied_updates = 0;
    let mut applied_pins = 0;
    let mut updated_files: Vec<std::path::PathBuf> = Vec::new();

    for scanned_file in scanned_results {
        let selected_changes =
            collect_selected_changes_for_file(&scanned_file, &mut approved_change_counts);
        if selected_changes.is_empty() {
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
            })
            .collect();
        let rewritten = apply_version_updates(
            &content,
            &updates,
            scanned_file.file_type,
            cli.full_precision,
        )
        .map_err(|e| anyhow::anyhow!("Failed to rewrite {}: {}", scanned_file.path.display(), e))?;

        if rewritten.content == content {
            continue;
        }

        write_file_atomic(&scanned_file.path, &rewritten.content)?;
        updated_files.push(scanned_file.path.clone());

        let file_str = scanned_file.path.display().to_string();
        for change in selected_changes {
            let location = match change.line_num {
                Some(n) => format!("{}:{}:", file_str, n),
                None => format!("{}:", file_str),
            };

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
            }
        }
    }

    // Regenerate lockfiles if requested and files were updated
    if cli.lock && !updated_files.is_empty() {
        let mut processed_dirs: HashSet<std::path::PathBuf> = HashSet::new();
        let mut regen_results: Vec<(PathBuf, LockfileRegenResult)> = Vec::new();

        for path in &updated_files {
            if let Some(dir) = path.parent() {
                let dir_path = dir.to_path_buf();
                if processed_dirs.insert(dir_path) {
                    let result = regenerate_lockfiles(path, cli.verbose);
                    regen_results.push((path.clone(), result));
                }
            }
        }

        // Only print the header when at least one lockfile will be regenerated.
        let has_work = regen_results.iter().any(|(_, r)| !r.no_lockfiles);

        if has_work && !cli.quiet {
            println!();
            println!("{}", "Regenerating lockfiles...".cyan());
        }

        let mut had_error = false;
        for (path, result) in regen_results {
            if result.no_lockfiles {
                let manifest_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                eprintln!(
                    "note: no lockfile found for {} — skipping (nothing to regenerate)",
                    manifest_name
                );
                continue;
            }
            for outcome in result.outcomes {
                if let Some(msg) = outcome.error_message() {
                    eprintln!("{}", format!("error: {msg}").red());
                    had_error = true;
                }
            }
        }
        if had_error {
            std::process::exit(2);
        }
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
    }

    Ok(())
}

async fn run_align(cli: &Cli) -> Result<()> {
    let text_mode = cli.format == upd::cli::OutputFormat::Text;

    // Resolve paths: explicit > VCS root > error
    let paths = match resolve_scan_paths(cli) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{} {}", "error:".red(), msg);
            std::process::exit(2);
        }
    };

    let files = discover_files(&paths, &cli.langs);
    let file_count = files.len();

    if files.is_empty() {
        if text_mode {
            if !cli.quiet {
                println!("{}", "No dependency files found.".yellow());
            }
        } else {
            emit_align_json(&[], 0)?;
        }
        return Ok(());
    }

    if cli.verbose && text_mode {
        println!(
            "{}",
            format!("Scanning {} dependency file(s) for alignment", file_count).cyan()
        );
    }

    // Scan all files for packages
    let packages = match scan_packages(&files) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", format!("Error scanning files: {}", e).red());
            return Err(e);
        }
    };

    // Find alignments
    let align_result = find_alignments(packages);

    // Filter to only misaligned packages
    let misaligned: Vec<&PackageAlignment> = align_result
        .packages
        .iter()
        .filter(|p| p.has_misalignment())
        .collect();

    if !text_mode {
        let to_report: Vec<PackageAlignment> = align_result
            .packages
            .iter()
            .filter(|p| p.has_misalignment())
            .cloned()
            .collect();
        emit_align_json(&to_report, file_count)?;
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

    // Mutations are opt-in for align as well.
    let dry_run = cli.check || cli.dry_run || (!cli.apply && !cli.interactive);

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
    }

    // In check mode, exit with code 1 if any misalignments exist
    if cli.check && !misaligned.is_empty() {
        std::process::exit(1);
    }

    Ok(())
}

fn emit_align_json(packages: &[PackageAlignment], file_count: usize) -> Result<()> {
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

    println!("{}", serde_json::to_string_pretty(&report)?);
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
) -> Vec<AuditPackage> {
    let mut audit_packages: Vec<AuditPackage> = Vec::new();
    let mut seen: HashSet<(String, String, String)> = HashSet::new();

    for ((name, lang), occurrences) in packages {
        // OSV doesn't cover GitHub Actions, pre-commit hooks, mise tools, or Terraform; skip
        if *lang == Lang::Actions
            || *lang == Lang::PreCommit
            || *lang == Lang::Mise
            || *lang == Lang::Terraform
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
            Lang::Actions | Lang::PreCommit | Lang::Mise | Lang::Terraform => {
                unreachable!("filtered above")
            }
        };

        for occurrence in occurrences {
            let key = (
                name.clone(),
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

    audit_packages
}

/// Per-file edit list used by the --fix-audit apply path.
///
/// Each entry is `(file_type, [(package_name, old_version, new_version, line_num)])`.
type FileEdits = (FileType, Vec<(String, String, String, Option<usize>)>);

async fn run_audit(cli: &Cli) -> Result<()> {
    let no_fail = matches!(&cli.command, Some(Command::Audit { no_fail, .. }) if *no_fail);
    let fix_audit = matches!(&cli.command, Some(Command::Audit { fix_audit, .. }) if *fix_audit);
    let offline = matches!(&cli.command, Some(Command::Audit { offline, .. }) if *offline);
    let text_mode = cli.format == upd::cli::OutputFormat::Text;
    let sarif_mode = cli.format == upd::cli::OutputFormat::Sarif;
    // Audit never mutates files, so no VCS check is needed. Fall back to CWD.
    let paths = {
        let explicit = cli.get_paths();
        if explicit.is_empty() {
            vec![PathBuf::from(".")]
        } else {
            explicit
        }
    };
    let files = discover_files(&paths, &cli.langs);
    let file_count = files.len();

    if files.is_empty() {
        if text_mode {
            if !cli.quiet {
                println!("{}", "No dependency files found.".yellow());
            }
        } else if sarif_mode {
            emit_audit_sarif(&AuditResult::default(), &HashMap::new())?;
        } else {
            emit_audit_json(&AuditResult::default(), "complete")?;
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
    let packages = match scan_packages(&files) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", format!("Error scanning files: {}", e).red());
            return Err(e);
        }
    };

    // Convert to audit packages (deduplicate by name+version+ecosystem)
    let audit_packages = build_audit_packages(&packages);

    if audit_packages.is_empty() {
        if text_mode {
            if !cli.quiet {
                println!(
                    "{} Scanned {} file(s), no packages found",
                    "✓".green(),
                    file_count
                );
            }
        } else if sarif_mode {
            emit_audit_sarif(&AuditResult::default(), &HashMap::new())?;
        } else {
            emit_audit_json(&AuditResult::default(), "complete")?;
        }
        return Ok(());
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
    let audit_result = osv_client
        .check_packages_cached(&audit_packages, audit_cache.as_ref(), offline)
        .await?;

    // Persist cache to disk after a successful query.
    if let Some(ref c) = audit_cache {
        // Non-fatal: a save failure should not abort the audit report.
        let _ = AuditCache::save_shared(c);
    }

    let status = audit_status(&audit_result);

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
                    if audit_result.vulnerable.is_empty() {
                        println!(
                            "\n{} Audit incomplete: {} error(s) occurred while checking {} package(s)",
                            "⚠".yellow().bold(),
                            audit_result.errors.len().to_string().yellow().bold(),
                            audit_packages.len()
                        );
                    } else {
                        print_audit_vulnerabilities(&audit_result);
                        println!(
                            "\n{} Audit incomplete: {} error(s) occurred while checking dependencies",
                            "⚠".yellow().bold(),
                            audit_result.errors.len().to_string().yellow().bold()
                        );
                    }
                }
            }
        }

        for error in &audit_result.errors {
            eprintln!("{} {}", "Error:".red(), error);
        }
    } else if sarif_mode {
        // Build the per-package occurrence map from already-scanned data.
        let sarif_occurrences = build_sarif_occurrences(&packages);
        emit_audit_sarif(&audit_result, &sarif_occurrences)?;
    } else {
        let status_str = match status {
            AuditStatus::Clean | AuditStatus::Vulnerable => "complete",
            AuditStatus::Incomplete => "incomplete",
        };
        emit_audit_json(&audit_result, status_str)?;
    }

    // --fix-audit: bump each vulnerable package to its minimum safe version.
    if fix_audit && !audit_result.vulnerable.is_empty() {
        let (fixable, unfixable) = compute_fix_plan(&audit_result);

        // Report unfixable packages.
        if text_mode && !cli.quiet {
            for (name, reason) in &unfixable {
                eprintln!(
                    "{} Cannot auto-fix {}: {}",
                    "⚠".yellow().bold(),
                    name.bold(),
                    reason
                );
            }
        }

        if !fixable.is_empty() {
            // Build a per-file map of (package_name, old_version, new_version, line_num)
            // using the already-scanned package occurrences.
            //
            // We use the same approach as apply_alignments: collect edits per file,
            // then apply them with apply_version_updates.
            let mut edits_by_file: HashMap<PathBuf, FileEdits> = HashMap::new();

            for ((name_lower, lang), occurrences) in &packages {
                // Only ecosystems OSV covers.
                if *lang == Lang::Actions
                    || *lang == Lang::PreCommit
                    || *lang == Lang::Mise
                    || *lang == Lang::Terraform
                {
                    continue;
                }

                for occ in occurrences {
                    if !occ.is_bumpable {
                        continue;
                    }
                    // Match against the fixable map using original_name (preserves casing).
                    // Fall back to the lowercased key if not found.
                    let fix_version = fixable
                        .get(&occ.original_name)
                        .or_else(|| fixable.get(name_lower.as_str()));
                    let Some(new_version) = fix_version else {
                        continue;
                    };

                    let entry = edits_by_file
                        .entry(occ.file_path.clone())
                        .or_insert_with(|| (occ.file_type, Vec::new()));
                    entry.1.push((
                        occ.original_name.clone(),
                        occ.version.clone(),
                        new_version.clone(),
                        occ.line_number,
                    ));
                }
            }

            // Mutations are opt-in; without --apply this is a dry-run preview.
            let effective_dry_run = cli.is_effective_dry_run();

            let mut total_fixed: usize = 0;
            let mut fix_errors: Vec<String> = Vec::new();

            for (path, (file_type, edits)) in &edits_by_file {
                if text_mode && !cli.quiet {
                    for (name, old_ver, new_ver, line_num) in edits {
                        let location = match line_num {
                            Some(n) => format!("{}:{}:", path.display(), n),
                            None => format!("{}:", path.display()),
                        };
                        if effective_dry_run {
                            println!(
                                "{} {} {} {} → {} {}",
                                location.blue().underline(),
                                "Would fix".yellow(),
                                name.bold(),
                                old_ver.dimmed(),
                                new_ver.yellow(),
                                "(security fix)".dimmed(),
                            );
                        } else {
                            println!(
                                "{} {} {} {} → {} {}",
                                location.blue().underline(),
                                "Fixed".green(),
                                name.bold(),
                                old_ver.dimmed(),
                                new_ver.green(),
                                "(security fix)".dimmed(),
                            );
                        }
                    }
                }

                if !effective_dry_run {
                    let updates: Vec<VersionEdit<'_>> = edits
                        .iter()
                        .map(|(name, old_ver, new_ver, line_num)| VersionEdit {
                            package: name.as_str(),
                            old_version: old_ver.as_str(),
                            new_version: new_ver.as_str(),
                            line_num: *line_num,
                        })
                        .collect();

                    match read_file_safe(path) {
                        Ok(content) => {
                            match apply_version_updates(
                                &content,
                                &updates,
                                *file_type,
                                cli.full_precision,
                            ) {
                                Ok(applied) => {
                                    if applied.content != content {
                                        if let Err(e) = write_file_atomic(path, &applied.content) {
                                            let msg =
                                                format!("Error writing {}: {}", path.display(), e);
                                            eprintln!("{}", msg.red());
                                            fix_errors.push(msg);
                                        } else {
                                            total_fixed += applied.applied_count();
                                        }
                                    }
                                }
                                Err(e) => {
                                    let msg = format!(
                                        "Error applying fixes to {}: {}",
                                        path.display(),
                                        e
                                    );
                                    eprintln!("{}", msg.red());
                                    fix_errors.push(msg);
                                }
                            }
                        }
                        Err(e) => {
                            let msg = format!("Error reading {}: {}", path.display(), e);
                            eprintln!("{}", msg.red());
                            fix_errors.push(msg);
                        }
                    }
                } else {
                    total_fixed += edits.len();
                }
            }

            if text_mode && !cli.quiet {
                if effective_dry_run {
                    if total_fixed > 0 {
                        println!(
                            "\n{} Would fix {} vulnerable package occurrence(s). Run with --apply to write changes.",
                            "→".yellow(),
                            total_fixed.to_string().yellow().bold()
                        );
                    }
                } else {
                    println!(
                        "\n{} Fixed {} vulnerable package occurrence(s)",
                        "✓".green(),
                        total_fixed.to_string().green().bold()
                    );
                }
            }

            // Exit-code contract for --fix-audit:
            // - errors during fix → 2
            // - dry-run with pending fixes and !no_fail → 1
            // - applied successfully (or no_fail) → 0
            let fix_exit_code = if !fix_errors.is_empty() {
                2
            } else if effective_dry_run && total_fixed > 0 && !no_fail {
                1
            } else {
                0
            };

            if fix_exit_code != 0 {
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

fn emit_audit_json(audit: &AuditResult, status: &'static str) -> Result<()> {
    use upd::output::build_audit_report;
    let report = build_audit_report(audit, 0, status);
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
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

/// Express `path` as a relative URI under `cwd` when possible.
///
/// Tries a plain `strip_prefix` against the invocation cwd first, then falls
/// back to the canonicalized forms of both sides (macOS symlinks `/var` to
/// `/private/var`, so a pass without canonicalization can miss).
fn relativize_for_sarif(
    path: &std::path::Path,
    cwd: Option<&std::path::Path>,
    cwd_canonical: Option<&std::path::Path>,
) -> String {
    if let Some(cwd) = cwd
        && let Ok(rel) = path.strip_prefix(cwd)
    {
        return rel.display().to_string();
    }
    if let Some(cwd_canon) = cwd_canonical
        && let Ok(canon_path) = path.canonicalize()
        && let Ok(rel) = canon_path.strip_prefix(cwd_canon)
    {
        return rel.display().to_string();
    }
    path.display().to_string()
}

/// Build the occurrence map required by SARIF output from the scanned package data.
///
/// Returns a map keyed by `(package_name, version, ecosystem_str)` with values
/// being the list of `(file_path, line_number)` where that pin appears.
fn build_sarif_occurrences(
    packages: &HashMap<(String, Lang), Vec<PackageOccurrence>>,
) -> upd::output::SarifOccurrenceMap {
    let mut map: upd::output::SarifOccurrenceMap = HashMap::new();

    for ((_, lang), occurrences) in packages {
        // Only ecosystems that OSV covers and that will appear in the audit result.
        if *lang == Lang::Actions
            || *lang == Lang::PreCommit
            || *lang == Lang::Mise
            || *lang == Lang::Terraform
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
            Lang::Actions | Lang::PreCommit | Lang::Mise | Lang::Terraform => {
                unreachable!("filtered above")
            }
        };

        let cwd = std::env::current_dir().ok();
        let cwd_canonical = cwd.as_ref().and_then(|c| c.canonicalize().ok());
        for occ in occurrences {
            let key = (
                occ.original_name.clone(),
                occ.version.clone(),
                ecosystem.as_str().to_string(),
            );
            let uri =
                relativize_for_sarif(&occ.file_path, cwd.as_deref(), cwd_canonical.as_deref());
            map.entry(key).or_default().push((uri, occ.line_number));
        }
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

            println!(
                "    {} {} {} {}",
                "├──".dimmed(),
                vuln.id.yellow(),
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
        Lang::Actions => " (actions)",
        Lang::PreCommit => " (pre-commit)",
        Lang::Mise => " (mise)",
        Lang::Terraform => " (terraform)",
    };

    println!(
        "  {}{}",
        alignment.package_name.bold(),
        lang_indicator.dimmed()
    );
    println!("    → {} (highest)", alignment.highest_version.green());

    for occurrence in &alignment.occurrences {
        let location = match occurrence.line_number {
            Some(n) => format!("{}:{}", occurrence.file_path.display(), n),
            None => occurrence.file_path.display().to_string(),
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
                });
        }
    }

    let mut total_updated = 0;

    for (path, (file_type, updates)) in updates_by_file {
        let content = read_file_safe(path)?;
        let applied_updates = apply_version_updates(&content, &updates, file_type, full_precision)
            .map_err(|e| anyhow::anyhow!("Failed to rewrite {}: {}", path.display(), e))?;

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

#[derive(Debug)]
struct TextDocument {
    lines: Vec<String>,
    line_ending: &'static str,
    has_trailing_newline: bool,
}

impl TextDocument {
    fn from_content(content: &str) -> Self {
        let line_ending = if content.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let has_trailing_newline = content.ends_with('\n');
        let body = if has_trailing_newline {
            content
                .strip_suffix("\r\n")
                .or_else(|| content.strip_suffix('\n'))
                .unwrap_or(content)
        } else {
            content
        };

        let lines = if body.is_empty() {
            Vec::new()
        } else {
            body.split(line_ending).map(str::to_string).collect()
        };

        Self {
            lines,
            line_ending,
            has_trailing_newline,
        }
    }

    fn into_content(self) -> String {
        let mut content = self.lines.join(self.line_ending);
        if self.has_trailing_newline {
            content.push_str(self.line_ending);
        }
        content
    }
}

fn apply_version_updates(
    content: &str,
    updates: &[VersionEdit<'_>],
    file_type: FileType,
    full_precision: bool,
) -> Result<AppliedVersionUpdates> {
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
            FileType::Csproj => apply_csproj_version(&mut document, update, &target_version),
            FileType::TerraformTf => {
                apply_terraform_version(&mut document, update, &target_version)
            }
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
    let pattern = format!(
        r#"(gem\s+['"]{}['"]\s*,\s*['"](?:~>\s*|>=\s*|<=\s*|>\s*|<\s*|=\s*|!=\s*)?){}(['"])"#,
        regex::escape(update.package),
        regex::escape(update.old_version)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    let replacement = format!("${{1}}{}${{2}}", target_version);

    apply_line_replacement(document, update.line_num, |line| {
        replace_first_match(line, &re, &replacement)
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
    let pattern = format!(r#"(^\s*rev:\s*['"]?){}"#, regex::escape(update.old_version));
    let re = regex::Regex::new(&pattern).unwrap();
    let replacement = format!("${{1}}{}", target_version);

    apply_line_replacement(document, update.line_num, |line| {
        replace_first_match(line, &re, &replacement)
    })
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
    let pattern = format!(
        r#"(^\s*version\s*=\s*"(?:~>\s*|>=\s*|<=\s*|>\s*|<\s*|=\s*|!=\s*)?){}(")"#,
        regex::escape(update.old_version)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    let replacement = format!(r#"${{1}}{}${{2}}"#, target_version);

    apply_line_replacement(document, update.line_num, |line| {
        replace_first_match(line, &re, &replacement)
    })
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
}

/// Counts updates by type, respecting the filter.
/// Returns (major_count, minor_count, patch_count, filtered_total)
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
    {
        return;
    }

    let action = if dry_run { "Would update" } else { "Updated" };

    for (package, old, new, line_num) in &result.updated {
        let update_type = classify_update(old, new);

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

        println!(
            "{} {} {} {} → {}{}",
            location.blue().underline(),
            action.green(),
            package.bold(),
            old.dimmed(),
            new.green(),
            type_indicator
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

    // Cooldown-related lines share a per-file location and ecosystem-derived
    // cooldown duration; compute both once.
    if !result.held_back.is_empty() || !result.skipped_by_cooldown.is_empty() {
        let file_location = format!("{}:", path);
        let cooldown = cooldown_policy
            .map(|p| p.effective_for(ecosystem_for_file_type(file_type)))
            .unwrap_or_else(Duration::zero);
        let now = Utc::now();

        for (package, old, chosen, skipped_latest, skipped_pub_at) in &result.held_back {
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

        for (package, _current, skipped_latest, skipped_pub_at) in &result.skipped_by_cooldown {
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

fn print_summary(
    result: &UpdateResult,
    file_count: usize,
    dry_run: bool,
    filter: UpdateFilter,
) -> usize {
    let action = if dry_run { "Would update" } else { "Updated" };

    // Count by update type, respecting filter
    let (major_count, minor_count, patch_count, filtered_total) =
        count_updates_by_type(&result.updated, filter);

    let pinned_count = result.pinned.len();
    let ignored_count = result.ignored.len();
    let held_back_count = result.held_back.len();
    let skipped_cooldown_count = result.skipped_by_cooldown.len();

    if filtered_total == 0
        && pinned_count == 0
        && held_back_count == 0
        && skipped_cooldown_count == 0
    {
        println!(
            "{} Scanned {} file(s), all dependencies up to date",
            "✓".green(),
            file_count
        );
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

    filtered_total
}

fn clean_cache() -> Result<()> {
    Cache::clean()?;
    println!("{}", "Cache cleaned successfully.".green());
    Ok(())
}

async fn self_update() -> Result<()> {
    println!("Checking for updates...");

    let client = reqwest::Client::new();
    let response = client
        .get("https://api.github.com/repos/rvben/upd/releases/latest")
        .header("User-Agent", "upd")
        .send()
        .await?;

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

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("1"), Some((1, 0, 0)));
        assert_eq!(parse_version("10.20.30"), Some((10, 20, 30)));
    }

    #[test]
    fn test_parse_version_invalid() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("a.b.c"), None);
    }

    #[test]
    fn test_classify_update_major() {
        assert_eq!(classify_update("1.0.0", "2.0.0"), UpdateType::Major);
        assert_eq!(classify_update("1.5.3", "2.0.0"), UpdateType::Major);
        assert_eq!(classify_update("0.9.0", "1.0.0"), UpdateType::Major);
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
        }];

        let applied = apply_version_updates(content, &updates, FileType::PyProject, false).unwrap();

        assert_eq!(applied.applied_count(), 1);
        assert_eq!(
            applied.content,
            "[project]\ndependencies = [\"django>=3.10,<4\"]\n"
        );
    }

    #[test]
    fn test_apply_version_updates_uses_unique_fallback_when_target_line_does_not_match() {
        let content = "[project]\ndependencies = [\"django>=3.2,<4\"]\n";
        let updates = [VersionEdit {
            package: "django",
            old_version: "3.2",
            new_version: "3.10.0",
            line_num: Some(1),
        }];

        let applied = apply_version_updates(content, &updates, FileType::PyProject, false).unwrap();

        assert_eq!(applied.applied_count(), 1);
        assert_eq!(
            applied.content,
            "[project]\ndependencies = [\"django>=3.10,<4\"]\n"
        );
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
            },
            VersionEdit {
                package: "react",
                old_version: "18.1.0",
                new_version: "19.0.0",
                line_num: Some(3),
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
            },
            VersionEdit {
                package: "serde",
                old_version: "1.0.1",
                new_version: "1.0.2",
                line_num: Some(6),
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

        let audit_pkgs = build_audit_packages(&packages);

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

        let audit_pkgs = build_audit_packages(&packages);

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
            pub_at,
            Duration::days(7),
            fixed_now(),
        );
        assert!(line.contains("Skipped"), "line: {line}");
        assert!(line.contains("express"), "line: {line}");
        assert!(line.contains("4.19.0"), "line: {line}");
        assert!(line.contains("6h ago"), "line: {line}");
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
                Utc::now(),
            )],
            ..Default::default()
        };
        assert!(
            !has_checkable_manifest_changes(&result, UpdateFilter::from_cli(&[], None)),
            "skipped_by_cooldown entries are steady state and must not count as pending changes"
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
}
