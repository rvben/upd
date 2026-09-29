//! The only way the suites spawn `git` or a shell script.
//!
//! The suite runs as a pre-push hook, and a hook in a linked worktree
//! inherits `GIT_DIR`, `GIT_WORK_TREE` and `GIT_INDEX_FILE` for that worktree.
//! A git command meant for a fixture directory, run directly or from a
//! script, would act on the developer's repository instead: `git init`
//! writes `core.worktree` into its shared config (breaking the main checkout
//! once the worktree is gone) and `git config` writes the fixture identity.

use std::ffi::OsStr;
use std::process::Command;

/// The variables `git rev-parse --local-env-vars` reports: everything that
/// points git at a particular repository rather than the one around its
/// working directory.
pub const REPOSITORY_ENV: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// A command for `program` with none of [`REPOSITORY_ENV`] inherited.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    for name in REPOSITORY_ENV {
        command.env_remove(name);
    }
    command
}
