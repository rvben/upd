use super::{
    FileType, ParsedDependency, SkipStatus, SkippedUpdate, UpdateOptions, UpdateResult, Updater,
    downgrade_warning, read_file_safe, write_file_atomic,
};
use crate::align::compare_versions;
use crate::registry::Registry;
use crate::updater::{AnnotationSource, Lang, RegistrySet};
use crate::version::match_version_precision;
use anyhow::Result;
use futures::future::join_all;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;

/// Updater for `.mise.toml` and `.tool-versions` files.
///
/// Symbolic versions (`latest`, `lts`, `system`, `global`, and `ref:` prefixes)
/// are intentionally preserved - upd cannot resolve them to a concrete release.
/// Only pinned numeric versions (e.g. `1.22.1`) are checked and updated.
///
/// For `.tool-versions` files with multiple versions on one line
/// (e.g. `python 3.11.0 3.10.0`), only the **first** version is updated.
pub struct MiseUpdater {
    /// Matches `tool version` lines in .tool-versions (captures first version only)
    tool_versions_re: Regex,
    /// Matches TOML section headers like [tools], [settings], etc.
    section_re: Regex,
    /// One registry per backend a mise entry can name.
    registries: RegistrySet,
}

/// Map a mise/asdf tool name to its GitHub `owner/repo` for release lookups
fn tool_to_github_repo(tool: &str) -> Option<&'static str> {
    match tool {
        "node" | "nodejs" => Some("nodejs/node"),
        "deno" => Some("denoland/deno"),
        "bun" => Some("oven-sh/bun"),
        "zig" => Some("ziglang/zig"),
        "go" | "golang" => Some("golang/go"),
        "python" => Some("python/cpython"),
        "ruby" => Some("ruby/ruby"),
        "rust" => Some("rust-lang/rust"),
        "terraform" => Some("hashicorp/terraform"),
        "kubectl" => Some("kubernetes/kubernetes"),
        "helm" => Some("helm/helm"),
        "just" => Some("casey/just"),
        "ripgrep" | "rg" => Some("BurntSushi/ripgrep"),
        "fd" => Some("sharkdp/fd"),
        "bat" => Some("sharkdp/bat"),
        "jq" => Some("jqlang/jq"),
        "yq" => Some("mikefarah/yq"),
        "shellcheck" => Some("koalaman/shellcheck"),
        "shfmt" => Some("mvdan/sh"),
        "hugo" => Some("gohugoio/hugo"),
        "act" => Some("nektos/act"),
        "uv" => Some("astral-sh/uv"),
        "ruff" => Some("astral-sh/ruff"),
        _ => None,
    }
}

/// Which registry answers for a `[tools]` entry, or why none can.
///
/// A mise entry names its backend explicitly (`cargo:cargo-zigbuild`) or leaves
/// it to mise's registry (`node`). The two cases produce the same three
/// outcomes, so discovery and reporting ask one function rather than two.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolution {
    /// The registry to query, and the name to query it for.
    Registry(AnnotationSource, String),
    /// A backend mise supports and upd has no registry for.
    UnsupportedBackend(String),
    /// A bare tool name outside the core table, whose backend only mise's own
    /// registry knows.
    UnknownTool,
    /// A version mise resolves at install time (`latest`, `lts`, `ref:main`),
    /// which names no release upd could compare against.
    SymbolicVersion,
}

impl Resolution {
    /// Why upd cannot check this entry, as a stable token and a message, or
    /// `None` when a registry answers for it.
    ///
    /// Every reason here leaves a real pin unchecked, so each one is reported
    /// rather than counted as up to date.
    fn unexamined(&self) -> Option<(&'static str, String)> {
        match self {
            Resolution::Registry(..) => None,
            Resolution::UnsupportedBackend(backend) => Some((
                "unsupported-backend",
                format!("upd has no registry for mise's `{backend}` backend"),
            )),
            Resolution::UnknownTool => Some((
                "unknown-tool",
                "no registry known for this tool; name its backend (for example \
                 `aqua:owner/repo`) to have it checked"
                    .to_string(),
            )),
            Resolution::SymbolicVersion => Some((
                "symbolic-version",
                "mise resolves this version at install time, so there is no pin to compare"
                    .to_string(),
            )),
        }
    }
}

/// Resolve a `[tools]` key to the registry that answers for it.
///
/// An explicit backend prefix wins. `aqua` package names carry an optional
/// third segment (`aqua:kubernetes/kubernetes/kubectl`), and the GitHub repo is
/// always the first two.
fn resolve_entry(key: &str) -> Resolution {
    let Some((backend, name)) = key.split_once(':') else {
        return match tool_to_github_repo(key) {
            Some(repo) => Resolution::Registry(AnnotationSource::GitHubReleases, repo.to_string()),
            None => Resolution::UnknownTool,
        };
    };

    if backend == "aqua" {
        let mut segments = name.split('/');
        return match (segments.next(), segments.next()) {
            (Some(owner), Some(repo)) if !owner.is_empty() && !repo.is_empty() => {
                Resolution::Registry(AnnotationSource::GitHubReleases, format!("{owner}/{repo}"))
            }
            _ => Resolution::UnknownTool,
        };
    }

    let source = match backend {
        "cargo" => AnnotationSource::Crates,
        "npm" => AnnotationSource::Npm,
        "pipx" => AnnotationSource::PyPi,
        "gem" => AnnotationSource::RubyGems,
        "dotnet" => AnnotationSource::NuGet,
        "go" => AnnotationSource::Go,
        "github" | "ubi" => AnnotationSource::GitHubReleases,
        _ => return Resolution::UnsupportedBackend(backend.to_string()),
    };

    Resolution::Registry(source, name.to_string())
}

