//! `upd gitlab run`: maintain one rolling dependency merge request.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

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
    pub packages: String,
    pub min_age: String,
    pub max_bump: String,
    pub lock: bool,
    pub auto_merge: bool,
    pub prepare_command: String,
    pub validation_command: String,
    /// The updater to run; this executable unless `UPD_EXECUTABLE` names one.
    pub updater: PathBuf,
}

impl Settings {
    pub fn from_env() -> Result<Self, Error> {
        Self::from_lookup(|name| env::var(name).ok())
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, Error> {
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
            packages: optional("UPD_PACKAGES", ""),
            min_age: optional("UPD_MIN_AGE", ""),
            max_bump: optional("UPD_MAX_BUMP", ""),
            lock: flag("UPD_LOCK")?,
            auto_merge: flag("UPD_AUTO_MERGE")?,
            prepare_command: optional("UPD_PREPARE_COMMAND", ""),
            validation_command: optional("UPD_VALIDATION_COMMAND", ""),
            updater,
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

    fn git_url(&self) -> String {
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
    /// The update was pushed and its merge request created or refreshed.
    Published {
        merge_request: String,
        created: bool,
        commit: String,
        auto_merge: AutoMerge,
    },
    /// The branch holds commits automation did not write; it was left alone.
    Paused {
        merge_request: String,
        notice_added: bool,
    },
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
                commit,
                auto_merge,
            } => format!(
                "{} {merge_request} for {} on {branch}; auto-merge {}.",
                if *created { "Created" } else { "Updated" },
                commit.get(..12).unwrap_or(commit),
                match auto_merge {
                    AutoMerge::Enabled => "enabled",
                    AutoMerge::Disabled => "disabled",
                    AutoMerge::Off => "off",
                },
            ),
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
        }
    }
}

pub async fn run(settings: &Settings) -> Result<Outcome, Error> {
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
    let api = Client::new(&settings.api_url, &settings.project_id, &settings.token)?;
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

    if !expected_remote_sha.is_empty()
        && !branch_is_owned(&git, settings, &default_ref, &expected_remote_sha).await?
    {
        return pause(&api, settings).await;
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

    let report = run_updater(settings).await?;
    eprintln!("{}", present::summary_line(&report)?);
    if !present::report_is_error_free(&report)? {
        return Err(Error::Refused(
            "upd reported errors; refusing to publish a partial result".to_string(),
        ));
    }

    git.run(["add", "--all"]).await?;
    let changed = !git.test(["diff", "--cached", "--quiet"]).await?;
    let changed_paths = staged_paths(&git).await?;
    let mut presentation = Presentation::from_report(
        &report,
        &present::Context {
            min_age: &settings.min_age,
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
        return close_obsolete(&api, &git, settings, &url, &expected_remote_sha).await;
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

    git.commit(
        &settings.commit_message,
        &settings.git_name,
        &settings.git_email,
    )
    .await?;
    let commit = git.read(["rev-parse", "HEAD"]).await?;
    if let Push::Stale(detail) = git
        .push_with_lease(&url, &settings.branch, &expected_remote_sha, "HEAD")
        .await?
    {
        return Err(lease_conflict(&settings.branch, &detail));
    }

    let existing = single_open_merge_request(&api, settings).await?;
    let title = if settings.mr_title.is_empty() {
        presentation.title.clone()
    } else {
        settings.mr_title.clone()
    };
    let description = presentation.description();
    write_artifact(settings, "upd-mr-description.md", &description)?;

    let created = existing.is_none();
    let response = match existing {
        None => {
            api.create(
                &settings.branch,
                &settings.default_branch,
                &title,
                &description,
            )
            .await?
        }
        Some(existing) => {
            api.edit(
                existing.iid,
                json!({"title": title, "description": description}),
            )
            .await?
        }
    };
    let merge_request = MergeRequest::from_response(&response)?;
    eprintln!("Merge request: {}", merge_request.web_url);

    let auto_merge = if settings.auto_merge {
        api.enable_auto_merge(merge_request.iid, &commit).await?;
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
        commit,
        auto_merge,
    })
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

/// Whether the remote automation branch holds exactly the one commit this
/// automation writes: a single commit on top of the default branch, with the
/// configured author, committer and message.
async fn branch_is_owned(
    git: &Git,
    settings: &Settings,
    default_ref: &str,
    tip: &str,
) -> Result<bool, Error> {
    let commits = git
        .read(["rev-list", "--count", &format!("{default_ref}..{tip}")])
        .await?;
    if commits != "1" {
        return Ok(false);
    }
    let parents = git.read(["rev-list", "--parents", "-n", "1", tip]).await?;
    let parents: Vec<&str> = parents.split_whitespace().skip(1).collect();
    if parents.len() != 1
        || !git
            .test(["merge-base", "--is-ancestor", parents[0], default_ref])
            .await?
    {
        return Ok(false);
    }
    let identity = git
        .read(["show", "-s", "--format=%ae%x00%ce%x00%s", tip])
        .await?;
    let expected = format!(
        "{}\0{}\0{}",
        settings.git_email, settings.git_email, settings.commit_message
    );
    Ok(identity == expected)
}

async fn pause(api: &Client, settings: &Settings) -> Result<Outcome, Error> {
    eprintln!(
        "Paused: unexpected commits on {}; leaving the branch untouched",
        settings.branch
    );
    let open = api
        .open_merge_requests(&settings.branch, &settings.default_branch)
        .await?;
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

async fn run_updater(settings: &Settings) -> Result<Value, Error> {
    let mut args: Vec<&str> = vec!["update", "--apply", "--format", "json"];
    if settings.lock {
        args.push("--lock");
    }
    for (flag, value) in [
        ("--min-age", &settings.min_age),
        ("--max-bump", &settings.max_bump),
        ("--lang", &settings.langs),
        ("--package", &settings.packages),
    ] {
        if !value.is_empty() {
            args.extend([flag, value.as_str()]);
        }
    }
    args.extend(settings.paths.iter().map(String::as_str));

    // `spawn` rather than `output`: `output` would capture stderr too, hiding
    // the updater's progress and diagnostics from the job log.
    let child = git::child(&settings.project_dir, &settings.updater)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
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
    url: &str,
    expected_remote_sha: &str,
) -> Result<Outcome, Error> {
    let existing = single_open_merge_request(api, settings).await?;
    let merge_request = match existing {
        Some(merge_request) => {
            api.edit(merge_request.iid, json!({"state_event": "close"}))
                .await?;
            eprintln!("Closed obsolete merge request: {}", merge_request.web_url);
            Some(merge_request.web_url)
        }
        None => None,
    };
    let branch_deleted = !expected_remote_sha.is_empty();
    if branch_deleted {
        if let Push::Stale(detail) = git
            .push_with_lease(url, &settings.branch, expected_remote_sha, "")
            .await?
        {
            return Err(lease_conflict(&settings.branch, &detail));
        }
        eprintln!("Removed obsolete automation branch: {}", settings.branch);
    }
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
        Settings::from_lookup(|name| vars.get(name).map(|value| value.to_string()))
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
    fn outcome_json_names_the_command_and_branch() {
        let outcome = Outcome::Published {
            merge_request: "https://gitlab.example.test/p/-/merge_requests/7".to_string(),
            created: true,
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
                "commit": "abc",
                "auto_merge": "off",
            })
        );
    }
}
