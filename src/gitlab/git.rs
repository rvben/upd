//! Git and child-process plumbing for a GitLab run.
//!
//! Every child runs in the project checkout. Only upd's own network git calls
//! receive the GitLab token (through an askpass helper); repository commands
//! and the updater run with it removed from their environment. That removal is
//! hygiene, not isolation: a process with the same uid can still read the
//! environment upd itself was started with.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use tokio::process::Command;

use super::Error;

/// Environment variable carrying the token to the askpass helper.
pub const TOKEN_VAR: &str = "UPD_GITLAB_TOKEN";

const ASKPASS: &str = "#!/bin/sh\ncase \"$1\" in\n  Username*) printf '%s\\n' oauth2 ;;\n  *) printf '%s\\n' \"$UPD_GITLAB_TOKEN\" ;;\nesac\n";

pub struct Git {
    dir: PathBuf,
    token: String,
    askpass: tempfile::TempDir,
}

/// Result of pushing with a lease.
pub enum Push {
    Done,
    /// The remote ref no longer matched the lease.
    Stale(String),
}

impl Git {
    pub fn new(dir: &Path, token: &str) -> Result<Self, Error> {
        let askpass = tempfile::Builder::new()
            .prefix("upd-gitlab-")
            .tempdir()
            .map_err(|error| Error::Io(format!("cannot create a work directory: {error}")))?;
        let script = askpass.path().join("askpass");
        let mut file = fs::File::create(&script)
            .map_err(|error| Error::Io(format!("cannot write the git askpass helper: {error}")))?;
        file.write_all(ASKPASS.as_bytes())
            .and_then(|()| make_executable(&file))
            .map_err(|error| Error::Io(format!("cannot write the git askpass helper: {error}")))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            token: token.to_string(),
            askpass,
        })
    }

    /// A local git command, without credentials.
    fn local<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = child(&self.dir, "git");
        command.args(args);
        command
    }

    /// A git command that talks to the GitLab remote.
    fn remote<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.local(args);
        command
            .env(TOKEN_VAR, &self.token)
            .env("GIT_ASKPASS", self.askpass.path().join("askpass"))
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    /// Runs a local git command and returns its trimmed stdout.
    pub async fn read<I, S>(&self, args: I) -> Result<String, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = checked(self.local(args), "git").await?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string())
    }

    /// Runs a local git command and returns its stdout exactly as printed.
    pub async fn bytes<I, S>(&self, args: I) -> Result<Vec<u8>, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Ok(checked(self.local(args), "git").await?.stdout)
    }

    /// Runs a local git command on the index file `index` instead of the
    /// repository's own, and returns its stdout exactly as printed.
    pub async fn bytes_with_index<I, S>(&self, index: &Path, args: I) -> Result<Vec<u8>, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.local(args);
        command.env("GIT_INDEX_FILE", index);
        Ok(checked(command, "git").await?.stdout)
    }

    /// Runs a local git command whose exit status is a yes/no answer.
    pub async fn test<I, S>(&self, args: I) -> Result<bool, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = output(self.local(args), "git").await?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failure("git", &output)),
        }
    }

    /// Runs a local git command for its effect.
    pub async fn run<I, S>(&self, args: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        checked(self.local(args), "git").await.map(drop)
    }

    /// Runs `git commit` as the automation identity, storing `message`
    /// verbatim so the commit can later be recognised by it.
    pub async fn commit(&self, message: &str, name: &str, email: &str) -> Result<(), Error> {
        let mut command = self.local([
            "commit",
            "--quiet",
            "--cleanup=verbatim",
            "--message",
            message,
        ]);
        command
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email);
        checked(command, "git commit").await.map(drop)
    }

    /// Fetches `branch` from `url` into `refs/remotes/origin/<branch>`.
    pub async fn fetch(&self, url: &str, branch: &str) -> Result<(), Error> {
        let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
        checked(
            self.remote(["fetch", "--no-tags", url, &refspec]),
            "git fetch",
        )
        .await
        .map(drop)
    }

    /// Whether `branch` exists on the remote. A lookup that fails is an
    /// error, never an absent branch.
    pub async fn remote_has_branch(&self, url: &str, branch: &str) -> Result<bool, Error> {
        let reference = format!("refs/heads/{branch}");
        let output = output(
            self.remote(["ls-remote", "--exit-code", url, &reference]),
            "git ls-remote",
        )
        .await?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(2) => Ok(false),
            _ => Err(failure("git ls-remote", &output)),
        }
    }

    /// Pushes `source` to `branch` (or deletes it when `source` is empty),
    /// only while the remote branch still points at `expected` (absent when
    /// empty).
    pub async fn push_with_lease(
        &self,
        url: &str,
        branch: &str,
        expected: &str,
        source: &str,
    ) -> Result<Push, Error> {
        let lease = format!("--force-with-lease=refs/heads/{branch}:{expected}");
        let refspec = format!("{source}:refs/heads/{branch}");
        let output = output(
            self.remote(["push", "--porcelain", &lease, url, &refspec]),
            "git push",
        )
        .await?;
        if output.status.success() {
            return Ok(Push::Done);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout
            .lines()
            .any(|line| line.starts_with('!') && line.contains("(stale info)"))
        {
            return Ok(Push::Stale(stdout.trim().to_string()));
        }
        Err(failure("git push", &output))
    }
}

#[cfg(unix)]
fn make_executable(file: &fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn make_executable(_file: &fs::File) -> std::io::Result<()> {
    Ok(())
}

/// Variables that point git at a particular repository instead of the one
/// around its working directory. Inherited from a git hook (or any caller that
/// exports `GIT_DIR`), they would turn `git init` in a work directory into a
/// re-initialization of the caller's repository. The `GIT_CONFIG*` variables
/// are kept: CI jobs legitimately pass `safe.directory` and URL rewrites that
/// way.
const REPOSITORY_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
];

/// A child process in `dir` that acts on the repository found there, with the
/// GitLab token removed from its environment.
pub fn child(dir: &Path, program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(dir)
        .env_remove(TOKEN_VAR)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for name in REPOSITORY_ENV {
        command.env_remove(name);
    }
    command
}

/// Runs a repository-supplied shell command with Bash strict mode, streaming
/// its output to stderr.
pub async fn shell(dir: &Path, what: &str, script: &str) -> Result<(), Error> {
    let mut command = child(dir, "bash");
    command
        .args(["-euo", "pipefail", "-c", script])
        .stdout(std::io::stderr())
        .stderr(Stdio::inherit());
    let status = command
        .status()
        .await
        .map_err(|error| Error::Io(format!("cannot start the {what}: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Io(format!("the {what} failed ({status})")))
    }
}

async fn output(mut command: Command, what: &str) -> Result<Output, Error> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| Error::Io(format!("cannot start {what}: {error}")))
}

async fn checked(command: Command, what: &str) -> Result<Output, Error> {
    let output = output(command, what).await?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(failure(what, &output))
    }
}

fn failure(what: &str, output: &Output) -> Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    Error::Io(format!(
        "{what} failed ({}): {}",
        output.status,
        stderr.trim()
    ))
}