/// A registry and the name to ask it for.
///
/// Two entries with the same lookup are one request: `kubectl` and
/// `aqua:kubernetes/kubernetes/kubectl` both ask GitHub about
/// `kubernetes/kubernetes`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Lookup {
    source: AnnotationSource,
    name: String,
}

/// One `[tools]` entry, with the registry that answers for it already decided.
///
/// Resolution happens during the scan so that an entry upd cannot check is
/// carried forward and reported, rather than filtered out and forgotten.
#[derive(Debug, Clone)]
struct MiseEntry {
    /// The key exactly as the file spells it, backend prefix included.
    key: String,
    version: String,
    line_number: usize,
    resolution: Resolution,
}

impl MiseEntry {
    fn new(key: String, version: &str, line_number: usize) -> Self {
        let resolution = if is_symbolic_version(version) {
            Resolution::SymbolicVersion
        } else {
            resolve_entry(&key)
        };
        Self {
            key,
            version: version.to_string(),
            line_number,
            resolution,
        }
    }
}

/// The entries a registry can answer for, as the `Updater` trait's parse hook
/// reports them. Entries upd cannot check are reported by `update` instead,
/// where there is a channel that can carry the reason.
fn resolvable(entries: Vec<MiseEntry>) -> Vec<ParsedDependency> {
    entries
        .into_iter()
        .filter(|entry| matches!(entry.resolution, Resolution::Registry(..)))
        .map(|entry| ParsedDependency {
            name: entry.key,
            version: entry.version,
            line_number: Some(entry.line_number),
            has_upper_bound: false,
            is_bumpable: true,
        })
        .collect()
}

/// Where in a `.mise.toml` the scanner currently is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Section {
    /// Inside `[tools]`, where each line names its own tool.
    Tools,
    /// Inside `[tools.<name>]`, where the section header names the tool.
    Tool(String),
    /// Anywhere else, where no line declares a tool version.
    Other,
}

impl Section {
    fn detect(header: &str) -> Self {
        if header == "tools" {
            return Self::Tools;
        }
        match header.strip_prefix("tools.") {
            Some(name) => Self::Tool(unquote(name.trim()).to_string()),
            None => Self::Other,
        }
    }
}

/// Split `key = value` into its halves, with the key unquoted.
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    let key = unquote(key.trim());
    if key.is_empty() {
        return None;
    }
    Some((key, value.trim()))
}

fn unquote(text: &str) -> &str {
    text.strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(text)
}

/// The version a `[tools]` value declares, across mise's three value shapes.
///
/// An inline table must be read by key: the first quoted string in
/// `{ postinstall = "...", version = "1.0" }` is not the version.
fn version_in_value(value: &str) -> Option<&str> {
    match value.strip_prefix('{') {
        Some(table) => table
            .split(',')
            .filter_map(|field| split_key_value(field.trim_end_matches('}')))
            .find(|(key, _)| *key == "version")
            .and_then(|(_, version)| first_quoted(version)),
        None => first_quoted(value),
    }
}

/// The contents of the first double-quoted string in `text`.
fn first_quoted(text: &str) -> Option<&str> {
    let (_, rest) = text.split_once('"')?;
    let (quoted, _) = rest.split_once('"')?;
    Some(quoted)
}

/// Return true if `version` is a symbolic specifier that cannot be resolved
/// to a concrete release (e.g. `latest`, `lts`, `system`, `global`, `ref:*`).
fn is_symbolic_version(version: &str) -> bool {
    matches!(version, "latest" | "lts" | "system" | "global")
        || version.starts_with("ref:")
        || version.starts_with("prefix:")
}

/// Strip tool-specific version prefixes from GitHub release tags.
/// GitHub tags often have prefixes like `v1.0.0` or `go1.22.1`,
/// but mise/asdf versions are typically bare (e.g., `1.0.0`, `1.22.1`).
fn strip_tool_version_prefix<'a>(tool: &str, version: &'a str) -> &'a str {
    match tool {
        "go" | "golang" => version.strip_prefix("go").unwrap_or(version),
        _ => version.strip_prefix('v').unwrap_or(version),
    }
}

impl MiseUpdater {
    pub fn new(registries: RegistrySet) -> Self {
        // Match: tool_name version (space-delimited)
        let tool_versions_re = Regex::new(r"^(\S+)\s+(\S+)").expect("Invalid tool_versions regex");
        // Match TOML section headers
        let section_re = Regex::new(r"^\[([^\]]+)\]").expect("Invalid section regex");
        Self {
            tool_versions_re,
            section_re,
            registries,
        }
    }

    /// An updater that can read a file but not resolve anything in it, for
    /// callers that only ever call `parse_dependencies`.
    pub fn new_parse_only() -> Self {
        Self::new(RegistrySet::parse_only())
    }

