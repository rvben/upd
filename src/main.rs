use anyhow::Result;
use clap::Parser;
use colored::Colorize;

use std::sync::Arc;
use upd::cache::{Cache, CachedRegistry};
use upd::cli::{Cli, Command};
use upd::registry::{CratesIoRegistry, GoProxyRegistry, NpmRegistry, PyPiRegistry};
use upd::updater::{
    CargoTomlUpdater, FileType, GoModUpdater, PackageJsonUpdater, PyProjectUpdater,
    RequirementsUpdater, UpdateOptions, UpdateResult, Updater, discover_files,
};

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

    let pypi = CachedRegistry::new(PyPiRegistry::new(), Arc::clone(&cache), cache_enabled);
    let npm = CachedRegistry::new(NpmRegistry::new(), Arc::clone(&cache), cache_enabled);
    let crates_io = CachedRegistry::new(CratesIoRegistry::new(), Arc::clone(&cache), cache_enabled);
    let go_proxy = CachedRegistry::new(GoProxyRegistry::new(), Arc::clone(&cache), cache_enabled);

    // Create updaters
    let requirements_updater = RequirementsUpdater::new();
    let pyproject_updater = PyProjectUpdater::new();
    let package_json_updater = PackageJsonUpdater::new();
    let cargo_toml_updater = CargoTomlUpdater::new();
    let go_mod_updater = GoModUpdater::new();

    // Create update options
    let update_options = UpdateOptions {
        dry_run: cli.dry_run,
        full_precision: cli.full_precision,
    };

    let mut total_result = UpdateResult::default();

    for (path, file_type) in files {
        if cli.verbose {
            println!("{}", format!("Processing: {}", path.display()).cyan());
        }

        let result = match file_type {
            FileType::Requirements => {
                requirements_updater
                    .update(&path, &pypi, update_options)
                    .await
            }
            FileType::PyProject => pyproject_updater.update(&path, &pypi, update_options).await,
            FileType::PackageJson => {
                package_json_updater
                    .update(&path, &npm, update_options)
                    .await
            }
            FileType::CargoToml => {
                cargo_toml_updater
                    .update(&path, &crates_io, update_options)
                    .await
            }
            FileType::GoMod => {
                go_mod_updater
                    .update(&path, &go_proxy, update_options)
                    .await
            }
        };

        match result {
            Ok(file_result) => {
                print_file_result(
                    &path.display().to_string(),
                    &file_result,
                    cli.dry_run,
                    filter,
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

    // Save cache to disk
    if cache_enabled {
        let _ = Cache::save_shared(&cache);
    }

    // Print summary
    println!();
    print_summary(&total_result, file_count, cli.dry_run, filter);

    Ok(())
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

fn print_file_result(path: &str, result: &UpdateResult, dry_run: bool, filter: UpdateFilter) {
    if result.updated.is_empty() && result.errors.is_empty() {
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

    if filtered_total == 0 {
        println!(
            "{} Scanned {} file(s), all dependencies up to date",
            "✓".green(),
            file_count
        );
    } else {
        // Build breakdown string
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

        println!(
            "{} {} package(s){} in {} file(s), {} up to date",
            action,
            filtered_total.to_string().green().bold(),
            breakdown,
            file_count,
            result.unchanged
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
