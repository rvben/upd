//! Nix flake lockfiles (`flake.lock`).
//!
//! A flake input names a branch or tag (`github:NixOS/nixpkgs/nixos-unstable`)
//! in `flake.nix`, and `flake.lock` pins it to one commit. Updating the input
//! therefore never touches a version number: the manifest keeps its reference
//! and the lock moves to the commit that reference points at today. Each
//! change is a [`BumpKind::Revision`](super::BumpKind::Revision).
//!
//! Upstream heads are read natively from the GitHub and GitLab APIs, one
//! request per input, so a dry run needs no Nix installation. Writing does:
//! the lock records a NAR hash of the fetched source, which only Nix computes,
//! so the change is applied by `nix flake update <input>...` and then checked
//! against the commits the dry run resolved.

use super::{
    Lang, ParsedDependency, SkipStatus, SkippedUpdate, UpdateContext, UpdateOptions, UpdateResult,
    Updater,
};
use crate::registry::Registry;
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use futures::future::join_all;
use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use url::Url;

/// Characters of a commit hash shown in reports. Enough to be unambiguous in
/// any repository `nix` fetches from, short enough to read in a table.
const SHORT_REV: usize = 12;

/// Where a direct input of the flake comes from, as far as updating it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    GitHub {
        owner: String,
        repo: String,
        reference: Option<String>,
    },
    GitLab {
        host: String,
        owner: String,
        repo: String,
        reference: Option<String>,
    },
    /// `flake.nix` names an exact commit, so there is nothing to move to.
    PinnedRev,
    /// A source whose upstream head `upd` cannot resolve on its own.
    Unsupported(String),
}

/// A direct input of the root flake, read from `flake.lock`.
#[derive(Debug, Clone)]
struct FlakeInput {
    name: String,
    source: Source,
    locked_rev: Option<String>,
    /// Commit time of the locked revision, as recorded by Nix.
    last_modified: Option<DateTime<Utc>>,
}

pub struct FlakeLockUpdater {
    client: Client,
    github_api: String,
    /// Replaces `https://<host>/api/v4` for every GitLab input when set.
    gitlab_api: Option<String>,
    /// A GitLab token and the one host it may be sent to.
    gitlab_token: Option<(String, String)>,
    nix_program: OsString,
}

impl Default for FlakeLockUpdater {
    fn default() -> Self {
        Self::new()
    }
}

impl FlakeLockUpdater {
    pub fn new() -> Self {
        Self::with_endpoints("https://api.github.com".to_string(), None)
    }

