use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "upd")]
#[command(
    author,
    version,
    about = "A fast dependency updater for Python and Node.js projects"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Paths to update (files or directories)
    #[arg(global = true)]
    pub paths: Vec<PathBuf>,

    /// Show what would change without writing
    #[arg(short = 'n', long, global = true)]
    pub dry_run: bool,

    /// Disable version caching
    #[arg(long, global = true)]
    pub no_cache: bool,

    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Only show/apply major updates
    #[arg(long, global = true)]
    pub major: bool,

    /// Only show/apply minor updates
    #[arg(long, global = true)]
    pub minor: bool,

    /// Only show/apply patch updates
    #[arg(long, global = true)]
    pub patch: bool,

    /// Use full version precision (e.g., 3.1.5 instead of 3.1)
    #[arg(long, global = true)]
    pub full_precision: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Update dependencies (default when no command specified)
    Update {
        /// Paths to update
        #[arg()]
        paths: Vec<PathBuf>,
    },

    /// Show version information
    Version,

    /// Clear the version cache
    CleanCache,

    /// Update upd itself
    SelfUpdate,
}

impl Cli {
    pub fn get_paths(&self) -> Vec<PathBuf> {
        match &self.command {
            Some(Command::Update { paths }) if !paths.is_empty() => paths.clone(),
            _ if !self.paths.is_empty() => self.paths.clone(),
            _ => vec![PathBuf::from(".")],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_cli_parses_default() {
        let cli = Cli::try_parse_from(["upd"]).unwrap();
        assert!(!cli.dry_run);
        assert!(!cli.no_cache);
        assert!(!cli.verbose);
        assert!(!cli.major);
        assert!(!cli.minor);
        assert!(!cli.patch);
        assert!(!cli.full_precision);
        assert!(cli.paths.is_empty());
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_cli_parses_dry_run() {
        let cli = Cli::try_parse_from(["upd", "-n"]).unwrap();
        assert!(cli.dry_run);

        let cli = Cli::try_parse_from(["upd", "--dry-run"]).unwrap();
        assert!(cli.dry_run);
    }

    #[test]
    fn test_cli_parses_no_cache() {
        let cli = Cli::try_parse_from(["upd", "--no-cache"]).unwrap();
        assert!(cli.no_cache);
    }

    #[test]
    fn test_cli_parses_verbose() {
        let cli = Cli::try_parse_from(["upd", "-v"]).unwrap();
        assert!(cli.verbose);

        let cli = Cli::try_parse_from(["upd", "--verbose"]).unwrap();
        assert!(cli.verbose);
    }

    #[test]
    fn test_cli_parses_update_type_filters() {
        let cli = Cli::try_parse_from(["upd", "--major"]).unwrap();
        assert!(cli.major);
        assert!(!cli.minor);
        assert!(!cli.patch);

        let cli = Cli::try_parse_from(["upd", "--minor"]).unwrap();
        assert!(!cli.major);
        assert!(cli.minor);
        assert!(!cli.patch);

        let cli = Cli::try_parse_from(["upd", "--patch"]).unwrap();
        assert!(!cli.major);
        assert!(!cli.minor);
        assert!(cli.patch);
    }

    #[test]
    fn test_cli_parses_full_precision() {
        let cli = Cli::try_parse_from(["upd", "--full-precision"]).unwrap();
        assert!(cli.full_precision);
    }

    #[test]
    fn test_cli_parses_paths() {
        let cli = Cli::try_parse_from(["upd", "path1", "path2"]).unwrap();
        assert_eq!(cli.paths.len(), 2);
        assert_eq!(cli.paths[0], PathBuf::from("path1"));
        assert_eq!(cli.paths[1], PathBuf::from("path2"));
    }

    #[test]
    fn test_cli_parses_update_command() {
        let cli = Cli::try_parse_from(["upd", "update", "path1"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Update { .. })));
    }

    #[test]
    fn test_cli_parses_version_command() {
        let cli = Cli::try_parse_from(["upd", "version"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Version)));
    }

    #[test]
    fn test_cli_parses_clean_cache_command() {
        let cli = Cli::try_parse_from(["upd", "clean-cache"]).unwrap();
        assert!(matches!(cli.command, Some(Command::CleanCache)));
    }

    #[test]
    fn test_cli_parses_self_update_command() {
        let cli = Cli::try_parse_from(["upd", "self-update"]).unwrap();
        assert!(matches!(cli.command, Some(Command::SelfUpdate)));
    }

    #[test]
    fn test_get_paths_defaults_to_current_dir() {
        let cli = Cli::try_parse_from(["upd"]).unwrap();
        let paths = cli.get_paths();
        assert_eq!(paths, vec![PathBuf::from(".")]);
    }

    #[test]
    fn test_get_paths_uses_global_paths() {
        let cli = Cli::try_parse_from(["upd", "path1", "path2"]).unwrap();
        let paths = cli.get_paths();
        assert_eq!(paths, vec![PathBuf::from("path1"), PathBuf::from("path2")]);
    }

    #[test]
    fn test_get_paths_uses_update_command_paths() {
        let cli = Cli::try_parse_from(["upd", "update", "cmd_path"]).unwrap();
        let paths = cli.get_paths();
        assert_eq!(paths, vec![PathBuf::from("cmd_path")]);
    }

    #[test]
    fn test_cli_combined_options() {
        let cli =
            Cli::try_parse_from(["upd", "-n", "-v", "--no-cache", "--major", "path1"]).unwrap();
        assert!(cli.dry_run);
        assert!(cli.verbose);
        assert!(cli.no_cache);
        assert!(cli.major);
        assert_eq!(cli.paths, vec![PathBuf::from("path1")]);
    }
}
