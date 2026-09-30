//! Organization lock mode: one project lane split across three jobs so that
//! no job that runs repository code holds the group token.
//!
//! `prepare` holds the token. It opens the project as an organization run
//! does, applies the security fixes and the update to manifests only, and
//! either finishes the lane itself (nothing to relock) or hands the lock job
//! its work: the commit to start from, as a Git bundle, and the planned
//! manifest edits, sealed with the token so no later job can alter them.
//!
//! `lock-worker` holds no token. It checks out the bundled commit, runs the
//! same fixes and update with lockfile regeneration, which may run
//! repository code, and writes its whole change as a patch.
//!
//! `publish` holds the token again. It verifies the seal, admits from the
//! lock job's patch only the planned edits and in-place lockfile edits
//! [`patch::check`] accepts, gates on the lock job's reports as a run gates
//! on its own, and publishes with the lease `prepare` observed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::Error;
use super::api::Client;
use super::git::{self, Git};
use super::org::{self, Consented, OrgSettings, Project, State};
use super::patch;
use super::present::{Base, Security};
use super::run::{self, Build, Lane, Log, Outcome, Proposal, Reports, Session, Settings};
use crate::lockfile::{self, LockfileType, ToolProbe};

/// The version of the files the three jobs exchange.
const FORMAT: u32 = 1;
const WORK: &str = "work.json";
const SEAL: &str = "work.seal";
const PLANNED: &str = "planned.patch";
const BUNDLE: &str = "base.bundle";
const RESULT_DIR: &str = "result";
const RESULT: &str = "result.json";
const RESULT_PATCH: &str = "result.patch";
/// The ref the bundle carries the base commit under.
const BASE_REF: &str = "refs/upd/base";
/// The largest file `publish` reads from the lock job.
const MAX_FILE: u64 = 64 * 1024 * 1024;
/// Separates the seal from every other use of the token as a key.
const SEAL_DOMAIN: &[u8] = b"upd gitlab org work v1\0";

/// What `prepare` hands the later jobs for one project lane. Sealed whether
/// or not a lock job has work, so a lock job cannot turn a finished lane
/// into one that publishes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Work {
    format: u32,
    pipeline: String,
    project: u64,
    /// The project's full path.
    path: String,
    lane: Lane,
    /// The lane's automation branch.
    branch: String,
    /// Absent when `prepare` finished the lane itself.
    plan: Option<Plan>,
}

/// The lane's settings as `prepare` resolved them, with every consent
/// already combined with what the organization allows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    default_branch: String,
    /// The default branch commit both patches start from.
    base_sha: String,
    /// The remote automation branch tip `prepare` inspected, empty when the
    /// branch was absent; `publish` pushes with this lease.
    expected_remote_sha: String,
    planned_sha256: String,
    /// Lockfiles the lock job regenerates, relative to the checkout.
    lockfiles: Vec<String>,
    /// The programs regenerating them needs.
    tools: Vec<String>,
    config: Option<PathBuf>,
    paths: Vec<String>,
    langs: String,
    exclude_langs: String,
    min_age_floor: String,
    max_bump: String,
    /// The ordinary lane's consents; the major lane derives its own.
    auto_merge: bool,
    major_mr: bool,
    security_remediation: bool,
    lock_build: bool,
}

/// What the lock job did, for `publish` to check and gate on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockResult {
    format: u32,
    /// The work this result answers.
    work_sha256: String,
    patch_sha256: String,
    reports: Reports,
    /// Each tool's `--version` output, when it printed one.
    tools: BTreeMap<String, Option<String>>,
}

/// What one split command did for one project lane.
#[derive(Debug)]
pub struct StepReport {
    pub command: &'static str,
    pub project: u64,
    pub path: String,
    pub lane: Lane,
    pub branch: String,
    pub step: Step,
}

/// Why a project that `prepare` opened gets no proposal.
#[derive(Debug)]
pub enum Stop {
    NotOptedIn(String),
    ConfigInvalid { config: String, message: String },
}

impl Stop {
    fn from_state(state: State) -> Result<Self, Error> {
        match state {
            State::NotOptedIn(reason) => Ok(Self::NotOptedIn(reason)),
            State::ConfigInvalid { config, message } => Ok(Self::ConfigInvalid { config, message }),
            State::Failed(error) => Err(error),
            State::Skipped(reason) => Err(Error::Refused(format!(
                "The project is not a candidate: {reason}"
            ))),
            State::Processed(_) => Err(Error::Io(
                "opening the project reported it processed".to_string(),
            )),
        }
    }
}