    /// Build an updater that queries the given API roots instead of the public
    /// services, for tests and GitHub Enterprise style mirrors.
    pub fn with_endpoints(github_api: String, gitlab_api: Option<String>) -> Self {
        let client = crate::http::apply(
            Client::builder()
                .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(30))
                .connect_timeout(std::time::Duration::from_secs(10)),
        )
        .build()
        .expect("Failed to create HTTP client for flake inputs.");
        Self {
            client,
            github_api: github_api.trim_end_matches('/').to_string(),
            gitlab_api: gitlab_api.map(|api| api.trim_end_matches('/').to_string()),
            gitlab_token: gitlab_credentials(
                std::env::var("GITLAB_TOKEN").ok(),
                std::env::var("GITLAB_HOST").ok(),
            ),
            nix_program: OsString::from("nix"),
        }
    }

    /// Run this program instead of `nix` from `PATH`.
    pub fn with_nix_program(mut self, program: impl Into<OsString>) -> Self {
        self.nix_program = program.into();
        self
    }

    /// Send `token` to GitLab lookups for inputs on `host`, and to no other.
    pub fn with_gitlab_token(mut self, host: &str, token: &str) -> Self {
        self.gitlab_token = gitlab_credentials(Some(token.to_string()), Some(host.to_string()));
        self
    }

    fn github_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        // The `sha` media type answers with the bare commit hash, which is all
        // an input update needs, instead of the full commit with its diff.
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github.sha"),
        );
        if let Some(token) = crate::registry::GitHubReleasesRegistry::detect_token()
            && let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}"))
        {
            headers.insert(AUTHORIZATION, value);
        }
        headers
    }

    /// The commit the input's reference points at upstream right now.
    async fn resolve_head(&self, source: &Source) -> Result<String> {
        match source {
            Source::GitHub {
                owner,
                repo,
                reference,
            } => {
                let mut url = Url::parse(&self.github_api)?;
                {
                    let mut segments = url
                        .path_segments_mut()
                        .map_err(|()| anyhow!("invalid GitHub API URL"))?;
                    segments.extend(["repos", owner, repo, "commits"]);
                    // GitHub reads a branch name containing `/` from the raw path.
                    segments.extend(reference.as_deref().unwrap_or("HEAD").split('/'));
                }
                let response = self
                    .client
                    .get(url.clone())
                    .headers(Self::github_headers())
                    .send()
                    .await
                    .map_err(|e| crate::http::wrap_send_err(e, url.as_str()))?;
                let status = response.status();
                let body = response.text().await?;
                if !status.is_success() {
                    bail!("GitHub returned {status} for {owner}/{repo}");
                }
                commit_hash(body.trim())
            }
            Source::GitLab {
                host,
                owner,
                repo,
                reference,
            } => {
                let api = self
                    .gitlab_api
                    .clone()
                    .unwrap_or_else(|| format!("https://{host}/api/v4"));
                let mut url = Url::parse(&api)?;
                // GitLab identifies a project by its full path, subgroups
                // included, as one encoded segment. Nix writes the subgroup
                // separator of a `gitlab:` owner already encoded.
                let project = format!("{}/{repo}", owner.replace("%2F", "/").replace("%2f", "/"));
                url.path_segments_mut()
                    .map_err(|()| anyhow!("invalid GitLab API URL"))?
                    .extend([
                        "projects",
                        project.as_str(),
                        "repository",
                        "commits",
                        reference.as_deref().unwrap_or("HEAD"),
                    ]);
                let token = self
                    .gitlab_token
                    .as_ref()
                    .filter(|(token_host, _)| token_host.eq_ignore_ascii_case(host))
                    .map(|(_, token)| token);
                let mut request = self.client.get(url.clone());
                if let Some(token) = token {
                    request = request.header("PRIVATE-TOKEN", token);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| crate::http::wrap_send_err(e, url.as_str()))?;
                let status = response.status();
                if !status.is_success() {
                    // GitLab answers 404 rather than 401 for a private project.
                    let hint = if token.is_none() && matches!(status.as_u16(), 401 | 403 | 404) {
                        "; for a private project set GITLAB_TOKEN, and GITLAB_HOST when it is not on gitlab.com"
                    } else {
                        ""
                    };
                    bail!("GitLab returned {status} for {project}{hint}");
                }
                let body: Value = response.json().await?;
                let id = body
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("GitLab returned no commit id for {project}"))?;
                commit_hash(id)
            }
            Source::PinnedRev | Source::Unsupported(_) => {
                unreachable!("only GitHub and GitLab inputs are resolved")
            }
        }
    }

    /// Move the inputs named in `expected` with Nix, and confirm the lock at
    /// `path` then holds each at its expected commit (given in full or as a
    /// prefix) and every other input exactly where it was.
    ///
    /// On any failure the lock is put back byte for byte, so a refused update
    /// never leaves a half-written file behind.
    pub async fn refresh_inputs(
        &self,
        path: &Path,
        expected: &BTreeMap<String, String>,
    ) -> Result<String> {
        let original =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        match self.run_nix_update(path, &original, expected).await {
            Ok(content) => Ok(content),
            Err(e) => {
                std::fs::write(path, &original)
                    .with_context(|| format!("restoring {}", path.display()))?;
                Err(e)
            }
        }
    }

    async fn run_nix_update(
        &self,
        path: &Path,
        original: &str,
        expected: &BTreeMap<String, String>,
    ) -> Result<String> {
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        let output = tokio::process::Command::new(&self.nix_program)
            .args(["--extra-experimental-features", "nix-command flakes"])
            .args(["flake", "update"])
            .args(expected.keys())
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => anyhow!(
                    "updating flake.lock requires Nix (`{}` was not found); install Nix or leave `nix` out of --lang",
                    self.nix_program.to_string_lossy()
                ),
                _ => anyhow!("running `nix flake update` failed: {e}"),
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "`nix flake update` exited with {}: {}",
                output.status,
                stderr.trim()
            );
        }

        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading {} after nix flake update", path.display()))?;
        let after = parse_inputs(&content)?;
        for (name, wanted) in expected {
            let now = after
                .iter()
                .find(|input| &input.name == name)
                .ok_or_else(|| anyhow!("input `{name}` disappeared from the lock"))?;
            let rev = now.locked_rev.as_deref().unwrap_or("no revision");
            if wanted.is_empty() || !rev.starts_with(wanted.as_str()) {
                bail!("input `{name}` locked {rev} after nix flake update, expected {wanted}");
            }
        }

        // Inputs of an updated input may move with it. Everything else, direct
        // or transitive, has to be locked exactly as it was, and nothing may
        // appear that was not locked before.
        let locks_before = input_locks(original)?;
        let locks_after = input_locks(&content)?;
        let untouched = |input_path: &&Vec<String>| !expected.contains_key(&input_path[0]);
        for input_path in locks_before
            .keys()
            .chain(locks_after.keys())
            .filter(untouched)
        {
            match (locks_before.get(input_path), locks_after.get(input_path)) {
                (Some(before), Some(after)) if before == after => {}
                (None, Some(_)) => bail!(
                    "nix flake update also locked the new input `{}`; lock it with `nix flake lock` first",
                    input_path.join("/")
                ),
                _ => bail!(
                    "input `{}` changed after nix flake update, though `{}` was not updated",
                    input_path.join("/"),
                    input_path[0]
                ),
            }
        }
        Ok(content)
    }
}

