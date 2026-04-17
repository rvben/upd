use crate::updater::Lang;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Kind of version bump to include when filtering updates.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[value(rename_all = "lower")]
pub enum BumpLevel {
    Major,
    Minor,
    Patch,
}

#[derive(Parser)]
#[command(name = "upd")]
#[command(
    author,
    version,
    about = "A fast dependency updater for Python, Node.js, Rust, Go, Ruby, .NET, Terraform, GitHub Actions, pre-commit, and Mise/asdf projects"
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

    /// Interactive mode - approve each update individually
    #[arg(short, long, global = true)]
    pub interactive: bool,

    /// Include only these bump levels (repeatable). Omit to include all.
    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "LEVEL",
        value_delimiter = ','
    )]
    pub bump: Vec<BumpLevel>,

    /// Use full version precision (e.g., 3.1.5 instead of 3.1)
    #[arg(long, global = true)]
    pub full_precision: bool,

    /// Filter by language/ecosystem (can be specified multiple times)
    #[arg(short = 'l', long = "lang", value_name = "LANG", global = true)]
    pub langs: Vec<Lang>,

    /// Check mode: exit with code 1 if updates are available (implies --dry-run)
    #[arg(long, global = true)]
    pub check: bool,

    /// Regenerate lockfiles after updating (runs poetry lock, npm install, etc.)
    #[arg(long, global = true)]
    pub lock: bool,

    /// Path to config file (default: auto-discover .updrc.toml, upd.toml, or .updrc)
    #[arg(short = 'c', long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Update dependencies (default when no command specified)
    Update {
        /// Paths to update
        #[arg()]
        paths: Vec<PathBuf>,
    },

    /// Align all packages to the highest version found in the repository
    Align {
        /// Paths to scan and align
        #[arg()]
        paths: Vec<PathBuf>,
    },

    /// Check dependencies for known security vulnerabilities
    Audit {
        /// Paths to scan
        #[arg()]
        paths: Vec<PathBuf>,
    },

    /// Clear the version cache
    CleanCache,

    /// Update upd itself
    SelfUpdate,
}