#[derive(Debug)]
pub enum Step {
    /// The project has not opted in, or its opt-in cannot be read.
    Stopped(Stop),
    /// The project did not enable the major lane, or the organization did not.
    LaneOff,
    /// The lane published, closed or paused; the proposal says how, with
    /// what its security step changed when it ran.
    Finished(Proposal),
    /// `prepare` handed these lockfiles to the lock job.
    Locking { lockfiles: Vec<String> },
    /// The lock job regenerated the lockfiles with these tools.
    Locked {
        tools: BTreeMap<String, Option<String>>,
    },
    /// `prepare` finished the lane; nothing was left for this job.
    NothingToDo,
}

impl StepReport {
    /// The work handing this lane on, sealed to `pipeline`.
    fn work(&self, pipeline: &str, plan: Option<Plan>) -> Work {
        Work {
            format: FORMAT,
            pipeline: pipeline.to_string(),
            project: self.project,
            path: self.path.clone(),
            lane: self.lane,
            branch: self.branch.clone(),
            plan,
        }
    }

    fn step_name(&self) -> &'static str {
        match &self.step {
            Step::Stopped(_) => "stopped",
            Step::LaneOff => "lane_off",
            Step::Finished(_) => "finished",
            Step::Locking { .. } => "locking",
            Step::Locked { .. } => "locked",
            Step::NothingToDo => "nothing_to_do",
        }
    }

    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "command": self.command,
            "project": self.project,
            "path": self.path,
            "lane": self.lane,
            "branch": self.branch,
            "step": self.step_name(),
        });
        match &self.step {
            Step::Stopped(Stop::NotOptedIn(reason)) => {
                value["state"] = json!("not_opted_in");
                value["reason"] = json!(reason);
            }
            Step::Stopped(Stop::ConfigInvalid { config, message }) => {
                value["state"] = json!("config_invalid");
                value["config"] = json!(config);
                value["message"] = json!(message);
            }
            Step::Finished(proposal) => {
                let mut result = proposal.to_json(&self.branch);
                if let Value::Object(fields) = &mut result {
                    fields.remove("command");
                    fields.remove("branch");
                }
                value["result"] = result;
            }
            Step::Locking { lockfiles } => {
                value["lockfiles"] = json!(lockfiles);
            }
            Step::Locked { tools } => {
                value["tools"] = json!(tools);
            }
            Step::LaneOff | Step::NothingToDo => {}
        }
        value
    }

    pub fn render_text(&self) -> String {
        let detail = match &self.step {
            Step::Stopped(Stop::NotOptedIn(reason)) => format!("not opted in ({reason})"),
            Step::Stopped(Stop::ConfigInvalid { config, message }) => {
                format!("configuration invalid in {config}: {message}")
            }
            Step::LaneOff => "the major lane is not enabled".to_string(),
            Step::Finished(proposal) => proposal.outcome.render_text(&self.branch),
            Step::Locking { lockfiles } => {
                format!("handed {} to the lock job", lockfiles.join(", "))
            }
            Step::Locked { tools } => format!(
                "regenerated the lockfiles with {}",
                tools
                    .iter()
                    .map(|(tool, version)| match version {
                        Some(version) => format!("{tool} ({version})"),
                        None => tool.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Step::NothingToDo => "nothing to do: prepare finished this lane".to_string(),
        };
        let lane = match self.lane {
            Lane::Ordinary => "",
            Lane::Major => " (major lane)",
        };
        format!("{}{lane}: {detail}", self.path)
    }

    /// Whether the step needs someone's attention: the project's opt-in
    /// cannot be read.
    pub fn is_failure(&self) -> bool {
        matches!(self.step, Step::Stopped(Stop::ConfigInvalid { .. }))
    }
}

/// Resolves `dir` from the job's directory: git runs in the checkout, so a
/// path relative to the job would name a file inside the checkout.
fn absolute(dir: &Path) -> Result<PathBuf, Error> {
    std::path::absolute(dir)
        .map_err(|error| Error::Io(format!("cannot resolve {}: {error}", dir.display())))
}

/// Opens the project lane and either finishes it or writes the lock job's
/// work into `dir`, which must be absent or empty.
pub async fn prepare(
    settings: &OrgSettings,
    project_id: u64,
    lane: Lane,
    dir: &Path,
    log: &Log,
) -> Result<StepReport, Error> {
    let pipeline = pipeline(settings)?;
    let dir = &absolute(dir)?;
    create_empty_dir(dir)?;
    let api = Client::new(&settings.api_url, &settings.token)?;
    let project = group_project(settings, &api, project_id).await?;
    let mut report = StepReport {
        command: "gitlab org prepare",
        project: project.id,
        path: project.path.clone(),
        lane,
        branch: match lane {
            Lane::Ordinary => settings.branch.clone(),
            Lane::Major => settings.major_branch.clone(),
        },
        step: Step::NothingToDo,
    };
    let finish = |report: &mut StepReport, step: Step| -> Result<(), Error> {
        report.step = step;
        write_work(settings, dir, &report.work(pipeline, None))
    };

    let consented = match org::open_project(settings, &api, &project, log).await? {
        org::Opened::Stopped(state) => {
            finish(&mut report, Step::Stopped(Stop::from_state(state)?))?;
            return Ok(report);
        }
        org::Opened::Consented(consented) => *consented,
    };
    let Consented {
        session,
        lock,
        lock_build,
        checkout: _checkout,
        ..
    } = consented;
    let ordinary = session.settings.clone();
    let session = match lane {
        Lane::Ordinary => session,
        Lane::Major => {
            let Some(major) = ordinary.major_lane() else {
                finish(&mut report, Step::LaneOff)?;
                return Ok(report);
            };
            log.line(format_args!(
                "Major lane: proposing major-version upgrades on {}",
                major.branch
            ));
            Session::open(major, log).await?
        }
    };
    if !lock {
        let proposal = session.propose().await?;
        finish(&mut report, Step::Finished(proposal))?;
        return Ok(report);
    }

    let mut security = None;
    let (presentation, changed) = match session.build(&mut security).await? {
        Build::Done(outcome) => {
            finish(
                &mut report,
                Step::Finished(proposal(outcome, security.as_ref())),
            )?;
            return Ok(report);
        }
        Build::Ready {
            presentation,
            changed,
        } => (presentation, changed),
    };
    let checkout = session.settings.project_dir.clone();
    let lockfiles = lockfiles_to_refresh(&session.git, &checkout, security.as_ref()).await?;
    if lockfiles.is_empty() {
        let outcome = session.publish(presentation, changed).await?;
        finish(
            &mut report,
            Step::Finished(proposal(outcome, security.as_ref())),
        )?;
        return Ok(report);
    }
    let unsupported: BTreeSet<&str> = lockfiles
        .values()
        .filter(|kind| !super::sources::is_checkable(**kind))
        .map(|kind| kind.filename())
        .collect();
    if !unsupported.is_empty() {
        return Err(Error::Refused(format!(
            "lock mode does not support {} yet; nothing was published",
            unsupported.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    let tools: BTreeSet<String> = lockfiles
        .values()
        .map(|kind| kind.command_for(&[], None).0.to_string())
        .collect();

    let git = &session.git;
    let planned = git
        .bytes([
            "diff",
            "--cached",
            "--binary",
            "--full-index",
            "--no-renames",
            "HEAD",
        ])
        .await?;
    let base_sha = git.read(["rev-parse", "--verify", "HEAD^{commit}"]).await?;
    git.run(["update-ref", BASE_REF, &base_sha]).await?;
    let bundle = dir.join(BUNDLE);
    git.run([
        "bundle".as_ref(),
        "create".as_ref(),
        "--quiet".as_ref(),
        bundle.as_os_str(),
        BASE_REF.as_ref(),
    ])
    .await?;
    write_file(&dir.join(PLANNED), &planned)?;

    let plan = Plan {
        default_branch: project.default_branch.clone(),
        base_sha,
        expected_remote_sha: session.expected_remote_sha().to_string(),
        planned_sha256: sha256(&planned),
        lockfiles: lockfiles.keys().cloned().collect(),
        tools: tools.into_iter().collect(),
        config: ordinary.config.clone(),
        paths: ordinary.paths.clone(),
        langs: ordinary.langs.clone(),
        exclude_langs: ordinary.exclude_langs.clone(),
        min_age_floor: ordinary.min_age_floor.clone(),
        max_bump: ordinary.max_bump.clone(),
        auto_merge: ordinary.auto_merge,
        major_mr: ordinary.major_mr,
        security_remediation: ordinary.security_remediation,
        lock_build,
    };
    report.step = Step::Locking {
        lockfiles: plan.lockfiles.clone(),
    };
    write_work(settings, dir, &report.work(pipeline, Some(plan)))?;
    Ok(report)
}

/// Runs the security fixes and the update with lockfile regeneration on the
/// commit `prepare` bundled into `dir`, and writes the change as a patch
/// with the reports beside it. Holds no token and publishes nothing; only
/// `publish` decides what of the result to trust.
pub async fn lock_worker(dir: &Path, updater: PathBuf, log: &Log) -> Result<StepReport, Error> {
    if std::env::var_os(git::TOKEN_VAR).is_some_and(|token| !token.is_empty()) {
        return Err(Error::Refused(format!(
            "{} is set; the lock job runs repository code and must not hold the token",
            git::TOKEN_VAR
        )));
    }
    let dir = &absolute(dir)?;
    let work_bytes = read_regular(&dir.join(WORK))?;
    let work = parse_work(&work_bytes)?;
    let mut report = StepReport {
        command: "gitlab org lock-worker",
        project: work.project,
        path: work.path.clone(),
        lane: work.lane,
        branch: work.branch.clone(),
        step: Step::NothingToDo,
    };
    let Some(plan) = &work.plan else {
        return Ok(report);
    };
    let planned = read_regular(&dir.join(PLANNED))?;
    if sha256(&planned) != plan.planned_sha256 {
        return Err(Error::Refused(format!(
            "{PLANNED} is not the patch the work describes"
        )));
    }
    let result_dir = dir.join(RESULT_DIR);
    if fs::symlink_metadata(&result_dir).is_ok() {
        return Err(Error::Refused(format!(
            "{} already exists; the lock job writes a fresh result",
            result_dir.display()
        )));
    }

    let mut tools = BTreeMap::new();
    for tool in &plan.tools {
        match lockfile::probe_tool(tool) {
            ToolProbe::Missing => {
                return Err(Error::Io(format!(
                    "lock tool missing: {tool} is not on PATH, and regenerating {} needs it",
                    plan.lockfiles.join(", ")
                )));
            }
            ToolProbe::Present { version } => {
                tools.insert(tool.clone(), version);
            }
        }
    }

    let (_work_dir, checkout) = org::empty_checkout().await?;
    let git = Git::new(&checkout, "")?;
    let bundle = dir.join(BUNDLE);
    git.run([
        "fetch".as_ref(),
        "--quiet".as_ref(),
        "--no-tags".as_ref(),
        bundle.as_os_str(),
        format!("{BASE_REF}:{BASE_REF}").as_ref(),
    ])
    .await?;
    git.run(["checkout", "--quiet", "--detach", BASE_REF])
        .await?;
    let head = git.read(["rev-parse", "--verify", "HEAD^{commit}"]).await?;
    if head != plan.base_sha {
        return Err(Error::Refused(format!(
            "The bundle holds {head}, not the commit the work starts from ({})",
            plan.base_sha
        )));
    }

    let settings = lane_settings(
        worker_settings(&work, plan, checkout.clone(), updater),
        work.lane,
    )?;
    run::prepare_artifact_dir(&git, &checkout).await?;
    let applied = run::apply_changes(&settings, &git, log).await?;
    git.run(["add", "--all"]).await?;
    let patch = git
        .bytes([
            "diff",
            "--cached",
            "--binary",
            "--full-index",
            "--no-renames",
            "HEAD",
        ])
        .await?;

    let result = LockResult {
        format: FORMAT,
        work_sha256: sha256(&work_bytes),
        patch_sha256: sha256(&patch),
        reports: applied.reports,
        tools: tools.clone(),
    };
    fs::create_dir(&result_dir)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", result_dir.display())))?;
    write_file(&result_dir.join(RESULT_PATCH), &patch)?;
    write_file(&result_dir.join(RESULT), &to_json_bytes(&result)?)?;
    report.step = Step::Locked { tools };
    Ok(report)
}

/// Verifies the work `prepare` sealed and the lock job's result, rebuilds
/// the proposal from what [`patch::check`] admits, and publishes it with the
/// lease `prepare` observed.
pub async fn publish(
    settings: &OrgSettings,
    project_id: u64,
    lane: Lane,
    dir: &Path,
    log: &Log,
) -> Result<StepReport, Error> {
    let pipeline = pipeline(settings)?;
    let work_bytes = read_regular(&dir.join(WORK))?;
    let seal = read_regular(&dir.join(SEAL))?;
    verify_seal(&settings.token, pipeline, &work_bytes, &seal)?;
    let work = parse_work(&work_bytes)?;
    if work.pipeline != pipeline || work.project != project_id || work.lane != lane {
        return Err(Error::Refused(format!(
            "The work in {} was prepared for pipeline {}, project {}, the {} lane; this job publishes pipeline {pipeline}, project {project_id}, the {} lane",
            dir.display(),
            work.pipeline,
            work.project,
            lane_name(work.lane),
            lane_name(lane),
        )));
    }
    let mut report = StepReport {
        command: "gitlab org publish",
        project: project_id,
        path: work.path.clone(),
        lane,
        branch: work.branch.clone(),
        step: Step::NothingToDo,
    };
    let Some(plan) = work.plan else {
        return Ok(report);
    };
    let branch = work.branch.as_str();

    let planned = read_regular(&dir.join(PLANNED))?;
    if sha256(&planned) != plan.planned_sha256 {
        return Err(Error::Refused(format!(
            "{PLANNED} is not the patch prepare sealed"
        )));
    }
    let result_dir = dir.join(RESULT_DIR);
    let result: LockResult = serde_json::from_slice(&read_regular(&result_dir.join(RESULT))?)
        .map_err(|error| {
            Error::Refused(format!("The lock job's {RESULT} cannot be read: {error}"))
        })?;
    let result_patch = read_regular(&result_dir.join(RESULT_PATCH))?;
    if result.format != FORMAT {
        return Err(Error::Refused(format!(
            "The lock job wrote format {}, not {FORMAT}",
            result.format
        )));
    }
    if result.work_sha256 != sha256(&work_bytes) {
        return Err(Error::Refused(
            "The lock job's result answers other work than this job's".to_string(),
        ));
    }
    if result.patch_sha256 != sha256(&result_patch) {
        return Err(Error::Refused(format!(
            "{RESULT_PATCH} is not the patch the lock job's result describes"
        )));
    }

    let project = Project {
        id: project_id,
        path: work.path.clone(),
        default_branch: plan.default_branch.clone(),
    };
    let (_work_dir, checkout) = org::empty_checkout().await?;
    let mut ordinary = settings.for_project(&project, checkout);
    ordinary.auto_merge = plan.auto_merge;
    ordinary.major_mr = plan.major_mr;
    ordinary.security_remediation = plan.security_remediation;
    ordinary.config = plan.config.clone();
    ordinary.paths = plan.paths.clone();
    ordinary.langs = plan.langs.clone();
    ordinary.exclude_langs = plan.exclude_langs.clone();
    ordinary.min_age_floor = plan.min_age_floor.clone();
    ordinary.max_bump = plan.max_bump.clone();
    ordinary.lock = true;
    ordinary.no_build = !plan.lock_build;
    let lane_settings = lane_settings(ordinary, lane)?;
    if lane_settings.branch != branch {
        return Err(Error::Refused(format!(
            "prepare planned {branch} for this lane, but this job's automation branch is {}",
            lane_settings.branch
        )));
    }

    let session = Session::open(lane_settings, log).await?;
    if session.expected_remote_sha() != plan.expected_remote_sha {
        return Err(run::lease_conflict(
            branch,
            &format!(
                "prepare saw {}, now {}",
                or_absent(&plan.expected_remote_sha),
                or_absent(session.expected_remote_sha())
            ),
        ));
    }
    let git = &session.git;
    let base = plan.base_sha.as_str();
    let on_default = git
        .test([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{base}^{{commit}}"),
        ])
        .await?
        && git
            .test(["merge-base", "--is-ancestor", base, &session.default_ref])
            .await?;
    if !on_default {
        return Err(Error::Conflict(format!(
            "{} no longer contains {base}, the commit this lane was prepared on; nothing was published",
            plan.default_branch
        )));
    }
    let latest = git
        .read([
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", session.default_ref),
        ])
        .await?;

    git.run([
        "switch",
        "--quiet",
        "--discard-changes",
        "--force-create",
        branch,
        base,
    ])
    .await?;
    let tree = patch::check(git, base, &planned, &result_patch, &plan.paths).await?;
    git.run(["read-tree", "--reset", "-u", &tree]).await?;

    let security = run::assess(&session.settings, &result.reports, log)?;
    let base = if latest == base {
        Base::Latest
    } else {
        Base::Behind(base)
    };
    let (mut presentation, changed) = run::stage_and_present(
        &session.settings,
        git,
        &result.reports.update,
        security.as_ref(),
        base,
    )
    .await?;
    if changed {
        presentation.validation.proposal_integrity_passed = true;
        run::write_artifact(
            &session.settings,
            "upd-presentation.json",
            &presentation.to_artifact(),
        )?;
    }
    let outcome = session.publish(Box::new(presentation), changed).await?;
    report.step = Step::Finished(proposal(outcome, security.as_ref()));
    Ok(report)
}

/// What a lane did, with the counts of the security step when it ran.
fn proposal(outcome: Outcome, security: Option<&Security>) -> Proposal {
    Proposal {
        outcome,
        security: security.map(|security| security.counts),
    }
}

/// The pipeline the work is sealed to.
fn pipeline(settings: &OrgSettings) -> Result<&str, Error> {
    settings
        .pipeline_id
        .as_deref()
        .ok_or_else(|| Error::Input("CI_PIPELINE_ID is not set: GitLab CI provides it".to_string()))
}

/// The group project `project_id`, refused when it lies outside the group
/// the organization run covers.
async fn group_project(
    settings: &OrgSettings,
    api: &Client,
    project_id: u64,
) -> Result<Project, Error> {
    let group = api.group_path(&settings.group).await?;
    let raw = api.project(project_id).await?;
    let path = raw["path_with_namespace"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if !path.starts_with(&format!("{group}/")) {
        return Err(Error::Refused(format!(
            "Project {project_id} ({path}) is not in {group}; the organization run covers only that group"
        )));
    }
    match org::classify(settings, raw) {
        Ok(project) => Ok(project),
        Err(State::Failed(error)) => Err(error),
        Err(State::Skipped(reason)) => Err(Error::Refused(format!(
            "Project {project_id} ({path}) is not a candidate: {reason}"
        ))),
        Err(_) => Err(Error::Io(format!(
            "classifying project {project_id} ({path}) gave no answer"
        ))),
    }
}

/// Every lockfile the lock job must regenerate, by path relative to
/// `checkout`: those beside a manifest the proposal changes, and those the
/// security fixes left pending a relock.
async fn lockfiles_to_refresh(
    git: &Git,
    checkout: &Path,
    security: Option<&Security>,
) -> Result<BTreeMap<String, LockfileType>, Error> {
    let staged = git
        .bytes([
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--no-renames",
            "HEAD",
        ])
        .await?;
    let staged = staged
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect::<Vec<_>>();
    let relock = security.into_iter().flat_map(Security::relock_paths);
    let mut lockfiles = BTreeMap::new();
    for path in staged.iter().map(String::as_str).chain(relock) {
        let path = path.trim_start_matches("./");
        let full = checkout.join(path);
        let name = full
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(kind) = LockfileType::from_filename(&name) {
            lockfiles.insert(path.to_string(), kind);
            continue;
        }
        let (owner, kinds) = lockfile::refreshed_lockfiles(&full).map_err(|error| {
            Error::Refused(format!(
                "cannot tell which lockfiles {path} needs regenerated: {error:#}"
            ))
        })?;
        let owner = owner.unwrap_or(full);
        let owner_dir = lockfile::containing_dir(&owner);
        for kind in kinds {
            let lockfile = owner_dir.join(kind.filename());
            let relative = lockfile.strip_prefix(checkout).map_err(|_| {
                Error::Refused(format!(
                    "{} lies outside the checkout; refusing to regenerate it",
                    lockfile.display()
                ))
            })?;
            lockfiles.insert(relative.to_string_lossy().into_owned(), kind);
        }
    }
    Ok(lockfiles)
}

/// The lock job's settings: the plan's, with no token and nothing that
/// talks to GitLab.
fn worker_settings(work: &Work, plan: &Plan, checkout: PathBuf, updater: PathBuf) -> Settings {
    Settings {
        token: String::new(),
        api_url: String::new(),
        default_branch: plan.default_branch.clone(),
        project_dir: checkout,
        project_id: String::new(),
        project_path: work.path.clone(),
        server_url: String::new(),
        branch: work.branch.clone(),
        commit_message: String::new(),
        mr_title: String::new(),
        git_name: String::new(),
        git_email: String::new(),
        paths: plan.paths.clone(),
        langs: plan.langs.clone(),
        exclude_langs: plan.exclude_langs.clone(),
        packages: String::new(),
        min_age: String::new(),
        min_age_floor: plan.min_age_floor.clone(),
        max_bump: plan.max_bump.clone(),
        lock: true,
        no_build: !plan.lock_build,
        auto_merge: plan.auto_merge,
        security_remediation: plan.security_remediation,
        prepare_command: String::new(),
        validation_command: String::new(),
        updater,
        config: plan.config.clone(),
        dry_run: false,
        major_mr: plan.major_mr,
        major_branch: work.branch.clone(),
        major_commit_message: String::new(),
        lane: Lane::Ordinary,
    }
}

/// The settings of `lane`, from the ordinary lane's.
fn lane_settings(ordinary: Settings, lane: Lane) -> Result<Settings, Error> {
    match lane {
        Lane::Ordinary => Ok(ordinary),
        Lane::Major => ordinary.major_lane().ok_or_else(|| {
            Error::Refused(
                "The work is for the major lane, which this project does not enable".to_string(),
            )
        }),
    }
}

fn lane_name(lane: Lane) -> &'static str {
    match lane {
        Lane::Ordinary => "ordinary",
        Lane::Major => "major",
    }
}

fn or_absent(sha: &str) -> &str {
    if sha.is_empty() { "no branch" } else { sha }
}

fn parse_work(bytes: &[u8]) -> Result<Work, Error> {
    let work: Work = serde_json::from_slice(bytes)
        .map_err(|error| Error::Refused(format!("{WORK} cannot be read: {error}")))?;
    if work.format != FORMAT {
        return Err(Error::Refused(format!(
            "{WORK} is format {}, not {FORMAT}; prepare and this job must run the same upd",
            work.format
        )));
    }
    Ok(work)
}

fn write_work(settings: &OrgSettings, dir: &Path, work: &Work) -> Result<(), Error> {
    let bytes = to_json_bytes(work)?;
    write_file(&dir.join(WORK), &bytes)?;
    let seal = seal(&settings.token, &work.pipeline, &bytes)?;
    write_file(&dir.join(SEAL), seal.as_bytes())
}

fn to_json_bytes(value: &impl Serialize) -> Result<Vec<u8>, Error> {
    serde_json::to_vec_pretty(value)
        .map_err(|error| Error::Io(format!("cannot serialize the work: {error}")))
}

fn mac(token: &str, pipeline: &str, work: &[u8]) -> Result<Hmac<Sha256>, Error> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(token.as_bytes())
        .map_err(|error| Error::Io(format!("cannot key the seal: {error}")))?;
    mac.update(SEAL_DOMAIN);
    mac.update(pipeline.as_bytes());
    mac.update(b"\0");
    mac.update(work);
    Ok(mac)
}

/// The seal over `work`, as lowercase hex.
fn seal(token: &str, pipeline: &str, work: &[u8]) -> Result<String, Error> {
    Ok(hex(&mac(token, pipeline, work)?.finalize().into_bytes()))
}

/// Refuses work whose seal the token did not make for this pipeline.
fn verify_seal(token: &str, pipeline: &str, work: &[u8], seal: &[u8]) -> Result<(), Error> {
    let refused = || {
        Error::Refused(format!(
            "{WORK} does not carry this pipeline's seal; it was altered, or prepared elsewhere"
        ))
    };
    let expected = unhex(seal.trim_ascii()).ok_or_else(refused)?;
    mac(token, pipeline, work)?
        .verify_slice(&expected)
        .map_err(|_| refused())
}

fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &[u8]) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.chunks(2)
        .map(|pair| {
            let digit = |c: u8| (c as char).to_digit(16);
            Some((digit(pair[0])? * 16 + digit(pair[1])?) as u8)
        })
        .collect()
}

/// Creates `dir`, or accepts it empty. Refuses one holding anything, rather
/// than deleting what an earlier job left there.
fn create_empty_dir(dir: &Path) -> Result<(), Error> {
    match fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(Error::Refused(format!(
                    "{} is not empty; prepare writes its work into an empty directory",
                    dir.display()
                )));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(dir)
            .map_err(|error| Error::Io(format!("cannot create {}: {error}", dir.display()))),
        Err(error) => Err(Error::Io(format!("cannot read {}: {error}", dir.display()))),
    }
}

fn write_file(path: &Path, content: &[u8]) -> Result<(), Error> {
    fs::write(path, content)
        .map_err(|error| Error::Io(format!("cannot write {}: {error}", path.display())))
}

/// Reads `path`, refusing anything but a regular file of at most
/// [`MAX_FILE`] bytes: an artifact can carry a symbolic link to a file the
/// job that reads it can see.
fn read_regular(path: &Path) -> Result<Vec<u8>, Error> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| Error::Refused(format!("cannot read {}: {error}", path.display())))?;
    if !metadata.file_type().is_file() {
        return Err(Error::Refused(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let file = fs::File::open(path)
        .map_err(|error| Error::Io(format!("cannot open {}: {error}", path.display())))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::Io(format!("cannot read {}: {error}", path.display())))?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(Error::Refused(format!(
            "{} is larger than {} MiB",
            path.display(),
            MAX_FILE / 1024 / 1024
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(plan: Option<Plan>) -> Work {
        Work {
            format: FORMAT,
            pipeline: "981".to_string(),
            project: 7,
            path: "group/project".to_string(),
            lane: Lane::Ordinary,
            branch: "upd/dependencies".to_string(),
            plan,
        }
    }

    #[test]
    fn a_seal_verifies_only_for_its_token_pipeline_and_bytes() {
        let bytes = to_json_bytes(&work(None)).unwrap();
        let sealed = seal("token", "981", &bytes).unwrap();
        assert!(verify_seal("token", "981", &bytes, sealed.as_bytes()).is_ok());
        assert!(verify_seal("token", "981", &bytes, format!("{sealed}\n").as_bytes()).is_ok());

        assert!(verify_seal("other", "981", &bytes, sealed.as_bytes()).is_err());
        assert!(verify_seal("token", "982", &bytes, sealed.as_bytes()).is_err());
        let mut altered = bytes.clone();
        altered.push(b' ');
        assert!(verify_seal("token", "981", &altered, sealed.as_bytes()).is_err());
        assert!(verify_seal("token", "981", &bytes, b"").is_err());
        assert!(verify_seal("token", "981", &bytes, b"zz").is_err());
        assert!(verify_seal("token", "981", &bytes, &sealed.as_bytes()[1..]).is_err());
    }

    #[test]
    fn the_pipeline_and_work_are_delimited_inside_the_seal() {
        // "98" + "1{..." must not seal the same as "981" + "{...".
        let bytes = to_json_bytes(&work(None)).unwrap();
        let mut shifted = b"1".to_vec();
        shifted.extend_from_slice(&bytes);
        assert_ne!(
            seal("token", "981", &bytes).unwrap(),
            seal("token", "98", &shifted).unwrap()
        );
    }

    #[test]
    fn hex_round_trips_and_rejects_malformed_text() {
        let bytes = [0x00, 0x7f, 0xab, 0xff];
        assert_eq!(hex(&bytes), "007fabff");
        assert_eq!(unhex(b"007fabff").unwrap(), bytes);
        assert_eq!(unhex(b"007FABFF").unwrap(), bytes);
        assert!(unhex(b"0").is_none());
        assert!(unhex(b"0g").is_none());
    }

    #[test]
    fn work_of_another_format_or_shape_is_refused() {
        let mut other = work(None);
        other.format = FORMAT + 1;
        let error = parse_work(&to_json_bytes(&other).unwrap()).unwrap_err();
        assert!(error.message().contains("format 2"), "{error:?}");

        let mut value = serde_json::to_value(work(None)).unwrap();
        value["extra"] = json!(true);
        assert!(parse_work(value.to_string().as_bytes()).is_err());
        assert!(parse_work(&to_json_bytes(&work(None)).unwrap()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn only_a_small_regular_file_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, b"content").unwrap();
        assert_eq!(read_regular(&file).unwrap(), b"content");

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let error = read_regular(&link).unwrap_err();
        assert!(error.message().contains("not a regular file"), "{error:?}");
        assert!(read_regular(dir.path()).is_err());
        assert!(read_regular(&dir.path().join("absent")).is_err());

        let large = dir.path().join("large");
        fs::File::create(&large)
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        let error = read_regular(&large).unwrap_err();
        assert!(error.message().contains("larger than"), "{error:?}");
    }

    #[test]
    fn prepare_writes_only_into_an_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("a/b");
        create_empty_dir(&fresh).unwrap();
        create_empty_dir(&fresh).unwrap();
        fs::write(fresh.join(WORK), b"{}").unwrap();
        let error = create_empty_dir(&fresh).unwrap_err();
        assert!(error.message().contains("not empty"), "{error:?}");
        assert!(fresh.join(WORK).exists());
    }
}