/// Every input, direct and transitive, keyed by its input path from the root
/// (`["home-manager", "nixpkgs"]`): what it was asked for (`original`), where
/// it is locked, and whether it is a flake. Its `inputs` are left out because
/// they name nodes, and Nix renumbers node names (`nixpkgs_2`) as the graph
/// changes; the paths below it cover them instead.
fn input_locks(content: &str) -> Result<BTreeMap<Vec<String>, Value>> {
    let lock: Value = serde_json::from_str(content).context("flake.lock is not valid JSON")?;
    let nodes = lock
        .get("nodes")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("flake.lock has no `nodes`"))?;
    let root = lock.get("root").and_then(Value::as_str).unwrap_or("root");

    let mut found = BTreeMap::new();
    let mut pending: Vec<(Vec<String>, &str)> = vec![(Vec::new(), root)];
    while let Some((input_path, node_name)) = pending.pop() {
        let Some(node) = nodes.get(node_name) else {
            continue;
        };
        if !input_path.is_empty() {
            let mut entry = node.clone();
            if let Some(entry) = entry.as_object_mut() {
                entry.remove("inputs");
            }
            found.insert(input_path.clone(), entry);
        }
        // A lock is a DAG, so this only guards against a malformed file.
        if input_path.len() > nodes.len() {
            bail!(
                "flake.lock inputs form a cycle at `{}`",
                input_path.join("/")
            );
        }
        // An input that `follows` another is locked wherever that one is.
        for (name, target) in node
            .get("inputs")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if let Some(target) = target.as_str() {
                let mut child = input_path.clone();
                child.push(name.clone());
                pending.push((child, target));
            }
        }
    }
    Ok(found)
}