    /// Scan `.mise.toml` for every `[tools]` entry that declares a version.
    ///
    /// mise accepts three shapes for a tool's version, and all three pin exactly
    /// as hard as each other:
    ///
    /// ```toml
    /// [tools]
    /// rust = "1.96.0"
    /// uv = { version = "0.12.5" }
    /// node = ["20.11.0", "18.0.0"]
    ///
    /// [tools.ruff]
    /// version = "0.5.0"
    /// ```
    ///
    /// An entry declaring no version at all (`python = { virtualenv = ".venv" }`)
    /// makes no claim that can go stale and is not an entry here.
    fn scan_mise_toml(&self, content: &str) -> Vec<MiseEntry> {
        let mut entries = Vec::new();
        let mut section = Section::Other;

        for (line_idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Skip empty lines and comments
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            // Check for section headers
            if let Some(caps) = self.section_re.captures(trimmed) {
                section = Section::detect(caps.get(1).unwrap().as_str().trim());
                continue;
            }

            let (tool, version) = match &section {
                Section::Other => continue,
                // `[tools.ruff]` names the tool; the version is a key inside it.
                Section::Tool(name) => match split_key_value(trimmed) {
                    Some(("version", value)) => match first_quoted(value) {
                        Some(version) => (name.clone(), version),
                        None => continue,
                    },
                    _ => continue,
                },
                // `[tools]` names the tool on the line that carries the version.
                Section::Tools => match split_key_value(trimmed) {
                    Some((key, value)) => match version_in_value(value) {
                        Some(version) => (key.to_string(), version),
                        None => continue,
                    },
                    None => continue,
                },
            };

            entries.push(MiseEntry::new(tool, version, line_idx + 1));
        }

        entries
    }

    /// Scan `.tool-versions` for every `tool version` line.
    ///
    /// A line may carry several versions (`python 3.11.0 3.10.0`); the first is
    /// the one mise activates and the only one upd rewrites.
    fn scan_tool_versions(&self, content: &str) -> Vec<MiseEntry> {
        let mut entries = Vec::new();

        for (line_idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Skip empty lines and comments
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            if let Some(caps) = self.tool_versions_re.captures(trimmed) {
                entries.push(MiseEntry::new(
                    caps.get(1).unwrap().as_str().to_string(),
                    caps.get(2).unwrap().as_str(),
                    line_idx + 1,
                ));
            }
        }

        entries
    }

    /// Scan for tool entries based on file type
    fn scan(&self, content: &str, file_type: FileType) -> Vec<MiseEntry> {
        match file_type {
            FileType::MiseToml => self.scan_mise_toml(content),
            FileType::ToolVersions => self.scan_tool_versions(content),
            _ => Vec::new(),
        }
    }

    /// Parse dependencies based on file type
    fn parse_content(&self, content: &str, file_type: FileType) -> Vec<ParsedDependency> {
        resolvable(self.scan(content, file_type))
    }

    /// Compute the updated version string, preserving precision
    fn compute_updated_version(
        tool: &str,
        current: &str,
        latest_tag: &str,
        full_precision: bool,
    ) -> String {
        let stripped = strip_tool_version_prefix(tool, latest_tag);

        if full_precision {
            stripped.to_string()
        } else {
            match_version_precision(current, stripped)
        }
    }
}

