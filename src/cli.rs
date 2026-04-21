use crate::updater::Lang;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

pub const REVERT_TIP: &str = "Tip: changes are applied in-place \u{2014} use git to revert.";

/// Kind of version bump to include when filtering updates.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[value(rename_all = "lower")]
pub enum BumpLevel {
    Major,
    Minor,
    Patch,
}

/// Output format for command results.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[value(rename_all = "lower")]
pub enum OutputFormat {
    /// Human-readable coloured text (default).
    #[default]
    Text,
    /// Machine-readable JSON on stdout.
    Json,
}

#[derive(Parser)]
#[command(name = "upd")]
#[command(
    author,
    version,
    about = "A fast dependency updater for Python, Node.js, Rust, Go, Ruby, .NET, Terraform, GitHub Actions, pre-commit, and Mise/asdf projects",
    after_help = REVERT_TIP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Paths to update (files or directories)
    #[arg(global = true)]
    pub paths: Vec<PathBuf>,

    /// Show available updates without writing any files.
    ///
    /// Exits with code 1 when updates are available, 2 on errors.
    /// Equivalent to --check when you also want CI to fail on outdated deps.
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

    /// Suppress all output except errors and warnings.
    ///
    /// Useful in scripts where you only care about the exit code.
    #[arg(long, short = 'q', global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Prompt before applying each update.
    ///
    /// Presents each available update one at a time so you can accept or skip it.
    #[arg(short, long, global = true)]
    pub interactive: bool,

    /// Include only updates whose bump level exactly matches one of the given levels.
    ///
    /// Repeatable or comma-separated. Use when you want to restrict to an exact set
    /// of bump levels (e.g. `--only-bump minor,patch` skips major updates).
    /// Mutually exclusive with `--max-bump`.
    #[arg(
        long = "only-bump",
        global = true,
        value_enum,
        value_name = "LEVEL",
        value_delimiter = ',',
        conflicts_with = "max_bump"
    )]
    pub only_bump: Vec<BumpLevel>,

    /// Include updates up to and including the given bump level.
    ///
    /// `--max-bump patch` allows only patch updates; `--max-bump minor` allows
    /// patch and minor but not major; `--max-bump major` allows everything.
    /// Mutually exclusive with `--only-bump`.
    #[arg(
        long = "max-bump",
        global = true,
        value_enum,
        value_name = "LEVEL",
        conflicts_with = "only_bump"
    )]
    pub max_bump: Option<BumpLevel>,

    /// Use full version precision (e.g., 3.1.5 instead of 3.1)
    #[arg(long, global = true)]
    pub full_precision: bool,

    /// Limit to one or more ecosystems (repeatable, or comma-separated).
    ///
    /// Examples: --lang python  |  --lang python,rust  |  -l go -l node
    #[arg(
        short = 'l',
        long = "lang",
        value_name = "LANG",
        global = true,
        value_delimiter = ','
    )]
    pub langs: Vec<Lang>,

    /// Exit with code 1 if updates are available, without writing any changes.
    ///
    /// Intended for CI pipelines that should fail when dependencies are outdated.
    #[arg(long, global = true)]
    pub check: bool,

    /// Regenerate lockfiles after updating.
    ///
    /// Runs the appropriate lock command for each ecosystem (e.g. `poetry lock`,
    /// `npm install`, `cargo update`).
    #[arg(long, global = true)]
    pub lock: bool,

    /// Apply updates to files. Without --apply, runs in dry-run mode.
    ///
    /// When a positional path or no path (VCS root) is used, --apply is required
    /// to mutate files. --check, --dry-run, and --interactive do not require
    /// --apply.
    #[arg(long, global = true)]
    pub apply: bool,

    /// Path to config file (default: auto-discover .updrc.toml, upd.toml, or .updrc)
    #[arg(short = 'c', long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Set output format: text (default) or json.
    ///
    /// Use --format json for machine-readable output in scripts or CI.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Text, value_name = "FORMAT")]
    pub format: OutputFormat,

    /// Print the effective configuration and exit.
    ///
    /// Shows which config file was loaded and the resolved ignore/pin settings.
    #[arg(long, global = true)]
    pub show_config: bool,

    /// Update only the named package(s), skipping all others.
    ///
    /// Comma-separated or repeatable. Exact case-sensitive match.
    #[arg(
        long = "package",
        value_name = "NAME",
        global = true,
        value_delimiter = ','
    )]
    pub packages: Vec<String>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Update dependencies (default when no command specified)
    Update {
        /// Paths to update
        #[arg()]
        paths: Vec<PathBuf>,
    },

    /// Align duplicate packages to their highest pinned version across files.
    ///
    /// Useful for monorepos where the same package appears at different versions
    /// in multiple files (e.g. requirements.txt and pyproject.toml). Writes the
    /// highest version found back to every file that has a lower pin.
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

        /// Exit 0 even when vulnerabilities are found (useful for scheduled scans that should not break CI)
        #[arg(long)]
        no_fail: bool,
    },

    /// Clear the version cache
    CleanCache,

    /// Update upd itself
    SelfUpdate,
}