/// The GitLab token from `GITLAB_TOKEN` and the host it belongs to, from
/// `GITLAB_HOST` (as glab reads them) or gitlab.com. The token is only ever
/// sent to that host, so an input on another instance cannot receive it.
fn gitlab_credentials(token: Option<String>, host: Option<String>) -> Option<(String, String)> {
    let token = token.filter(|token| !token.trim().is_empty())?;
    let host = host
        .map(|host| {
            let host = host.trim();
            let host = host
                .strip_prefix("https://")
                .or_else(|| host.strip_prefix("http://"))
                .unwrap_or(host);
            host.trim_end_matches('/').to_string()
        })
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "gitlab.com".to_string());
    Some((host, token.trim().to_string()))
}

/// Validate a commit hash returned by a forge before it is compared to the
/// lock or shown to anyone.
fn commit_hash(value: &str) -> Result<String> {
    if value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        bail!("expected a commit hash, got {value:?}")
    }
}

fn short(rev: &str) -> String {
    rev.chars().take(SHORT_REV).collect()
}

fn text(object: &Value, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_string)
}

/// The direct inputs of the root flake, in lock order.
///
/// An input that `follows` another input has no lock node of its own and
/// moves with the input it follows, so it is left out.
fn parse_inputs(content: &str) -> Result<Vec<FlakeInput>> {
    let lock: Value = serde_json::from_str(content).context("flake.lock is not valid JSON")?;
    let nodes = lock
        .get("nodes")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("flake.lock has no `nodes`"))?;
    let root_name = lock.get("root").and_then(Value::as_str).unwrap_or("root");
    let Some(root_inputs) = nodes
        .get(root_name)
        .and_then(|root| root.get("inputs"))
        .and_then(Value::as_object)
    else {
        return Ok(Vec::new());
    };

    let mut inputs = Vec::new();
    for (name, target) in root_inputs {
        let Some(node_name) = target.as_str() else {
            continue;
        };
        let node = nodes.get(node_name).ok_or_else(|| {
            anyhow!("flake.lock input `{name}` points at missing node `{node_name}`")
        })?;
        let original = node.get("original").cloned().unwrap_or(Value::Null);
        let locked = node.get("locked").cloned().unwrap_or(Value::Null);
        let kind = text(&original, "type").unwrap_or_default();

        let source = if original.get("rev").is_some() {
            Source::PinnedRev
        } else if text(&locked, "type").as_deref() != Some(kind.as_str()) {
            Source::Unsupported(format!(
                "`{kind}` input resolved through `{}`",
                text(&locked, "type").unwrap_or_default()
            ))
        } else {
            match (
                kind.as_str(),
                text(&original, "owner"),
                text(&original, "repo"),
            ) {
                ("github", Some(owner), Some(repo)) if text(&original, "host").is_none() => {
                    Source::GitHub {
                        owner,
                        repo,
                        reference: text(&original, "ref"),
                    }
                }
                ("gitlab", Some(owner), Some(repo)) => Source::GitLab {
                    host: text(&original, "host").unwrap_or_else(|| "gitlab.com".to_string()),
                    owner,
                    repo,
                    reference: text(&original, "ref"),
                },
                ("github", ..) => Source::Unsupported("GitHub Enterprise input".to_string()),
                _ => Source::Unsupported(format!("`{kind}` input")),
            }
        };

        inputs.push(FlakeInput {
            name: name.clone(),
            source,
            locked_rev: text(&locked, "rev"),
            last_modified: locked
                .get("lastModified")
                .and_then(Value::as_i64)
                .and_then(|secs| DateTime::from_timestamp(secs, 0)),
        });
    }
    Ok(inputs)
}

