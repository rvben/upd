//! `upd gitlab run`: maintain one rolling dependency merge request.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Error;
use super::api::{Client, MergeRequest};
use super::git::{self, Git, Push};
use super::present::{self, Base, Presentation, Security, SecurityCounts};

/// Marks a merge request that automation has paused on, so the notice is
/// added once.
pub const PAUSE_MARKER: &str = "<!-- upd-human-commit-pause -->";

const PAUSE_NOTICE: &str = "\n\n<!-- upd-human-commit-pause -->\n> **Automation paused:** this branch has commits outside the generated upd commit. Preserve them or remove them before automation resumes.\n";

const ARTIFACT_DIR: &str = ".upd-ci";

/// Job configuration, read from the environment the CI template provides.
#[derive(Debug, Clone)]
pub struct Settings {
    pub token: String,
    pub api_url: String,
    pub default_branch: String,
    pub project_dir: PathBuf,
    pub project_id: String,
    pub project_path: String,
    pub server_url: String,
    pub branch: String,
    pub commit_message: String,
    pub mr_title: String,
    pub git_name: String,
    pub git_email: String,
    pub paths: Vec<String>,
    pub langs: String,
    /// Ecosystems left out whatever `langs` or the repository selects.
    pub exclude_langs: String,
    pub packages: String,
    pub min_age: String,
    /// Shortest release age accepted whatever the repository configures;
    /// empty for none.
    pub min_age_floor: String,
    pub max_bump: String,
    pub lock: bool,
    /// Whether regenerating a lockfile must not build a package from
    /// source; sets `UV_NO_BUILD` for every updater command.
    pub no_build: bool,
    pub auto_merge: bool,
    /// Whether the ordinary lane first fixes every dependency with a
    /// published advisory, outside the update policy.
    pub security_remediation: bool,
    pub prepare_command: String,
    pub validation_command: String,
    /// The updater to run; this executable unless `UPD_EXECUTABLE` names one.
    pub updater: PathBuf,
    /// Configuration file, relative to the checkout, the updater must read
    /// instead of discovering one.
    pub config: Option<PathBuf>,
    /// Report what the run would do without pushing or writing to GitLab.
    pub dry_run: bool,
    /// Whether a second lane proposes major-version upgrades on their own
    /// branch and merge request.
    pub major_mr: bool,
    pub major_branch: String,
    pub major_commit_message: String,
    /// Which lane these settings drive.
    pub lane: Lane,
}

/// The two merge requests a project can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// Every update the bump ceiling allows; the only lane unless
    /// `major_mr` is set.
    Ordinary,
    /// Major-version upgrades only, never merged by upd.
    Major,
}

impl Settings {
    pub fn from_env(dry_run: bool) -> Result<Self, Error> {
        Self::from_lookup(|name| env::var(name).ok(), dry_run)
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>, dry_run: bool) -> Result<Self, Error> {
        let required = |name: &str, hint: &str| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| Error::Input(format!("{name} is not set: {hint}")))
        };
        let optional = |name: &str, default: &str| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        let flag = |name: &str| match lookup(name).as_deref() {
            None | Some("") | Some("false") => Ok(false),
            Some("true") => Ok(true),
            Some(_) => Err(Error::Input(format!("{name} must be true or false"))),
        };

        let gitlab = "GitLab CI provides it";
        let updater = match lookup("UPD_EXECUTABLE").filter(|value| !value.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => env::current_exe()
                .map_err(|error| Error::Io(format!("cannot locate the upd executable: {error}")))?,
        };
        let paths_input = optional("UPD_PATHS", ".");
        let settings = Self {
            token: required(
                "UPD_GITLAB_TOKEN",
                "set the masked, protected UPD_GITLAB_TOKEN CI/CD variable",
            )?,
            api_url: required("CI_API_V4_URL", gitlab)?,
            default_branch: required("CI_DEFAULT_BRANCH", gitlab)?,
            project_dir: PathBuf::from(required("CI_PROJECT_DIR", gitlab)?),
            project_id: required("CI_PROJECT_ID", gitlab)?,
            project_path: required("CI_PROJECT_PATH", gitlab)?,
            server_url: required("CI_SERVER_URL", gitlab)?,
            branch: optional("UPD_BRANCH", "automation/upd-dependencies"),
            commit_message: optional(
                "UPD_COMMIT_MESSAGE",
                "chore(deps): update dependencies with upd",
            ),
            mr_title: optional("UPD_MR_TITLE", ""),
            git_name: optional("UPD_GIT_NAME", "upd automation"),
            git_email: optional("UPD_GIT_EMAIL", "upd-automation@noreply.invalid"),
            paths: paths_input.split_whitespace().map(str::to_string).collect(),
            langs: optional("UPD_LANGS", ""),
            exclude_langs: String::new(),
            packages: optional("UPD_PACKAGES", ""),
            min_age: optional("UPD_MIN_AGE", ""),
            min_age_floor: String::new(),
            max_bump: optional("UPD_MAX_BUMP", ""),
            lock: flag("UPD_LOCK")?,
            no_build: false,
            auto_merge: flag("UPD_AUTO_MERGE")?,
            security_remediation: match lookup("UPD_SECURITY_REMEDIATION").as_deref() {
                None | Some("") => true,
                _ => flag("UPD_SECURITY_REMEDIATION")?,
            },
            prepare_command: optional("UPD_PREPARE_COMMAND", ""),
            validation_command: optional("UPD_VALIDATION_COMMAND", ""),
            updater,
            config: None,
            dry_run,
            major_mr: flag("UPD_MAJOR_MR")?,
            major_branch: optional("UPD_MAJOR_BRANCH", "automation/upd-dependencies-major"),
            major_commit_message: optional(
                "UPD_MAJOR_COMMIT_MESSAGE",
                "chore(deps): update major dependencies with upd",
            ),
            lane: Lane::Ordinary,
        };
        if settings.project_id.contains('/') {
            return Err(Error::Input(
                "CI_PROJECT_ID must be a numeric project ID".to_string(),
            ));
        }
        if branches_collide(&settings.branch, &settings.default_branch) {
            return Err(Error::Input(
                "The automation branch must differ from the default branch, and neither may be nested under the other's name".to_string(),
            ));
        }
        if settings.major_mr {
            check_major_lane(
                &settings.max_bump,
                "UPD_MAX_BUMP",
                &settings.branch,
                &settings.major_branch,
            )?;
            if branches_collide(&settings.major_branch, &settings.default_branch) {
                return Err(Error::Input(
                    "The major automation branch must differ from the default branch, and neither may be nested under the other's name".to_string(),
                ));
            }
        }
        Ok(settings)
    }

    /// The settings of the major lane, when it is enabled: its own branch
    /// and commit message, no bump ceiling, no auto-merge, no security fixes
    /// (the ordinary lane proposes those), and a title upd derives rather
    /// than the ordinary lane's configured one.
    pub fn major_lane(&self) -> Option<Self> {
        if !self.major_mr || self.lane == Lane::Major {
            return None;
        }
        Some(Self {
            branch: self.major_branch.clone(),
            commit_message: self.major_commit_message.clone(),
            mr_title: String::new(),
            max_bump: String::new(),
            auto_merge: false,
            security_remediation: false,
            lane: Lane::Major,
            ..self.clone()
        })
    }

    /// The project's open merge requests from the major branch. The link
    /// holds whatever the major lane did this run, and whichever merge
    /// request it keeps open.
    pub fn major_merge_requests_url(&self) -> String {
        let branch: String =
            url::form_urlencoded::byte_serialize(self.major_branch.as_bytes()).collect();
        format!(
            "{}/{}/-/merge_requests?state=opened&source_branch={branch}",
            self.server_url.trim_end_matches('/'),
            self.project_path
        )
    }

    pub(super) fn git_url(&self) -> String {
        format!(
            "{}/{}.git",
            self.server_url.trim_end_matches('/'),
            self.project_path
        )
    }

    /// Where the artifact `name` goes; the major lane writes beside the
    /// ordinary lane's artifacts rather than over them.
    fn artifact(&self, name: &str) -> PathBuf {
        let name = match self.lane {
            Lane::Ordinary => name.to_string(),
            Lane::Major => name.replacen("upd-", "upd-major-", 1),
        };
        self.project_dir.join(ARTIFACT_DIR).join(name)
    }
}