impl Cli {
    /// Returns explicitly provided paths, or an empty vec when none were given.
    ///
    /// Callers that need a default path (e.g. the VCS root) must resolve it
    /// themselves; this method only surfaces what the user typed.
    pub fn get_paths(&self) -> Vec<PathBuf> {
        match &self.command {
            Some(Command::Update { paths }) if !paths.is_empty() => paths.clone(),
            Some(Command::Align { paths }) if !paths.is_empty() => paths.clone(),
            Some(Command::Audit { paths, .. }) if !paths.is_empty() => paths.clone(),
            _ if !self.paths.is_empty() => self.paths.clone(),
            _ => vec![],
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
        assert!(!cli.quiet);
        assert!(!cli.interactive);
        assert!(cli.only_bump.is_empty());
        assert!(cli.max_bump.is_none());
        assert!(!cli.full_precision);
        assert!(!cli.check);
        assert!(!cli.lock);
        assert!(!cli.apply);
        assert!(cli.paths.is_empty());
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_cli_parses_quiet_long() {
        let cli = Cli::try_parse_from(["upd", "--quiet"]).unwrap();
        assert!(cli.quiet);
    }

    #[test]
    fn test_cli_parses_quiet_short() {
        let cli = Cli::try_parse_from(["upd", "-q"]).unwrap();
        assert!(cli.quiet);
    }

    #[test]
    fn test_cli_quiet_conflicts_with_verbose() {
        let result = Cli::try_parse_from(["upd", "--quiet", "--verbose"]);
        assert!(
            result.is_err(),
            "--quiet and --verbose must conflict; got Ok"
        );
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
    fn test_cli_parses_only_bump_single_level() {
        let cli = Cli::try_parse_from(["upd", "--only-bump", "major"]).unwrap();
        assert_eq!(cli.only_bump, vec![BumpLevel::Major]);

        let cli = Cli::try_parse_from(["upd", "--only-bump", "minor"]).unwrap();
        assert_eq!(cli.only_bump, vec![BumpLevel::Minor]);

        let cli = Cli::try_parse_from(["upd", "--only-bump", "patch"]).unwrap();
        assert_eq!(cli.only_bump, vec![BumpLevel::Patch]);
    }

    #[test]
    fn test_cli_parses_only_bump_repeatable() {
        let cli =
            Cli::try_parse_from(["upd", "--only-bump", "major", "--only-bump", "minor"]).unwrap();
        assert_eq!(cli.only_bump, vec![BumpLevel::Major, BumpLevel::Minor]);
    }

    #[test]
    fn test_cli_parses_only_bump_comma_separated() {
        let cli = Cli::try_parse_from(["upd", "--only-bump", "minor,patch"]).unwrap();
        assert_eq!(cli.only_bump, vec![BumpLevel::Minor, BumpLevel::Patch]);
    }

    #[test]
    fn test_cli_rejects_invalid_only_bump_level() {
        let rendered = match Cli::try_parse_from(["upd", "--only-bump", "breaking"]) {
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
        for flag in ["--major", "--minor", "--patch", "--bump"] {
            assert!(
                Cli::try_parse_from(["upd", flag]).is_err(),
                "expected {flag} to be rejected; it is not a valid flag"
            );
        }
    }

    #[test]
    fn test_cli_parses_max_bump_major() {
        let cli = Cli::try_parse_from(["upd", "--max-bump", "major"]).unwrap();
        assert_eq!(cli.max_bump, Some(BumpLevel::Major));
    }

    #[test]
    fn test_cli_parses_max_bump_minor() {
        let cli = Cli::try_parse_from(["upd", "--max-bump", "minor"]).unwrap();
        assert_eq!(cli.max_bump, Some(BumpLevel::Minor));
    }

    #[test]
    fn test_cli_parses_max_bump_patch() {
        let cli = Cli::try_parse_from(["upd", "--max-bump", "patch"]).unwrap();
        assert_eq!(cli.max_bump, Some(BumpLevel::Patch));
    }

    #[test]
    fn test_cli_only_bump_and_max_bump_are_mutually_exclusive() {
        let result = Cli::try_parse_from(["upd", "--only-bump", "minor", "--max-bump", "minor"]);
        assert!(
            result.is_err(),
            "--only-bump and --max-bump must conflict; got Ok"
        );
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
    fn test_get_paths_returns_empty_when_no_paths_given() {
        let cli = Cli::try_parse_from(["upd"]).unwrap();
        let paths = cli.get_paths();
        assert!(
            paths.is_empty(),
            "get_paths() must return an empty vec when no paths are given; got: {paths:?}"
        );
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
        let cli = Cli::try_parse_from([
            "upd",
            "-n",
            "-v",
            "--no-cache",
            "--only-bump",
            "major",
            "path1",
        ])
        .unwrap();
        assert!(cli.dry_run);
        assert!(cli.verbose);
        assert!(cli.no_cache);
        assert_eq!(cli.only_bump, vec![BumpLevel::Major]);
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
    fn test_cli_parses_lang_comma_separated() {
        let cli = Cli::try_parse_from(["upd", "--lang", "python,rust"]).unwrap();
        assert_eq!(cli.langs, vec![Lang::Python, Lang::Rust]);
    }

    #[test]
    fn test_cli_parses_lang_repeated_flag() {
        let cli = Cli::try_parse_from(["upd", "--lang", "python", "--lang", "rust"]).unwrap();
        assert_eq!(cli.langs, vec![Lang::Python, Lang::Rust]);
    }

    #[test]
    fn test_cli_parses_lang_mixed_comma_and_repeated() {
        let cli = Cli::try_parse_from(["upd", "--lang", "python,rust", "--lang", "go"]).unwrap();
        assert_eq!(cli.langs, vec![Lang::Python, Lang::Rust, Lang::Go]);
    }

    #[test]
    fn test_cli_rejects_unknown_lang_in_comma_list() {
        match Cli::try_parse_from(["upd", "--lang", "python,nonsense"]) {
            Ok(_) => panic!("unknown lang value should be rejected"),
            Err(err) => {
                let msg = err.to_string();
                assert!(
                    msg.contains("nonsense"),
                    "error should mention the unknown value, got: {msg}"
                );
            }
        }
    }

    #[test]
    fn test_cli_lang_trailing_comma_behaviour() {
        // clap with value_delimiter treats a trailing comma as an empty segment and
        // rejects it with an "invalid value" error because "" is not a valid Lang variant.
        let result = Cli::try_parse_from(["upd", "--lang", "python,"]);
        assert!(
            result.is_err(),
            "trailing comma should produce a parse error"
        );
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
            Some(Command::Audit { paths, .. }) => {
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

    #[test]
    fn test_cli_format_defaults_to_text() {
        let cli = Cli::try_parse_from(["upd"]).unwrap();
        assert_eq!(cli.format, OutputFormat::Text);
    }

    #[test]
    fn test_cli_format_accepts_json() {
        let cli = Cli::try_parse_from(["upd", "--format", "json"]).unwrap();
        assert_eq!(cli.format, OutputFormat::Json);
    }

    #[test]
    fn test_cli_format_accepts_text() {
        let cli = Cli::try_parse_from(["upd", "--format", "text"]).unwrap();
        assert_eq!(cli.format, OutputFormat::Text);
    }

    #[test]
    fn test_cli_format_is_global_across_subcommands() {
        let cli = Cli::try_parse_from(["upd", "audit", "--format", "json"]).unwrap();
        assert_eq!(cli.format, OutputFormat::Json);
        assert!(matches!(cli.command, Some(Command::Audit { .. })));
    }

    #[test]
    fn test_cli_format_rejects_unknown_value() {
        match Cli::try_parse_from(["upd", "--format", "yaml"]) {
            Err(err) => assert!(err.to_string().contains("invalid value")),
            Ok(_) => panic!("expected invalid format to be rejected"),
        }
    }

    #[test]
    fn test_cli_parses_show_config() {
        let cli = Cli::try_parse_from(["upd", "--show-config"]).unwrap();
        assert!(cli.show_config);
    }

    #[test]
    fn test_cli_show_config_default_false() {
        let cli = Cli::try_parse_from(["upd"]).unwrap();
        assert!(!cli.show_config);
    }

    #[test]
    fn test_cli_show_config_is_global() {
        let cli = Cli::try_parse_from(["upd", "update", "--show-config"]).unwrap();
        assert!(cli.show_config);
    }

    // P6: --help should produce longer output than -h because field doc comments
    // have both a short first line (used by -h) and extended paragraphs (--help only).
    #[test]
    fn test_long_help_is_longer_than_short_help() {
        use clap::CommandFactory;
        let mut short_buf = Vec::new();
        Cli::command()
            .write_help(&mut short_buf)
            .expect("short help failed");

        let mut long_buf = Vec::new();
        Cli::command()
            .write_long_help(&mut long_buf)
            .expect("long help failed");

        assert!(
            long_buf.len() > short_buf.len(),
            "--help ({} bytes) should be longer than -h ({} bytes)",
            long_buf.len(),
            short_buf.len()
        );
    }

    #[test]
    fn test_long_help_contains_extended_descriptions() {
        use clap::CommandFactory;
        let mut buf = Vec::new();
        Cli::command()
            .write_long_help(&mut buf)
            .expect("long help failed");
        let help = String::from_utf8(buf).unwrap();

        // Extended paragraphs that only appear in --help (not in -h)
        assert!(
            help.contains("Exit with code 1"),
            "--help should contain extended --check description; got:\n{help}"
        );
        // The `align` subcommand description (long form) includes "monorepos";
        // verify it appears in the align subcommand's own long help.
        let mut align_buf = Vec::new();
        Cli::command()
            .find_subcommand_mut("align")
            .expect("align subcommand must exist")
            .write_long_help(&mut align_buf)
            .expect("align long help failed");
        let align_help = String::from_utf8(align_buf).unwrap();
        assert!(
            align_help.contains("monorepos"),
            "align --help should describe monorepo use-case; got:\n{align_help}"
        );
    }
}