#[async_trait::async_trait]
impl Updater for MiseUpdater {
    /// The registry each entry needs is decided by the entry itself, so the one
    /// the trait hands every updater is unused here.
    async fn update(
        &self,
        path: &Path,
        _registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let file_type = FileType::detect(path).unwrap_or(FileType::MiseToml);
        let mut result = UpdateResult::default();

        // Pass 1: Collect tools to check
        let entries = self.scan(&content, file_type);

        let mut pinned_tools: Vec<(usize, String, String, String)> = Vec::new();
        let mut tools_to_check: Vec<(usize, String, String, Lookup)> = Vec::new();

        for entry in entries {
            let line_idx = entry.line_number - 1;

            // An entry the caller excluded is not reported at all: they said not
            // to look at it, so upd not having looked is not news.
            if options.is_package_filtered_out(&entry.key) {
                result.unchanged += 1;
                continue;
            }
            if options.should_ignore(&entry.key) {
                result
                    .ignored
                    .push((entry.key, entry.version, Some(entry.line_number)));
                continue;
            }

            let Resolution::Registry(source, name) = entry.resolution else {
                // Naming what upd could not check keeps an unchecked pin out of
                // the up-to-date count.
                let (reason, message) = entry
                    .resolution
                    .unexamined()
                    .expect("a non-Registry resolution always states its reason");
                result.skipped.push(SkippedUpdate {
                    package: entry.key,
                    current: entry.version,
                    status: SkipStatus::NotExamined,
                    reason,
                    message,
                    line_number: Some(entry.line_number),
                });
                continue;
            };

            if let Some(pinned_version) = options.get_pinned_version(&entry.key) {
                pinned_tools.push((
                    line_idx,
                    entry.key,
                    entry.version,
                    pinned_version.to_string(),
                ));
            } else {
                tools_to_check.push((line_idx, entry.key, entry.version, Lookup { source, name }));
            }
        }

        // Pass 2: Fetch versions in parallel, one request per distinct lookup
        // however many entries name it.
        let mut lookups: Vec<Lookup> = Vec::new();
        for (_, _, _, lookup) in &tools_to_check {
            if !lookups.contains(lookup) {
                lookups.push(lookup.clone());
            }
        }

        let version_futures: Vec<_> = lookups
            .iter()
            .map(|lookup| async move {
                self.registries
                    .for_source(lookup.source)?
                    .get_latest_version(&lookup.name)
                    .await
            })
            .collect();

        let version_results = join_all(version_futures).await;

        let tool_versions: HashMap<Lookup, Result<String, String>> = lookups
            .into_iter()
            .zip(version_results)
            .map(|(lookup, result)| (lookup, result.map_err(|e| e.to_string())))
            .collect();

        // Build version map per line index
        let mut version_map: HashMap<usize, Result<String, anyhow::Error>> = HashMap::new();
        for (line_idx, _, _, lookup) in &tools_to_check {
            if let Some(result) = tool_versions.get(lookup) {
                match result {
                    Ok(version) => {
                        version_map.insert(*line_idx, Ok(version.clone()));
                    }
                    Err(e) => {
                        version_map.insert(*line_idx, Err(anyhow::anyhow!("{}", e)));
                    }
                }
            }
        }

        // Add pinned versions
        for (line_idx, _, _, pinned_version) in &pinned_tools {
            version_map.insert(*line_idx, Ok(pinned_version.clone()));
        }

        // Build tool info map: line_idx -> (tool_name, current_version, lookup)
        // A pinned tool carries no lookup: its version came from the caller.
        let mut tool_info: HashMap<usize, (String, String, Option<Lookup>)> = tools_to_check
            .into_iter()
            .map(|(idx, tool_name, version, lookup)| (idx, (tool_name, version, Some(lookup))))
            .collect();

        for (line_idx, tool_name, current_version, _) in pinned_tools {
            tool_info.insert(line_idx, (tool_name, current_version, None));
        }

        // Pass 3: Apply updates
        let mut new_lines: Vec<String> = Vec::new();

        for (line_idx, line) in content.lines().enumerate() {
            let line_num = line_idx + 1;

            if let Some(version_result) = version_map.remove(&line_idx) {
                let Some((tool_name, current_version, lookup)) = tool_info.get(&line_idx) else {
                    new_lines.push(line.to_string());
                    continue;
                };
                let is_pinned = lookup.is_none();

                match version_result {
                    Ok(latest_tag) => {
                        // Apply cooldown policy before writing (registry path only; pins bypass it).
                        // The publish dates come from the same registry the
                        // version did, under the name that registry knows.
                        let (latest_tag, held_back_record) = match lookup {
                            None => (latest_tag, None),
                            Some(lookup) => {
                                let (outcome, note) = crate::updater::apply_cooldown(
                                    self.registries.for_source(lookup.source)?,
                                    &lookup.name,
                                    current_version,
                                    &latest_tag,
                                    None,
                                    false,
                                    &options,
                                )
                                .await;
                                if let Some(msg) = note {
                                    options.note_cooldown_unavailable(&msg);
                                }
                                match outcome {
                                    crate::updater::CooldownOutcome::Unchanged(v) => (v, None),
                                    crate::updater::CooldownOutcome::HeldBack {
                                        chosen,
                                        skipped_version,
                                        skipped_published_at,
                                    } => (chosen, Some((skipped_version, skipped_published_at))),
                                    crate::updater::CooldownOutcome::Skipped {
                                        skipped_version,
                                        skipped_published_at,
                                    } => {
                                        result.skipped_by_cooldown.push((
                                            tool_name.clone(),
                                            current_version.clone(),
                                            skipped_version,
                                            skipped_published_at,
                                        ));
                                        new_lines.push(line.to_string());
                                        continue;
                                    }
                                }
                            }
                        };

                        let new_version = Self::compute_updated_version(
                            tool_name,
                            current_version,
                            &latest_tag,
                            options.full_precision,
                        );

                        if new_version != *current_version {
                            // Refuse to write a downgrade (registry path only; pins are intentional).
                            if !is_pinned
                                && compare_versions(&new_version, current_version, Lang::Mise)
                                    != std::cmp::Ordering::Greater
                            {
                                result.warnings.push(downgrade_warning(
                                    tool_name,
                                    &new_version,
                                    current_version,
                                ));
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                            } else if !is_pinned
                                && !options.allows_bump(current_version, &new_version)
                            {
                                // Bump level exceeds the --only-bump/--max-bump ceiling.
                                // Configured pins are intentional and bypass the ceiling.
                                result.record_capped(
                                    tool_name,
                                    current_version,
                                    &new_version,
                                    Some(line_num),
                                );
                                new_lines.push(line.to_string());
                            } else {
                                let new_line = line.replacen(current_version, &new_version, 1);
                                new_lines.push(new_line);

                                if is_pinned {
                                    result.pinned.push((
                                        tool_name.clone(),
                                        current_version.clone(),
                                        new_version,
                                        Some(line_num),
                                    ));
                                } else {
                                    result.updated.push((
                                        tool_name.clone(),
                                        current_version.clone(),
                                        new_version.clone(),
                                        Some(line_num),
                                    ));
                                    if let Some((skipped_version, skipped_published_at)) =
                                        held_back_record
                                    {
                                        result.held_back.push((
                                            tool_name.clone(),
                                            current_version.clone(),
                                            new_version,
                                            skipped_version,
                                            skipped_published_at,
                                        ));
                                    }
                                }
                            }
                        } else {
                            new_lines.push(line.to_string());
                            result.unchanged += 1;
                        }
                    }
                    Err(e) => {
                        new_lines.push(line.to_string());
                        result.errors.push(format!("{}: {}", tool_name, e));
                    }
                }
            } else {
                new_lines.push(line.to_string());
            }
        }

        if (!result.updated.is_empty() || !result.pinned.is_empty()) && !options.dry_run {
            let line_ending = if content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let new_content = new_lines.join(line_ending);

            let final_content = if content.ends_with('\n') && !new_content.ends_with('\n') {
                format!("{}{}", new_content, line_ending)
            } else {
                new_content
            };

            write_file_atomic(path, &final_content)?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::MiseToml || file_type == FileType::ToolVersions
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let file_type = FileType::detect(path).unwrap_or(FileType::MiseToml);
        Ok(self.parse_content(&content, file_type))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MockRegistry;
    use crate::updater::SkipStatus;
    use std::fs;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[test]
    fn test_tool_to_github_repo() {
        assert_eq!(tool_to_github_repo("node"), Some("nodejs/node"));
        assert_eq!(tool_to_github_repo("nodejs"), Some("nodejs/node"));
        assert_eq!(tool_to_github_repo("deno"), Some("denoland/deno"));
        assert_eq!(tool_to_github_repo("bun"), Some("oven-sh/bun"));
        assert_eq!(tool_to_github_repo("zig"), Some("ziglang/zig"));
        assert_eq!(tool_to_github_repo("go"), Some("golang/go"));
        assert_eq!(tool_to_github_repo("golang"), Some("golang/go"));
        assert_eq!(tool_to_github_repo("python"), Some("python/cpython"));
        assert_eq!(tool_to_github_repo("rust"), Some("rust-lang/rust"));
        assert_eq!(tool_to_github_repo("uv"), Some("astral-sh/uv"));
        assert_eq!(tool_to_github_repo("ruff"), Some("astral-sh/ruff"));
        assert_eq!(tool_to_github_repo("unknown-tool"), None);
        assert_eq!(tool_to_github_repo(""), None);
    }

    #[test]
    fn a_backend_prefix_resolves_to_the_registry_that_answers_for_it() {
        let cases = [
            (
                "cargo:cargo-zigbuild",
                AnnotationSource::Crates,
                "cargo-zigbuild",
            ),
            ("npm:prettier", AnnotationSource::Npm, "prettier"),
            ("pipx:black", AnnotationSource::PyPi, "black"),
            ("gem:rubocop", AnnotationSource::RubyGems, "rubocop"),
            (
                "dotnet:dotnet-outdated-tool",
                AnnotationSource::NuGet,
                "dotnet-outdated-tool",
            ),
            (
                "go:github.com/rhysd/actionlint",
                AnnotationSource::Go,
                "github.com/rhysd/actionlint",
            ),
            (
                "github:PyO3/maturin",
                AnnotationSource::GitHubReleases,
                "PyO3/maturin",
            ),
            ("ubi:cli/cli", AnnotationSource::GitHubReleases, "cli/cli"),
        ];

        for (key, source, name) in cases {
            assert_eq!(
                resolve_entry(key),
                Resolution::Registry(source, name.to_string()),
                "{key} should resolve to {source:?} as {name}"
            );
        }
    }

    #[test]
    fn an_aqua_package_names_its_repo_in_the_first_two_segments() {
        assert_eq!(
            resolve_entry("aqua:rhysd/actionlint"),
            Resolution::Registry(
                AnnotationSource::GitHubReleases,
                "rhysd/actionlint".to_string()
            )
        );
        // aqua package paths carry an optional third segment naming the binary.
        assert_eq!(
            resolve_entry("aqua:kubernetes/kubernetes/kubectl"),
            Resolution::Registry(
                AnnotationSource::GitHubReleases,
                "kubernetes/kubernetes".to_string()
            )
        );
    }

    #[test]
    fn an_aqua_package_without_a_repo_path_is_not_guessed_at() {
        assert_eq!(resolve_entry("aqua:standalone"), Resolution::UnknownTool);
    }

    #[test]
    fn a_backend_upd_cannot_query_is_named_rather_than_dropped() {
        for backend in [
            "asdf", "vfox", "conda", "forgejo", "gitlab", "pkgx", "spm", "http", "s3",
        ] {
            assert_eq!(
                resolve_entry(&format!("{backend}:whatever")),
                Resolution::UnsupportedBackend(backend.to_string()),
                "{backend} has no upd registry and must say so"
            );
        }
    }

    #[test]
    fn a_bare_name_outside_the_core_table_is_unresolved_not_absent() {
        assert_eq!(resolve_entry("actionlint"), Resolution::UnknownTool);
    }

    #[test]
    fn a_bare_core_tool_still_resolves_through_the_core_table() {
        assert_eq!(
            resolve_entry("zig"),
            Resolution::Registry(AnnotationSource::GitHubReleases, "ziglang/zig".to_string())
        );
    }

    #[test]
    fn an_inline_table_declares_a_version_like_a_bare_string_does() {
        let updater = MiseUpdater::new_parse_only();
        let deps = updater.parse_content(
            "[tools]\nuv = { version = \"0.12.5\" }\n",
            FileType::MiseToml,
        );

        assert_eq!(deps.len(), 1, "inline table entry must be seen");
        assert_eq!(deps[0].name, "uv");
        assert_eq!(deps[0].version, "0.12.5");
        assert_eq!(deps[0].line_number, Some(2));
    }

    #[test]
    fn a_version_array_declares_its_first_entry() {
        let updater = MiseUpdater::new_parse_only();
        let deps = updater.parse_content(
            "[tools]\nnode = [\"20.11.0\", \"18.0.0\"]\n",
            FileType::MiseToml,
        );

        assert_eq!(deps.len(), 1, "array entry must be seen");
        assert_eq!(deps[0].name, "node");
        assert_eq!(deps[0].version, "20.11.0");
    }

    #[test]
    fn a_tools_subtable_declares_the_version_of_the_tool_it_names() {
        let updater = MiseUpdater::new_parse_only();
        let deps = updater.parse_content("[tools.ruff]\nversion = \"0.5.0\"\n", FileType::MiseToml);

        assert_eq!(deps.len(), 1, "subtable entry must be seen");
        assert_eq!(deps[0].name, "ruff");
        assert_eq!(deps[0].version, "0.5.0");
        assert_eq!(
            deps[0].line_number,
            Some(2),
            "the version line is the one that gets rewritten"
        );
    }

    #[test]
    fn a_settings_key_named_version_is_not_a_tool() {
        let updater = MiseUpdater::new_parse_only();
        let deps = updater.parse_content("[settings]\nversion = \"0.5.0\"\n", FileType::MiseToml);

        assert!(
            deps.is_empty(),
            "only [tools.*] subtables declare tools, got {deps:?}"
        );
    }

    /// `MiseUpdater` ignores the registry the trait hands it, so every test
    /// needs some `&dyn Registry` to pass and none of them care which.
    fn unused_registry() -> MockRegistry {
        MockRegistry::new("unused")
    }

    fn updater_with(entries: Vec<(AnnotationSource, MockRegistry)>) -> MiseUpdater {
        MiseUpdater::new(RegistrySet::with_sources(
            entries
                .into_iter()
                .map(|(source, registry)| (source, Arc::new(registry) as Arc<dyn Registry>))
                .collect(),
        ))
    }

    #[tokio::test]
    async fn each_backend_is_checked_against_the_registry_that_owns_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".mise.toml");
        fs::write(
            &path,
            "[tools]\n\"cargo:cargo-zigbuild\" = \"0.23.0\"\n\"github:PyO3/maturin\" = \"1.14.1\"\n",
        )
        .unwrap();

        let updater = updater_with(vec![
            (
                AnnotationSource::Crates,
                MockRegistry::new("crates").with_version("cargo-zigbuild", "0.24.0"),
            ),
            (
                AnnotationSource::GitHubReleases,
                MockRegistry::new("github-releases").with_version("PyO3/maturin", "v1.15.0"),
            ),
        ]);

        let result = updater
            .update(&path, &unused_registry(), UpdateOptions::default())
            .await
            .unwrap();

        let updated: Vec<(&str, &str)> = result
            .updated
            .iter()
            .map(|(name, _, new, _)| (name.as_str(), new.as_str()))
            .collect();
        assert_eq!(
            updated,
            vec![
                ("cargo:cargo-zigbuild", "0.24.0"),
                ("github:PyO3/maturin", "1.15.0"),
            ]
        );
    }