/// Refuses a major lane that would propose what the ordinary lane already
/// does, or share its branch.
pub(super) fn check_major_lane(
    max_bump: &str,
    max_bump_name: &str,
    branch: &str,
    major_branch: &str,
) -> Result<(), Error> {
    if !matches!(max_bump, "minor" | "patch") {
        return Err(Error::Input(format!(
            "The major merge request needs {max_bump_name} set to minor or patch, not '{max_bump}': without a lower ceiling the ordinary merge request already carries major upgrades"
        )));
    }
    if branches_collide(major_branch, branch) {
        return Err(Error::Input(
            "The major automation branch must differ from the automation branch, and neither may be nested under the other's name".to_string(),
        ));
    }
    Ok(())
}

/// Whether git could not hold both branches: the same name, or one nested
/// under the other's (`deps` and `deps/major`), since a ref cannot be both a
/// branch and a directory of branches.
pub(super) fn branches_collide(a: &str, b: &str) -> bool {
    let nested = |outer: &str, inner: &str| {
        inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    a == b || nested(a, b) || nested(b, a)
}

/// What a run did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum Outcome {
    /// Nothing to propose, and nothing obsolete to clean up.
    Clean,
    /// Nothing to propose; the obsolete merge request and/or branch was removed.
    Closed {
        merge_request: Option<String>,
        branch_deleted: bool,
    },
    /// The update is on the rolling branch and its merge request created or
    /// refreshed. `pushed` is false when the branch already held exactly this
    /// commit's content and was left as it was.
    Published {
        merge_request: String,
        created: bool,
        pushed: bool,
        commit: String,
        auto_merge: AutoMerge,
    },
    /// The branch holds commits automation did not write; it was left alone.
    Paused {
        merge_request: String,
        notice_added: bool,
    },
    /// Dry run: an update is ready and would be published under `title`;
    /// `push` is false when the branch already holds it.
    WouldPublish { title: String, push: bool },
    /// Dry run: nothing to propose; the obsolete merge request and/or branch
    /// would be removed.
    WouldClose {
        merge_request: Option<String>,
        delete_branch: bool,
    },
    /// Dry run: the branch holds commits automation did not write.
    WouldPause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoMerge {
    Enabled,
    Disabled,
    Off,
}

impl Outcome {
    pub fn to_json(&self, branch: &str) -> Value {
        let mut value = serde_json::to_value(self).expect("outcome serializes");
        value["command"] = json!("gitlab run");
        value["branch"] = json!(branch);
        value
    }

    /// One-line account of the run, for the job log.
    pub fn render_text(&self, branch: &str) -> String {
        match self {
            Self::Clean => "No dependency updates within policy.".to_string(),
            Self::Closed {
                merge_request,
                branch_deleted,
            } => {
                let mut actions = Vec::new();
                if let Some(url) = merge_request {
                    actions.push(format!("closed {url}"));
                }
                if *branch_deleted {
                    actions.push(format!("removed {branch}"));
                }
                format!(
                    "No dependency updates within policy; {}.",
                    actions.join(" and ")
                )
            }
            Self::Published {
                merge_request,
                created,
                pushed,
                commit,
                auto_merge,
            } => {
                let commit = commit.get(..12).unwrap_or(commit);
                let auto_merge = match auto_merge {
                    AutoMerge::Enabled => "enabled",
                    AutoMerge::Disabled => "disabled",
                    AutoMerge::Off => "off",
                };
                match (created, pushed) {
                    (true, true) => format!(
                        "Created {merge_request} for {commit} on {branch}; auto-merge {auto_merge}."
                    ),
                    (false, true) => format!(
                        "Updated {merge_request} to {commit} on {branch}; auto-merge {auto_merge}."
                    ),
                    (true, false) => format!(
                        "Created {merge_request} for {commit}, already on {branch}; auto-merge {auto_merge}."
                    ),
                    (false, false) => format!(
                        "{merge_request} already proposes {commit} on {branch}; nothing pushed; auto-merge {auto_merge}."
                    ),
                }
            }
            Self::Paused {
                merge_request,
                notice_added,
            } => format!(
                "Paused: {branch} holds commits outside the generated upd commit; {} {merge_request}.",
                if *notice_added {
                    "added a notice to"
                } else {
                    "notice already on"
                }
            ),
            Self::WouldPublish { title, push: true } => {
                format!("Dry run: would publish \"{title}\" on {branch}.")
            }
            Self::WouldPublish { title, push: false } => format!(
                "Dry run: {branch} already holds \"{title}\"; would push nothing and only refresh its merge request."
            ),
            Self::WouldClose {
                merge_request,
                delete_branch,
            } => {
                let mut actions = Vec::new();
                if let Some(url) = merge_request {
                    actions.push(format!("close {url}"));
                }
                if *delete_branch {
                    actions.push(format!("remove {branch}"));
                }
                format!(
                    "Dry run: no dependency updates within policy; would {}.",
                    actions.join(" and ")
                )
            }
            Self::WouldPause => format!(
                "Dry run: would pause; {branch} holds commits outside the generated upd commit."
            ),
        }
    }
}

/// What a lane did, and what its security step changed when it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub outcome: Outcome,
    /// Absent when security remediation did not run.
    pub security: Option<SecurityCounts>,
}

impl Proposal {
    pub fn to_json(&self, branch: &str) -> Value {
        let mut value = self.outcome.to_json(branch);
        if let Some(security) = &self.security {
            value["security"] = json!(security);
        }
        value
    }
}

/// Where a run's progress goes: straight to stderr, or into a buffer the
/// caller prints as one block, so concurrent runs do not interleave.
#[derive(Debug, Default)]
pub struct Log {
    buffer: Option<Mutex<String>>,
}

impl Log {
    pub fn direct() -> Self {
        Self { buffer: None }
    }

    pub fn buffered() -> Self {
        Self {
            buffer: Some(Mutex::new(String::new())),
        }
    }

    pub fn line(&self, text: impl std::fmt::Display) {
        match &self.buffer {
            None => eprintln!("{text}"),
            Some(buffer) => {
                let mut buffer = buffer.lock().unwrap_or_else(|poison| poison.into_inner());
                buffer.push_str(&text.to_string());
                buffer.push('\n');
            }
        }
    }

    /// Appends captured process output verbatim.
    fn raw(&self, bytes: &[u8]) {
        if let Some(buffer) = &self.buffer {
            let mut buffer = buffer.lock().unwrap_or_else(|poison| poison.into_inner());
            buffer.push_str(&String::from_utf8_lossy(bytes));
            if !buffer.is_empty() && !buffer.ends_with('\n') {
                buffer.push('\n');
            }
        }
    }

    fn is_buffered(&self) -> bool {
        self.buffer.is_some()
    }

    /// Everything buffered so far; empty for a direct log.
    pub fn take(&self) -> String {
        match &self.buffer {
            None => String::new(),
            Some(buffer) => {
                std::mem::take(&mut *buffer.lock().unwrap_or_else(|poison| poison.into_inner()))
            }
        }
    }
}

