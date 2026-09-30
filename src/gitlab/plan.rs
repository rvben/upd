//! `upd gitlab org plan`: the first job of an organization run in lock mode.
//!
//! Lock mode regenerates lockfiles in jobs that hold no token, so every
//! project that consents to it gets three jobs of its own per lane: prepare
//! and publish with the token, the lock job between them without it. The
//! number of those jobs depends on the group, so this job writes them into a
//! child pipeline that the template's trigger job starts. The child also
//! carries one `organization-run` job that updates every other project as
//! `gitlab org run` does.
//!
//! Planning reads each project's opt-in through the API only: no clone, no
//! project code, no writes. The child's jobs run the binary this job runs,
//! copied into the job's artifacts, so they need no download of their own.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::Path;

use futures::StreamExt;
use serde_json::{Map, Value, json};

use super::Error;
use super::api::Client;
use super::org::{self, Consent, OrgSettings, Project, Shard};
use super::run::{self, Lane};

/// Where the plan writes the child pipeline and the binary its jobs run,
/// relative to the job's directory; the template keeps it as an artifact.
pub const PLAN_DIR: &str = ".upd-ci";
/// The child pipeline configuration, inside [`PLAN_DIR`].
pub const PIPELINE_FILE: &str = "upd-lock-pipeline.yml";
/// The binary the child's jobs run, inside [`PLAN_DIR`].
const BINARY: &str = "bin/upd";

/// GitLab's default `max_artifacts_content_include_size`: the largest
/// artifact archive a trigger job includes a pipeline from.
const MAX_INCLUDED_ARCHIVE: u64 = 5 * 1024 * 1024;
/// Where each lane's jobs exchange their work, inside [`PLAN_DIR`].
const WORK_DIR: &str = "work";

/// The organization settings the child's jobs read, set by value in each
/// job's script. The token is never among them: the child's jobs that need
/// it get it from the environment it is scoped to, and the lock jobs must not
/// get it at all. `UPD_EXECUTABLE` stays with the runner's own environment,
/// since it names a program on the machine that runs the job, and
/// `UPD_LOCK_HANDED_OFF` is set on the one job that reads it.
const FORWARDED: [&str; 17] = [
    "UPD_GROUP",
    "UPD_BRANCH",
    "UPD_COMMIT_MESSAGE",
    "UPD_GIT_NAME",
    "UPD_GIT_EMAIL",
    "UPD_LANGS",
    "UPD_MIN_AGE",
    "UPD_MAX_BUMP",
    "UPD_EXCLUDE",
    "UPD_AUTO_MERGE",
    "UPD_MAJOR_MR",
    "UPD_SECURITY_REMEDIATION",
    "UPD_LOCK",
    "UPD_MAJOR_BRANCH",
    "UPD_MAJOR_COMMIT_MESSAGE",
    "UPD_CONCURRENCY",
    "UPD_SHARD",
];

/// Installs git when the job image lacks it; every child job needs it.
const SETUP: &str = r#"if ! command -v git >/dev/null 2>&1; then
  if command -v apt-get >/dev/null 2>&1; then
    apt-get update
    apt-get install --yes --no-install-recommends ca-certificates git
  elif command -v apk >/dev/null 2>&1; then
    apk add --no-cache ca-certificates git
  else
    echo "git is missing from the job image; use an image that has it" >&2
    exit 2
  fi
fi"#;

/// How the child pipeline's jobs are placed, read from the environment the
/// organization template provides.
#[derive(Debug, Clone)]
pub struct PlanSettings {
    pub org: OrgSettings,
    /// The pipeline running the plan job, whose artifacts the child's jobs
    /// fetch.
    pub pipeline_id: u64,
    /// The plan job's name, which the child's jobs fetch the binary from.
    pub job: String,
    /// The image of the jobs that hold the token.
    pub image: String,
    /// The image of the lock jobs, which carries the lock tools.
    pub lock_image: String,
    /// Runner tags of the jobs that hold the token; empty for any runner.
    pub runner_tags: Vec<String>,
    /// Runner tags of the lock jobs; never empty.
    pub lock_runner_tags: Vec<String>,
    /// The environment the token is scoped to.
    pub environment: String,
    /// The organization settings the child's jobs read, as set here.
    pub forwarded: BTreeMap<&'static str, String>,
}

