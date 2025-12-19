use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use futures::stream::{self, StreamExt};

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use upd::align::{PackageAlignment, find_alignments, scan_packages};
use upd::audit::{Ecosystem, OsvClient, Package as AuditPackage};
use upd::cache::{Cache, CachedRegistry};
use upd::cli::{Cli, Command};
use upd::config::UpdConfig;
use upd::interactive::{PendingUpdate, prompt_all};
use upd::lockfile::regenerate_lockfiles;
use upd::registry::{
    CratesIoRegistry, GoProxyRegistry, MultiPyPiRegistry, NpmRegistry, PyPiRegistry,
};
use upd::updater::{
    CargoTomlUpdater, FileType, GoModUpdater, Lang, PackageJsonUpdater, PyProjectUpdater,
    RequirementsUpdater, UpdateOptions, UpdateResult, Updater, discover_files, read_file_safe,
    write_file_atomic,
};
use upd::version::match_version_precision;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateType {
    Major,
    Minor,
    Patch,
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle no-color flag
    if cli.no_color {
        colored::control::set_override(false);
    }

    match &cli.command {
        Some(Command::Version) => {
            println!("upd version {}", VERSION);
        }
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
    let paths = cli.get_paths();
    let files = discover_files(&paths, &cli.langs);
    let file_count = files.len();

    if files.is_empty() {
        println!("{}", "No dependency files found.".yellow());
        return Ok(());
    }

    // Load configuration from file or auto-discover
    let config: Option<Arc<UpdConfig>> = if let Some(config_path) = &cli.config {
        // Load from specified path - show detailed error if it fails
        match UpdConfig::load_from_path_with_error(config_path) {
            Ok(config) => {
                if cli.verbose {
                    println!(
                        "{}",
                        format!("Using config from: {}", config_path.display()).cyan()
                    );
                }
                Some(Arc::new(config))
            }
            Err(error) => {
                eprintln!("{}", format!("Error: {}", error).red());
                None
            }
        }
    } else {
        // Auto-discover from current directory
        let start_dir = paths
            .first()
            .map(|p| {
                if p.is_file() {
                    p.parent().unwrap_or(p.as_path())
                } else {
                    p.as_path()
                }
            })
            .unwrap_or(std::path::Path::new("."));

        if let Some((config, path)) = UpdConfig::discover(start_dir) {
            if cli.verbose && config.has_config() {
                println!(
                    "{}",
                    format!("Using config from: {}", path.display()).cyan()
                );
                if !config.ignore.is_empty() {
                    println!(
                        "{}",
                        format!("  Ignoring {} package(s)", config.ignore.len()).dimmed()
                    );
                }
                if !config.pin.is_empty() {
                    println!(
                        "{}",
                        format!("  Pinning {} package(s)", config.pin.len()).dimmed()
                    );
                }
            }
            Some(Arc::new(config))
        } else {
            None
        }
    };

    if cli.verbose {
        println!(
            "{}",
            format!("Found {} dependency file(s)", file_count).cyan()
        );
    }

    // Create filter from CLI flags
    let filter = UpdateFilter::new(cli.major, cli.minor, cli.patch);

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
        if cli.verbose && credentials.is_some() {
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

    // Create updaters wrapped in Arc for parallel processing
    let requirements_updater = Arc::new(RequirementsUpdater::new());
    let pyproject_updater = Arc::new(PyProjectUpdater::new());
    let package_json_updater = Arc::new(PackageJsonUpdater::new());
    let cargo_toml_updater = Arc::new(CargoTomlUpdater::new());
    let go_mod_updater = Arc::new(GoModUpdater::new());

    // Wrap registries in Arc for parallel processing
    let pypi = Arc::new(pypi);
    let npm = Arc::new(npm);
    let crates_io = Arc::new(crates_io);
    let go_proxy = Arc::new(go_proxy);

    // Interactive mode: first discover updates, then prompt, then apply approved ones
    if cli.interactive {
        return run_interactive_update(
            cli,
            &files,
            filter,
            &config,
            &pypi,
            &npm,
            &crates_io,
            &go_proxy,
            &requirements_updater,
            &pyproject_updater,
            &package_json_updater,
            &cargo_toml_updater,
            &go_mod_updater,
            &cache,
            cache_enabled,
        )
        .await;
    }

    // Non-interactive mode: process files in parallel
    // Create update options (--check implies --dry-run)
    let dry_run = cli.dry_run || cli.check;
    let update_options = {
        let mut opts = UpdateOptions::new(dry_run, cli.full_precision);
        if let Some(ref cfg) = config {
            opts = opts.with_config(Arc::clone(cfg));
        }
        opts
    };

    let verbose = cli.verbose;

    // Process files in parallel with a concurrency limit
    let concurrency_limit = 8; // Process up to 8 files concurrently

    let results: Vec<(PathBuf, Result<UpdateResult, String>)> = stream::iter(files)
        .map(|(path, file_type)| {
            let pypi = Arc::clone(&pypi);
            let npm = Arc::clone(&npm);
            let crates_io = Arc::clone(&crates_io);
            let go_proxy = Arc::clone(&go_proxy);
            let requirements_updater = Arc::clone(&requirements_updater);
            let pyproject_updater = Arc::clone(&pyproject_updater);
            let package_json_updater = Arc::clone(&package_json_updater);
            let cargo_toml_updater = Arc::clone(&cargo_toml_updater);
            let go_mod_updater = Arc::clone(&go_mod_updater);
            let update_options = update_options.clone();

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
                };
                (path, result.map_err(|e| e.to_string()))
            }
        })
        .buffer_unordered(concurrency_limit)
        .collect()
        .await;

    // Process results in order and merge
    let mut total_result = UpdateResult::default();
    let mut updated_files: Vec<PathBuf> = Vec::new();

    for (path, result) in results {
        if verbose {
            println!("{}", format!("Processed: {}", path.display()).cyan());
        }

        match result {
            Ok(file_result) => {
                // Track files that were actually updated (not just checked)
                if !dry_run && !file_result.updated.is_empty() {
                    updated_files.push(path.clone());
                }
                print_file_result(
                    &path.display().to_string(),
                    &file_result,
                    dry_run,
                    filter,
                    verbose,
                );
                total_result.merge(file_result);
            }
            Err(e) => {
                eprintln!(
                    "{}",
                    format!("Error processing {}: {}", path.display(), e).red()
                );
            }
        }
    }

    // Regenerate lockfiles if requested and files were updated
    if cli.lock && !dry_run && !updated_files.is_empty() {
        println!();
        println!("{}", "Regenerating lockfiles...".cyan());

        // Deduplicate directories (multiple files in same dir should only regenerate once)
        let mut processed_dirs: HashSet<PathBuf> = HashSet::new();

        for path in &updated_files {
            if let Some(dir) = path.parent() {
                let dir_path = dir.to_path_buf();
                if processed_dirs.insert(dir_path) {
                    let results = regenerate_lockfiles(path, verbose);
                    for result in results {
                        if let Err(e) = result {
                            eprintln!("{}", format!("Warning: {}", e).yellow());
                        }
                    }
                }
            }
        }
    }

    // Save cache to disk
    if cache_enabled {
        let _ = Cache::save_shared(&cache);
    }

    // Print summary
    println!();
    print_summary(&total_result, file_count, dry_run, filter);

    // In check mode, exit with code 1 if any updates are available
    if cli.check {
        let (_, _, _, filtered_total) = count_updates_by_type(&total_result.updated, filter);
        if filtered_total > 0 {
            std::process::exit(1);
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_interactive_update(
    cli: &Cli,
    files: &[(std::path::PathBuf, FileType)],
    filter: UpdateFilter,
    config: &Option<Arc<UpdConfig>>,
    pypi: &Arc<CachedRegistry<MultiPyPiRegistry>>,
    npm: &Arc<CachedRegistry<NpmRegistry>>,
    crates_io: &Arc<CachedRegistry<CratesIoRegistry>>,
    go_proxy: &Arc<CachedRegistry<GoProxyRegistry>>,
    requirements_updater: &Arc<RequirementsUpdater>,
    pyproject_updater: &Arc<PyProjectUpdater>,
    package_json_updater: &Arc<PackageJsonUpdater>,
    cargo_toml_updater: &Arc<CargoTomlUpdater>,
    go_mod_updater: &Arc<GoModUpdater>,
    cache: &Arc<std::sync::Mutex<Cache>>,
    cache_enabled: bool,
) -> Result<()> {
    // Phase 1: Discover all available updates (dry-run)
    let dry_run_options = {
        let mut opts = UpdateOptions::new(true, cli.full_precision);
        if let Some(cfg) = config {
            opts = opts.with_config(Arc::clone(cfg));
        }
        opts
    };

    let mut pending_updates: Vec<PendingUpdate> = Vec::new();

    for (path, file_type) in files {
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
        };

        if let Ok(file_result) = result {
            for (package, old_version, new_version, line_num) in file_result.updated {
                let update_type = classify_update(&old_version, &new_version);

                // Apply filter
                if !filter.matches(update_type) {
                    continue;
                }

                pending_updates.push(PendingUpdate::new(
                    path.display().to_string(),
                    line_num,
                    package,
                    old_version,
                    new_version,
                    update_type == UpdateType::Major,
                ));
            }
        }
    }

    if pending_updates.is_empty() {
        println!(
            "{} Scanned {} file(s), all dependencies up to date",
            "✓".green(),
            files.len()
        );
        return Ok(());
    }

    // Phase 2: Prompt user for each update
    let updates_with_decisions = prompt_all(pending_updates)?;

    // Build set of approved packages per file
    let approved: HashSet<(String, String)> = updates_with_decisions
        .iter()
        .filter(|u| u.approved)
        .map(|u| (u.file.clone(), u.package.clone()))
        .collect();

    let approved_count = approved.len();

    if approved_count == 0 {
        println!("\n{}", "No updates applied.".yellow());
        return Ok(());
    }

    // Phase 3: Apply approved updates
    println!(
        "\n{}",
        format!("Applying {} update(s)...", approved_count).cyan()
    );

    let apply_options = {
        let mut opts = UpdateOptions::new(false, cli.full_precision);
        if let Some(cfg) = config {
            opts = opts.with_config(Arc::clone(cfg));
        }
        opts
    };

    let mut total_applied = 0;
    let mut updated_files: Vec<std::path::PathBuf> = Vec::new();

    for (path, file_type) in files {
        // Check if any approved updates are for this file
        let file_str = path.display().to_string();
        let has_approved = approved.iter().any(|(f, _)| f == &file_str);
        if !has_approved {
            continue;
        }

        let result = match file_type {
            FileType::Requirements => {
                requirements_updater
                    .update(path, pypi.as_ref(), apply_options.clone())
                    .await
            }
            FileType::PyProject => {
                pyproject_updater
                    .update(path, pypi.as_ref(), apply_options.clone())
                    .await
            }
            FileType::PackageJson => {
                package_json_updater
                    .update(path, npm.as_ref(), apply_options.clone())
                    .await
            }
            FileType::CargoToml => {
                cargo_toml_updater
                    .update(path, crates_io.as_ref(), apply_options.clone())
                    .await
            }
            FileType::GoMod => {
                go_mod_updater
                    .update(path, go_proxy.as_ref(), apply_options.clone())
                    .await
            }
        };

        if let Ok(file_result) = result {
            let mut file_had_updates = false;
            for (package, old_version, new_version, line_num) in &file_result.updated {
                // Only count if it was approved
                if approved.contains(&(file_str.clone(), package.clone())) {
                    total_applied += 1;
                    file_had_updates = true;
                    let location = match line_num {
                        Some(n) => format!("{}:{}:", file_str, n),
                        None => format!("{}:", file_str),
                    };
                    println!(
                        "{} {} {} {} → {}",
                        location.blue().underline(),
                        "Updated".green(),
                        package.bold(),
                        old_version.dimmed(),
                        new_version.green(),
                    );
                }
            }
            if file_had_updates {
                updated_files.push(path.clone());
            }
        }
    }

    // Regenerate lockfiles if requested and files were updated
    if cli.lock && !updated_files.is_empty() {
        println!();
        println!("{}", "Regenerating lockfiles...".cyan());

        let mut processed_dirs: HashSet<std::path::PathBuf> = HashSet::new();

        for path in &updated_files {
            if let Some(dir) = path.parent() {
                let dir_path = dir.to_path_buf();
                if processed_dirs.insert(dir_path) {
                    let results = regenerate_lockfiles(path, cli.verbose);
                    for result in results {
                        if let Err(e) = result {
                            eprintln!("{}", format!("Warning: {}", e).yellow());
                        }
                    }
                }
            }
        }
    }

    // Save cache to disk
    if cache_enabled {
        let _ = Cache::save_shared(cache);
    }

    println!(
        "\n{} {} package(s)",
        "Updated".green(),
        total_applied.to_string().green().bold()
    );

    Ok(())
}

async fn run_align(cli: &Cli) -> Result<()> {
    let paths = cli.get_paths();
    let files = discover_files(&paths, &cli.langs);
    let file_count = files.len();

    if files.is_empty() {
        println!("{}", "No dependency files found.".yellow());
        return Ok(());
    }

    if cli.verbose {
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

    if misaligned.is_empty() {
        println!(
            "{} Scanned {} file(s), all packages are aligned",
            "✓".green(),
            file_count
        );
        return Ok(());
    }

    // Display misalignments
    let dry_run = cli.dry_run || cli.check;
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

    // Apply alignments if not dry-run
    if !dry_run {
        let updated_count = apply_alignments(&misaligned, cli.full_precision)?;
        println!(
            "\n{} {} package occurrence(s)",
            "Aligned".green(),
            updated_count.to_string().green().bold()
        );
    } else {
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

async fn run_audit(cli: &Cli) -> Result<()> {
    let paths = cli.get_paths();
    let files = discover_files(&paths, &cli.langs);
    let file_count = files.len();

    if files.is_empty() {
        println!("{}", "No dependency files found.".yellow());
        return Ok(());
    }

    if cli.verbose {
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
    let mut audit_packages: Vec<AuditPackage> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();

    for ((name, lang), occurrences) in &packages {
        let ecosystem = match lang {
            Lang::Python => Ecosystem::PyPI,
            Lang::Node => Ecosystem::Npm,
            Lang::Rust => Ecosystem::CratesIo,
            Lang::Go => Ecosystem::Go,
        };

        for occurrence in occurrences {
            let key = (
                name.clone(),
                occurrence.version.clone(),
                ecosystem.as_str().to_string(),
            );
            if seen.insert(key) {
                audit_packages.push(AuditPackage {
                    name: name.clone(),
                    version: occurrence.version.clone(),
                    ecosystem,
                });
            }
        }
    }

    if audit_packages.is_empty() {
        println!(
            "{} Scanned {} file(s), no packages found",
            "✓".green(),
            file_count
        );
        return Ok(());
    }

    println!(
        "{}",
        format!(
            "Checking {} unique package(s) for vulnerabilities...",
            audit_packages.len()
        )
        .cyan()
    );

    // Query OSV API
    let osv_client = OsvClient::new();
    let audit_result = osv_client.check_packages(&audit_packages).await?;

    // Display results
    if audit_result.vulnerable.is_empty() {
        println!(
            "\n{} No vulnerabilities found in {} package(s)",
            "✓".green(),
            audit_packages.len()
        );
    } else {
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

        // Print summary
        println!(
            "{} {} vulnerable package(s), {} total vulnerability/ies",
            "Summary:".bold(),
            audit_result.vulnerable_packages().to_string().yellow(),
            audit_result.total_vulnerabilities().to_string().red()
        );
    }

    // Print errors if any
    for error in &audit_result.errors {
        eprintln!("{} {}", "Error:".red(), error);
    }

    // In check mode, exit with code 1 if any vulnerabilities found
    if cli.check && !audit_result.vulnerable.is_empty() {
        std::process::exit(1);
    }

    Ok(())
}

fn print_alignment(alignment: &PackageAlignment, _dry_run: bool) {
    let lang_indicator = match alignment.lang {
        Lang::Python => "",
        Lang::Node => " (npm)",
        Lang::Rust => " (cargo)",
        Lang::Go => " (go)",
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

    // Group updates by file path
    let mut updates_by_file: HashMap<&std::path::Path, Vec<(&str, &str, &str)>> = HashMap::new();

    for alignment in alignments {
        for occurrence in alignment.misaligned_occurrences() {
            updates_by_file
                .entry(occurrence.file_path.as_path())
                .or_default()
                .push((
                    &alignment.package_name,
                    &occurrence.version,
                    &alignment.highest_version,
                ));
        }
    }

    let mut total_updated = 0;

    for (path, updates) in updates_by_file {
        let content = read_file_safe(path)?;
        let file_type = FileType::detect(path).unwrap();

        let new_content = apply_version_updates(&content, &updates, file_type, full_precision);

        if new_content != content {
            write_file_atomic(path, &new_content)?;
            total_updated += updates.len();
        }
    }

    Ok(total_updated)
}

fn apply_version_updates(
    content: &str,
    updates: &[(&str, &str, &str)],
    file_type: FileType,
    full_precision: bool,
) -> String {
    let mut result = content.to_string();

    for (package, old_version, new_version) in updates {
        let target_version = if full_precision {
            (*new_version).to_string()
        } else {
            match_version_precision(old_version, new_version)
        };

        result = match file_type {
            FileType::Requirements => {
                replace_requirements_version(&result, package, old_version, &target_version)
            }
            FileType::PyProject => {
                replace_pyproject_version(&result, package, old_version, &target_version)
            }
            FileType::PackageJson => {
                replace_package_json_version(&result, package, old_version, &target_version)
            }
            FileType::CargoToml => {
                replace_cargo_toml_version(&result, package, old_version, &target_version)
            }
            FileType::GoMod => {
                replace_go_mod_version(&result, package, old_version, &target_version)
            }
        };
    }

    result
}

fn replace_requirements_version(content: &str, package: &str, old: &str, new: &str) -> String {
    // Pattern: package_name followed by version specifier and old version
    let pattern = format!(
        r"(?m)^({}(?:\[[^\]]*\])?\s*(?:==|>=|~=))\s*{}",
        regex::escape(package),
        regex::escape(old)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    re.replace_all(content, format!("${{1}}{}", new))
        .to_string()
}

fn replace_pyproject_version(content: &str, package: &str, old: &str, new: &str) -> String {
    // For pyproject.toml, we need to handle both PEP 621 array format and Poetry table format
    let mut result = content.to_string();

    // PEP 621: "package>=old" -> "package>=new"
    let pattern = format!(
        r#""({}(?:\[[^\]]*\])?\s*(?:==|>=|~=|<=|!=|>|<)\s*){}""#,
        regex::escape(package),
        regex::escape(old)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    result = re
        .replace_all(&result, format!(r#""${{1}}{}""#, new))
        .to_string();

    // Poetry: version = "^old" or "old"
    // This is trickier - need to preserve the constraint operator
    let pattern = format!(
        r#"(\[tool\.poetry(?:\.[^\]]+)?\](?:[^\[]*?{}\s*=\s*(?:\{{[^}}]*version\s*=\s*)?")[~^>=<]*){}""#,
        regex::escape(package),
        regex::escape(old)
    );
    if let Ok(re) = regex::Regex::new(&pattern) {
        result = re
            .replace_all(&result, format!(r#"${{1}}{}""#, new))
            .to_string();
    }

    result
}

fn replace_package_json_version(content: &str, package: &str, old: &str, new: &str) -> String {
    // Pattern: "package": "^old" or "~old" etc.
    let pattern = format!(
        r#"("{}"\s*:\s*"[\^~>=<]*){}""#,
        regex::escape(package),
        regex::escape(old)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    re.replace_all(content, format!(r#"${{1}}{}""#, new))
        .to_string()
}

fn replace_cargo_toml_version(content: &str, package: &str, old: &str, new: &str) -> String {
    let mut result = content.to_string();

    // Simple format: package = "version"
    let pattern = format!(
        r#"({}\s*=\s*")[~^>=<]*{}""#,
        regex::escape(package),
        regex::escape(old)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    result = re
        .replace_all(&result, format!(r#"${{1}}{}""#, new))
        .to_string();

    // Table format: package = { version = "old" }
    let pattern = format!(
        r#"({}\s*=\s*\{{[^}}]*version\s*=\s*")[~^>=<]*{}""#,
        regex::escape(package),
        regex::escape(old)
    );
    if let Ok(re) = regex::Regex::new(&pattern) {
        result = re
            .replace_all(&result, format!(r#"${{1}}{}""#, new))
            .to_string();
    }

    result
}

fn replace_go_mod_version(content: &str, package: &str, old: &str, new: &str) -> String {
    // Pattern: module/path vOLD
    let pattern = format!(
        r"({}\s+){}(\s|$)",
        regex::escape(package),
        regex::escape(old)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    re.replace_all(content, format!("${{1}}{}${{2}}", new))
        .to_string()
}

/// Filter configuration for update types
#[derive(Clone, Copy)]
struct UpdateFilter {
    major: bool,
    minor: bool,
    patch: bool,
}

impl UpdateFilter {
    fn new(major: bool, minor: bool, patch: bool) -> Self {
        // If no filter specified, show all
        if !major && !minor && !patch {
            Self {
                major: true,
                minor: true,
                patch: true,
            }
        } else {
            Self {
                major,
                minor,
                patch,
            }
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
    result: &UpdateResult,
    dry_run: bool,
    filter: UpdateFilter,
    verbose: bool,
) {
    if result.updated.is_empty()
        && result.pinned.is_empty()
        && result.ignored.is_empty()
        && result.errors.is_empty()
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
        println!(
            "{} {} {}",
            location.blue().underline(),
            "Error:".red(),
            error
        );
    }
}

fn print_summary(result: &UpdateResult, file_count: usize, dry_run: bool, filter: UpdateFilter) {
    let action = if dry_run { "Would update" } else { "Updated" };

    // Count by update type, respecting filter
    let (major_count, minor_count, patch_count, filtered_total) =
        count_updates_by_type(&result.updated, filter);

    let pinned_count = result.pinned.len();
    let ignored_count = result.ignored.len();

    if filtered_total == 0 && pinned_count == 0 {
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
        println!(
            "{} error(s) occurred",
            result.errors.len().to_string().red().bold()
        );
    }
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
        let filter = UpdateFilter::new(false, false, false);
        assert!(filter.major);
        assert!(filter.minor);
        assert!(filter.patch);
    }

    #[test]
    fn test_update_filter_major_only() {
        let filter = UpdateFilter::new(true, false, false);
        assert!(filter.major);
        assert!(!filter.minor);
        assert!(!filter.patch);
    }

    #[test]
    fn test_update_filter_minor_only() {
        let filter = UpdateFilter::new(false, true, false);
        assert!(!filter.major);
        assert!(filter.minor);
        assert!(!filter.patch);
    }

    #[test]
    fn test_update_filter_patch_only() {
        let filter = UpdateFilter::new(false, false, true);
        assert!(!filter.major);
        assert!(!filter.minor);
        assert!(filter.patch);
    }

    #[test]
    fn test_update_filter_combined() {
        let filter = UpdateFilter::new(true, true, false);
        assert!(filter.major);
        assert!(filter.minor);
        assert!(!filter.patch);
    }

    #[test]
    fn test_update_filter_matches() {
        let filter = UpdateFilter::new(true, false, false);
        assert!(filter.matches(UpdateType::Major));
        assert!(!filter.matches(UpdateType::Minor));
        assert!(!filter.matches(UpdateType::Patch));

        let filter = UpdateFilter::new(false, true, true);
        assert!(!filter.matches(UpdateType::Major));
        assert!(filter.matches(UpdateType::Minor));
        assert!(filter.matches(UpdateType::Patch));
    }

    #[test]
    fn test_count_updates_by_type_empty() {
        let updates: Vec<(String, String, String, Option<usize>)> = vec![];
        let filter = UpdateFilter::new(false, false, false); // show all

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
        let filter = UpdateFilter::new(false, false, false); // show all

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
        let filter = UpdateFilter::new(true, false, false); // major only

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
        let filter = UpdateFilter::new(false, true, true); // minor + patch

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
        let filter = UpdateFilter::new(false, false, false); // show all

        let (major, minor, patch, total) = count_updates_by_type(&updates, filter);
        assert_eq!(major, 1);
        assert_eq!(minor, 1);
        assert_eq!(patch, 0);
        assert_eq!(total, 2);
    }
}