/// What building a proposal left: an outcome reached without publishing,
/// or a staged and presented change for [`Session::publish`].
pub(super) enum Build {
    Done(Outcome),
    Ready {
        presentation: Box<Presentation>,
        /// Whether the staged tree differs from the default branch.
        changed: bool,
    },
}

/// A checkout holding the fetched default branch and, when it exists, the
/// automation branch, before anything is changed.
pub struct Session<'a> {
    pub settings: Settings,
    pub git: Git,
    pub log: &'a Log,
    api: Client,
    url: String,
    /// `refs/remotes/origin/<default branch>`.
    pub default_ref: String,
    /// The remote automation branch tip, empty when the branch is absent.
    expected_remote_sha: String,
}

/// What each lane of a run did. A lane that failed does not stop the other.
#[derive(Debug)]
pub struct Lanes {
    pub ordinary: Result<Proposal, Error>,
    /// Absent unless the major lane is enabled.
    pub major: Option<Result<Outcome, Error>>,
}

impl Lanes {
    /// The first failure, ordinary lane first: it decides the exit code.
    pub fn failure(&self) -> Option<&Error> {
        self.ordinary
            .as_ref()
            .err()
            .or_else(|| self.major.as_ref().and_then(|major| major.as_ref().err()))
    }

    /// Both lanes' results: the ordinary lane's at the top level, as a run
    /// without a major lane reports it, and the major lane's under `major`.
    /// A lane that failed has the outcome `failed` and its `error`.
    pub fn to_json(&self, settings: &Settings) -> Value {
        let mut value = match &self.ordinary {
            Ok(proposal) => proposal.to_json(&settings.branch),
            Err(error) => failed_json(error, &settings.branch),
        };
        value["command"] = json!("gitlab run");
        if let Some(major) = &self.major {
            value["major"] = major_lane_json(major, &settings.major_branch);
        }
        value
    }

    /// One line per lane.
    pub fn render_text(&self, settings: &Settings) -> String {
        let mut lines = vec![match &self.ordinary {
            Ok(proposal) => proposal.outcome.render_text(&settings.branch),
            Err(error) => failed_text(error, &settings.branch),
        }];
        if let Some(major) = &self.major {
            lines.push(format!(
                "Major lane: {}",
                lane_text(major, &settings.major_branch)
            ));
        }
        lines.join("\n")
    }
}

fn lane_json(result: &Result<Outcome, Error>, branch: &str) -> Value {
    match result {
        Ok(outcome) => outcome.to_json(branch),
        Err(error) => failed_json(error, branch),
    }
}

fn failed_json(error: &Error, branch: &str) -> Value {
    json!({
        "branch": branch,
        "outcome": "failed",
        "error": {
            "kind": error.kind(),
            "message": error.message(),
            "exit_code": error.exit_code(),
        },
    })
}

/// The major lane's result as it nests under its parent's `major` field,
/// which already names the command.
pub(super) fn major_lane_json(result: &Result<Outcome, Error>, branch: &str) -> Value {
    let mut value = lane_json(result, branch);
    if let Value::Object(fields) = &mut value {
        fields.remove("command");
    }
    value
}

pub(super) fn lane_text(result: &Result<Outcome, Error>, branch: &str) -> String {
    match result {
        Ok(outcome) => outcome.render_text(branch),
        Err(error) => failed_text(error, branch),
    }
}

fn failed_text(error: &Error, branch: &str) -> String {
    format!("Failed on {branch} ({}): {}", error.kind(), error.message())
}

/// Runs the ordinary lane and then, when enabled, the major lane. Only a
/// malformed branch name or a checkout with local changes stops the run
/// before either lane starts.
pub async fn run(settings: &Settings, log: &Log) -> Result<Lanes, Error> {
    check_branch_name(&settings.project_dir, &settings.branch).await?;
    let major = settings.major_lane();
    if let Some(major) = &major {
        check_branch_name(&settings.project_dir, &major.branch).await?;
    }
    check_clean_checkout(&settings.project_dir).await?;
    let ordinary = match Session::open(settings.clone(), log).await {
        Ok(session) => session.propose().await,
        Err(error) => Err(error),
    };
    let major = match major {
        Some(major) => Some(propose_major(major, log).await),
        None => None,
    };
    Ok(Lanes { ordinary, major })
}

/// Runs the major lane in the checkout the ordinary lane used.
pub async fn propose_major(settings: Settings, log: &Log) -> Result<Outcome, Error> {
    log.line(format_args!(
        "Major lane: proposing major-version upgrades on {}",
        settings.branch
    ));
    let proposal = Session::open(settings, log).await?.propose().await?;
    Ok(proposal.outcome)
}

/// Refuses a checkout holding edits or untracked files. Each lane resets the
/// checkout to the default branch, and the major lane discards whatever it
/// finds there, so anything not committed would be lost. The artifact
/// directory a previous run left behind is not a local change.
async fn check_clean_checkout(dir: &Path) -> Result<(), Error> {
    let output = git::child(dir, "git")
        .args(["status", "--porcelain", "--untracked-files=all", "--", "."])
        .arg(format!(":(exclude,top){ARTIFACT_DIR}/"))
        .stderr(Stdio::inherit())
        .output()
        .await
        .map_err(|error| Error::Io(format!("cannot start git: {error}")))?;
    if !output.status.success() {
        return Err(Error::Io(format!("git status failed in {}", dir.display())));
    }
    if output.stdout.is_empty() {
        Ok(())
    } else {
        Err(Error::Refused(format!(
            "The checkout in {} has uncommitted changes or untracked files; upd gitlab run resets it to the default branch, so it needs a clean checkout",
            dir.display()
        )))
    }
}

pub(super) async fn check_branch_name(dir: &Path, branch: &str) -> Result<(), Error> {
    if git::child(dir, "git")
        .args(["check-ref-format", "--branch", branch])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|error| Error::Io(format!("cannot start git: {error}")))?
        .success()
    {
        Ok(())
    } else {
        Err(Error::Input(format!("Invalid automation branch: {branch}")))
    }
}

impl<'a> Session<'a> {
    /// Validates the branch name and fetches both branches into the
    /// checkout at `settings.project_dir`.
    pub async fn open(settings: Settings, log: &'a Log) -> Result<Self, Error> {
        let dir = &settings.project_dir;
        let git = Git::new(dir, &settings.token)?;
        check_branch_name(dir, &settings.branch).await?;

        prepare_artifact_dir(&git, dir).await?;
        let api =
            Client::new(&settings.api_url, &settings.token)?.for_project(&settings.project_id);
        let url = settings.git_url();
        let default_ref = format!("refs/remotes/origin/{}", settings.default_branch);
        let branch_ref = format!("refs/remotes/origin/{}", settings.branch);

        git.fetch(&url, &settings.default_branch).await?;
        let expected_remote_sha = if git.remote_has_branch(&url, &settings.branch).await? {
            git.fetch(&url, &settings.branch).await?;
            git.read(["rev-parse", "--verify", &format!("{branch_ref}^{{commit}}")])
                .await?
        } else {
            String::new()
        };
        Ok(Self {
            settings,
            git,
            log,
            api,
            url,
            default_ref,
            expected_remote_sha,
        })
    }

    /// Rebuilds the automation branch from the default branch, runs the
    /// update and publishes, closes or pauses accordingly.
    pub async fn propose(self) -> Result<Proposal, Error> {
        let mut security = None;
        let outcome = match self.build(&mut security).await? {
            Build::Done(outcome) => outcome,
            Build::Ready {
                presentation,
                changed,
            } => self.publish(presentation, changed).await?,
        };
        Ok(Proposal {
            outcome,
            security: security.map(|security| security.counts),
        })
    }