impl PlanSettings {
    pub fn from_env(dry_run: bool) -> Result<Self, Error> {
        Self::from_lookup(|name| env::var(name).ok(), dry_run)
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>, dry_run: bool) -> Result<Self, Error> {
        let org = OrgSettings::from_lookup(&lookup, dry_run)?;
        let value = |name: &str| lookup(name).filter(|value| !value.is_empty());
        if !org.lock {
            return Err(Error::Input(
                "gitlab org plan plans lock mode; set UPD_LOCK=true, or use gitlab org run"
                    .to_string(),
            ));
        }
        let pipeline_id = value("CI_PIPELINE_ID")
            .ok_or_else(|| {
                Error::Input("CI_PIPELINE_ID is not set: GitLab CI provides it".to_string())
            })?
            .parse::<u64>()
            .map_err(|_| {
                Error::Input("CI_PIPELINE_ID must be a numeric pipeline ID".to_string())
            })?;
        let job = value("CI_JOB_NAME").ok_or_else(|| {
            Error::Input("CI_JOB_NAME is not set: GitLab CI provides it".to_string())
        })?;
        let named = value("UPD_ORGANIZATION_JOB").ok_or_else(|| {
            Error::Input(
                "UPD_ORGANIZATION_JOB is not set: the template's organization_job input sets it"
                    .to_string(),
            )
        })?;
        if named != job {
            return Err(Error::Input(format!(
                "the organization_job input names {named}, but this job is {job}; set organization_job to the name of the job that extends .upd-organization-update, so the trigger job and the child pipeline find its artifacts"
            )));
        }
        let image = value("UPD_IMAGE").ok_or_else(|| {
            Error::Input("UPD_IMAGE is not set: the template's image input sets it".to_string())
        })?;
        check_image("UPD_IMAGE", &image)?;
        let lock_image = value("UPD_LOCK_IMAGE").unwrap_or_else(|| image.clone());
        check_image("UPD_LOCK_IMAGE", &lock_image)?;
        let runner_tags = tags("UPD_RUNNER_TAGS", value("UPD_RUNNER_TAGS"))?;
        let lock_runner_tags = tags("UPD_LOCK_RUNNER_TAGS", value("UPD_LOCK_RUNNER_TAGS"))?;
        if lock_runner_tags.is_empty() {
            return Err(Error::Input(
                "UPD_LOCK_RUNNER_TAGS is empty: set the lock_runner_tags input to tags of runners reserved for lock jobs, which run each project's code".to_string(),
            ));
        }
        let environment =
            value("UPD_ENVIRONMENT").unwrap_or_else(|| "upd-organization".to_string());
        if !is_environment_name(&environment) {
            return Err(Error::Input(format!(
                "UPD_ENVIRONMENT must be an environment name of letters, digits, spaces, '-', '_' and '/', not '{environment}'"
            )));
        }
        let forwarded = FORWARDED
            .into_iter()
            .filter_map(|name| value(name).map(|value| (name, value)))
            .collect();
        Ok(Self {
            org,
            pipeline_id,
            job,
            image,
            lock_image,
            runner_tags,
            lock_runner_tags,
            environment,
            forwarded,
        })
    }
}

fn check_image(name: &str, image: &str) -> Result<(), Error> {
    if image
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '$')
    {
        return Err(Error::Input(format!(
            "{name} must be one image reference without spaces or variables, not '{image}'"
        )));
    }
    Ok(())
}

/// Comma-separated runner tags.
fn tags(name: &str, text: Option<String>) -> Result<Vec<String>, Error> {
    let Some(text) = text else {
        return Ok(Vec::new());
    };
    text.split(',')
        .map(str::trim)
        .map(|tag| {
            if tag.is_empty() || tag.chars().any(|c| c.is_control() || c == '$') {
                Err(Error::Input(format!(
                    "{name} must be comma-separated runner tags without variables, not '{text}'"
                )))
            } else {
                Ok(tag.to_string())
            }
        })
        .collect()
}

fn is_environment_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.ends_with('/')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'-' | b'_' | b'/'))
}

/// A project whose lanes run in their own jobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandedOff {
    pub id: u64,
    pub path: String,
    pub lanes: Vec<Lane>,
}

/// A project the plan could not read; `organization-run` handles it.
#[derive(Debug)]
pub struct Unplanned {
    pub id: u64,
    pub path: String,
    pub error: Error,
}

/// What the plan decided.
#[derive(Debug)]
pub struct Plan {
    pub group: String,
    pub dry_run: bool,
    pub shard: Option<Shard>,
    /// Projects listed in the group, or in its shard.
    pub projects: usize,
    pub handed_off: Vec<HandedOff>,
    pub unplanned: Vec<Unplanned>,
}

impl Plan {
    fn lanes(&self) -> usize {
        self.handed_off
            .iter()
            .map(|project| project.lanes.len())
            .sum()
    }

    /// The child pipeline's job count: three per lane and `organization-run`.
    pub fn jobs(&self) -> usize {
        1 + 3 * self.lanes()
    }

    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "command": "gitlab org plan",
            "group": self.group,
            "dry_run": self.dry_run,
            "pipeline": format!("{PLAN_DIR}/{PIPELINE_FILE}"),
            "counts": {
                "projects": self.projects,
                "handed_off": self.handed_off.len(),
                "lanes": self.lanes(),
                "jobs": self.jobs(),
                "unplanned": self.unplanned.len(),
            },
            "handed_off": self.handed_off.iter().map(|project| json!({
                "id": project.id,
                "path": project.path,
                "lanes": project.lanes.iter().map(|lane| lane_name(*lane)).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "unplanned": self.unplanned.iter().map(|project| json!({
                "id": project.id,
                "path": project.path,
                "error": {
                    "kind": project.error.kind(),
                    "message": project.error.message(),
                    "exit_code": project.error.exit_code(),
                },
            })).collect::<Vec<_>>(),
        });
        if let Some(shard) = &self.shard {
            value["shard"] = json!(shard.to_string());
        }
        value
    }

