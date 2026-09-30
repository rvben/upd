//! `upd gitlab org run`: one rolling dependency merge request in every
//! project of a group that opts in through its own configuration file.
//!
//! The job runs in one trusted project with a token that can reach the whole
//! group. A project takes part only when the configuration file at the root
//! of its default branch sets `[automation] dependency_updates = true`; the
//! repository cannot supply commands, so no repository code runs. Each
//! project is then handled exactly as `upd gitlab run` handles its own.

use std::collections::BTreeSet;
use std::env;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use clap::ValueEnum;
use futures::StreamExt;
use globset::{Glob, GlobMatcher};
use serde_json::{Value, json};

use super::Error;
use super::api::Client;
use super::git::{self, Git};
use super::run::{self, Lane, Log, Outcome, Session, Settings};
use crate::config::{CONFIG_FILE_NAMES, UpdConfig};
use crate::lockfile::LockfileType;
use crate::updater::Lang;

const DEFAULT_CONCURRENCY: usize = 4;
/// Where a project gets its lockfiles regenerated under its own token.
const SINGLE_PROJECT_TEMPLATE: &str = "ci/gitlab-dependency-update.yml";
const MAX_CONCURRENCY: usize = 16;

/// Job configuration, read from the environment the organization template
/// provides.
#[derive(Debug, Clone)]
pub struct OrgSettings {
    pub token: String,
    pub api_url: String,
    pub server_url: String,
    pub group: String,
    /// The project running the job; never updated by it.
    pub central_project: Option<u64>,
    pub branch: String,
    pub commit_message: String,
    pub git_name: String,
    pub git_email: String,
    pub langs: String,
    /// Shortest release age accepted in any project; empty for none.
    pub min_age_floor: String,
    pub max_bump: String,
    /// Whether auto-merge is allowed at all; each project must also consent.
    pub auto_merge: bool,
    /// Whether a major-upgrade merge request is allowed at all; each project
    /// must also consent.
    pub major_mr: bool,
    /// Whether security fixes are allowed at all; each project must also
    /// consent.
    pub security_remediation: bool,
    /// Whether lockfiles may be regenerated at all, in jobs that hold no
    /// token; each project must also consent.
    pub lock: bool,
    /// The pipeline running the job, which lock mode seals each project's
    /// work to; absent outside GitLab CI.
    pub pipeline_id: Option<String>,
    pub major_branch: String,
    pub major_commit_message: String,
    pub concurrency: usize,
    pub exclude: Vec<(String, GlobMatcher)>,
    pub updater: PathBuf,
    pub dry_run: bool,
    /// Projects the lock pipeline gave to their own prepare, lock and publish
    /// jobs; this run leaves them alone.
    pub handed_off: BTreeSet<u64>,
    /// The part of the group this run covers; the whole group when absent.
    pub shard: Option<Shard>,
}

/// One of `count` disjoint parts of a group, selected by project ID, so
/// several schedules can split a group too large for one pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shard {
    /// 1-based.
    pub index: u64,
    pub count: u64,
}

impl Shard {
    fn parse(text: &str) -> Option<Self> {
        let (index, count) = text.split_once('/')?;
        let digits = |part: &str| {
            (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse::<u64>().ok())
                .flatten()
        };
        let (index, count) = (digits(index)?, digits(count)?);
        (1..=count)
            .contains(&index)
            .then_some(Self { index, count })
    }

    /// Whether project `id` belongs to this shard.
    pub fn contains(&self, id: u64) -> bool {
        id % self.count == self.index - 1
    }
}

impl fmt::Display for Shard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.index, self.count)
    }
}