    /// The remote automation branch tip this session inspected, empty when
    /// the branch is absent.
    pub(super) fn expected_remote_sha(&self) -> &str {
        &self.expected_remote_sha
    }

    /// Claims the automation branch, rebuilds it from the default branch,
    /// applies the security fixes and the update, and presents and validates
    /// the staged result. Records what the security step changed in
    /// `security` once the result is presented.
    pub(super) async fn build(&self, security: &mut Option<Security>) -> Result<Build, Error> {
        let Self {
            settings,
            git,
            log,
            api,
            default_ref,
            expected_remote_sha,
            ..
        } = self;
        let dir = &settings.project_dir;

        if !expected_remote_sha.is_empty() {
            match claim(git, api, settings, default_ref, expected_remote_sha).await? {
                Claim::Written => {}
                Claim::Recorded => log.line(format_args!(
                    "{} holds the commit its merge request records, written before the automation identity or commit message changed; replacing it",
                    settings.branch
                )),
                Claim::Foreign(open) => {
                    if settings.dry_run {
                        return Ok(Build::Done(Outcome::WouldPause));
                    }
                    return pause(api, settings, log, &open).await.map(Build::Done);
                }
            }
        }

        match settings.lane {
            Lane::Ordinary => {
                git.run([
                    "switch",
                    "--quiet",
                    "--force-create",
                    &settings.branch,
                    default_ref,
                ])
                .await?;
            }
            // The ordinary lane ran first in this checkout and may have left
            // its update staged, or its untracked output behind when it
            // stopped early. Neither belongs in this lane's commit.
            Lane::Major => {
                git.run([
                    "switch",
                    "--quiet",
                    "--discard-changes",
                    "--force-create",
                    &settings.branch,
                    default_ref,
                ])
                .await?;
                git.run(["clean", "--quiet", "--force", "-d"]).await?;
            }
        }

        if !settings.prepare_command.is_empty() {
            git::shell(dir, "prepare command", &settings.prepare_command).await?;
        }
        if !git.read(["status", "--porcelain"]).await?.is_empty() {
            return Err(Error::Refused(
                "The prepare command changed repository files; refusing to mix setup with updates"
                    .to_string(),
            ));
        }

        let applied = apply_changes(settings, git, log).await?;
        let (mut presentation, changed) = stage_and_present(
            settings,
            git,
            &applied.reports.update,
            applied.security.as_ref(),
            Base::Latest,
        )
        .await?;
        *security = applied.security;

        if !changed {
            return Ok(Build::Ready {
                presentation: Box::new(presentation),
                changed,
            });
        }

        if !settings.validation_command.is_empty() {
            git::shell(dir, "validation command", &settings.validation_command).await?;
        }
        let unstaged = !git.test(["diff", "--quiet"]).await?;
        let untracked = !git
            .read(["ls-files", "--others", "--exclude-standard"])
            .await?
            .is_empty();
        if unstaged || untracked {
            return Err(Error::Refused(
                "Validation changed uncommitted files; refusing to publish an unvalidated diff"
                    .to_string(),
            ));
        }
        presentation.validation.proposal_integrity_passed = true;
        write_artifact(
            settings,
            "upd-presentation.json",
            &presentation.to_artifact(),
        )?;
        Ok(Build::Ready {
            presentation: Box::new(presentation),
            changed,
        })
    }

    /// Publishes a built proposal: commits and lease-pushes the staged
    /// change and opens or updates its merge request, or, when nothing
    /// changed, closes the merge request the change made obsolete.
    pub(super) async fn publish(
        &self,
        presentation: Box<Presentation>,
        changed: bool,
    ) -> Result<Outcome, Error> {
        let Self {
            settings,
            git,
            log,
            api,
            url,
            expected_remote_sha,
            ..
        } = self;
        if !changed {
            return close_obsolete(api, git, settings, log, url, expected_remote_sha).await;
        }

        let title = if settings.mr_title.is_empty() {
            presentation.title.clone()
        } else {
            settings.mr_title.clone()
        };
        // Rewriting an identical commit would only change its timestamps, yet
        // it restarts the merge request's pipeline and approvals.
        let pushed = expected_remote_sha.is_empty()
            || !already_proposed(git, settings, expected_remote_sha).await?;
        if settings.dry_run {
            return Ok(Outcome::WouldPublish {
                title,
                push: pushed,
            });
        }

        let commit = if pushed {
            git.commit(
                &settings.commit_message,
                &settings.git_name,
                &settings.git_email,
            )
            .await?;
            git.read(["rev-parse", "HEAD"]).await?
        } else {
            log.line(format_args!(
                "{} already holds this update; leaving it as it is",
                settings.branch
            ));
            expected_remote_sha.clone()
        };
        // Unpushed, the same lease is a no-op that still proves the branch is
        // the commit this run inspected before the merge request is touched.
        if let Push::Stale(detail) = git
            .push_with_lease(url, &settings.branch, expected_remote_sha, &commit)
            .await?
        {
            return Err(lease_conflict(&settings.branch, &detail));
        }

        let existing = single_open_merge_request(api, settings).await?;
        let description = presentation.description(&commit);
        write_artifact(settings, "upd-mr-description.md", &description)?;

        let created = existing.is_none();
        let merge_request = match existing {
            None => MergeRequest::from_response(
                &api.create(
                    &settings.branch,
                    &settings.default_branch,
                    &title,
                    &description,
                )
                .await?,
            )?,
            Some(existing)
                if existing.raw["title"] == title.as_str()
                    && existing.raw["description"] == description.as_str() =>
            {
                existing
            }
            Some(existing) => MergeRequest::from_response(
                &api.edit(
                    existing.iid,
                    json!({"title": title, "description": description}),
                )
                .await?,
            )?,
        };
        log.line(format_args!("Merge request: {}", merge_request.web_url));

        let auto_merge = if settings.auto_merge {
            // Auto-merge armed earlier is bound to this same commit when
            // nothing was pushed; arming it again would be a no-op write,
            // unless it no longer removes the branch once merged.
            let armed = merge_request.auto_merge_enabled()
                && merge_request.raw["should_remove_source_branch"] == true;
            if pushed || !armed {
                api.enable_auto_merge(merge_request.iid, &commit).await?;
            }
            AutoMerge::Enabled
        } else if merge_request.auto_merge_enabled() {
            api.cancel_auto_merge(merge_request.iid).await?;
            log.line(match settings.lane {
                Lane::Ordinary => format!(
                    "Cancelled auto-merge on {}: auto-merge is off",
                    merge_request.web_url
                ),
                Lane::Major => format!(
                    "Cancelled auto-merge on {}: upd never merges a major upgrade",
                    merge_request.web_url
                ),
            });
            AutoMerge::Disabled
        } else {
            AutoMerge::Off
        };

        Ok(Outcome::Published {
            merge_request: merge_request.web_url,
            created,
            pushed,
            commit,
            auto_merge,
        })
    }
}

/// The freshness policy as the merge request states it.
fn policy_min_age(settings: &Settings) -> String {
    match (settings.min_age.as_str(), settings.min_age_floor.as_str()) {
        ("", "") => String::new(),
        ("", floor) => format!("repository configuration, at least {floor}"),
        (min_age, _) => min_age.to_string(),
    }
}

/// The reports the security fixes, the update and the audit of the updated
/// tree printed, as [`apply_changes`] ran them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Reports {
    /// Absent when security remediation is off.
    pub security: Option<Value>,
    pub update: Value,
    /// Absent unless the update changed the tree the fixes left.
    pub recheck: Option<Value>,
}