#[async_trait::async_trait]
impl Updater for FlakeLockUpdater {
    async fn update(
        &self,
        path: &Path,
        _registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = super::read_file_safe(path)?;
        let inputs = parse_inputs(&content)?;
        let mut result = UpdateResult::default();

        let mut candidates: Vec<(&FlakeInput, &str)> = Vec::new();
        for input in &inputs {
            if options.is_package_filtered_out(&input.name) {
                result.unchanged += 1;
                continue;
            }
            let current = input.locked_rev.as_deref().map(short).unwrap_or_default();
            if options.should_ignore(&input.name) {
                result.ignored.push((input.name.clone(), current, None));
                continue;
            }
            if options.get_pinned_version(&input.name).is_some() {
                result.errors.push(format!(
                    "{}: a flake input cannot be pinned through upd; name the commit in flake.nix (`?rev=`) or ignore the input",
                    input.name
                ));
                continue;
            }
            match &input.source {
                Source::PinnedRev => result.unchanged += 1,
                Source::Unsupported(what) => result.skipped.push(SkippedUpdate {
                    package: input.name.clone(),
                    current,
                    status: SkipStatus::NotExamined,
                    reason: "unsupported-flake-input",
                    message: format!("{what}; upd resolves only github: and gitlab: flake inputs"),
                    line_number: None,
                }),
                Source::GitHub { .. } | Source::GitLab { .. } => match &input.locked_rev {
                    Some(rev) => candidates.push((input, rev)),
                    None => result.errors.push(format!(
                        "{}: flake.lock records no locked revision",
                        input.name
                    )),
                },
            }
        }

        let heads = join_all(
            candidates
                .iter()
                .map(|(input, _)| self.resolve_head(&input.source)),
        )
        .await;

        let cooldown = options
            .cooldown_policy
            .as_ref()
            .map_or_else(Duration::zero, |policy| {
                policy.effective_for("nix", Some(Lang::Nix.cli_name()))
            });
        let now = options.cooldown_now.unwrap_or_else(Utc::now);
        let mut expected = BTreeMap::new();
        for ((input, locked), head) in candidates.into_iter().zip(heads) {
            let head = match head {
                Ok(head) => head,
                Err(e) => {
                    result.errors.push(format!("{}: {e}", input.name));
                    continue;
                }
            };
            if head.eq_ignore_ascii_case(locked) {
                result.unchanged += 1;
                continue;
            }
            // A revision has no release date to age. Cooldown instead limits
            // how often an input moves: it moves once its locked commit is
            // older than the window, and then to the newest commit.
            if cooldown > Duration::zero() {
                let Some(locked_at) = input.last_modified else {
                    result.skipped.push(SkippedUpdate {
                        package: input.name.clone(),
                        current: short(locked),
                        status: SkipStatus::Blocked,
                        reason: "cooldown-age-unknown",
                        message: "flake.lock records no lastModified for this input, so its age under the cooldown is unknown".to_string(),
                        line_number: None,
                    });
                    continue;
                };
                if now - locked_at < cooldown {
                    result.skipped_by_cooldown.push((
                        input.name.clone(),
                        short(locked),
                        short(&head),
                        None,
                    ));
                    continue;
                }
            }
            let index = result.updated.len();
            result
                .updated
                .push((input.name.clone(), short(locked), short(&head), None));
            result.update_context.insert(
                index,
                UpdateContext {
                    lang: Lang::Nix,
                    section: None,
                    previous_spec: None,
                    new_spec: None,
                },
            );
            expected.insert(input.name.clone(), head);
        }

        if !options.dry_run
            && !expected.is_empty()
            && let Err(e) = self.refresh_inputs(path, &expected).await
        {
            result.updated.clear();
            result.update_context.clear();
            result.errors.push(e.to_string());
        }

        Ok(result)
    }

    fn handles(&self, file_type: super::FileType) -> bool {
        file_type == super::FileType::FlakeLock
    }

    /// Commits have no order to align across files, so a lock contributes no
    /// dependencies to alignment.
    fn parse_dependencies(&self, _path: &Path) -> Result<Vec<ParsedDependency>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
#[path = "flake_lock_tests.rs"]
mod tests;
