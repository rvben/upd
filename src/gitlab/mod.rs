//! GitLab automation: one rolling dependency merge request per project.
//!
//! `upd gitlab run` owns the whole job the CI template used to script: it
//! checks that the automation branch still holds only its generated commit,
//! rebuilds that branch from the default branch, runs the updater, gates on
//! the report, validates, publishes with a lease, and maintains the merge
//! request. Configuration is the template's `UPD_*` and `CI_*` environment.

mod api;
mod git;
pub mod org;
mod patch;
pub mod plan;
pub mod present;
pub mod run;
pub mod sources;
pub mod split;

use std::fmt;

/// Why a GitLab run stopped without completing.
#[derive(Debug)]
pub enum Error {
    /// Configuration is missing or malformed.
    Input(String),
    /// The run found a state it must not act on, and left it untouched.
    Refused(String),
    /// GitLab rejected a request.
    Api(String),
    /// GitLab or the network could not answer; a later run may succeed.
    Network(String),
    /// A local process or file operation failed.
    Io(String),
    /// The remote branch moved while the run was working.
    Conflict(String),
}

impl Error {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Input(_) => "parse_error",
            Self::Refused(_) => "refused",
            Self::Api(_) => "api_error",
            Self::Network(_) => "network_error",
            Self::Io(_) => "io_error",
            Self::Conflict(_) => "conflict",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Input(_) => 4,
            Self::Network(_) => 3,
            Self::Conflict(_) => 5,
            Self::Refused(_) | Self::Api(_) | Self::Io(_) => 2,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Input(message)
            | Self::Refused(message)
            | Self::Api(message)
            | Self::Network(message)
            | Self::Io(message)
            | Self::Conflict(message) => message,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Error {}

impl From<present::ReportShapeError> for Error {
    fn from(error: present::ReportShapeError) -> Self {
        Self::Io(error.to_string())
    }
}