/// What [`apply_changes`] left in the checkout, read back from its reports.
pub(super) struct Applied {
    pub reports: Reports,
    /// What the security step changed, when it ran.
    pub security: Option<Security>,
}

/// Applies the security fixes and the update to the checkout, then audits
/// the result again when the update changed what the fixes left. Each
/// report passes its gate before the next step runs.
pub(super) async fn apply_changes(
    settings: &Settings,
    git: &Git,
    log: &Log,
) -> Result<Applied, Error> {
    let (security_report, mut security) = if settings.security_remediation {
        let report = run_security_fixes(settings, log).await?;
        let security = check_fixes(&report, settings.lock, log)?;
        (Some(report), Some(security))
    } else {
        (None, None)
    };
    // The tree the fixes left, to tell whether the update changed it.
    let fixed_tree = match &security {
        Some(security) if security.is_recheckable() => Some(staged_tree(git).await?),
        _ => None,
    };

    let update = run_updater(settings, log).await?;
    check_update(&update, log)?;

    // An update that changed the tree the fixes left can move a fixed
    // dependency back to a release an advisory affects, so the final tree is
    // audited again.
    let mut recheck = None;
    if let (Some(security), Some(fixed_tree)) = (security.as_mut(), fixed_tree)
        && staged_tree(git).await? != fixed_tree
    {
        let report = run_security_recheck(settings, log).await?;
        check_recheck(security, &report, log)?;
        recheck = Some(report);
    }
    Ok(Applied {
        reports: Reports {
            security: security_report,
            update,
            recheck,
        },
        security,
    })
}

/// Reads back reports another job's [`apply_changes`] printed, through the
/// same gates, so they stop a publish exactly as they would have stopped
/// the run that printed them.
pub(super) fn assess(
    settings: &Settings,
    reports: &Reports,
    log: &Log,
) -> Result<Option<Security>, Error> {
    if reports.security.is_some() != settings.security_remediation {
        return Err(Error::Refused(format!(
            "The lock job's result {} a security report, but security remediation is {} for this project",
            if reports.security.is_some() {
                "carries"
            } else {
                "lacks"
            },
            if settings.security_remediation {
                "on"
            } else {
                "off"
            },
        )));
    }
    let mut security = match &reports.security {
        Some(report) => Some(check_fixes(report, settings.lock, log)?),
        None => None,
    };
    check_update(&reports.update, log)?;
    match (security.as_mut(), &reports.recheck) {
        (Some(security), Some(report)) => check_recheck(security, report, log)?,
        (None, Some(_)) => {
            return Err(Error::Refused(
                "The lock job's result audits the updated tree without having applied security fixes"
                    .to_string(),
            ));
        }
        (_, None) => {}
    }
    Ok(security)
}

/// Reads the security step's report and refuses one that reports errors.
fn check_fixes(report: &Value, lock: bool, log: &Log) -> Result<Security, Error> {
    let security = Security::from_report(report, lock)?;
    log.line(security.summary_line(report)?);
    for warning in security.warnings() {
        log.line(warning);
    }
    if !present::report_is_error_free(report)? {
        return Err(Error::Refused(
            "upd reported errors while applying security fixes; refusing to publish a partial result"
                .to_string(),
        ));
    }
    Ok(security)
}

/// Refuses an update report that reports errors.
fn check_update(report: &Value, log: &Log) -> Result<(), Error> {
    log.line(present::summary_line(report)?);
    if !present::report_is_error_free(report)? {
        return Err(Error::Refused(
            "upd reported errors; refusing to publish a partial result".to_string(),
        ));
    }
    Ok(())
}

/// Reads the audit of the updated tree into `security`, refusing one that
/// reports errors.
fn check_recheck(security: &mut Security, report: &Value, log: &Log) -> Result<(), Error> {
    if !present::report_is_error_free(report)? {
        return Err(Error::Refused(
            "upd reported errors while auditing the updated tree; refusing to publish a partial result"
                .to_string(),
        ));
    }
    security.apply_recheck(report)?;
    for warning in security.recheck_warnings() {
        log.line(warning);
    }
    Ok(())
}

/// Stages everything in the checkout and presents it against `HEAD`, the
/// commit the proposal is built on. Returns the presentation and whether
/// the staged tree differs from that commit.
pub(super) async fn stage_and_present(
    settings: &Settings,
    git: &Git,
    report: &Value,
    security: Option<&Security>,
    base: Base<'_>,
) -> Result<(Presentation, bool), Error> {
    git.run(["add", "--all"]).await?;
    let changed = !git.test(["diff", "--cached", "--quiet"]).await?;
    let changed_paths = staged_paths(git).await?;
    let min_age = policy_min_age(settings);
    let merge_requests = settings.major_merge_requests_url();
    let lane = match settings.lane {
        Lane::Ordinary if settings.major_mr => present::Lane::BesideMajor {
            branch: &settings.major_branch,
            merge_requests: &merge_requests,
        },
        Lane::Ordinary => present::Lane::Ordinary,
        Lane::Major => present::Lane::Major,
    };
    let presentation = Presentation::from_report(
        report,
        &present::Context {
            lane,
            min_age: &min_age,
            max_bump: &settings.max_bump,
            lock: settings.lock,
            auto_merge: settings.auto_merge,
            validation_configured: !settings.validation_command.is_empty(),
            changed,
            changed_paths: &changed_paths,
            security,
            base,
        },
    )?;
    write_artifact(
        settings,
        "upd-presentation.json",
        &presentation.to_artifact(),
    )?;
    Ok((presentation, changed))
}

/// Creates the artifact directory and keeps it out of the repository's view,
/// so artifacts never reach a commit or trip a cleanliness check.
pub(super) async fn prepare_artifact_dir(git: &Git, dir: &Path) -> Result<(), Error> {
    fs::create_dir_all(dir.join(ARTIFACT_DIR))
        .map_err(|error| Error::Io(format!("cannot create {ARTIFACT_DIR}/: {error}")))?;
    let exclude = dir.join(
        git.read(["rev-parse", "--git-path", "info/exclude"])
            .await?,
    );
    let entry = format!("/{ARTIFACT_DIR}/");
    let current = match fs::read_to_string(&exclude) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(Error::Io(format!(
                "cannot read {}: {error}",
                exclude.display()
            )));
        }
    };
    if current.lines().any(|line| line == entry) {
        return Ok(());
    }
    let mut updated = current;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&entry);
    updated.push('\n');
    if let Some(parent) = exclude.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| Error::Io(format!("cannot create {}: {error}", parent.display())))?;
    }
    fs::write(&exclude, updated)
        .map_err(|error| Error::Io(format!("cannot write {}: {error}", exclude.display())))
}

/// Why a run may, or may not, replace the existing automation branch.
enum Claim {
    /// One commit on the default branch, with the configured author,
    /// committer and message.
    Written,
    /// One commit on the default branch that upd's open merge request
    /// records as the one it proposes, written under an earlier identity or
    /// message.
    Recorded,
    /// Work automation did not write, with the open merge requests to pause
    /// on.
    Foreign(Vec<Value>),
}

/// Decides whether the remote automation branch at `tip` holds exactly the
/// one commit this automation writes. The merge requests are consulted only
/// when the commit itself does not settle it.
async fn claim(
    git: &Git,
    api: &Client,
    settings: &Settings,
    default_ref: &str,
    tip: &str,
) -> Result<Claim, Error> {
    let single = is_single_commit_on(git, default_ref, tip).await?;
    if single && is_written_as_configured(git, settings, tip).await? {
        return Ok(Claim::Written);
    }
    let open = api
        .open_merge_requests(&settings.branch, &settings.default_branch)
        .await?;
    let record = present::commit_record(tip);
    let recorded = matches!(open.as_slice(), [only]
        if only["description"].as_str().is_some_and(|text| text.contains(&record)));
    Ok(if single && recorded {
        Claim::Recorded
    } else {
        Claim::Foreign(open)
    })
}

