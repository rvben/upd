pub mod align;
pub mod audit;
pub mod cache;
pub mod cli;
pub mod config;
pub mod interactive;
pub mod lockfile;
pub mod output;
pub mod registry;
pub mod updater;
pub mod version;

pub use align::{AlignResult, PackageAlignment, PackageOccurrence, find_alignments, scan_packages};
pub use audit::{AuditResult, Ecosystem, OsvClient, Package, PackageAuditResult, Vulnerability};
pub use cache::Cache;
pub use cli::{Cli, Command};
pub use config::UpdConfig;
pub use lockfile::{LockfileType, detect_lockfiles, regenerate_lockfiles};
pub use registry::{
    GitHubReleasesRegistry, NpmRegistry, NuGetRegistry, PyPiRegistry, Registry, RubyGemsRegistry,
    TerraformRegistry,
};
pub use updater::{FileType, Lang, UpdateResult, Updater, discover_files};

/// Determine the process exit code given the outcome of a run.
///
/// - `2` — one or more errors occurred (network, parse, IO, …); takes
///   precedence over all other conditions so that CI can reliably distinguish
///   a broken run from a clean one.
/// - `1` — `check_mode` is active and there are pending updates (no errors).
/// - `0` — everything is clean.
pub fn decide_exit_code(check_mode: bool, has_pending_updates: bool, has_errors: bool) -> i32 {
    if has_errors {
        2
    } else if check_mode && has_pending_updates {
        1
    } else {
        0
    }
}