    pub fn render_text(&self) -> String {
        let mut lines: Vec<String> = self
            .unplanned
            .iter()
            .map(|project| {
                format!(
                    "{}: opt-in unreadable, left to organization-run ({}): {}",
                    project.path,
                    project.error.kind(),
                    project.error.message()
                )
            })
            .collect();
        lines.extend(self.handed_off.iter().map(|project| {
            let lanes: Vec<&str> = project.lanes.iter().map(|lane| lane_name(*lane)).collect();
            format!("{}: lock jobs for {}", project.path, lanes.join(" and "))
        }));
        let shard = match &self.shard {
            Some(shard) => format!(" (shard {shard})"),
            None => String::new(),
        };
        lines.push(format!(
            "{}{} projects in {}{shard}: {} handed to lock jobs, {} unplanned; the child pipeline has {} jobs.",
            if self.dry_run { "Dry run: " } else { "" },
            self.projects,
            self.group,
            self.handed_off.len(),
            self.unplanned.len(),
            self.jobs(),
        ));
        lines.join("\n")
    }
}

fn lane_name(lane: Lane) -> &'static str {
    match lane {
        Lane::Ordinary => "ordinary",
        Lane::Major => "major",
    }
}

/// Plans the run and writes the child pipeline and the binary its jobs run
/// into `dir`.
pub async fn plan(settings: &PlanSettings, dir: &Path) -> Result<Plan, Error> {
    let org = &settings.org;
    run::check_branch_name(&env::temp_dir(), &org.branch).await?;
    if org.major_mr {
        run::check_branch_name(&env::temp_dir(), &org.major_branch).await?;
    }
    let plan = if org.dry_run {
        // A dry run hands nothing off: organization-run previews every
        // project, lock consenting ones included.
        let listed = org::in_shard(
            org,
            Client::new(&org.api_url, &org.token)?
                .group_projects(&org.group)
                .await?,
        );
        Plan {
            group: org.group.clone(),
            dry_run: true,
            shard: org.shard,
            projects: listed.len(),
            handed_off: Vec::new(),
            unplanned: Vec::new(),
        }
    } else {
        discover(org).await?
    };
    write(settings, &plan, dir)?;
    Ok(plan)
}

/// Refuses a central project that lets CI/CD job tokens push to it. Every
/// lock job holds such a token and runs repository code, which could then
/// rewrite the pipeline that hands out the group token. GitLab shows the
/// setting only to a token with the Maintainer role, so a project that does
/// not show it is refused as well.
async fn check_central_project(api: &Client, central: Option<u64>) -> Result<(), Error> {
    let central = central.ok_or_else(|| {
        Error::Input(
            "lock mode runs in the central project's pipeline, but CI_PROJECT_ID is not set"
                .to_string(),
        )
    })?;
    let project = api.project(central).await?;
    match project.get("ci_push_repository_for_job_token_allowed") {
        Some(Value::Bool(false)) => Ok(()),
        Some(Value::Bool(true)) => Err(Error::Refused(format!(
            "project {central} lets CI/CD job tokens push to it, so a lock job could change the pipeline that holds the group token; turn off Settings > CI/CD > Job token permissions > Allow Git push requests to the repository"
        ))),
        _ => Err(Error::Refused(format!(
            "GitLab does not show whether project {central} lets CI/CD job tokens push to it; give the token the Maintainer role there so lock mode can check that it does not"
        ))),
    }
}

async fn discover(org: &OrgSettings) -> Result<Plan, Error> {
    let api = Client::new(&org.api_url, &org.token)?;
    check_central_project(&api, org.central_project).await?;
    let listed = org::in_shard(org, api.group_projects(&org.group).await?);
    let projects = listed.len();
    let candidates: Vec<Project> = listed
        .into_iter()
        .filter_map(|raw| org::classify(org, raw).ok())
        .collect();
    let answers = futures::stream::iter(candidates.into_iter().map(|project| {
        let api = &api;
        async move {
            let consent = org::api_consent(api, &project).await;
            (project, consent)
        }
    }))
    .buffer_unordered(org.concurrency)
    .collect::<Vec<_>>()
    .await;

    let mut handed_off = Vec::new();
    let mut unplanned = Vec::new();
    for (project, consent) in answers {
        match consent {
            Ok(Consent::Yes {
                lock: true,
                major_mr,
                ..
            }) => {
                let mut lanes = vec![Lane::Ordinary];
                if org.major_mr && major_mr {
                    lanes.push(Lane::Major);
                }
                handed_off.push(HandedOff {
                    id: project.id,
                    path: project.path,
                    lanes,
                });
            }
            Ok(_) => {}
            Err(error) => unplanned.push(Unplanned {
                id: project.id,
                path: project.path,
                error,
            }),
        }
    }
    handed_off.sort_by_key(|project| project.id);
    unplanned.sort_by_key(|project| project.id);
    Ok(Plan {
        group: org.group.clone(),
        dry_run: false,
        shard: org.shard,
        projects,
        handed_off,
        unplanned,
    })
}