/// Whether `tip` is a single commit on top of the default branch's history.
async fn is_single_commit_on(git: &Git, default_ref: &str, tip: &str) -> Result<bool, Error> {
    let commits = git
        .read(["rev-list", "--count", &format!("{default_ref}..{tip}")])
        .await?;
    if commits != "1" {
        return Ok(false);
    }
    let parents = git.read(["rev-list", "--parents", "-n", "1", tip]).await?;
    let parents: Vec<&str> = parents.split_whitespace().skip(1).collect();
    Ok(parents.len() == 1
        && git
            .test(["merge-base", "--is-ancestor", parents[0], default_ref])
            .await?)
}

/// Whether `tip` carries the configured author, committer and message.
async fn is_written_as_configured(
    git: &Git,
    settings: &Settings,
    tip: &str,
) -> Result<bool, Error> {
    let identity = git
        .read(["show", "-s", "--format=%ae%x00%ce%x00%B", tip])
        .await?;
    let expected = format!(
        "{}\0{}\0{}",
        settings.git_email,
        settings.git_email,
        settings.commit_message.trim_end()
    );
    Ok(identity == expected)
}

/// Whether `tip`, a branch `claim` accepted, already is the commit
/// this run would write from the staged result: the same tree, directly on
/// `HEAD`, the commit the proposal is built on, with the configured identity
/// and message.
async fn already_proposed(git: &Git, settings: &Settings, tip: &str) -> Result<bool, Error> {
    let base = git.read(["rev-parse", "HEAD^{commit}"]).await?;
    let parent = git.read(["rev-parse", &format!("{tip}^")]).await?;
    if parent != base {
        return Ok(false);
    }
    let tree = git.read(["write-tree"]).await?;
    if tree != git.read(["rev-parse", &format!("{tip}^{{tree}}")]).await? {
        return Ok(false);
    }
    let identity = git
        .read(["show", "-s", "--format=%an%x00%ae%x00%cn%x00%ce%x00%B", tip])
        .await?;
    let expected = format!(
        "{name}\0{email}\0{name}\0{email}\0{}",
        settings.commit_message.trim_end(),
        name = settings.git_name,
        email = settings.git_email,
    );
    Ok(identity == expected)
}

async fn pause(
    api: &Client,
    settings: &Settings,
    log: &Log,
    open: &[Value],
) -> Result<Outcome, Error> {
    log.line(format_args!(
        "Paused: unexpected commits on {}; leaving the branch untouched",
        settings.branch
    ));
    if open.len() != 1 {
        return Err(Error::Refused(format!(
            "Cannot publish a pause notice: expected one open merge request, found {}",
            open.len()
        )));
    }
    let Some(description) = open[0]["description"].as_str() else {
        return Err(Error::Refused(
            "Cannot preserve the merge-request description while publishing a pause notice"
                .to_string(),
        ));
    };
    let merge_request = MergeRequest::from_response(&open[0])?;
    if description.contains(PAUSE_MARKER) {
        return Ok(Outcome::Paused {
            merge_request: merge_request.web_url,
            notice_added: false,
        });
    }
    let paused = format!("{description}\n{PAUSE_NOTICE}");
    api.edit(merge_request.iid, json!({"description": paused}))
        .await?;
    Ok(Outcome::Paused {
        merge_request: merge_request.web_url,
        notice_added: true,
    })
}

async fn run_updater(settings: &Settings, log: &Log) -> Result<Value, Error> {
    let mut args: Vec<&str> = vec!["update", "--apply", "--format", "json"];
    if settings.lock {
        args.push("--lock");
    }
    // The ordinary lane's ceiling and the major lane's level are exclusive,
    // so which one the updater gets follows from the lane alone.
    let max_bump = match settings.lane {
        Lane::Ordinary => settings.max_bump.as_str(),
        // `--strict-bump` also holds the pins, revisions and rewrites that
        // name no level, which the ordinary lane already carries.
        Lane::Major => {
            args.extend(["--only-bump", "major", "--strict-bump"]);
            ""
        }
    };
    let config = settings
        .config
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    for (flag, value) in [
        ("--config", config.as_str()),
        ("--min-age", &settings.min_age),
        ("--min-age-floor", &settings.min_age_floor),
        ("--max-bump", max_bump),
        ("--lang", &settings.langs),
        ("--exclude-lang", &settings.exclude_langs),
        ("--package", &settings.packages),
    ] {
        if !value.is_empty() {
            args.extend([flag, value]);
        }
    }
    args.extend(settings.paths.iter().map(String::as_str));
    invoke_updater(
        settings,
        log,
        &args,
        Invocation {
            artifact: "upd-report.json",
            accepted: &[0],
            failed: "the updater failed",
        },
    )
    .await
}

/// Moves every dependency with a published advisory to the lowest release
/// that resolves it. The step answers to the advisories, not to the update
/// policy, so the bump ceiling and package filter are never passed on. The
/// freshness window is, but only to read each relock back against: a fix is
/// written however young, and what else its relock locked inside the window
/// is reported rather than held.
async fn run_security_fixes(settings: &Settings, log: &Log) -> Result<Value, Error> {
    let mut args: Vec<&str> = vec![
        "audit",
        "--fix-audit",
        "--apply",
        "--full-precision",
        "--format",
        "json",
    ];
    // Without lockfile regeneration a fix writes the manifest alone and
    // reports it pending a relock, instead of editing the lockfile in place.
    if !settings.lock {
        args.push("--no-lock");
    }
    let config = settings
        .config
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    for (flag, value) in [
        ("--config", config.as_str()),
        ("--min-age", &settings.min_age),
        ("--min-age-floor", &settings.min_age_floor),
        ("--lang", &settings.langs),
        ("--exclude-lang", &settings.exclude_langs),
    ] {
        if !value.is_empty() {
            args.extend([flag, value]);
        }
    }
    args.extend(settings.paths.iter().map(String::as_str));
    // Exit 6 reports a fix a requirement blocked or an advisory no release
    // resolves; the rest of the fixes applied and the report lists both.
    invoke_updater(
        settings,
        log,
        &args,
        Invocation {
            artifact: "upd-security-report.json",
            accepted: &[0, 6],
            failed: "the updater failed while applying security fixes",
        },
    )
    .await
}

/// Audits the tree the update left, read-only, over the scope the security
/// step covered.
async fn run_security_recheck(settings: &Settings, log: &Log) -> Result<Value, Error> {
    let mut args: Vec<&str> = vec!["audit", "--format", "json"];
    let config = settings
        .config
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    for (flag, value) in [
        ("--config", config.as_str()),
        ("--lang", &settings.langs),
        ("--exclude-lang", &settings.exclude_langs),
    ] {
        if !value.is_empty() {
            args.extend([flag, value]);
        }
    }
    args.extend(settings.paths.iter().map(String::as_str));
    // Exit 6 is the finding this audit looks for, not a failure.
    invoke_updater(
        settings,
        log,
        &args,
        Invocation {
            artifact: "upd-security-recheck.json",
            accepted: &[0, 6],
            failed: "the updater failed while auditing the updated tree",
        },
    )
    .await
}

/// Stages every change and names the tree the index then records.
async fn staged_tree(git: &Git) -> Result<String, Error> {
    git.run(["add", "--all"]).await?;
    git.read(["write-tree"]).await
}

/// How one updater command is run and judged.
struct Invocation<'a> {
    /// The pipeline artifact that keeps the command's report.
    artifact: &'a str,
    /// Exit codes that still leave a report to act on.
    accepted: &'a [i32],
    /// The failure message, before the exit status.
    failed: &'a str,
}

