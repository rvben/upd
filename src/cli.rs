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

    /// Verify dependency compatibility after update
    #[arg(long, global = true)]
    pub verify: bool,

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
