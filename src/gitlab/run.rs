//! `upd gitlab run`: maintain one rolling dependency merge request.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{Value, json};

use super::Error;
use super::api::{Client, MergeRequest};
use super::git::{self, Git, Push};
use super::present::{self, Presentation};

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
    pub auto_merge: bool,
    pub prepare_command: String,
    pub validation_command: String,
    /// The updater to run; this executable unless `UPD_EXECUTABLE` names one.
    pub updater: PathBuf,
    /// Configuration file, relative to the checkout, the updater must read
    /// instead of discovering one.
    pub config: Option<PathBuf>,
    /// Report what the run would do without pushing or writing to GitLab.
    pub dry_run: bool,
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
            auto_merge: flag("UPD_AUTO_MERGE")?,
            prepare_command: optional("UPD_PREPARE_COMMAND", ""),
            validation_command: optional("UPD_VALIDATION_COMMAND", ""),
            updater,
            config: None,
            dry_run,
        };
        if settings.project_id.contains('/') {
            return Err(Error::Input(
                "CI_PROJECT_ID must be a numeric project ID".to_string(),
            ));
        }
        if settings.branch == settings.default_branch {
            return Err(Error::Input(
                "The automation branch must differ from the default branch".to_string(),
            ));
        }
        Ok(settings)
    }

    pub(super) fn git_url(&self) -> String {
        format!(
            "{}/{}.git",
            self.server_url.trim_end_matches('/'),
            self.project_path
        )
    }

    fn artifact(&self, name: &str) -> PathBuf {
        self.project_dir.join(ARTIFACT_DIR).join(name)
    }
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

pub async fn run(settings: &Settings, log: &Log) -> Result<Outcome, Error> {
    Session::open(settings.clone(), log).await?.propose().await
}

impl<'a> Session<'a> {
    /// Validates the branch name and fetches both branches into the
    /// checkout at `settings.project_dir`.
    pub async fn open(settings: Settings, log: &'a Log) -> Result<Self, Error> {
        let dir = &settings.project_dir;
        let git = Git::new(dir, &settings.token)?;
        if !git::child(dir, "git")
            .args(["check-ref-format", "--branch", &settings.branch])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|error| Error::Io(format!("cannot start git: {error}")))?
            .success()
        {
            return Err(Error::Input(format!(
                "Invalid automation branch: {}",
                settings.branch
            )));
        }

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
    pub async fn propose(self) -> Result<Outcome, Error> {
        let Self {
            settings,
            git,
            log,
            api,
            url,
            default_ref,
            expected_remote_sha,
        } = self;
        let settings = &settings;
        let dir = &settings.project_dir;

        if !expected_remote_sha.is_empty() {
            match claim(&git, &api, settings, &default_ref, &expected_remote_sha).await? {
                Claim::Written => {}
                Claim::Recorded => log.line(format_args!(
                    "{} holds the commit its merge request records, written before the automation identity or commit message changed; replacing it",
                    settings.branch
                )),
                Claim::Foreign(open) => {
                    if settings.dry_run {
                        return Ok(Outcome::WouldPause);
                    }
                    return pause(&api, settings, log, &open).await;
                }
            }
        }

        git.run([
            "switch",
            "--quiet",
            "--force-create",
            &settings.branch,
            &default_ref,
        ])
        .await?;

        if !settings.prepare_command.is_empty() {
            git::shell(dir, "prepare command", &settings.prepare_command).await?;
        }
        if !git.read(["status", "--porcelain"]).await?.is_empty() {
            return Err(Error::Refused(
                "The prepare command changed repository files; refusing to mix setup with updates"
                    .to_string(),
            ));
        }

        let report = run_updater(settings, log).await?;
        log.line(present::summary_line(&report)?);
        if !present::report_is_error_free(&report)? {
            return Err(Error::Refused(
                "upd reported errors; refusing to publish a partial result".to_string(),
            ));
        }

        git.run(["add", "--all"]).await?;
        let changed = !git.test(["diff", "--cached", "--quiet"]).await?;
        let changed_paths = staged_paths(&git).await?;
        let min_age = policy_min_age(settings);
        let mut presentation = Presentation::from_report(
            &report,
            &present::Context {
                min_age: &min_age,
                max_bump: &settings.max_bump,
                lock: settings.lock,
                auto_merge: settings.auto_merge,
                validation_configured: !settings.validation_command.is_empty(),
                changed,
                changed_paths: &changed_paths,
            },
        )?;
        write_artifact(
            settings,
            "upd-presentation.json",
            &presentation.to_artifact(),
        )?;

        if !changed {
            return close_obsolete(&api, &git, settings, log, &url, &expected_remote_sha).await;
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
        let title = if settings.mr_title.is_empty() {
            presentation.title.clone()
        } else {
            settings.mr_title.clone()
        };
        // Rewriting an identical commit would only change its timestamps, yet
        // it restarts the merge request's pipeline and approvals.
        let pushed = expected_remote_sha.is_empty()
            || !already_proposed(&git, settings, &default_ref, &expected_remote_sha).await?;
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
            .push_with_lease(&url, &settings.branch, &expected_remote_sha, &commit)
            .await?
        {
            return Err(lease_conflict(&settings.branch, &detail));
        }

        let existing = single_open_merge_request(&api, settings).await?;
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

/// Creates the artifact directory and keeps it out of the repository's view,
/// so artifacts never reach a commit or trip a cleanliness check.
async fn prepare_artifact_dir(git: &Git, dir: &Path) -> Result<(), Error> {
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
/// the current default branch, with the configured identity and message.
async fn already_proposed(
    git: &Git,
    settings: &Settings,
    default_ref: &str,
    tip: &str,
) -> Result<bool, Error> {
    let base = git
        .read(["rev-parse", &format!("{default_ref}^{{commit}}")])
        .await?;
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
    let config = settings
        .config
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    for (flag, value) in [
        ("--config", &config),
        ("--min-age", &settings.min_age),
        ("--min-age-floor", &settings.min_age_floor),
        ("--max-bump", &settings.max_bump),
        ("--lang", &settings.langs),
        ("--exclude-lang", &settings.exclude_langs),
        ("--package", &settings.packages),
    ] {
        if !value.is_empty() {
            args.extend([flag, value.as_str()]);
        }
    }
    args.extend(settings.paths.iter().map(String::as_str));

    // `spawn` rather than `output`: `output` would capture stderr too, hiding
    // the updater's progress and diagnostics from a direct job log.
    let child = git::child(&settings.project_dir, &settings.updater)
        .args(&args)
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
        "upd-report.json",
        &String::from_utf8_lossy(&output.stdout),
    )?;
    if !output.status.success() {
        let message = format!("the updater failed ({})", output.status);
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

fn lease_conflict(branch: &str, detail: &str) -> Error {
    Error::Conflict(format!(
        "{branch} changed on the remote while this run was working; nothing was overwritten ({detail})"
    ))
}

fn write_artifact(settings: &Settings, name: &str, content: &str) -> Result<(), Error> {
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
            &[("UPD_BRANCH", "main")],
            &[("CI_PROJECT_ID", "../../groups/1")],
        ] {
            let error = settings(overrides).unwrap_err();
            assert_eq!(error.exit_code(), 4, "{overrides:?}: {error}");
        }
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