/// Runs the updater and reads the single JSON report it prints, keeping the
/// report as a pipeline artifact whatever the outcome.
async fn invoke_updater(
    settings: &Settings,
    log: &Log,
    args: &[&str],
    invocation: Invocation<'_>,
) -> Result<Value, Error> {
    // `spawn` rather than `output`: `output` would capture stderr too, hiding
    // the updater's progress and diagnostics from a direct job log.
    let mut command = git::child(&settings.project_dir, &settings.updater);
    if settings.no_build {
        command.env("UV_NO_BUILD", "1");
    }
    let child = command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(if log.is_buffered() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .spawn()
        .map_err(|error| {
            Error::Io(format!(
                "cannot start the updater {}: {error}",
                settings.updater.display()
            ))
        })?;
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| Error::Io(format!("cannot read the updater output: {error}")))?;
    log.raw(&output.stderr);
    write_artifact(
        settings,
        invocation.artifact,
        &String::from_utf8_lossy(&output.stdout),
    )?;
    let accepted = output
        .status
        .code()
        .is_some_and(|code| invocation.accepted.contains(&code));
    if !accepted {
        let message = format!("{} ({})", invocation.failed, output.status);
        return Err(match output.status.code() {
            Some(3) => Error::Network(message),
            Some(4) => Error::Input(message),
            _ => Error::Io(message),
        });
    }
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(Value::Null | Value::Bool(false)) => Err(Error::Io(
            "the updater report is empty (null or false)".to_string(),
        )),
        Ok(report) => Ok(report),
        Err(error) => Err(Error::Io(format!(
            "the updater report is not a single JSON document: {error}"
        ))),
    }
}

/// Repository paths the index changes relative to HEAD, exactly as git
/// stores them.
async fn staged_paths(git: &Git) -> Result<Vec<String>, Error> {
    let raw = git.read(["diff", "--cached", "--name-only", "-z"]).await?;
    Ok(raw
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect())
}

async fn close_obsolete(
    api: &Client,
    git: &Git,
    settings: &Settings,
    log: &Log,
    url: &str,
    expected_remote_sha: &str,
) -> Result<Outcome, Error> {
    // The lease-protected delete runs before the close: a human commit pushed
    // after the ownership check fails the lease, and the merge request that
    // carries it stays open.
    let existing = single_open_merge_request(api, settings).await?;
    let branch_deleted = !expected_remote_sha.is_empty();
    if settings.dry_run {
        return Ok(if existing.is_none() && !branch_deleted {
            Outcome::Clean
        } else {
            Outcome::WouldClose {
                merge_request: existing.map(|merge_request| merge_request.web_url),
                delete_branch: branch_deleted,
            }
        });
    }
    if branch_deleted {
        if let Push::Stale(detail) = git
            .push_with_lease(url, &settings.branch, expected_remote_sha, "")
            .await?
        {
            return Err(lease_conflict(&settings.branch, &detail));
        }
        log.line(format_args!(
            "Removed obsolete automation branch: {}",
            settings.branch
        ));
    }
    let merge_request = match existing {
        Some(merge_request) => {
            api.edit(merge_request.iid, json!({"state_event": "close"}))
                .await?;
            log.line(format_args!(
                "Closed obsolete merge request: {}",
                merge_request.web_url
            ));
            Some(merge_request.web_url)
        }
        None => None,
    };
    if merge_request.is_none() && !branch_deleted {
        return Ok(Outcome::Clean);
    }
    Ok(Outcome::Closed {
        merge_request,
        branch_deleted,
    })
}

async fn single_open_merge_request(
    api: &Client,
    settings: &Settings,
) -> Result<Option<MergeRequest>, Error> {
    let open = api
        .open_merge_requests(&settings.branch, &settings.default_branch)
        .await?;
    match open.as_slice() {
        [] => Ok(None),
        [only] => MergeRequest::from_response(only).map(Some),
        _ => Err(Error::Refused(format!(
            "More than one open upd merge request uses {}; refusing to choose one",
            settings.branch
        ))),
    }
}

pub(super) fn lease_conflict(branch: &str, detail: &str) -> Error {
    Error::Conflict(format!(
        "{branch} changed on the remote while this run was working; nothing was overwritten ({detail})"
    ))
}

