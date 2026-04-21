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
pub use cli::{Cli, Command, REVERT_TIP};
pub use config::UpdConfig;
pub use lockfile::{
    LockfileRegenResult, LockfileType, RegenOutcome, detect_lockfiles, regenerate_lockfiles,
    tool_available,
};
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
/// - `1` — `non_mutating` is true (i.e. `--check` or `--dry-run`) and there
///   are pending updates (no errors).
/// - `0` — everything is clean.
pub fn decide_exit_code(non_mutating: bool, has_pending_updates: bool, has_errors: bool) -> i32 {
    if has_errors {
        2
    } else if non_mutating && has_pending_updates {
        1
    } else {
        0
    }
}

/// Determine the process exit code for the `audit` subcommand.
///
/// - `2` — scan errors occurred; errors take precedence over vulnerability
///   findings so that CI can distinguish a broken scan from a clean one.
/// - `3` — vulnerabilities were found and `no_fail` is `false`; distinct from
///   the update exit codes (1 = pending updates, 2 = errors) so callers can
///   handle each condition independently.
/// - `0` — no vulnerabilities found, or `no_fail` suppresses the non-zero exit.
pub fn decide_audit_exit_code(vuln_count: usize, error_count: usize, no_fail: bool) -> i32 {
    if error_count > 0 {
        2
    } else if vuln_count > 0 && !no_fail {
        3
    } else {
        0
    }
}