impl Cli {
    pub fn get_paths(&self) -> Vec<PathBuf> {
        match &self.command {
            Some(Command::Update { paths }) if !paths.is_empty() => paths.clone(),
            Some(Command::Align { paths }) if !paths.is_empty() => paths.clone(),
            Some(Command::Audit { paths }) if !paths.is_empty() => paths.clone(),
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
        assert!(!cli.interactive);
        assert!(cli.bump.is_empty());
        assert!(!cli.full_precision);
        assert!(!cli.check);
        assert!(!cli.lock);
        assert!(cli.paths.is_empty());
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_cli_parses_lock() {
        let cli = Cli::try_parse_from(["upd", "--lock"]).unwrap();
        assert!(cli.lock);
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
    fn test_cli_parses_interactive() {
        let cli = Cli::try_parse_from(["upd", "-i"]).unwrap();
        assert!(cli.interactive);

        let cli = Cli::try_parse_from(["upd", "--interactive"]).unwrap();
        assert!(cli.interactive);
    }

    #[test]
    fn test_cli_parses_bump_single_level() {
        let cli = Cli::try_parse_from(["upd", "--bump", "major"]).unwrap();
        assert_eq!(cli.bump, vec![BumpLevel::Major]);

        let cli = Cli::try_parse_from(["upd", "--bump", "minor"]).unwrap();
        assert_eq!(cli.bump, vec![BumpLevel::Minor]);

        let cli = Cli::try_parse_from(["upd", "--bump", "patch"]).unwrap();
        assert_eq!(cli.bump, vec![BumpLevel::Patch]);
    }

    #[test]
    fn test_cli_parses_bump_repeatable() {
        let cli = Cli::try_parse_from(["upd", "--bump", "major", "--bump", "minor"]).unwrap();
        assert_eq!(cli.bump, vec![BumpLevel::Major, BumpLevel::Minor]);
    }

    #[test]
    fn test_cli_parses_bump_comma_separated() {
        let cli = Cli::try_parse_from(["upd", "--bump", "minor,patch"]).unwrap();
        assert_eq!(cli.bump, vec![BumpLevel::Minor, BumpLevel::Patch]);
    }

    #[test]
    fn test_cli_rejects_invalid_bump_level() {
        let rendered = match Cli::try_parse_from(["upd", "--bump", "breaking"]) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("expected invalid bump level to be rejected"),
        };
        assert!(
            rendered.contains("invalid value"),
            "unexpected error: {rendered}"
        );
    }

    #[test]
    fn test_cli_rejects_removed_boolean_flags() {
        for flag in ["--major", "--minor", "--patch"] {
            assert!(
                Cli::try_parse_from(["upd", flag]).is_err(),
                "expected {flag} to be rejected after consolidation into --bump"
            );
        }
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
    fn test_cli_version_subcommand_removed() {
        let cli = Cli::try_parse_from(["upd", "version"]).unwrap();
        assert!(
            cli.command.is_none(),
            "`version` should no longer be recognised as a subcommand"
        );
        assert_eq!(
            cli.paths,
            vec![PathBuf::from("version")],
            "bare `version` argument should fall through to paths"
        );
    }

    #[test]
    fn test_cli_builtin_version_flag_still_works() {
        match Cli::try_parse_from(["upd", "--version"]) {
            Err(err) => assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion),
            Ok(_) => panic!("--version should trigger DisplayVersion, not Ok"),
        }
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
            Cli::try_parse_from(["upd", "-n", "-v", "--no-cache", "--bump", "major", "path1"])
                .unwrap();
        assert!(cli.dry_run);
        assert!(cli.verbose);
        assert!(cli.no_cache);
        assert_eq!(cli.bump, vec![BumpLevel::Major]);
        assert_eq!(cli.paths, vec![PathBuf::from("path1")]);
    }

    #[test]
    fn test_cli_parses_lang_single() {
        let cli = Cli::try_parse_from(["upd", "--lang", "python"]).unwrap();
        assert_eq!(cli.langs.len(), 1);
        assert_eq!(cli.langs[0], Lang::Python);

        let cli = Cli::try_parse_from(["upd", "-l", "node"]).unwrap();
        assert_eq!(cli.langs.len(), 1);
        assert_eq!(cli.langs[0], Lang::Node);
    }

    #[test]
    fn test_cli_parses_lang_multiple() {
        let cli = Cli::try_parse_from(["upd", "--lang", "python", "--lang", "rust"]).unwrap();
        assert_eq!(cli.langs.len(), 2);
        assert_eq!(cli.langs[0], Lang::Python);
        assert_eq!(cli.langs[1], Lang::Rust);

        let cli = Cli::try_parse_from(["upd", "-l", "go", "-l", "node"]).unwrap();
        assert_eq!(cli.langs.len(), 2);
        assert_eq!(cli.langs[0], Lang::Go);
        assert_eq!(cli.langs[1], Lang::Node);
    }

    #[test]
    fn test_cli_parses_lang_empty() {
        let cli = Cli::try_parse_from(["upd"]).unwrap();
        assert!(cli.langs.is_empty());
    }

    #[test]
    fn test_cli_parses_check() {
        let cli = Cli::try_parse_from(["upd", "--check"]).unwrap();
        assert!(cli.check);
    }

    #[test]
    fn test_cli_short_c_is_config_not_check() {
        let cli = Cli::try_parse_from(["upd", "-c", "custom.toml"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("custom.toml")));
        assert!(!cli.check);
    }

    #[test]
    fn test_cli_parses_check_with_lang() {
        let cli = Cli::try_parse_from(["upd", "--check", "--lang", "python"]).unwrap();
        assert!(cli.check);
        assert_eq!(cli.langs.len(), 1);
        assert_eq!(cli.langs[0], Lang::Python);
    }

    #[test]
    fn test_cli_parses_align_command() {
        let cli = Cli::try_parse_from(["upd", "align"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Align { .. })));
    }

    #[test]
    fn test_cli_parses_align_command_with_paths() {
        let cli = Cli::try_parse_from(["upd", "align", "path1", "path2"]).unwrap();
        match cli.command {
            Some(Command::Align { paths }) => {
                assert_eq!(paths.len(), 2);
                assert_eq!(paths[0], PathBuf::from("path1"));
                assert_eq!(paths[1], PathBuf::from("path2"));
            }
            _ => panic!("Expected Align command"),
        }
    }

    #[test]
    fn test_get_paths_uses_align_command_paths() {
        let cli = Cli::try_parse_from(["upd", "align", "cmd_path"]).unwrap();
        let paths = cli.get_paths();
        assert_eq!(paths, vec![PathBuf::from("cmd_path")]);
    }

    #[test]
    fn test_cli_parses_audit_command() {
        let cli = Cli::try_parse_from(["upd", "audit"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Audit { .. })));
    }

    #[test]
    fn test_cli_parses_audit_command_with_paths() {
        let cli = Cli::try_parse_from(["upd", "audit", "path1", "path2"]).unwrap();
        match cli.command {
            Some(Command::Audit { paths }) => {
                assert_eq!(paths.len(), 2);
                assert_eq!(paths[0], PathBuf::from("path1"));
                assert_eq!(paths[1], PathBuf::from("path2"));
            }
            _ => panic!("Expected Audit command"),
        }
    }

    #[test]
    fn test_get_paths_uses_audit_command_paths() {
        let cli = Cli::try_parse_from(["upd", "audit", "cmd_path"]).unwrap();
        let paths = cli.get_paths();
        assert_eq!(paths, vec![PathBuf::from("cmd_path")]);
    }

    #[test]
    fn test_cli_parses_audit_with_check() {
        let cli = Cli::try_parse_from(["upd", "audit", "--check"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Audit { .. })));
        assert!(cli.check);
    }

    #[test]
    fn test_cli_parses_audit_with_lang_filter() {
        let cli = Cli::try_parse_from(["upd", "audit", "--lang", "python"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Audit { .. })));
        assert_eq!(cli.langs.len(), 1);
        assert_eq!(cli.langs[0], Lang::Python);
    }

    #[test]
    fn test_cli_parses_config_flag() {
        let cli = Cli::try_parse_from(["upd", "--config", "/path/to/config.toml"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/path/to/config.toml")));
    }

    #[test]
    fn test_cli_parses_config_flag_with_command() {
        let cli =
            Cli::try_parse_from(["upd", "update", "--config", "custom.toml", "path1"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("custom.toml")));
        assert!(matches!(cli.command, Some(Command::Update { .. })));
    }

    #[test]
    fn test_cli_config_flag_is_optional() {
        let cli = Cli::try_parse_from(["upd"]).unwrap();
        assert!(cli.config.is_none());
    }
}