pub(super) fn write_artifact(settings: &Settings, name: &str, content: &str) -> Result<(), Error> {
    let path = settings.artifact(name);
    fs::write(&path, content)
        .map_err(|error| Error::Io(format!("cannot write {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn settings(overrides: &[(&str, &str)]) -> Result<Settings, Error> {
        let mut vars: HashMap<&str, &str> = HashMap::from([
            ("UPD_GITLAB_TOKEN", "token"),
            ("CI_API_V4_URL", "https://gitlab.example.test/api/v4"),
            ("CI_DEFAULT_BRANCH", "main"),
            ("CI_PROJECT_DIR", "/builds/group/project"),
            ("CI_PROJECT_ID", "42"),
            ("CI_PROJECT_PATH", "group/project"),
            ("CI_SERVER_URL", "https://gitlab.example.test"),
            ("UPD_EXECUTABLE", "/usr/local/bin/upd"),
        ]);
        vars.extend(overrides.iter().copied());
        Settings::from_lookup(|name| vars.get(name).map(|value| value.to_string()), false)
    }

    #[test]
    fn unset_optional_settings_take_the_template_defaults() {
        let settings = settings(&[]).unwrap();
        assert_eq!(settings.branch, "automation/upd-dependencies");
        assert_eq!(
            settings.commit_message,
            "chore(deps): update dependencies with upd"
        );
        assert_eq!(settings.git_email, "upd-automation@noreply.invalid");
        assert_eq!(settings.paths, vec!["."]);
        assert!(!settings.lock && !settings.auto_merge);
        assert_eq!(
            settings.git_url(),
            "https://gitlab.example.test/group/project.git"
        );
    }

    #[test]
    fn missing_or_malformed_settings_are_input_errors() {
        for overrides in [
            &[("UPD_GITLAB_TOKEN", "")][..],
            &[("UPD_LOCK", "yes")],
            &[("UPD_AUTO_MERGE", "1")],
            &[("UPD_SECURITY_REMEDIATION", "yes")],
            &[("UPD_BRANCH", "main")],
            &[("CI_PROJECT_ID", "../../groups/1")],
        ] {
            let error = settings(overrides).unwrap_err();
            assert_eq!(error.exit_code(), 4, "{overrides:?}: {error}");
        }
    }

    #[test]
    fn the_major_lane_is_off_unless_asked_for_and_has_its_own_defaults() {
        let off = settings(&[]).unwrap();
        assert!(!off.major_mr);
        assert!(off.major_lane().is_none());

        let on = settings(&[("UPD_MAJOR_MR", "true"), ("UPD_MAX_BUMP", "minor")]).unwrap();
        assert_eq!(on.major_branch, "automation/upd-dependencies-major");
        assert_eq!(
            on.major_commit_message,
            "chore(deps): update major dependencies with upd"
        );
        let major = on.major_lane().unwrap();
        assert_eq!(major.branch, "automation/upd-dependencies-major");
        assert_eq!(
            major.commit_message,
            "chore(deps): update major dependencies with upd"
        );
        assert_eq!(major.lane, Lane::Major);
        assert!(major.major_lane().is_none());
        assert_eq!(on.lane, Lane::Ordinary);
    }

    #[test]
    fn security_remediation_is_on_unless_turned_off() {
        assert!(settings(&[]).unwrap().security_remediation);
        assert!(
            settings(&[("UPD_SECURITY_REMEDIATION", "")])
                .unwrap()
                .security_remediation
        );
        assert!(
            settings(&[("UPD_SECURITY_REMEDIATION", "true")])
                .unwrap()
                .security_remediation
        );
        assert!(
            !settings(&[("UPD_SECURITY_REMEDIATION", "false")])
                .unwrap()
                .security_remediation
        );
    }

    #[test]
    fn only_the_ordinary_lane_applies_security_fixes() {
        let on = settings(&[("UPD_MAJOR_MR", "true"), ("UPD_MAX_BUMP", "minor")]).unwrap();
        assert!(on.security_remediation);
        assert!(!on.major_lane().unwrap().security_remediation);
    }

    #[test]
    fn the_major_lane_never_auto_merges_and_ignores_the_ordinary_title() {
        let on = settings(&[
            ("UPD_MAJOR_MR", "true"),
            ("UPD_MAX_BUMP", "patch"),
            ("UPD_AUTO_MERGE", "true"),
            ("UPD_MR_TITLE", "chore: dependencies"),
            ("UPD_MAJOR_BRANCH", "deps/major"),
            ("UPD_MAJOR_COMMIT_MESSAGE", "chore(deps): majors"),
        ])
        .unwrap();
        assert!(on.auto_merge);
        let major = on.major_lane().unwrap();
        assert!(!major.auto_merge);
        assert!(major.mr_title.is_empty());
        assert_eq!(major.branch, "deps/major");
        assert_eq!(major.commit_message, "chore(deps): majors");
        assert_eq!(major.max_bump, "", "the major lane passes no ceiling");
    }

    #[test]
    fn a_major_lane_that_would_repeat_the_ordinary_lane_is_refused() {
        for overrides in [
            &[("UPD_MAJOR_MR", "true")][..],
            &[("UPD_MAJOR_MR", "true"), ("UPD_MAX_BUMP", "major")],
            &[
                ("UPD_MAJOR_MR", "true"),
                ("UPD_MAX_BUMP", "minor"),
                ("UPD_MAJOR_BRANCH", "automation/upd-dependencies"),
            ],
            &[
                ("UPD_MAJOR_MR", "true"),
                ("UPD_MAX_BUMP", "minor"),
                ("UPD_MAJOR_BRANCH", "main"),
            ],
            // Git cannot hold a branch and another nested under its name.
            &[
                ("UPD_MAJOR_MR", "true"),
                ("UPD_MAX_BUMP", "minor"),
                ("UPD_MAJOR_BRANCH", "automation/upd-dependencies/major"),
            ],
            &[
                ("UPD_MAJOR_MR", "true"),
                ("UPD_MAX_BUMP", "minor"),
                ("UPD_MAJOR_BRANCH", "automation"),
            ],
            &[
                ("UPD_MAJOR_MR", "true"),
                ("UPD_MAX_BUMP", "minor"),
                ("UPD_MAJOR_BRANCH", "main/major"),
            ],
            &[("UPD_MAJOR_MR", "yes"), ("UPD_MAX_BUMP", "minor")],
            // The ordinary lane would refuse a ceiling it cannot read, so the
            // major lane must not run beside it.
            &[("UPD_MAJOR_MR", "true"), ("UPD_MAX_BUMP", "minro")],
            &[("UPD_MAJOR_MR", "true"), ("UPD_MAX_BUMP", "Minor")],
        ] {
            let error = settings(overrides).unwrap_err();
            assert_eq!(error.exit_code(), 4, "{overrides:?}: {error}");
        }
        // Off, the major inputs are not consulted at all.
        settings(&[("UPD_MAJOR_BRANCH", "automation/upd-dependencies")]).unwrap();
    }

    #[test]
    fn an_automation_branch_nested_with_the_default_branch_is_refused() {
        for branch in ["main", "main/deps"] {
            let error = settings(&[("UPD_BRANCH", branch)]).unwrap_err();
            assert_eq!(error.exit_code(), 4, "{branch}: {error}");
        }
        settings(&[("UPD_BRANCH", "main-deps")]).unwrap();
    }

    #[test]
    fn branches_collide_only_when_git_could_not_hold_both() {
        for (a, b) in [("deps", "deps"), ("deps", "deps/major"), ("a/b", "a/b/c/d")] {
            assert!(branches_collide(a, b), "{a} and {b}");
            assert!(branches_collide(b, a), "{b} and {a}");
        }
        for (a, b) in [
            ("deps", "deps-major"),
            ("deps/a", "deps/b"),
            (
                "automation/upd-dependencies",
                "automation/upd-dependencies-major",
            ),
        ] {
            assert!(!branches_collide(a, b), "{a} and {b}");
            assert!(!branches_collide(b, a), "{b} and {a}");
        }
    }

    #[test]
    fn the_major_merge_requests_link_names_the_encoded_branch() {
        let on = settings(&[
            ("UPD_MAJOR_MR", "true"),
            ("UPD_MAX_BUMP", "minor"),
            ("CI_SERVER_URL", "https://gitlab.example.test/"),
        ])
        .unwrap();
        assert_eq!(
            on.major_merge_requests_url(),
            "https://gitlab.example.test/group/project/-/merge_requests?state=opened&source_branch=automation%2Fupd-dependencies-major"
        );
    }

    #[test]
    fn whitespace_separated_paths_are_split() {
        assert_eq!(
            settings(&[("UPD_PATHS", " api  web\tdocs ")])
                .unwrap()
                .paths,
            vec!["api", "web", "docs"]
        );
    }

    #[test]
    fn the_text_outcome_says_whether_anything_was_pushed() {
        let branch = "automation/upd-dependencies";
        let url = "https://gitlab.example.test/p/-/merge_requests/7";
        let published = |created, pushed| Outcome::Published {
            merge_request: url.to_string(),
            created,
            pushed,
            commit: "0123456789abcdef".to_string(),
            auto_merge: AutoMerge::Enabled,
        };
        assert_eq!(
            published(true, true).render_text(branch),
            format!("Created {url} for 0123456789ab on {branch}; auto-merge enabled.")
        );
        assert_eq!(
            published(false, true).render_text(branch),
            format!("Updated {url} to 0123456789ab on {branch}; auto-merge enabled.")
        );
        assert_eq!(
            published(true, false).render_text(branch),
            format!("Created {url} for 0123456789ab, already on {branch}; auto-merge enabled.")
        );
        assert_eq!(
            published(false, false).render_text(branch),
            format!(
                "{url} already proposes 0123456789ab on {branch}; nothing pushed; auto-merge enabled."
            )
        );
        let would = |push| Outcome::WouldPublish {
            title: "chore(deps): refresh".to_string(),
            push,
        };
        assert_eq!(
            would(true).render_text(branch),
            format!("Dry run: would publish \"chore(deps): refresh\" on {branch}.")
        );
        assert_eq!(
            would(false).render_text(branch),
            format!(
                "Dry run: {branch} already holds \"chore(deps): refresh\"; would push nothing and only refresh its merge request."
            )
        );
    }

    #[test]
    fn outcome_json_names_the_command_and_branch() {
        let outcome = Outcome::Published {
            merge_request: "https://gitlab.example.test/p/-/merge_requests/7".to_string(),
            created: true,
            pushed: true,
            commit: "abc".to_string(),
            auto_merge: AutoMerge::Off,
        };
        assert_eq!(
            outcome.to_json("automation/upd-dependencies"),
            json!({
                "command": "gitlab run",
                "outcome": "published",
                "branch": "automation/upd-dependencies",
                "merge_request": "https://gitlab.example.test/p/-/merge_requests/7",
                "created": true,
                "pushed": true,
                "commit": "abc",
                "auto_merge": "off",
            })
        );
    }
}