    #[tokio::test]
    async fn an_entry_upd_cannot_check_is_reported_rather_than_counted_as_current() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".mise.toml");
        fs::write(
            &path,
            "[tools]\n\"asdf:private-tool\" = \"1.0.0\"\nactionlint = \"1.7.12\"\nnode = \"latest\"\nrust = \"1.91.1\"\n",
        )
        .unwrap();

        let updater = updater_with(vec![(
            AnnotationSource::GitHubReleases,
            MockRegistry::new("github-releases").with_version("rust-lang/rust", "1.91.1"),
        )]);

        let result = updater
            .update(&path, &unused_registry(), UpdateOptions::default())
            .await
            .unwrap();

        let reported: Vec<(&str, &str)> = result
            .skipped
            .iter()
            .map(|skip| (skip.package.as_str(), skip.reason))
            .collect();
        assert_eq!(
            reported,
            vec![
                ("asdf:private-tool", "unsupported-backend"),
                ("actionlint", "unknown-tool"),
                ("node", "symbolic-version"),
            ]
        );
        assert!(
            result
                .skipped
                .iter()
                .all(|skip| skip.status == SkipStatus::NotExamined),
            "an entry upd never looked at is not a blocked one"
        );
        assert_eq!(
            result.unchanged, 1,
            "only rust was examined and found current"
        );
    }

    #[test]
    fn test_strip_tool_version_prefix() {
        // Go uses "go" prefix
        assert_eq!(strip_tool_version_prefix("go", "go1.22.1"), "1.22.1");
        assert_eq!(strip_tool_version_prefix("golang", "go1.22.1"), "1.22.1");

        // Most tools use "v" prefix
        assert_eq!(strip_tool_version_prefix("node", "v20.11.0"), "20.11.0");
        assert_eq!(strip_tool_version_prefix("python", "v3.12.2"), "3.12.2");
        assert_eq!(strip_tool_version_prefix("rust", "v1.91.1"), "1.91.1");

        // No prefix passes through
        assert_eq!(strip_tool_version_prefix("node", "20.11.0"), "20.11.0");
        assert_eq!(strip_tool_version_prefix("go", "1.22.1"), "1.22.1");
    }

    #[test]
    fn test_parse_mise_toml() {
        let updater = MiseUpdater::new_parse_only();
        let content = r#"
[env]
RUST_BACKTRACE = "1"

[tools]
rust = "1.91.1"
python = "3.12"
uv = "latest"
"cargo:maturin" = "latest"
zig = "0.13"
node = "20.11.0"

[settings]
cargo_binstall = true
"#;
        let deps = updater.parse_content(content, FileType::MiseToml);
        assert_eq!(deps.len(), 4);
        assert_eq!(deps[0].name, "rust");
        assert_eq!(deps[0].version, "1.91.1");
        assert_eq!(deps[1].name, "python");
        assert_eq!(deps[1].version, "3.12");
        assert_eq!(deps[2].name, "zig");
        assert_eq!(deps[2].version, "0.13");
        assert_eq!(deps[3].name, "node");
        assert_eq!(deps[3].version, "20.11.0");
    }

    #[test]
    fn test_parse_mise_toml_skips_symbolic_versions() {
        // All symbolic specifiers must be preserved (skipped), not looked up.
        let updater = MiseUpdater::new_parse_only();
        let content = r#"
[tools]
node = "lts"
python = "latest"
rust = "1.91.1"
go = "system"
terraform = "global"
helm = "ref:master"
kubectl = "prefix:1.29"
"#;
        let deps = updater.parse_content(content, FileType::MiseToml);
        assert_eq!(deps.len(), 1, "only numeric-pinned rust should be returned");
        assert_eq!(deps[0].name, "rust");
    }

    #[test]
    fn test_parse_mise_toml_skips_latest() {
        let updater = MiseUpdater::new_parse_only();
        let content = r#"
[tools]
uv = "latest"
rust = "1.91.1"
"#;
        let deps = updater.parse_content(content, FileType::MiseToml);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "rust");
    }

    #[test]
    fn a_cargo_backend_tool_is_checked_against_crates_io() {
        let updater = MiseUpdater::new_parse_only();
        let content = r#"
[tools]
"cargo:maturin" = "1.0.0"
"cargo:cargo-zigbuild" = "latest"
rust = "1.91.1"
"#;
        let deps = updater.parse_content(content, FileType::MiseToml);

        // `latest` names no release to compare against, whatever its backend.
        let names: Vec<&str> = deps.iter().map(|dep| dep.name.as_str()).collect();
        assert_eq!(names, vec!["cargo:maturin", "rust"]);
    }

    #[test]
    fn test_parse_mise_toml_skips_unmapped_tools() {
        let updater = MiseUpdater::new_parse_only();
        let content = r#"
[tools]
rust = "1.91.1"
some-obscure-tool = "2.0.0"
"#;
        let deps = updater.parse_content(content, FileType::MiseToml);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "rust");
    }

    #[test]
    fn test_parse_tool_versions() {
        let updater = MiseUpdater::new_parse_only();
        let content = r#"# Development tools
node 20.11.0
python 3.12.2
golang 1.22.1
rust 1.91.1
"#;
        let deps = updater.parse_content(content, FileType::ToolVersions);
        assert_eq!(deps.len(), 4);
        assert_eq!(deps[0].name, "node");
        assert_eq!(deps[0].version, "20.11.0");
        assert_eq!(deps[1].name, "python");
        assert_eq!(deps[1].version, "3.12.2");
        assert_eq!(deps[2].name, "golang");
        assert_eq!(deps[2].version, "1.22.1");
        assert_eq!(deps[3].name, "rust");
        assert_eq!(deps[3].version, "1.91.1");
    }

    #[test]
    fn test_parse_tool_versions_skips_comments_and_empty() {
        let updater = MiseUpdater::new_parse_only();
        let content = r#"
# This is a comment
node 20.11.0

# Another comment
python 3.12.2
"#;
        let deps = updater.parse_content(content, FileType::ToolVersions);
        assert_eq!(deps.len(), 2);
    }

    #[test]
    fn test_parse_tool_versions_skips_unmapped() {
        let updater = MiseUpdater::new_parse_only();
        let content = "node 20.11.0\nunknown-tool 1.0.0\n";
        let deps = updater.parse_content(content, FileType::ToolVersions);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "node");
    }

    #[test]
    fn test_parse_tool_versions_skips_symbolic_versions() {
        // Symbolic specifiers in .tool-versions must be skipped.
        let updater = MiseUpdater::new_parse_only();
        let content = "node latest\ngo lts\nrust system\npython 3.12.2\n";
        let deps = updater.parse_content(content, FileType::ToolVersions);
        assert_eq!(deps.len(), 1, "only pinned python should be returned");
        assert_eq!(deps[0].name, "python");
    }

    #[test]
    fn test_parse_tool_versions_skips_latest() {
        let updater = MiseUpdater::new_parse_only();
        let content = "node latest\npython 3.12.2\n";
        let deps = updater.parse_content(content, FileType::ToolVersions);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "python");
    }

    #[test]
    fn test_parse_tool_versions_first_version_only() {
        // .tool-versions supports multiple versions per line but only the first is updated.
        let updater = MiseUpdater::new_parse_only();
        let content = "python 3.11.0 3.10.0 3.9.0\nnode 20.11.0\n";
        let deps = updater.parse_content(content, FileType::ToolVersions);
        // python should be parsed with 3.11.0 (the first version)
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "python");
        assert_eq!(
            deps[0].version, "3.11.0",
            "only the first version is parsed"
        );
        assert_eq!(deps[1].name, "node");
        assert_eq!(deps[1].version, "20.11.0");
    }

    #[test]
    fn test_compute_updated_version_strips_v_prefix() {
        assert_eq!(
            MiseUpdater::compute_updated_version("node", "20.11.0", "v22.5.0", false),
            "22.5.0"
        );
    }

    #[test]
    fn test_compute_updated_version_strips_go_prefix() {
        assert_eq!(
            MiseUpdater::compute_updated_version("go", "1.22.1", "go1.23.0", false),
            "1.23.0"
        );
        assert_eq!(
            MiseUpdater::compute_updated_version("golang", "1.22", "go1.23.0", false),
            "1.23"
        );
    }

    #[test]
    fn test_compute_updated_version_preserves_precision() {
        assert_eq!(
            MiseUpdater::compute_updated_version("python", "3.12", "v3.13.2", false),
            "3.13"
        );
        assert_eq!(
            MiseUpdater::compute_updated_version("python", "3.12.2", "v3.13.2", false),
            "3.13.2"
        );
    }

    #[test]
    fn test_compute_updated_version_full_precision() {
        assert_eq!(
            MiseUpdater::compute_updated_version("python", "3.12", "v3.13.2", true),
            "3.13.2"
        );
    }

    #[tokio::test]
    async fn test_update_mise_toml() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join(".mise.toml");
        fs::write(
            &file_path,
            r#"[tools]
rust = "1.90.0"
python = "3.12"
uv = "latest"
"#,
        )
        .unwrap();

        // The registry receives GitHub repo names and returns tags
        let registry = MockRegistry::new("github-releases")
            .with_version("rust-lang/rust", "v1.91.1")
            .with_version("python/cpython", "v3.13.2");

        let updater = updater_with(vec![(AnnotationSource::GitHubReleases, registry)]);
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(&file_path, &unused_registry(), options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.unchanged, 0);

        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.contains(r#"rust = "1.91.1""#));
        assert!(content.contains(r#"python = "3.13""#)); // precision preserved
        assert!(content.contains(r#"uv = "latest""#)); // unchanged
    }

    #[tokio::test]
    async fn test_update_tool_versions() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join(".tool-versions");
        fs::write(&file_path, "node 20.11.0\npython 3.12.2\ngolang 1.22.1\n").unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("nodejs/node", "v22.5.0")
            .with_version("python/cpython", "v3.13.2")
            .with_version("golang/go", "go1.23.0");

        let updater = updater_with(vec![(AnnotationSource::GitHubReleases, registry)]);
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(&file_path, &unused_registry(), options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 3);

        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("node 22.5.0"));
        assert!(content.contains("python 3.13.2"));
        assert!(content.contains("golang 1.23.0"));
    }

    #[tokio::test]
    async fn test_config_ignore_and_pin() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let temp = tempdir().unwrap();
        let file_path = temp.path().join(".mise.toml");
        fs::write(
            &file_path,
            "[tools]\nnode = \"20.11.0\"\nzig = \"0.13.0\"\nrust = \"1.80.0\"\n",
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("nodejs/node", "v22.0.0")
            .with_version("ziglang/zig", "0.14.0")
            .with_version("rust-lang/rust", "v1.85.0");

        let mut pins = std::collections::HashMap::new();
        pins.insert("zig".to_string(), "0.13.1".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec!["node".to_string()],
            pin: pins,
            cooldown: None,
            ..Default::default()
        };

        let updater = updater_with(vec![(AnnotationSource::GitHubReleases, registry)]);
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));
        let result = updater
            .update(&file_path, &unused_registry(), options)
            .await
            .unwrap();

        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "node");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "zig");
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "rust");
    }

    /// An entry the caller told upd to leave alone is reported as ignored and
    /// nothing else. Reporting it as unchecked as well would answer a question
    /// nobody asked, and would put a permanent line in the summary of every
    /// repo that ignores a tool upd has no registry for.
    #[tokio::test]
    async fn an_ignored_entry_is_not_also_reported_as_unchecked() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let temp = tempdir().unwrap();
        let file_path = temp.path().join(".mise.toml");
        fs::write(
            &file_path,
            "[tools]\n\"asdf:private-tool\" = \"1.0.0\"\nrust = \"1.80.0\"\n",
        )
        .unwrap();

        let config = UpdConfig {
            ignore: vec!["asdf:private-tool".to_string()],
            ..Default::default()
        };

        let updater = updater_with(vec![(
            AnnotationSource::GitHubReleases,
            MockRegistry::new("github-releases").with_version("rust-lang/rust", "v1.85.0"),
        )]);
        let options = UpdateOptions::new(true, false).with_config(Arc::new(config));
        let result = updater
            .update(&file_path, &unused_registry(), options)
            .await
            .unwrap();

        assert_eq!(
            result
                .ignored
                .iter()
                .map(|(name, _, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["asdf:private-tool"],
            "the ignore rule names the key exactly as the file spells it"
        );
        assert!(
            result.skipped.is_empty(),
            "an ignored entry is accounted for once, as ignored: {:?}",
            result.skipped
        );
    }

    #[tokio::test]
    async fn test_dry_run_mise_toml() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join(".mise.toml");
        let original = r#"[tools]
rust = "1.90.0"
"#;
        fs::write(&file_path, original).unwrap();

        let registry =
            MockRegistry::new("github-releases").with_version("rust-lang/rust", "v1.91.1");

        let updater = updater_with(vec![(AnnotationSource::GitHubReleases, registry)]);
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(&file_path, &unused_registry(), options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        // File should NOT be modified
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, original);
    }

    #[test]
    fn test_handles() {
        let updater = MiseUpdater::new_parse_only();
        assert!(updater.handles(FileType::MiseToml));
        assert!(updater.handles(FileType::ToolVersions));
        assert!(!updater.handles(FileType::Requirements));
    }

    #[tokio::test]
    async fn test_registry_error_populates_errors() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join(".mise.toml");
        fs::write(&file_path, "[tools]\nnode = \"20.0.0\"\n").unwrap();

        // Registry has no entry for nodejs/node → will error
        let registry = MockRegistry::new("github-releases");
        let updater = updater_with(vec![(AnnotationSource::GitHubReleases, registry)]);
        let options = UpdateOptions::new(true, false);
        let result = updater
            .update(&file_path, &unused_registry(), options)
            .await
            .unwrap();

        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("node"));
    }
}