fn write(settings: &PlanSettings, plan: &Plan, dir: &Path) -> Result<(), Error> {
    let io = |what: &str, path: &Path, error: std::io::Error| {
        Error::Io(format!("cannot {what} {}: {error}", path.display()))
    };
    let binary = dir.join(BINARY);
    let parent = binary.parent().expect("the binary path has a directory");
    fs::create_dir_all(parent).map_err(|error| io("create", parent, error))?;
    let current = env::current_exe()
        .map_err(|error| Error::Io(format!("cannot locate the upd executable: {error}")))?;
    // A plan run by the copy itself leaves it in place: copying a file onto
    // itself would truncate it.
    let same = matches!(
        (fs::canonicalize(&binary), fs::canonicalize(&current)),
        (Ok(binary), Ok(current)) if binary == current
    );
    if !same {
        fs::copy(&current, &binary)
            .map_err(|error| io("copy the upd executable to", &binary, error))?;
    }
    make_executable(&binary).map_err(|error| io("make executable", &binary, error))?;
    let digest =
        super::split::sha256(&fs::read(&binary).map_err(|error| io("read", &binary, error))?);
    let pipeline = dir.join(PIPELINE_FILE);
    let text = pipeline_text(settings, plan, &digest, MAX_INCLUDED_ARCHIVE)?;
    fs::write(&pipeline, text).map_err(|error| io("write", &pipeline, error))
}

/// The child pipeline file, refused when its artifact archive would be
/// larger than `limit`. The size is measured as the runner archives the
/// file: deflated, alone in a zip. GitLab would otherwise start a child
/// pipeline with no jobs and only a configuration error to show for it.
fn pipeline_text(
    settings: &PlanSettings,
    plan: &Plan,
    digest: &str,
    limit: u64,
) -> Result<String, Error> {
    use std::io::Write;
    let text = serde_json::to_string_pretty(&child_pipeline(settings, plan, digest))
        .map_err(|error| Error::Io(format!("cannot serialize the child pipeline: {error}")))?
        + "\n";
    let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let size = archive
        .start_file(
            format!("{PLAN_DIR}/{PIPELINE_FILE}"),
            zip::write::SimpleFileOptions::default(),
        )
        .and_then(|()| Ok(archive.write_all(text.as_bytes())?))
        .and_then(|()| archive.finish())
        .map_err(|error| Error::Io(format!("cannot measure the child pipeline: {error}")))?
        .into_inner()
        .len() as u64;
    if size > limit {
        return Err(Error::Refused(format!(
            "The lock pipeline is a {size}-byte artifact archive, past the {limit} bytes GitLab includes a pipeline from by default; split the group across schedules with UPD_SHARD"
        )));
    }
    Ok(text)
}