impl OrgSettings {
    pub fn from_env(dry_run: bool) -> Result<Self, Error> {
        Self::from_lookup(|name| env::var(name).ok(), dry_run)
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>, dry_run: bool) -> Result<Self, Error> {
        let value = |name: &str| lookup(name).filter(|value| !value.is_empty());
        let required = |name: &str, hint: &str| {
            value(name).ok_or_else(|| Error::Input(format!("{name} is not set: {hint}")))
        };
        let optional =
            |name: &str, default: &str| value(name).unwrap_or_else(|| default.to_string());

        let server_url = required("CI_SERVER_URL", "GitLab CI provides it")?
            .trim_end_matches('/')
            .to_string();
        let api_url = value("CI_API_V4_URL").unwrap_or_else(|| format!("{server_url}/api/v4"));
        let central_project = match value("CI_PROJECT_ID") {
            None => None,
            Some(id) => Some(id.parse::<u64>().map_err(|_| {
                Error::Input("CI_PROJECT_ID must be a numeric project ID".to_string())
            })?),
        };
        let flag = |name: &str| match value(name).as_deref() {
            None | Some("false") => Ok(false),
            Some("true") => Ok(true),
            Some(_) => Err(Error::Input(format!("{name} must be true or false"))),
        };
        let auto_merge = flag("UPD_AUTO_MERGE")?;
        let major_mr = flag("UPD_MAJOR_MR")?;
        let lock = flag("UPD_LOCK")?;
        // Unlike the other switches this one is on unless turned off: it
        // still enables nothing until a project consents too.
        let security_remediation = match value("UPD_SECURITY_REMEDIATION") {
            None => true,
            Some(_) => flag("UPD_SECURITY_REMEDIATION")?,
        };
        let concurrency = match value("UPD_CONCURRENCY") {
            None => DEFAULT_CONCURRENCY,
            Some(text) => text
                .parse::<usize>()
                .ok()
                .filter(|count| (1..=MAX_CONCURRENCY).contains(count))
                .ok_or_else(|| {
                    Error::Input(format!(
                        "UPD_CONCURRENCY must be a whole number from 1 to {MAX_CONCURRENCY}"
                    ))
                })?,
        };
        let min_age_floor = optional("UPD_MIN_AGE", "");
        if !min_age_floor.is_empty() {
            crate::cooldown::parse_duration(&min_age_floor).map_err(|error| {
                Error::Input(format!("UPD_MIN_AGE '{min_age_floor}' is invalid: {error}"))
            })?;
        }
        let langs = optional("UPD_LANGS", "")
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| match <Lang as ValueEnum>::from_str(name, false) {
                Ok(Lang::Nix) => Err(Error::Input(
                    "UPD_LANGS cannot select nix: organization mode never runs `nix flake update`"
                        .to_string(),
                )),
                Ok(lang) => Ok(lang.cli_name()),
                Err(_) => Err(Error::Input(format!(
                    "UPD_LANGS names an unknown ecosystem '{name}'; use names accepted by --lang"
                ))),
            })
            .collect::<Result<Vec<_>, _>>()?
            .join(",");
        let max_bump = optional("UPD_MAX_BUMP", "");
        if !max_bump.is_empty() && crate::cli::BumpLevel::from_str(&max_bump, false).is_err() {
            return Err(Error::Input(format!(
                "UPD_MAX_BUMP must be one of major, minor or patch, not '{max_bump}'"
            )));
        }
        let exclude = optional("UPD_EXCLUDE", "")
            .split_whitespace()
            .map(|pattern| {
                Glob::from_str(pattern)
                    .map(|glob| (pattern.to_string(), glob.compile_matcher()))
                    .map_err(|error| {
                        Error::Input(format!(
                            "UPD_EXCLUDE pattern '{pattern}' is invalid: {error}"
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let updater = match value("UPD_EXECUTABLE") {
            Some(path) => PathBuf::from(path),
            None => env::current_exe()
                .map_err(|error| Error::Io(format!("cannot locate the upd executable: {error}")))?,
        };
        let branch = optional("UPD_BRANCH", "automation/upd-dependencies");
        let major_branch = optional("UPD_MAJOR_BRANCH", "automation/upd-dependencies-major");
        if major_mr {
            run::check_major_lane(&max_bump, "UPD_MAX_BUMP", &branch, &major_branch)?;
        }
        let handed_off = optional("UPD_LOCK_HANDED_OFF", "")
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|id| !id.is_empty())
            .map(|id| {
                id.parse::<u64>().map_err(|_| {
                    Error::Input(format!(
                        "UPD_LOCK_HANDED_OFF must list numeric project IDs, not '{id}'"
                    ))
                })
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        if !handed_off.is_empty() && !lock {
            return Err(Error::Input(
                "UPD_LOCK_HANDED_OFF comes from the lock pipeline and needs UPD_LOCK=true"
                    .to_string(),
            ));
        }
        let shard = match value("UPD_SHARD") {
            None => None,
            Some(text) => Some(Shard::parse(&text).ok_or_else(|| {
                Error::Input(format!(
                    "UPD_SHARD must be k/n with 1 <= k <= n (for example 2/3), not '{text}'"
                ))
            })?),
        };

        Ok(Self {
            token: required(
                "UPD_GITLAB_TOKEN",
                "set the masked, protected UPD_GITLAB_TOKEN CI/CD variable",
            )?,
            api_url,
            server_url,
            group: required(
                "UPD_GROUP",
                "set the group's full path (for example my-org/platform) or numeric ID",
            )?,
            central_project,
            branch,
            commit_message: optional(
                "UPD_COMMIT_MESSAGE",
                "chore(deps): update dependencies with upd",
            ),
            git_name: optional("UPD_GIT_NAME", "upd automation"),
            git_email: optional("UPD_GIT_EMAIL", "upd-automation@noreply.invalid"),
            langs,
            min_age_floor,
            max_bump,
            auto_merge,
            major_mr,
            security_remediation,
            lock,
            pipeline_id: value("CI_PIPELINE_ID"),
            major_branch,
            major_commit_message: optional(
                "UPD_MAJOR_COMMIT_MESSAGE",
                "chore(deps): update major dependencies with upd",
            ),
            concurrency,
            exclude,
            updater,
            dry_run,
            handed_off,
            shard,
        })
    }

    /// The single-project settings for `project`, before its consent is read.
    pub(super) fn for_project(&self, project: &Project, dir: PathBuf) -> Settings {
        Settings {
            token: self.token.clone(),
            api_url: self.api_url.clone(),
            default_branch: project.default_branch.clone(),
            project_dir: dir,
            project_id: project.id.to_string(),
            project_path: project.path.clone(),
            server_url: self.server_url.clone(),
            branch: self.branch.clone(),
            commit_message: self.commit_message.clone(),
            mr_title: String::new(),
            git_name: self.git_name.clone(),
            git_email: self.git_email.clone(),
            paths: vec![".".to_string()],
            langs: self.langs.clone(),
            // `nix flake update` is the one update that runs a program
            // against repository content; it has no place in a job holding a
            // group-wide token, and the job image does not carry Nix.
            exclude_langs: Lang::Nix.cli_name().to_string(),
            packages: String::new(),
            min_age: String::new(),
            min_age_floor: self.min_age_floor.clone(),
            max_bump: self.max_bump.clone(),
            lock: false,
            no_build: false,
            auto_merge: false,
            security_remediation: false,
            prepare_command: String::new(),
            validation_command: String::new(),
            updater: self.updater.clone(),
            config: None,
            dry_run: self.dry_run,
            // The project's own consent, read from its tree, enables it.
            major_mr: false,
            major_branch: self.major_branch.clone(),
            major_commit_message: self.major_commit_message.clone(),
            lane: Lane::Ordinary,
        }
    }
}

/// A group project as discovery lists it.
#[derive(Debug, Clone)]
pub(super) struct Project {
    pub id: u64,
    pub path: String,
    pub default_branch: String,
}

/// What happened to one project.
#[derive(Debug)]
pub enum State {
    /// Not a candidate; `reason` says why.
    Skipped(&'static str),
    /// The repository has not opted in.
    NotOptedIn(String),
    /// The configuration file cannot be read as an opt-in.
    ConfigInvalid {
        config: String,
        message: String,
    },
    /// The project was handled; `Outcome` says how.
    Processed(Outcome),
    /// The project consented to lockfile regeneration after the lock
    /// pipeline was planned; the next run hands it to lock jobs.
    Deferred(String),
    Failed(Error),
}

#[derive(Debug)]
pub struct ProjectReport {
    pub id: u64,
    pub path: String,
    pub state: State,
    /// The major lane's result; absent unless both the organization and the
    /// project enabled it.
    pub major: Option<Result<Outcome, Error>>,
    /// Whether the ordinary lane applies security fixes; absent for a
    /// project that did not consent to updates.
    pub remediation: Option<Remediation>,
    /// Whether its lockfiles are regenerated; absent for a project that did
    /// not consent to updates or tracks no lockfile.
    pub lockfiles: Option<Lockfiles>,
}

/// Whether a consented project's ordinary lane applies security fixes.
#[derive(Debug)]
pub enum Remediation {
    Enabled,
    /// The reason names the side that did not allow it.
    Disabled(String),
}

impl Remediation {
    pub(super) fn to_json(&self) -> Value {
        match self {
            Self::Enabled => json!({"enabled": true}),
            Self::Disabled(reason) => json!({"enabled": false, "reason": reason}),
        }
    }
}

/// Whether a consented project's lockfiles are regenerated, for a project
/// whose default branch tracks one.
#[derive(Debug)]
pub enum Lockfiles {
    /// Lock jobs regenerate them.
    Regenerated,
    /// Updates leave them as they are; the reason names the side that did
    /// not allow regeneration and how to get it.
    Kept(String),
}

impl Lockfiles {
    fn to_json(&self) -> Value {
        match self {
            Self::Regenerated => json!({"regenerated": true}),
            Self::Kept(reason) => json!({"regenerated": false, "reason": reason}),
        }
    }
}

impl State {
    const NAMES: [&'static str; 6] = [
        "skipped",
        "not_opted_in",
        "config_invalid",
        "processed",
        "deferred",
        "failed",
    ];

    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Skipped(_) => "skipped",
            Self::NotOptedIn(_) => "not_opted_in",
            Self::ConfigInvalid { .. } => "config_invalid",
            Self::Processed(_) => "processed",
            Self::Deferred(_) => "deferred",
            Self::Failed(_) => "failed",
        }
    }
}

impl ProjectReport {
    fn to_json(&self, branch: &str, major_branch: &str) -> Value {
        let mut value = json!({"id": self.id, "path": self.path, "state": self.state.name()});
        if let Some(major) = &self.major {
            value["major"] = run::major_lane_json(major, major_branch);
        }
        if let Some(remediation) = &self.remediation {
            value["security_remediation"] = remediation.to_json();
        }
        if let Some(lockfiles) = &self.lockfiles {
            value["lockfiles"] = lockfiles.to_json();
        }
        match &self.state {
            State::Skipped(reason) => {
                value["reason"] = json!(reason);
            }
            State::NotOptedIn(reason) | State::Deferred(reason) => {
                value["reason"] = json!(reason);
            }
            State::ConfigInvalid { config, message } => {
                value["config"] = json!(config);
                value["message"] = json!(message);
            }
            State::Processed(outcome) => {
                let mut outcome = outcome.to_json(branch);
                if let Value::Object(fields) = &mut outcome {
                    fields.remove("command");
                    fields.remove("branch");
                    for (key, field) in std::mem::take(fields) {
                        value[key] = field;
                    }
                }
            }
            State::Failed(error) => {
                value["error"] = json!({
                    "kind": error.kind(),
                    "message": error.message(),
                    "exit_code": error.exit_code(),
                });
            }
        }
        value
    }

    fn render_text(&self, branch: &str, major_branch: &str) -> String {
        let detail = match &self.state {
            State::Skipped(reason) => format!("skipped ({reason})"),
            State::NotOptedIn(reason) => format!("not opted in ({reason})"),
            State::ConfigInvalid { config, message } => {
                format!("configuration invalid in {config}: {message}")
            }
            State::Processed(outcome) => outcome.render_text(branch),
            State::Deferred(reason) => format!("deferred ({reason})"),
            State::Failed(error) => format!("failed ({}): {}", error.kind(), error.message()),
        };
        let major = match &self.major {
            Some(major) => format!(
                "\n{}: major lane: {}",
                self.path,
                run::lane_text(major, major_branch)
            ),
            None => String::new(),
        };
        let remediation = match &self.remediation {
            Some(Remediation::Disabled(reason)) => {
                format!("\n{}: security fixes off ({reason})", self.path)
            }
            Some(Remediation::Enabled) | None => String::new(),
        };
        let lockfiles = match &self.lockfiles {
            Some(Lockfiles::Kept(reason)) => {
                format!("\n{}: lockfiles not regenerated ({reason})", self.path)
            }
            Some(Lockfiles::Regenerated) | None => String::new(),
        };
        format!("{}: {detail}{remediation}{lockfiles}{major}", self.path)
    }

    /// Whether the project needs someone's attention for the run to count
    /// as successful: either lane failed, or its opt-in could not be read.
    fn is_failure(&self) -> bool {
        matches!(self.state, State::Failed(_) | State::ConfigInvalid { .. })
            || matches!(self.major, Some(Err(_)))
    }
}

/// Everything an organization run did, in project-id order.
#[derive(Debug)]
pub struct Report {
    pub group: String,
    pub branch: String,
    /// The major lane's branch; absent when the organization did not enable
    /// the lane.
    pub major_branch: Option<String>,
    pub dry_run: bool,
    /// The part of the group the run covered; absent for the whole group.
    pub shard: Option<Shard>,
    pub projects: Vec<ProjectReport>,
}

impl Report {
    /// Projects that failed or whose opt-in could not be read.
    pub fn failures(&self) -> usize {
        self.projects.iter().filter(|p| p.is_failure()).count()
    }

    /// Projects whose major lane failed, whatever their ordinary lane did.
    fn major_failures(&self) -> usize {
        self.projects
            .iter()
            .filter(|project| matches!(project.major, Some(Err(_))))
            .count()
    }

    fn major_branch(&self) -> &str {
        self.major_branch.as_deref().unwrap_or_default()
    }

    fn count(&self, state: &str) -> usize {
        self.projects
            .iter()
            .filter(|project| project.state.name() == state)
            .count()
    }

    pub fn to_json(&self) -> Value {
        let mut counts = serde_json::Map::new();
        counts.insert("projects".to_string(), json!(self.projects.len()));
        for state in State::NAMES {
            counts.insert(state.to_string(), json!(self.count(state)));
        }
        if self.major_branch.is_some() {
            counts.insert("major_failed".to_string(), json!(self.major_failures()));
        }
        let projects: Vec<Value> = self
            .projects
            .iter()
            .map(|project| project.to_json(&self.branch, self.major_branch()))
            .collect();
        let mut value = json!({
            "command": "gitlab org run",
            "group": self.group,
            "branch": self.branch,
            "dry_run": self.dry_run,
            "counts": counts,
            "projects": projects,
        });
        if let Some(major_branch) = &self.major_branch {
            value["major_branch"] = json!(major_branch);
        }
        if let Some(shard) = &self.shard {
            value["shard"] = json!(shard.to_string());
        }
        value
    }

    /// One line per project that was considered, then a summary.
    pub fn render_text(&self) -> String {
        let mut lines: Vec<String> = self
            .projects
            .iter()
            .filter(|project| !matches!(project.state, State::Skipped(_)))
            .map(|project| project.render_text(&self.branch, self.major_branch()))
            .collect();
        lines.push(self.summary());
        lines.join("\n")
    }

    /// One line counting the projects in each state.
    pub fn summary(&self) -> String {
        let count = |state| self.count(state);
        let major = match self.major_branch {
            Some(_) => format!(", {} major lane(s) failed", self.major_failures()),
            None => String::new(),
        };
        let shard = match &self.shard {
            Some(shard) => format!(" (shard {shard})"),
            None => String::new(),
        };
        let deferred = match count("deferred") {
            0 => String::new(),
            deferred => format!(", {deferred} deferred to the next run's lock jobs"),
        };
        format!(
            "{}{} projects in {}{shard}: {} processed{deferred}, {} not opted in, {} with invalid configuration, {} failed, {} skipped{major}.",
            if self.dry_run { "Dry run: " } else { "" },
            self.projects.len(),
            self.group,
            count("processed"),
            count("not_opted_in"),
            count("config_invalid"),
            count("failed"),
            count("skipped"),
        )
    }
}

pub async fn run(settings: &OrgSettings) -> Result<Report, Error> {
    run::check_branch_name(&env::temp_dir(), &settings.branch).await?;
    if settings.major_mr {
        run::check_branch_name(&env::temp_dir(), &settings.major_branch).await?;
    }
    let api = Client::new(&settings.api_url, &settings.token)?;
    let listed = in_shard(settings, api.group_projects(&settings.group).await?);
    eprintln!(
        "Found {} projects in {}{}; checking which opted in",
        listed.len(),
        settings.group,
        match &settings.shard {
            Some(shard) => format!(" (shard {shard})"),
            None => String::new(),
        }
    );

    let mut projects = futures::stream::iter(listed.into_iter().map(|raw| {
        let api = &api;
        async move {
            let log = Log::buffered();
            let report = handle(settings, api, raw, &log).await;
            let buffered = log.take();
            if !buffered.is_empty() || !matches!(report.state, State::Skipped(_)) {
                eprint!(
                    "==> {}\n{buffered}{}\n",
                    report.path,
                    report.render_text(&settings.branch, &settings.major_branch)
                );
            }
            report
        }
    }))
    .buffer_unordered(settings.concurrency)
    .collect::<Vec<_>>()
    .await;
    projects.sort_by_key(|project| project.id);

    Ok(Report {
        group: settings.group.clone(),
        branch: settings.branch.clone(),
        major_branch: settings.major_mr.then(|| settings.major_branch.clone()),
        dry_run: settings.dry_run,
        shard: settings.shard,
        projects,
    })
}

/// The listed projects in the settings' shard, all of them without one.
pub(super) fn in_shard(settings: &OrgSettings, listed: Vec<Value>) -> Vec<Value> {
    match &settings.shard {
        None => listed,
        Some(shard) => listed
            .into_iter()
            .filter(|raw| raw["id"].as_u64().is_some_and(|id| shard.contains(id)))
            .collect(),
    }
}

async fn handle(settings: &OrgSettings, api: &Client, raw: Value, log: &Log) -> ProjectReport {
    let id = raw["id"].as_u64().unwrap_or_default();
    let path = raw["path_with_namespace"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let report = |handled: Handled| ProjectReport {
        id,
        path: path.clone(),
        state: handled.state,
        major: handled.major,
        remediation: handled.remediation,
        lockfiles: handled.lockfiles,
    };
    let project = match classify(settings, raw) {
        Ok(project) => project,
        Err(state) => return report(Handled::stopped(state)),
    };
    if settings.handed_off.contains(&project.id) {
        return report(Handled::stopped(State::Skipped("handed_off")));
    }
    match process(settings, api, &project, log).await {
        Ok(handled) => report(handled),
        Err(error) => report(Handled::stopped(State::Failed(error))),
    }
}

/// A project to process, or why it is not a candidate.
pub(super) fn classify(settings: &OrgSettings, raw: Value) -> Result<Project, State> {
    let id = raw["id"].as_u64().unwrap_or_default();
    let Some(path) = raw["path_with_namespace"].as_str().map(str::to_string) else {
        return Err(State::Failed(Error::Refused(
            "GitLab listed the project without a path_with_namespace".to_string(),
        )));
    };
    if !is_project_path(&path) {
        return Err(State::Failed(Error::Refused(format!(
            "GitLab listed an unusable project path: {path}"
        ))));
    }
    if settings.central_project == Some(id) {
        return Err(State::Skipped("central_project"));
    }
    if settings
        .exclude
        .iter()
        .any(|(_, matcher)| matcher.is_match(&path))
    {
        return Err(State::Skipped("excluded"));
    }
    let flag = |key: &str| raw[key].as_bool() == Some(true);
    if flag("archived") {
        return Err(State::Skipped("archived"));
    }
    if !raw["marked_for_deletion_at"].is_null() || !raw["marked_for_deletion_on"].is_null() {
        return Err(State::Skipped("pending_deletion"));
    }
    if raw["repository_access_level"].as_str() == Some("disabled") {
        return Err(State::Skipped("repository_disabled"));
    }
    if flag("empty_repo") {
        return Err(State::Skipped("empty_repository"));
    }
    let default_branch = raw["default_branch"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if default_branch.is_empty() {
        return Err(State::Skipped("no_default_branch"));
    }
    if run::branches_collide(&default_branch, &settings.branch) {
        return Err(State::Failed(Error::Input(format!(
            "The automation branch {} cannot sit beside the project's default branch {default_branch}: they are the same branch, or one is nested under the other's name",
            settings.branch
        ))));
    }
    if settings.major_mr && run::branches_collide(&default_branch, &settings.major_branch) {
        return Err(State::Failed(Error::Input(format!(
            "The major automation branch {} cannot sit beside the project's default branch {default_branch}: they are the same branch, or one is nested under the other's name",
            settings.major_branch
        ))));
    }
    Ok(Project {
        id,
        path,
        default_branch,
    })
}

/// Whether `path` is a plain `group/.../project` path, safe to place in a
/// clone URL.
fn is_project_path(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    segments.len() >= 2
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && !segment.starts_with('-')
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
}

/// The repository's answer to whether automation may update it.
pub(super) enum Consent {
    No(String),
    Invalid {
        config: String,
        message: String,
    },
    Yes {
        config: String,
        auto_merge: bool,
        major_mr: bool,
        security_remediation: bool,
        lock: bool,
        lock_build: bool,
    },
}

impl Consent {
    fn read(config: &str, content: &str, log: &Log) -> Self {
        match UpdConfig::parse_for_automation(content, config) {
            Err(message) => Self::Invalid {
                config: config.to_string(),
                message,
            },
            Ok((parsed, warnings)) => {
                for warning in warnings {
                    log.line(format_args!("warning: {warning}"));
                }
                if parsed.dependency_updates_enabled() {
                    Self::Yes {
                        config: config.to_string(),
                        auto_merge: parsed.auto_merge_enabled(),
                        major_mr: parsed.major_mr_enabled(),
                        security_remediation: parsed.security_remediation_enabled(),
                        lock: parsed.lock_enabled(),
                        lock_build: parsed.lock_build_enabled(),
                    }
                } else {
                    Self::No(format!(
                        "{config} does not set dependency_updates = true in [automation]"
                    ))
                }
            }
        }
    }

    fn absent() -> Self {
        Self::No(format!(
            "no {} at the root of the default branch",
            CONFIG_FILE_NAMES.join(", ")
        ))
    }
}

/// What processing a project left to report.
struct Handled {
    state: State,
    /// The major lane's result, when the project consented to one. The
    /// major lane runs even when the ordinary lane failed.
    major: Option<Result<Outcome, Error>>,
    /// Whether security fixes apply, when the project consented to updates.
    remediation: Option<Remediation>,
    /// Whether lockfiles are regenerated, when the project consented to
    /// updates and tracks a lockfile.
    lockfiles: Option<Lockfiles>,
}

impl Handled {
    fn stopped(state: State) -> Self {
        Self {
            state,
            major: None,
            remediation: None,
            lockfiles: None,
        }
    }
}

/// A consented project's checkout, holding the fetched default branch with
/// the consent applied to its settings, or why the project stops before one.
pub(super) enum Opened<'a> {
    Stopped(State),
    Consented(Box<Consented<'a>>),
}

pub(super) struct Consented<'a> {
    pub session: Session<'a>,
    pub remediation: Remediation,
    /// Whether both sides allow lockfile regeneration.
    pub lock: bool,
    /// Whether the project lets its lockfile regeneration run build
    /// backends.
    pub lock_build: bool,
    /// Whether lockfiles are regenerated, when the default branch tracks
    /// one.
    pub lockfiles: Option<Lockfiles>,
    /// Keeps the checkout alive as long as the session.
    pub checkout: tempfile::TempDir,
}

/// A new, empty git repository for a checkout, in a directory removed when
/// the returned guard drops.
pub(super) async fn empty_checkout() -> Result<(tempfile::TempDir, PathBuf), Error> {
    let work = tempfile::Builder::new()
        .prefix("upd-org-")
        .tempdir()
        .map_err(|error| Error::Io(format!("cannot create a work directory: {error}")))?;
    let dir = work.path().join("checkout");
    std::fs::create_dir(&dir)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", dir.display())))?;
    let init = git::child(&dir, "git")
        .args(["init", "--quiet"])
        .output()
        .await
        .map_err(|error| Error::Io(format!("cannot start git: {error}")))?;
    if !init.status.success() {
        return Err(Error::Io(format!(
            "git init failed ({}): {}",
            init.status,
            String::from_utf8_lossy(&init.stderr).trim()
        )));
    }
    Ok((work, dir))
}

/// Reads the project's consent and, when it opted in, fetches it into a
/// fresh checkout with the consent applied to the ordinary lane's settings.
pub(super) async fn open_project<'a>(
    settings: &OrgSettings,
    api: &Client,
    project: &Project,
    log: &'a Log,
) -> Result<Opened<'a>, Error> {
    // A cheap read through the API spares cloning projects that have not
    // opted in. Anything else goes on to the clone, whose own copy of the
    // file decides: the API follows no symbolic link and answers with the
    // link's target path, so only the tree can say what is wrong with it.
    if let Consent::No(reason) = api_consent(api, project).await? {
        return Ok(Opened::Stopped(State::NotOptedIn(reason)));
    }

    let (work, dir) = empty_checkout().await?;
    let mut session = Session::open(settings.for_project(project, dir), log).await?;
    match tree_consent(&session.git, &session.default_ref, log).await? {
        Consent::No(reason) => Ok(Opened::Stopped(State::NotOptedIn(reason))),
        Consent::Invalid { config, message } => {
            Ok(Opened::Stopped(State::ConfigInvalid { config, message }))
        }
        Consent::Yes {
            config,
            auto_merge,
            major_mr,
            security_remediation,
            lock,
            lock_build,
        } => {
            session.settings.auto_merge = settings.auto_merge && auto_merge;
            session.settings.major_mr = settings.major_mr && major_mr;
            session.settings.security_remediation =
                settings.security_remediation && security_remediation;
            let remediation = if !settings.security_remediation {
                Remediation::Disabled(
                    "the organization run turned security remediation off".to_string(),
                )
            } else if !security_remediation {
                Remediation::Disabled(format!(
                    "{config} does not set security_remediation = true in [automation]"
                ))
            } else {
                Remediation::Enabled
            };
            let lockfiles = if tracks_lockfile(&session.git, &session.default_ref).await? {
                Some(if !settings.lock {
                    Lockfiles::Kept(format!(
                        "the organization run does not regenerate lockfiles; turn its lock input on and set lock = true in {config}, or run {SINGLE_PROJECT_TEMPLATE} with lock: true in this project"
                    ))
                } else if !lock {
                    Lockfiles::Kept(format!(
                        "{config} does not set lock = true in [automation]; set it, or run {SINGLE_PROJECT_TEMPLATE} with lock: true in this project"
                    ))
                } else {
                    Lockfiles::Regenerated
                })
            } else {
                None
            };
            session.settings.config = Some(PathBuf::from(config));
            Ok(Opened::Consented(Box::new(Consented {
                session,
                remediation,
                lock: settings.lock && lock,
                lock_build,
                lockfiles,
                checkout: work,
            })))
        }
    }
}

async fn process(
    settings: &OrgSettings,
    api: &Client,
    project: &Project,
    log: &Log,
) -> Result<Handled, Error> {
    let consented = match open_project(settings, api, project, log).await? {
        Opened::Stopped(state) => return Ok(Handled::stopped(state)),
        Opened::Consented(consented) => *consented,
    };
    // Only lock jobs may regenerate a consenting project's lockfiles, and
    // this run holds the token; the project waits for the next plan to hand
    // it off rather than getting an update that leaves them stale.
    if consented.lock && !settings.dry_run {
        return Ok(Handled::stopped(State::Deferred(format!(
            "{} consented to lockfile regeneration after this run was planned; the next run regenerates its lockfiles in lock jobs",
            consented
                .session
                .settings
                .config
                .as_deref()
                .map_or_else(String::new, |config| config.display().to_string())
        ))));
    }
    let Consented {
        session,
        remediation,
        lockfiles,
        checkout: _checkout,
        ..
    } = consented;
    // The major lane reads the same configuration and consent the ordinary
    // lane was given, from the same checkout.
    let major = session.settings.major_lane();
    let state = match session.propose().await {
        Ok(proposal) => State::Processed(proposal.outcome),
        Err(error) => State::Failed(error),
    };
    let major = match major {
        Some(major) => Some(run::propose_major(major, log).await),
        None => None,
    };
    Ok(Handled {
        state,
        major,
        remediation: Some(remediation),
        lockfiles,
    })
}

/// Whether the tree at `default_ref` tracks a file named as a lockfile of
/// any format upd knows, at any depth.
async fn tracks_lockfile(git: &Git, default_ref: &str) -> Result<bool, Error> {
    let names = git
        .read(["ls-tree", "-r", "-z", "--name-only", default_ref])
        .await?;
    Ok(names.split('\0').any(|path| {
        let name = path.rsplit('/').next().unwrap_or(path);
        LockfileType::from_filename(name).is_some()
    }))
}

pub(super) async fn api_consent(api: &Client, project: &Project) -> Result<Consent, Error> {
    let quiet = Log::buffered();
    for name in CONFIG_FILE_NAMES {
        if let Some(content) = api
            .raw_file(project.id, name, &project.default_branch)
            .await?
        {
            return Ok(Consent::read(name, &content, &quiet));
        }
    }
    Ok(Consent::absent())
}

/// Reads the opt-in from the fetched default branch, the commit the update
/// will start from, in the order configuration discovery uses.
async fn tree_consent(git: &Git, default_ref: &str, log: &Log) -> Result<Consent, Error> {
    for name in CONFIG_FILE_NAMES {
        let entry = git.read(["ls-tree", "-z", default_ref, "--", name]).await?;
        let entry = entry.trim_end_matches('\0');
        if entry.is_empty() {
            continue;
        }
        let meta = entry.split_once('\t').map(|(meta, _)| meta).unwrap_or("");
        let fields: Vec<&str> = meta.split(' ').collect();
        let [mode, kind, object] = fields[..] else {
            return Err(Error::Io(format!("unexpected git ls-tree output: {entry}")));
        };
        if kind != "blob" || !matches!(mode, "100644" | "100755") {
            let entry = match (mode, kind) {
                ("120000", _) => "a symbolic link".to_string(),
                ("160000", _) => "a submodule".to_string(),
                (_, "tree") => "a directory".to_string(),
                _ => format!("a {kind} with mode {mode}"),
            };
            return Ok(Consent::Invalid {
                config: name.to_string(),
                message: format!("{name} must be a regular file, not {entry}"),
            });
        }
        let content = git.read(["cat-file", "blob", object]).await?;
        return Ok(Consent::read(name, &content, log));
    }
    Ok(Consent::absent())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn settings(overrides: &[(&str, &str)]) -> Result<OrgSettings, Error> {
        let mut vars: HashMap<&str, &str> = HashMap::from([
            ("UPD_GITLAB_TOKEN", "token"),
            ("CI_SERVER_URL", "https://gitlab.example.test/"),
            ("UPD_GROUP", "acme"),
            ("UPD_EXECUTABLE", "/usr/local/bin/upd"),
        ]);
        vars.extend(overrides.iter().copied());
        OrgSettings::from_lookup(|name| vars.get(name).map(|value| value.to_string()), false)
    }

    #[test]
    fn unset_optional_settings_take_the_documented_defaults() {
        let settings = settings(&[]).unwrap();
        assert_eq!(settings.api_url, "https://gitlab.example.test/api/v4");
        assert_eq!(settings.branch, "automation/upd-dependencies");
        assert_eq!(settings.concurrency, DEFAULT_CONCURRENCY);
        assert_eq!(settings.central_project, None);
        assert!(!settings.auto_merge);
        assert!(settings.security_remediation);
        assert!(settings.exclude.is_empty() && settings.min_age_floor.is_empty());
        let accepted = self::settings(&[("UPD_LANGS", "python, rust")]).unwrap();
        assert_eq!(accepted.langs, "python,rust");
        assert!(!settings.major_mr);
        assert!(!settings.lock);
        assert_eq!(settings.pipeline_id, None);
        let lock = self::settings(&[("UPD_LOCK", "true"), ("CI_PIPELINE_ID", "981")]).unwrap();
        assert!(lock.lock);
        assert_eq!(lock.pipeline_id.as_deref(), Some("981"));
        assert_eq!(settings.major_branch, "automation/upd-dependencies-major");
        assert_eq!(
            settings.major_commit_message,
            "chore(deps): update major dependencies with upd"
        );
    }

    #[test]
    fn a_project_starts_with_the_major_lane_off_until_it_consents() {
        let settings = settings(&[("UPD_MAJOR_MR", "true"), ("UPD_MAX_BUMP", "minor")]).unwrap();
        assert!(settings.major_mr);
        let project = classify(&settings, listed(json!({}))).unwrap();
        let single = settings.for_project(&project, PathBuf::from("/tmp/checkout"));
        assert!(!single.major_mr && single.major_lane().is_none());
        assert_eq!(single.major_branch, "automation/upd-dependencies-major");

        let state = classify(
            &settings,
            listed(json!({"default_branch": "automation/upd-dependencies-major"})),
        );
        assert!(
            matches!(state, Err(State::Failed(Error::Input(_)))),
            "{state:?}"
        );
    }

    #[test]
    fn an_automation_branch_nested_with_the_default_branch_is_refused() {
        let settings = settings(&[]).unwrap();
        for default_branch in ["automation/upd-dependencies", "automation"] {
            let state = classify(&settings, listed(json!({"default_branch": default_branch})));
            assert!(
                matches!(state, Err(State::Failed(Error::Input(_)))),
                "{default_branch}: {state:?}"
            );
        }
    }

    #[test]
    fn a_major_branch_nested_under_the_default_branch_is_refused() {
        let settings = settings(&[
            ("UPD_MAJOR_MR", "true"),
            ("UPD_MAX_BUMP", "minor"),
            ("UPD_MAJOR_BRANCH", "main/major"),
        ])
        .unwrap();
        let state = classify(&settings, listed(json!({"default_branch": "main"})));
        assert!(
            matches!(state, Err(State::Failed(Error::Input(_)))),
            "{state:?}"
        );
        // A project that keeps a different default branch is unaffected.
        classify(&settings, listed(json!({"default_branch": "develop"}))).unwrap();
    }

    #[test]
    fn missing_or_malformed_settings_are_input_errors() {
        for overrides in [
            &[("UPD_GITLAB_TOKEN", "")][..],
            &[("UPD_GROUP", "")],
            &[("CI_SERVER_URL", "")],
            &[("CI_PROJECT_ID", "acme/central")],
            &[("UPD_AUTO_MERGE", "yes")],
            &[("UPD_SECURITY_REMEDIATION", "yes")],
            &[("UPD_LOCK", "on")],
            &[("UPD_CONCURRENCY", "0")],
            &[("UPD_CONCURRENCY", "17")],
            &[("UPD_CONCURRENCY", "four")],
            &[("UPD_MIN_AGE", "a week")],
            &[("UPD_MAX_BUMP", "huge")],
            &[("UPD_EXCLUDE", "acme/[")],
            &[("UPD_LANGS", "python,nix")],
            &[("UPD_LANGS", "python,pypi")],
            &[("UPD_MAJOR_MR", "yes"), ("UPD_MAX_BUMP", "minor")],
            &[("UPD_MAJOR_MR", "true")],
            &[("UPD_MAJOR_MR", "true"), ("UPD_MAX_BUMP", "major")],
            &[
                ("UPD_MAJOR_MR", "true"),
                ("UPD_MAX_BUMP", "minor"),
                ("UPD_MAJOR_BRANCH", "automation/upd-dependencies"),
            ],
            &[
                ("UPD_MAJOR_MR", "true"),
                ("UPD_MAX_BUMP", "minor"),
                ("UPD_MAJOR_BRANCH", "automation/upd-dependencies/major"),
            ],
            &[("UPD_MAJOR_MR", "true"), ("UPD_MAX_BUMP", "minro")],
            &[("UPD_MAJOR_MR", "true"), ("UPD_MAX_BUMP", "Minor")],
        ] {
            let error = settings(overrides).unwrap_err();
            assert_eq!(error.exit_code(), 4, "{overrides:?}: {error}");
        }
    }

    fn listed(extra: Value) -> Value {
        let mut project = json!({
            "id": 7,
            "path_with_namespace": "acme/web/app",
            "default_branch": "main",
            "archived": false,
            "empty_repo": false,
            "repository_access_level": "enabled",
            "marked_for_deletion_at": null,
        });
        for (key, value) in extra.as_object().unwrap() {
            project[key] = value.clone();
        }
        project
    }

    fn skip_reason(settings: &OrgSettings, extra: Value) -> Option<&'static str> {
        match classify(settings, listed(extra)) {
            Ok(_) => None,
            Err(State::Skipped(reason)) => Some(reason),
            Err(other) => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn projects_that_cannot_take_part_are_skipped_with_a_reason() {
        let plain = settings(&[]).unwrap();
        assert_eq!(skip_reason(&plain, json!({})), None);
        for (extra, reason) in [
            (json!({"archived": true}), "archived"),
            (json!({"empty_repo": true}), "empty_repository"),
            (json!({"default_branch": null}), "no_default_branch"),
            (json!({"default_branch": ""}), "no_default_branch"),
            (
                json!({"repository_access_level": "disabled"}),
                "repository_disabled",
            ),
            (
                json!({"marked_for_deletion_at": "2026-09-01"}),
                "pending_deletion",
            ),
            (
                json!({"marked_for_deletion_on": "2026-09-01"}),
                "pending_deletion",
            ),
        ] {
            assert_eq!(skip_reason(&plain, extra.clone()), Some(reason), "{extra}");
        }
        let central = settings(&[("CI_PROJECT_ID", "7")]).unwrap();
        assert_eq!(skip_reason(&central, json!({})), Some("central_project"));
    }

    #[test]
    fn exclude_patterns_match_the_full_project_path() {
        let settings = settings(&[("UPD_EXCLUDE", "acme/web/* acme/legacy-*")]).unwrap();
        assert_eq!(skip_reason(&settings, json!({})), Some("excluded"));
        assert_eq!(
            skip_reason(
                &settings,
                json!({"path_with_namespace": "acme/legacy-billing"})
            ),
            Some("excluded")
        );
        assert_eq!(
            skip_reason(&settings, json!({"path_with_namespace": "acme/api"})),
            None
        );
    }

    #[test]
    fn unusable_project_paths_fail_rather_than_reach_a_clone_url() {
        let settings = settings(&[]).unwrap();
        for path in [
            json!(null),
            json!("app"),
            json!("acme/../app"),
            json!("acme//app"),
            json!("acme/.hidden"),
            json!("acme/-option"),
            json!("acme/app?x=1"),
            json!("acme/app@host"),
        ] {
            let state = classify(&settings, listed(json!({"path_with_namespace": path})));
            assert!(
                matches!(state, Err(State::Failed(Error::Refused(_)))),
                "{path}: {state:?}"
            );
        }
    }

    #[test]
    fn the_automation_branch_cannot_be_a_projects_default_branch() {
        let settings = settings(&[("UPD_BRANCH", "develop")]).unwrap();
        let state = classify(&settings, listed(json!({"default_branch": "develop"})));
        assert!(
            matches!(state, Err(State::Failed(Error::Input(_)))),
            "{state:?}"
        );
    }

    #[test]
    fn consent_requires_an_explicit_opt_in() {
        let log = Log::buffered();
        let read = |content: &str| Consent::read(".updrc.toml", content, &log);
        assert!(matches!(read(""), Consent::No(_)));
        assert!(matches!(read("[automation]\n"), Consent::No(_)));
        assert!(matches!(
            read("[automation]\ndependency_updates = false\n"),
            Consent::No(_)
        ));
        assert!(matches!(
            read("[automation]\ndependency_updates = true\n"),
            Consent::Yes {
                auto_merge: false,
                security_remediation: false,
                ..
            }
        ));
        assert!(matches!(
            read("[automation]\ndependency_updates = true\nsecurity_remediation = true\n"),
            Consent::Yes {
                security_remediation: true,
                ..
            }
        ));
        assert!(matches!(
            read("[automation]\ndependency_updates = true\nauto_merge = true\n"),
            Consent::Yes {
                auto_merge: true,
                ..
            }
        ));
        for invalid in [
            "[automation]\ndependency_update = true\n",
            "[automation]\ndependency_updates = \"yes\"\n",
            "[automation\n",
        ] {
            assert!(
                matches!(read(invalid), Consent::Invalid { .. }),
                "{invalid}"
            );
        }
    }
}
