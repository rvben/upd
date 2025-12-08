use anyhow::Result;
use clap::Parser;
use colored::Colorize;

use upd::cache::Cache;
use upd::cli::{Cli, Command};
use upd::registry::{NpmRegistry, PyPiRegistry};
use upd::updater::{
    FileType, PackageJsonUpdater, PyProjectUpdater, RequirementsUpdater, UpdateResult, Updater,
    discover_files,
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
    let files = discover_files(&paths);
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

    // Create registries
    let pypi = PyPiRegistry::new();
    let npm = NpmRegistry::new();

    // Create updaters
    let requirements_updater = RequirementsUpdater::new();
    let pyproject_updater = PyProjectUpdater::new();
    let package_json_updater = PackageJsonUpdater::new();

    // Load cache if enabled
    let mut cache = if !cli.no_cache {
        Cache::load().ok()
    } else {
        None
    };

    let mut total_result = UpdateResult::default();

    for (path, file_type) in files {
        if cli.verbose {
            println!("{}", format!("Processing: {}", path.display()).cyan());
        }

        let result = match file_type {
            FileType::Requirements => requirements_updater.update(&path, &pypi, cli.dry_run).await,
            FileType::PyProject => pyproject_updater.update(&path, &pypi, cli.dry_run).await,
            FileType::PackageJson => package_json_updater.update(&path, &npm, cli.dry_run).await,
        };

        match result {
            Ok(file_result) => {
                print_file_result(&path.display().to_string(), &file_result, cli.dry_run);
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

    // Save cache
    if let Some(ref mut cache) = cache {
        let _ = cache.save();
    }

    // Print summary
    println!();
    print_summary(&total_result, file_count, cli.dry_run);

    Ok(())
}

fn print_file_result(path: &str, result: &UpdateResult, dry_run: bool) {
    if result.updated.is_empty() && result.errors.is_empty() {
        return;
    }

    println!("{}", path.bold());

    let action = if dry_run { "Would update" } else { "Updated" };

    for (package, old, new) in &result.updated {
        let type_indicator = match classify_update(old, new) {
            UpdateType::Major => " (MAJOR)".yellow().bold().to_string(),
            UpdateType::Minor => String::new(),
            UpdateType::Patch => String::new(),
        };

        println!(
            "  {} {} {} → {}{}",
            action.green(),
            package.bold(),
            old.dimmed(),
            new.green(),
            type_indicator
        );
    }

    for error in &result.errors {
        println!("  {} {}", "Error:".red(), error);
    }
}

fn print_summary(result: &UpdateResult, file_count: usize, dry_run: bool) {
    let action = if dry_run { "Would update" } else { "Updated" };

    // Count by update type
    let (major_count, minor_count, patch_count) =
        result
            .updated
            .iter()
            .fold(
                (0, 0, 0),
                |(major, minor, patch), (_, old, new)| match classify_update(old, new) {
                    UpdateType::Major => (major + 1, minor, patch),
                    UpdateType::Minor => (major, minor + 1, patch),
                    UpdateType::Patch => (major, minor, patch + 1),
                },
            );

    if result.updated.is_empty() {
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
            result.updated.len().to_string().green().bold(),
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