/// Child jobs run on Linux runners; on other hosts the plan still writes
/// the binary, which only a Unix job would run.
#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The child pipeline, as JSON, which GitLab reads as YAML. `binary_sha256`
/// is the digest of the binary the plan wrote for the child's jobs.
pub fn child_pipeline(settings: &PlanSettings, plan: &Plan, binary_sha256: &str) -> Value {
    let binary = format!("{PLAN_DIR}/{BINARY}");
    // A clean directory per job: artifacts only, no stale work of an
    // earlier job on the same runner, and no clone of this project.
    let variables = json!({"GIT_STRATEGY": "empty"});
    // The settings the plan read, set by the script itself: GitLab ranks the
    // project's and group's CI/CD variables above any YAML variable, so only
    // the script can guarantee each job runs with the planned values, and
    // without a setting the plan did not have.
    let planned = pinned(
        settings
            .forwarded
            .iter()
            .map(|(name, value)| (*name, value.as_str())),
    );

    let from_plan = json!({"pipeline": settings.pipeline_id.to_string(), "job": settings.job});
    let job = |image: &str, tags: &[String], needs: Vec<Value>, script: Vec<String>| {
        let mut value = json!({
            "image": image,
            "interruptible": false,
            "needs": std::iter::once(from_plan.clone()).chain(needs).collect::<Vec<_>>(),
            "before_script": [SETUP, format!("chmod 0755 {binary}")],
            "script": std::iter::once(planned.clone()).chain(script).collect::<Vec<_>>(),
        });
        if !tags.is_empty() {
            value["tags"] = json!(tags);
        }
        value
    };
    // A job that holds the token runs only the binary the plan wrote. It
    // unpacks no artifacts of a job that runs project code, which could
    // carry another binary under the same name; the check holds that line
    // should a dependency ever be added.
    let verify = format!(
        r#"if [ "$(sha256sum {binary} | cut -d ' ' -f 1)" != {} ]; then
  echo "{binary} is not the upd binary the plan wrote; a job that holds the token runs no other" >&2
  exit 2
fi"#,
        quote(binary_sha256)
    );
    let with_token = |mut value: Value| {
        value["environment"] = json!({"name": settings.environment, "action": "access"});
        value["before_script"] = json!([SETUP, verify, format!("chmod 0755 {binary}")]);
        value
    };

    let mut jobs = Map::new();
    let handed: BTreeSet<u64> = plan.handed_off.iter().map(|project| project.id).collect();
    let mut run = format!("{binary} gitlab org run --output json");
    if plan.dry_run {
        run.push_str(" --dry-run");
    }
    let mut organization = with_token(job(
        &settings.image,
        &settings.runner_tags,
        Vec::new(),
        vec![
            format!(
                "export UPD_LOCK_HANDED_OFF={}",
                quote(
                    &handed
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                )
            ),
            format!("mkdir -p {PLAN_DIR}"),
            format!("{run} > {PLAN_DIR}/upd-org-report.json"),
        ],
    ));
    // The organization job's own time: this job does all of its work for
    // every project not handed off.
    organization["timeout"] = json!("2h");
    organization["artifacts"] = json!({
        "when": "always",
        "expire_in": "1 week",
        "access": "developer",
        "paths": [format!("{PLAN_DIR}/upd-org-report.json")],
    });
    jobs.insert("organization-run".to_string(), organization);

    for project in &plan.handed_off {
        for lane in &project.lanes {
            let name = match lane {
                Lane::Ordinary => project.id.to_string(),
                Lane::Major => format!("{}-major", project.id),
            };
            let work = format!("{PLAN_DIR}/{WORK_DIR}/{}-{}", project.id, lane_name(*lane));
            let step = |step: &str| {
                format!(
                    "{binary} gitlab org {step} --project {} --lane {} --dir {work}",
                    project.id,
                    lane_name(*lane)
                )
            };
            let (prepare, lock, publish) = (
                format!("prepare-{name}"),
                format!("lock-{name}"),
                format!("publish-{name}"),
            );
            // Each job's artifacts are readable only by the jobs that need
            // them: nobody downloads a project's work from the UI or API.
            let mut value = with_token(job(
                &settings.image,
                &settings.runner_tags,
                Vec::new(),
                vec![step("prepare")],
            ));
            value["artifacts"] =
                json!({"expire_in": "1 day", "access": "none", "paths": [format!("{work}/")]});
            jobs.insert(prepare.clone(), value);

            let mut value = job(
                &settings.lock_image,
                &settings.lock_runner_tags,
                vec![json!({"job": prepare})],
                vec![format!("{binary} gitlab org lock-worker --dir {work}")],
            );
            value["artifacts"] = json!({"expire_in": "1 day", "access": "none", "paths": [format!("{work}/result/")]});
            jobs.insert(lock.clone(), value);

            // Publish waits for the lock job but takes none of its
            // artifacts: GitLab would unpack the whole archive the lock job
            // uploaded, whatever `paths` says, and load any dotenv report
            // in it as this job's variables. Publish downloads the archive
            // itself and reads only the result from it.
            let value = with_token(job(
                &settings.image,
                &settings.runner_tags,
                vec![
                    json!({"job": prepare}),
                    json!({"job": lock, "artifacts": false}),
                ],
                vec![format!("{} --lock-job {lock}", step("publish"))],
            ));
            jobs.insert(publish, value);
        }
    }

    let mut pipeline = Map::new();
    pipeline.insert("variables".to_string(), variables);
    pipeline.extend(jobs);
    Value::Object(pipeline)
}

/// A shell script that exports each of `set` and unsets every other
/// forwarded setting, so no variable from elsewhere stands in for one. It
/// unsets `UPD_LOCK_HANDED_OFF` too, which only organization-run sets.
fn pinned<'a>(set: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let set: BTreeMap<&str, &str> = set.collect();
    FORWARDED
        .iter()
        .map(|name| match set.get(name) {
            Some(value) => format!("export {name}={}", quote(value)),
            None => format!("unset {name}"),
        })
        .chain(std::iter::once("unset UPD_LOCK_HANDED_OFF".to_string()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `value` as one single-quoted shell word, taken literally: `$`, backquotes,
/// backslashes and newlines included.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const BASE: [(&str, &str); 11] = [
        ("UPD_GITLAB_TOKEN", "secret-token"),
        ("CI_SERVER_URL", "https://gitlab.example.test"),
        ("UPD_GROUP", "acme"),
        ("UPD_EXECUTABLE", "/usr/local/bin/upd"),
        ("UPD_LOCK", "true"),
        ("CI_PIPELINE_ID", "981"),
        ("CI_JOB_NAME", "upd-organization-update"),
        ("UPD_ORGANIZATION_JOB", "upd-organization-update"),
        ("UPD_IMAGE", "debian:bookworm-slim"),
        ("UPD_LOCK_RUNNER_TAGS", "upd-lock, sandboxed"),
        ("UPD_COMMIT_MESSAGE", "chore(deps): bump $PRICE"),
    ];

    /// The digest of a binary no test runs.
    const DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn settings(overrides: &[(&str, &str)]) -> Result<PlanSettings, Error> {
        let mut vars: HashMap<&str, &str> = HashMap::from(BASE);
        vars.extend(overrides.iter().copied());
        PlanSettings::from_lookup(|name| vars.get(name).map(|value| value.to_string()), false)
    }

    fn plan(handed_off: Vec<HandedOff>) -> Plan {
        Plan {
            group: "acme".to_string(),
            dry_run: false,
            shard: None,
            projects: 5,
            handed_off,
            unplanned: Vec::new(),
        }
    }

    fn two_projects() -> Vec<HandedOff> {
        vec![
            HandedOff {
                id: 11,
                path: "acme/app".to_string(),
                lanes: vec![Lane::Ordinary],
            },
            HandedOff {
                id: 12,
                path: "acme/lib".to_string(),
                lanes: vec![Lane::Ordinary, Lane::Major],
            },
        ]
    }

    fn names(pipeline: &Value) -> Vec<&str> {
        pipeline
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .filter(|name| *name != "variables")
            .collect()
    }

    fn needed(job: &Value) -> Vec<Value> {
        job["needs"].as_array().unwrap().clone()
    }

    #[test]
    fn every_organization_setting_but_the_token_reaches_the_child() {
        // The names OrgSettings reads, recorded from the real reader.
        let read = RefCell::new(BTreeSet::new());
        let vars: HashMap<&str, &str> = HashMap::from(BASE);
        OrgSettings::from_lookup(
            |name| {
                read.borrow_mut().insert(name.to_string());
                vars.get(name).map(|value| value.to_string())
            },
            false,
        )
        .unwrap();
        let read: BTreeSet<String> = read
            .into_inner()
            .into_iter()
            .filter(|name| name.starts_with("UPD_"))
            .collect();
        let kept_back = ["UPD_GITLAB_TOKEN", "UPD_EXECUTABLE", "UPD_LOCK_HANDED_OFF"];
        let expected: BTreeSet<String> = read
            .iter()
            .filter(|name| !kept_back.contains(&name.as_str()))
            .cloned()
            .collect();
        let forwarded: BTreeSet<String> = FORWARDED.iter().map(|name| name.to_string()).collect();
        assert_eq!(forwarded, expected);
        for name in kept_back {
            assert!(
                read.contains(name),
                "{name} is no longer read; update this test"
            );
        }
    }

    #[test]
    fn the_child_never_carries_the_token_and_keeps_values_literal() {
        let message = "chore(deps): it's $PRICE\n\n`date` \\ $(id) \"quoted\"";
        let settings = settings(&[("UPD_COMMIT_MESSAGE", message)]).unwrap();
        let pipeline = child_pipeline(&settings, &plan(two_projects()), DIGEST);
        let text = serde_json::to_string(&pipeline).unwrap();
        assert!(!text.contains("secret-token"), "{text}");
        assert!(!text.contains("UPD_GITLAB_TOKEN"), "{text}");
        assert!(!text.contains("UPD_EXECUTABLE"), "{text}");
        assert_eq!(pipeline["variables"], json!({"GIT_STRATEGY": "empty"}));

        // Every job's script sets the planned values over whatever the
        // project's variables hold, and removes the ones the plan did not
        // have, before it runs anything else.
        for (name, job) in pipeline.as_object().unwrap() {
            if name == "variables" {
                continue;
            }
            let script: Vec<&str> = job["script"]
                .as_array()
                .unwrap()
                .iter()
                .map(|line| line.as_str().unwrap())
                .collect();
            let body = script.join("\n");
            let run = |hostile: &[(&str, &str)]| {
                let mut command = std::process::Command::new("sh");
                command
                    .env_clear()
                    .env("PATH", env::var_os("PATH").unwrap())
                    .args([
                        "-c",
                        &format!("{}\nenv", script[..script.len() - 1].join("\n")),
                    ]);
                for (variable, value) in hostile {
                    command.env(variable, value);
                }
                let output = command.output().unwrap();
                assert!(output.status.success(), "{name}: {body}");
                String::from_utf8(output.stdout).unwrap()
            };
            let environment = run(&[
                ("UPD_COMMIT_MESSAGE", "overridden"),
                ("UPD_GROUP", "elsewhere"),
                ("UPD_SHARD", "2/2"),
                ("UPD_LOCK_HANDED_OFF", "99"),
            ]);
            assert!(
                environment.contains(&format!("UPD_COMMIT_MESSAGE={message}\n")),
                "{name}: {environment}"
            );
            assert!(
                environment.contains("UPD_GROUP=acme\n"),
                "{name}: {environment}"
            );
            assert!(
                environment.contains("UPD_LOCK=true\n"),
                "{name}: {environment}"
            );
            assert!(!environment.contains("UPD_SHARD="), "{name}: {environment}");
            if name == "organization-run" {
                assert!(
                    environment.contains("UPD_LOCK_HANDED_OFF=11,12\n"),
                    "{environment}"
                );
            } else {
                assert!(!environment.contains("UPD_LOCK_HANDED_OFF="), "{name}");
            }
        }
    }

    #[test]
    fn each_lane_gets_prepare_lock_and_publish_and_only_token_jobs_get_the_environment() {
        let settings = settings(&[
            ("UPD_LOCK_IMAGE", "ghcr.example.test/uv:1"),
            ("UPD_RUNNER_TAGS", "trusted"),
        ])
        .unwrap();
        let plan = plan(two_projects());
        let pipeline = child_pipeline(&settings, &plan, DIGEST);
        assert_eq!(
            names(&pipeline),
            [
                "lock-11",
                "lock-12",
                "lock-12-major",
                "organization-run",
                "prepare-11",
                "prepare-12",
                "prepare-12-major",
                "publish-11",
                "publish-12",
                "publish-12-major",
            ]
        );
        assert_eq!(names(&pipeline).len(), plan.jobs());
        let from_plan = json!({"pipeline": "981", "job": "upd-organization-update"});
        let environment = json!({"name": "upd-organization", "action": "access"});
        for name in names(&pipeline) {
            let job = &pipeline[name];
            assert_eq!(needed(job)[0], from_plan, "{name}");
            assert_eq!(job["interruptible"], false, "{name}");
            let before = job["before_script"].as_array().unwrap();
            assert_eq!(before[0], SETUP, "{name}");
            assert_eq!(
                before.last().unwrap(),
                "chmod 0755 .upd-ci/bin/upd",
                "{name}"
            );
            if name.starts_with("lock-") {
                assert!(job.get("environment").is_none(), "{name} holds the token");
                assert_eq!(job["tags"], json!(["upd-lock", "sandboxed"]), "{name}");
                assert_eq!(job["image"], "ghcr.example.test/uv:1", "{name}");
                // The lock image need not carry sha256sum.
                assert_eq!(before.len(), 2, "{name}");
            } else {
                assert_eq!(job["environment"], environment, "{name}");
                assert_eq!(job["tags"], json!(["trusted"]), "{name}");
                assert_eq!(job["image"], "debian:bookworm-slim", "{name}");
            }
        }
        let lane = |suffix: &str, lane: &str, id: u64| {
            let work = format!(".upd-ci/work/{id}-{lane}");
            let prepare = &pipeline[format!("prepare-{suffix}").as_str()];
            assert_eq!(needed(prepare).len(), 1, "{suffix}");
            assert_eq!(
                prepare["script"].as_array().unwrap()[1..],
                [json!(format!(
                    ".upd-ci/bin/upd gitlab org prepare --project {id} --lane {lane} --dir {work}"
                ))]
            );
            assert_eq!(prepare["artifacts"]["access"], "none");
            assert_eq!(prepare["artifacts"]["paths"], json!([format!("{work}/")]));
            let lock = &pipeline[format!("lock-{suffix}").as_str()];
            assert_eq!(
                needed(lock)[1..],
                [json!({"job": format!("prepare-{suffix}")})],
                "{suffix}"
            );
            assert_eq!(
                lock["script"].as_array().unwrap()[1..],
                [json!(format!(
                    ".upd-ci/bin/upd gitlab org lock-worker --dir {work}"
                ))]
            );
            assert_eq!(lock["artifacts"]["access"], "none");
            assert_eq!(
                lock["artifacts"]["paths"],
                json!([format!("{work}/result/")])
            );
            let publish = &pipeline[format!("publish-{suffix}").as_str()];
            // Publish orders itself after the lock job without taking its
            // artifacts, which would unpack the lock job's archive and
            // inject its dotenv report; it downloads the result itself.
            assert_eq!(
                needed(publish)[1..],
                [
                    json!({"job": format!("prepare-{suffix}")}),
                    json!({"job": format!("lock-{suffix}"), "artifacts": false})
                ]
            );
            assert_eq!(
                publish["script"].as_array().unwrap()[1..],
                [json!(format!(
                    ".upd-ci/bin/upd gitlab org publish --project {id} --lane {lane} --dir {work} --lock-job lock-{suffix}"
                ))]
            );
            assert!(publish.get("artifacts").is_none());
        };
        lane("11", "ordinary", 11);
        lane("12", "ordinary", 12);
        lane("12-major", "major", 12);

        let organization = &pipeline["organization-run"];
        assert_eq!(needed(organization).len(), 1);
        assert_eq!(
            organization["script"][1],
            "export UPD_LOCK_HANDED_OFF='11,12'"
        );
        assert_eq!(
            organization["script"][3],
            ".upd-ci/bin/upd gitlab org run --output json > .upd-ci/upd-org-report.json"
        );
    }

    #[test]
    fn untagged_token_jobs_run_anywhere_and_a_dry_run_is_one_preview_job() {
        let settings = settings(&[]).unwrap();
        let mut dry = plan(Vec::new());
        dry.dry_run = true;
        let pipeline = child_pipeline(&settings, &dry, DIGEST);
        assert_eq!(names(&pipeline), ["organization-run"]);
        let organization = &pipeline["organization-run"];
        assert!(organization.get("tags").is_none());
        assert_eq!(organization["script"][1], "export UPD_LOCK_HANDED_OFF=''");
        assert!(
            organization["script"][3]
                .as_str()
                .unwrap()
                .contains(" --dry-run > ")
        );
    }

    /// A job that holds the token runs only the binary the plan wrote. A lock
    /// job runs project code, which can upload any archive as its artifacts
    /// with its job token; publish extracts that archive over the plan's, so
    /// a replaced binary would otherwise run with the token.
    #[cfg(unix)]
    #[test]
    fn a_token_job_runs_only_the_binary_the_plan_wrote() {
        let genuine = "#!/bin/sh\ntouch ran\n";
        let replaced = "#!/bin/sh\ntouch ran\n# replaced by a lock job\n";
        let settings = settings(&[]).unwrap();
        let digest = super::super::split::sha256(genuine.as_bytes());
        let pipeline = child_pipeline(&settings, &plan(two_projects()), &digest);
        let token_jobs: Vec<&str> = names(&pipeline)
            .into_iter()
            .filter(|name| pipeline[name].get("environment").is_some())
            .collect();
        assert_eq!(token_jobs.len(), 7, "{token_jobs:?}");
        for name in token_jobs {
            let job = &pipeline[name];
            let script: Vec<&str> = ["before_script", "script"]
                .iter()
                .flat_map(|part| job[part].as_array().unwrap())
                .map(|line| line.as_str().unwrap())
                .collect();
            for (binary, runs) in [(genuine, true), (replaced, false)] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join(PLAN_DIR).join(BINARY);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, binary).unwrap();
                let output = std::process::Command::new("sh")
                    .args(["-e", "-c", &script.join("\n")])
                    .current_dir(dir.path())
                    .env("UPD_GITLAB_TOKEN", "secret-token")
                    .output()
                    .unwrap();
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert_eq!(output.status.success(), runs, "{name}: {stderr}");
                assert_eq!(dir.path().join("ran").exists(), runs, "{name}: {stderr}");
                if !runs {
                    assert!(
                        stderr.contains("is not the upd binary the plan wrote"),
                        "{name}: {stderr}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_pipeline_gitlab_would_not_include_is_refused() {
        let settings = settings(&[]).unwrap();
        let plan = plan(two_projects());
        let text = |limit| pipeline_text(&settings, &plan, DIGEST, limit);
        let full = text(MAX_INCLUDED_ARCHIVE).unwrap();
        let size = (1..full.len() as u64)
            .find(|limit| text(*limit).is_ok())
            .expect("the archive is smaller than the file");
        assert!(
            size < full.len() as u64 / 2,
            "the archive is deflated: {size}"
        );
        assert_eq!(text(size).unwrap(), full);
        let error = text(size - 1).unwrap_err();
        assert_eq!(error.kind(), "refused");
        assert!(error.message().contains("UPD_SHARD"), "{error}");
        assert!(
            error.message().starts_with(&format!(
                "The lock pipeline is a {size}-byte artifact archive, past the {} bytes",
                size - 1
            )),
            "an archive exactly at the limit is included: {error}"
        );
    }

    #[test]
    fn the_child_pipeline_is_valid_yaml() {
        let settings = settings(&[]).unwrap();
        let text =
            serde_json::to_string_pretty(&child_pipeline(&settings, &plan(two_projects()), DIGEST))
                .unwrap();
        for event in saphyr_parser::Parser::new_from_str(&text) {
            event.unwrap_or_else(|error| panic!("{error}\n{text}"));
        }
    }

    #[test]
    fn the_setup_script_is_valid_shell() {
        let output = std::process::Command::new("sh")
            .args(["-n", "-c", SETUP])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn plan_settings_are_checked() {
        let accepted = settings(&[]).unwrap();
        assert_eq!(accepted.lock_image, "debian:bookworm-slim");
        assert!(accepted.runner_tags.is_empty());
        assert_eq!(accepted.lock_runner_tags, ["upd-lock", "sandboxed"]);
        assert_eq!(accepted.environment, "upd-organization");
        assert_eq!(accepted.pipeline_id, 981);
        for overrides in [
            &[("UPD_LOCK", "false")][..],
            &[("UPD_LOCK_RUNNER_TAGS", "")],
            &[("UPD_LOCK_RUNNER_TAGS", "upd-lock,,other")],
            &[("UPD_LOCK_RUNNER_TAGS", "$RUNNER")],
            &[("UPD_RUNNER_TAGS", "a,")],
            &[("CI_PIPELINE_ID", "")],
            &[("CI_PIPELINE_ID", "latest")],
            &[("CI_JOB_NAME", "")],
            &[("UPD_ORGANIZATION_JOB", "")],
            &[("UPD_ORGANIZATION_JOB", "dependency-updates")],
            &[("UPD_IMAGE", "")],
            &[("UPD_IMAGE", "debian bookworm")],
            &[("UPD_LOCK_IMAGE", "$IMAGE")],
            &[("UPD_ENVIRONMENT", "prod$X")],
            &[("UPD_ENVIRONMENT", "/upd")],
        ] {
            let error = settings(overrides).unwrap_err();
            assert_eq!(error.exit_code(), 4, "{overrides:?}: {error}");
        }
    }
}
