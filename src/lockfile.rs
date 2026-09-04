//! Lockfile regeneration support
//!
//! After updating manifest files, this module can regenerate lockfiles
//! by invoking the appropriate package manager.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use colored::Colorize;

use crate::updater::specifier_floor;

/// The outcome of attempting to regenerate a single lockfile.
#[derive(Debug)]
pub enum RegenOutcome {
    /// Lockfile was successfully regenerated.
    Ok(LockfileType),
    /// The required CLI tool was not found on PATH.
    ToolMissing {
        lockfile: LockfileType,
        /// Name of the missing tool (e.g. `"npm"`).
        tool: &'static str,
    },
    /// The tool ran but exited with a non-zero status.
    Failed {
        lockfile: LockfileType,
        message: String,
    },
}

impl RegenOutcome {
    /// Returns `true` if this outcome represents a hard error (tool missing or
    /// command failure), which should propagate to the process exit code.
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            RegenOutcome::ToolMissing { .. } | RegenOutcome::Failed { .. }
        )
    }

    /// Returns the error message for hard-error outcomes.
    pub fn error_message(&self) -> Option<String> {
        match self {
            RegenOutcome::Ok(_) => None,
            RegenOutcome::ToolMissing { lockfile, tool } => Some(format!(
                "{tool} not found on PATH - cannot regenerate {}\nhint: install {tool} or remove --lock",
                lockfile.filename()
            )),
            RegenOutcome::Failed { message, .. } => Some(message.clone()),
        }
    }
}

/// Lockfile variants supported by `upd --lock`. See `command()` for the
/// concrete invocation used per variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockfileType {
    PoetryLock,
    UvLock,
    PackageLockJson,
    NpmShrinkwrap,
    YarnLock,
    PnpmLock,
    /// bun's text lockfile, the default since bun 1.2.
    BunLock,
    /// bun's binary lockfile from before 1.2, still read when no `bun.lock`
    /// sits beside it.
    BunLockb,
    CargoLock,
    GoSum,
    GemfileLock,
    PackagesLockJson,
    TerraformLock,
}

impl LockfileType {
    /// Get the lockfile filename
    pub fn filename(&self) -> &'static str {
        match self {
            LockfileType::PoetryLock => "poetry.lock",
            LockfileType::UvLock => "uv.lock",
            LockfileType::PackageLockJson => "package-lock.json",
            LockfileType::NpmShrinkwrap => "npm-shrinkwrap.json",
            LockfileType::YarnLock => "yarn.lock",
            LockfileType::PnpmLock => "pnpm-lock.yaml",
            LockfileType::BunLock => "bun.lock",
            LockfileType::BunLockb => "bun.lockb",
            LockfileType::CargoLock => "Cargo.lock",
            LockfileType::GoSum => "go.sum",
            LockfileType::GemfileLock => "Gemfile.lock",
            LockfileType::PackagesLockJson => "packages.lock.json",
            LockfileType::TerraformLock => ".terraform.lock.hcl",
        }
    }

    /// Returns the command + args to regenerate this lockfile.
    ///
    /// `changed` is the list of package names that `upd` just rewrote in the
    /// corresponding manifest. Ecosystems whose CLI supports a targeted form use
    /// it (`cargo update -p …`, `bundle lock --update …`). Ecosystems whose CLI
    /// supports a lockfile-only flag prefer that over a full install. Everything
    /// else falls back to the manifest-wide refresh command.
    ///
    /// The command is the one for an installed tool whose version is not
    /// known; see [`LockfileType::command_for`] for the version-aware form.
    pub fn command(&self, changed: &[String]) -> (&'static str, Vec<String>) {
        self.command_for(changed, None)
    }

    /// Like [`LockfileType::command`], with `tool_version` carrying what the
    /// tool printed for `--version` where the invocation depends on it.
    ///
    /// Poetry is the ecosystem where it matters: Poetry 2 removed
    /// `lock --no-update` and made its behaviour the default, so a bare
    /// `poetry lock` keeps every locked version the manifest still admits.
    /// Poetry 1 needs the flag for the same result. Without a readable version
    /// the flagged form is used, because on Poetry 2 it fails loudly where the
    /// bare form on Poetry 1 would silently refresh every package.
    pub fn command_for(
        &self,
        changed: &[String],
        tool_version: Option<&str>,
    ) -> (&'static str, Vec<String>) {
        match self {
            LockfileType::PoetryLock => {
                let mut args = vec!["lock".to_string()];
                let poetry_2_or_later = tool_version
                    .and_then(poetry_major)
                    .is_some_and(|major| major >= 2);
                if !poetry_2_or_later {
                    args.push("--no-update".to_string());
                }
                ("poetry", args)
            }
            LockfileType::UvLock => ("uv", vec!["lock".to_string()]),
            LockfileType::PackageLockJson | LockfileType::NpmShrinkwrap => (
                "npm",
                vec![
                    "install".to_string(),
                    "--package-lock-only".to_string(),
                    "--ignore-scripts".to_string(),
                ],
            ),
            // Yarn Berry (v2+) only: `--mode update-lockfile` is the only
            // documented form that refreshes `yarn.lock` without running
            // install scripts. Yarn 1 (Classic) does not accept `--mode` and
            // will error; Yarn 1 reached EOL in 2023 and is unsupported here.
            LockfileType::YarnLock => (
                "yarn",
                vec![
                    "install".to_string(),
                    "--mode".to_string(),
                    "update-lockfile".to_string(),
                ],
            ),
            LockfileType::PnpmLock => (
                "pnpm",
                vec!["install".to_string(), "--lockfile-only".to_string()],
            ),
            LockfileType::BunLock | LockfileType::BunLockb => (
                "bun",
                vec!["install".to_string(), "--lockfile-only".to_string()],
            ),
            LockfileType::CargoLock => {
                if changed.is_empty() {
                    (
                        "cargo",
                        vec!["update".to_string(), "--workspace".to_string()],
                    )
                } else {
                    let mut args = vec!["update".to_string()];
                    for pkg in changed {
                        args.push("-p".to_string());
                        args.push(pkg.clone());
                    }
                    ("cargo", args)
                }
            }
            LockfileType::GoSum => ("go", vec!["mod".to_string(), "tidy".to_string()]),
            LockfileType::GemfileLock => {
                if changed.is_empty() {
                    ("bundle", vec!["lock".to_string()])
                } else {
                    let mut args = vec!["lock".to_string(), "--update".to_string()];
                    args.extend(changed.iter().cloned());
                    ("bundle", args)
                }
            }
            LockfileType::PackagesLockJson => ("dotnet", vec!["restore".to_string()]),
            LockfileType::TerraformLock => (
                "terraform",
                vec!["providers".to_string(), "lock".to_string()],
            ),
        }
    }

    /// Get the manifest file this lockfile corresponds to
    pub fn manifest(&self) -> &'static str {
        match self {
            LockfileType::PoetryLock | LockfileType::UvLock => "pyproject.toml",
            LockfileType::PackageLockJson
            | LockfileType::NpmShrinkwrap
            | LockfileType::YarnLock
            | LockfileType::PnpmLock
            | LockfileType::BunLock
            | LockfileType::BunLockb => "package.json",
            LockfileType::CargoLock => "Cargo.toml",
            LockfileType::GoSum => "go.mod",
            LockfileType::GemfileLock => "Gemfile",
            // .NET supports multiple manifest shapes; `.csproj` is the
            // canonical one but a central-management project may keep
            // versions in `Directory.Packages.props` or `Directory.Build.props`.
            LockfileType::PackagesLockJson => ".csproj",
            LockfileType::TerraformLock => ".tf",
        }
    }
}

/// True if this manifest is a .NET project file that can own a
/// `packages.lock.json` next to it.
fn is_dotnet_manifest(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name.ends_with(".csproj")
        || name == "Directory.Packages.props"
        || name == "Directory.Build.props"
}

/// The directory containing `path`, safe to use as a working directory.
///
/// `Path::parent` returns `Some("")` for a bare file name such as
/// `Cargo.toml`, so an `unwrap_or(".")` fallback never fires and the empty
/// path leaks through. `Command::current_dir("")` then fails with the same
/// "No such file or directory" error a missing binary produces, which
/// misreports a perfectly runnable tool as absent. Map the empty parent to
/// `.` alongside the absent one.
pub fn containing_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// Detect lockfiles in the directory containing the given manifest file
pub fn detect_lockfiles(manifest_path: &Path) -> Vec<LockfileType> {
    let dir = containing_dir(manifest_path);
    let mut lockfiles = Vec::new();

    // Check for Python lockfiles (only if manifest is pyproject.toml)
    if manifest_path
        .file_name()
        .map(|n| n == "pyproject.toml")
        .unwrap_or(false)
    {
        if dir.join("poetry.lock").exists() {
            lockfiles.push(LockfileType::PoetryLock);
        }
        if dir.join("uv.lock").exists() {
            lockfiles.push(LockfileType::UvLock);
        }
    }

    // Check for Node.js lockfiles (only if manifest is package.json)
    if manifest_path
        .file_name()
        .map(|n| n == "package.json")
        .unwrap_or(false)
    {
        // npm ignores package-lock.json when npm-shrinkwrap.json exists, so
        // shrinkwrap takes priority and package-lock.json is not also reported.
        if dir.join("npm-shrinkwrap.json").exists() {
            lockfiles.push(LockfileType::NpmShrinkwrap);
        } else if dir.join("package-lock.json").exists() {
            lockfiles.push(LockfileType::PackageLockJson);
        }
        if dir.join("yarn.lock").exists() {
            lockfiles.push(LockfileType::YarnLock);
        }
        if dir.join("pnpm-lock.yaml").exists() {
            lockfiles.push(LockfileType::PnpmLock);
        }
        // bun reads bun.lock when both formats exist, so the text lockfile
        // takes priority and the binary one is not also reported.
        if dir.join("bun.lock").exists() {
            lockfiles.push(LockfileType::BunLock);
        } else if dir.join("bun.lockb").exists() {
            lockfiles.push(LockfileType::BunLockb);
        }
    }

    // Check for Rust lockfile (only if manifest is Cargo.toml)
    if manifest_path
        .file_name()
        .map(|n| n == "Cargo.toml")
        .unwrap_or(false)
        && dir.join("Cargo.lock").exists()
    {
        lockfiles.push(LockfileType::CargoLock);
    }

    // Check for Go sum file (only if manifest is go.mod)
    if manifest_path
        .file_name()
        .map(|n| n == "go.mod")
        .unwrap_or(false)
        && dir.join("go.sum").exists()
    {
        lockfiles.push(LockfileType::GoSum);
    }

    // Check for Ruby lockfile (only if manifest is Gemfile)
    if manifest_path
        .file_name()
        .map(|n| n == "Gemfile")
        .unwrap_or(false)
        && dir.join("Gemfile.lock").exists()
    {
        lockfiles.push(LockfileType::GemfileLock);
    }

    // Check for .NET packages.lock.json (only if manifest is a .NET project file)
    if manifest_path
        .file_name()
        .map(is_dotnet_manifest)
        .unwrap_or(false)
        && dir.join("packages.lock.json").exists()
    {
        lockfiles.push(LockfileType::PackagesLockJson);
    }

    // Check for Terraform lockfile (only if manifest is a .tf file)
    if manifest_path
        .extension()
        .map(|ext| ext == "tf")
        .unwrap_or(false)
        && dir.join(".terraform.lock.hcl").exists()
    {
        lockfiles.push(LockfileType::TerraformLock);
    }

    lockfiles
}

/// The full paths of every lockfile [`detect_lockfiles`] maps for a
/// manifest, in detection order.
pub fn lockfile_paths_for(manifest_path: &Path) -> Vec<PathBuf> {
    let dir = containing_dir(manifest_path);
    detect_lockfiles(manifest_path)
        .into_iter()
        .map(|lockfile| dir.join(lockfile.filename()))
        .collect()
}

/// The major version in Poetry's `--version` line, which reads
/// `Poetry (version 2.2.1)` from 1.2 on and `Poetry version 1.1.15` before.
/// `None` when the line carries no version.
pub fn poetry_major(version_output: &str) -> Option<u64> {
    let (_, after) = version_output.split_once("version")?;
    after
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// What spawning `tool --version` found.
#[derive(Debug)]
pub enum ToolProbe {
    /// The OS could not find the binary on PATH.
    Missing,
    /// The binary exists; `version` is what it printed for `--version` when
    /// that succeeded, which not every tool supports.
    Present { version: Option<String> },
}

/// Probe for `tool` by spawning `tool --version`. Only `NotFound` counts as
/// missing: any other result, including a non-zero exit from `--version`
/// or an unexpected OS error, means the binary exists.
pub fn probe_tool(tool: &str) -> ToolProbe {
    match Command::new(tool).arg("--version").output() {
        Ok(output) => ToolProbe::Present {
            version: output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .filter(|version| !version.is_empty()),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => ToolProbe::Missing,
        Err(_) => ToolProbe::Present { version: None },
    }
}

/// Returns `true` if `tool` is found on PATH.
pub fn tool_available(tool: &str) -> bool {
    !matches!(probe_tool(tool), ToolProbe::Missing)
}

/// What [`Snapshot::capture`] found at a path.
#[derive(Debug)]
enum Captured {
    /// The file existed with these bytes.
    Bytes(Vec<u8>),
    /// Nothing at the path, so restoring removes whatever a later step
    /// created there.
    Absent,
    /// Something is at the path but could not be read, so it can be neither
    /// rewritten nor removed with confidence; `restore` reports it instead.
    Unreadable(String),
}

/// A file a [`Snapshot`] could not put back, and why.
#[derive(Debug)]
pub struct RestoreFailure {
    pub path: PathBuf,
    pub reason: String,
}

impl std::fmt::Display for RestoreFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} was not restored: {}",
            crate::path_display::display_path(&self.path),
            self.reason
        )
    }
}

/// A pre-write byte capture of every file a lock transaction may touch, so
/// a failed refresh can put the manifest and its lockfiles back exactly as
/// the run found them.
#[derive(Debug, Default)]
pub struct Snapshot {
    files: Vec<(PathBuf, Captured)>,
}

impl Snapshot {
    /// Capture `paths` as they are now.
    pub fn capture(paths: &[PathBuf]) -> Self {
        let mut snapshot = Self::default();
        snapshot.extend(paths);
        snapshot
    }

    /// Capture the paths not yet held. A path already captured keeps the
    /// bytes from its first capture, so a manifest captured before the
    /// updaters write is not replaced by its rewritten form when the
    /// lockfiles beside it are captured later.
    pub fn extend(&mut self, paths: &[PathBuf]) {
        for path in paths {
            if self.files.iter().any(|(held, _)| held == path) {
                continue;
            }
            let captured = match std::fs::read(path) {
                Ok(bytes) => Captured::Bytes(bytes),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Captured::Absent,
                Err(e) => Captured::Unreadable(e.to_string()),
            };
            self.files.push((path.clone(), captured));
        }
    }

    /// Put every captured file back. A file whose bytes did not change is
    /// left alone rather than rewritten. Returns the files that could not
    /// be restored.
    pub fn restore(&self) -> Vec<RestoreFailure> {
        let mut failures = Vec::new();
        for (path, captured) in &self.files {
            let result = match captured {
                Captured::Bytes(bytes) => {
                    if std::fs::read(path).is_ok_and(|current| current == *bytes) {
                        continue;
                    }
                    std::fs::write(path, bytes)
                }
                Captured::Absent => match std::fs::remove_file(path) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                    other => other,
                },
                Captured::Unreadable(reason) => Err(io::Error::other(format!(
                    "it could not be read before the run ({reason})"
                ))),
            };
            if let Err(e) = result {
                failures.push(RestoreFailure {
                    path: path.clone(),
                    reason: e.to_string(),
                });
            }
        }
        failures
    }
}

/// Regenerate a single lockfile by running the appropriate package manager.
///
/// `changed` is the list of package names that `upd` just rewrote in the
/// corresponding manifest. This is forwarded to [`LockfileType::command`] so
/// ecosystems that support targeted commands (e.g. `cargo update -p …`) only
/// touch the packages that actually changed.
///
/// Returns a [`RegenOutcome`] distinguishing success, missing tool, and
/// command failure.
pub fn regenerate_lockfile(
    manifest_path: &Path,
    lockfile_type: LockfileType,
    changed: &[String],
    verbose: bool,
) -> RegenOutcome {
    let dir = containing_dir(manifest_path);
    let (tool, _) = lockfile_type.command(&[]);
    let version = match probe_tool(tool) {
        ToolProbe::Missing => {
            return RegenOutcome::ToolMissing {
                lockfile: lockfile_type,
                tool,
            };
        }
        ToolProbe::Present { version } => version,
    };
    let (cmd, args) = match lockfile_type {
        LockfileType::CargoLock => {
            let specs = cargo_update_specs(manifest_path, changed);
            lockfile_type.command(&specs)
        }
        _ => lockfile_type.command_for(changed, version.as_deref()),
    };

    if verbose {
        println!(
            "{}",
            format!(
                "Regenerating {} with `{} {}`...",
                lockfile_type.filename(),
                cmd,
                args.join(" ")
            )
            .cyan()
        );
    }

    let output = match Command::new(cmd).args(&args).current_dir(dir).output() {
        Ok(o) => o,
        Err(e) => {
            return RegenOutcome::Failed {
                lockfile: lockfile_type,
                message: format!("Failed to run `{cmd}`: {e}"),
            };
        }
    };

    if output.status.success() {
        // Success reporting is the caller's job: this module cannot know
        // whether stdout is a JSON report that stray text would corrupt.
        RegenOutcome::Ok(lockfile_type)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        RegenOutcome::Failed {
            lockfile: lockfile_type,
            message: format!(
                "Failed to regenerate {}: {}",
                lockfile_type.filename(),
                stderr.trim()
            ),
        }
    }
}

/// Cargo's semver-compatibility key: the position of the leading non-zero
/// component and its value, so `1.2.3` answers `(0, 1)` and `0.39.6` `(1, 39)`.
///
/// Two versions of one crate can only sit in the same lockfile when their keys
/// differ, which is what makes the key enough to tell locked entries apart.
/// Answers `None` for a version whose leading components are not numeric.
fn cargo_compat_key(version: &str) -> Option<(usize, u64)> {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    for (index, component) in core.split('.').take(3).enumerate() {
        let value: u64 = component.trim().parse().ok()?;
        if value != 0 {
            return Some((index, value));
        }
    }
    Some((2, 0))
}

/// What `Cargo.lock` records about the packages sharing one name.
#[derive(Default)]
struct LockedCrate {
    /// Every locked version, in the order the lockfile lists them.
    versions: Vec<String>,
    /// The versions a workspace member depends on, which are the edges the
    /// rewritten manifests own.
    direct: Vec<String>,
}

/// What `Cargo.lock` holds, keyed by package name.
///
/// An unreadable or unparseable lockfile answers empty, which leaves every
/// spec bare and hands the diagnosis to cargo.
fn locked_versions(lock_path: &Path, owner: Option<&str>) -> HashMap<String, LockedCrate> {
    #[derive(serde::Deserialize)]
    struct Lock {
        #[serde(default)]
        package: Vec<LockPackage>,
    }
    #[derive(serde::Deserialize)]
    struct LockPackage {
        name: String,
        version: String,
        source: Option<String>,
        #[serde(default)]
        dependencies: Vec<String>,
    }

    let Ok(text) = std::fs::read_to_string(lock_path) else {
        return HashMap::new();
    };
    let Ok(lock) = toml::from_str::<Lock>(&text) else {
        return HashMap::new();
    };

    let mut crates: HashMap<String, LockedCrate> = HashMap::new();
    for package in &lock.package {
        crates
            .entry(package.name.clone())
            .or_default()
            .versions
            .push(package.version.clone());
    }

    // A workspace member carries no source, so its dependency list is the set of
    // edges the manifests declare. Cargo writes an edge as a bare name and adds
    // the version only where the name alone would be ambiguous, which is exactly
    // the case a spec has to resolve. Only the package the rewritten manifest
    // defines counts: a sibling member reaching a different copy of the same
    // crate would otherwise look like a second edge of this one. A virtual
    // workspace root defines no package, and its `[workspace.dependencies]`
    // govern every member, so there all members count.
    for package in &lock.package {
        if package.source.is_some() || owner.is_some_and(|owner| owner != package.name) {
            continue;
        }
        for edge in &package.dependencies {
            let mut parts = edge.split_whitespace();
            let (Some(name), Some(version)) = (parts.next(), parts.next()) else {
                continue;
            };
            if let Some(entry) = crates.get_mut(name) {
                entry.direct.push(version.to_string());
            }
        }
    }
    crates
}

/// Every version `name` is required at by a parsed `Cargo.toml`, across the
/// normal, dev, build, target-specific and workspace dependency tables.
///
/// One crate can appear in several tables at incompatible majors, so the
/// answers are collected rather than stopping at the first.
fn manifest_requirements(manifest: &toml::Table, name: &str) -> Vec<String> {
    const TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

    fn requirement_of(table: &toml::Value, name: &str) -> Option<String> {
        match table.get(name)? {
            toml::Value::String(version) => Some(version.clone()),
            entry => entry.get("version")?.as_str().map(str::to_string),
        }
    }

    let mut scopes: Vec<&toml::Value> = vec![manifest.get("workspace")]
        .into_iter()
        .flatten()
        .collect();
    if let Some(toml::Value::Table(targets)) = manifest.get("target") {
        scopes.extend(targets.values());
    }

    let mut requirements = Vec::new();
    for table in TABLES {
        if let Some(deps) = manifest.get(table)
            && let Some(requirement) = requirement_of(deps, name)
        {
            requirements.push(requirement);
        }
    }
    for scope in scopes {
        for table in TABLES {
            if let Some(deps) = scope.get(table)
                && let Some(requirement) = requirement_of(deps, name)
            {
                requirements.push(requirement);
            }
        }
    }
    requirements
}

/// The locked versions `cargo update -p` should be pointed at for one crate.
///
/// A requirement identifies a locked entry by its compatibility key. When the
/// manifest declares the crate at several keys, the entries to move are the
/// ones their own requirement has outgrown, since those are the declarations
/// `upd` just rewrote; a declaration the lockfile still satisfies is left
/// alone. A declaration whose key the lockfile does not carry at all has just
/// crossed a compatibility boundary, and is anchored on the entry it left
/// behind, which the lockfile's own dependency edges name. Falls back to every
/// entry a declaration claims, and to nothing at all when even that is unclear,
/// so the caller can hand the ambiguity back to cargo rather than guess.
fn locked_entries_to_update<'a>(requirements: &[String], locked: &'a LockedCrate) -> Vec<&'a str> {
    let below = |floor: &str| -> Vec<&'a str> {
        let floor = floor.to_string();
        locked
            .versions
            .iter()
            .map(String::as_str)
            .filter(|version| {
                crate::version::compare::compare_versions(version, &floor)
                    == std::cmp::Ordering::Less
            })
            .collect()
    };
    let highest = |versions: Vec<&'a str>| -> Option<&'a str> {
        versions
            .into_iter()
            .max_by(|a, b| crate::version::compare::compare_versions(a, b))
    };
    let is_direct = |version: &&str| locked.direct.iter().any(|edge| edge == version);

    let mut matched: Vec<&str> = Vec::new();
    let mut stale: Vec<&str> = Vec::new();
    let mut unsatisfied: Vec<&str> = Vec::new();

    for requirement in requirements {
        // Only a declaration upd can rewrite can have outgrown its locked
        // entry, so only one of those anchors the update. A ceiling or an
        // exclusive bound is left standing by every updater, and reading its
        // version as a floor points `cargo update` at a compatibility key
        // nothing in this manifest moved.
        let Some(floor) = specifier_floor(requirement, 0).filter(|f| f.raisable) else {
            continue;
        };
        let floor = &requirement[floor.range];
        let Some(key) = cargo_compat_key(floor) else {
            continue;
        };
        let candidates: Vec<&str> = locked
            .versions
            .iter()
            .map(String::as_str)
            .filter(|version| cargo_compat_key(version) == Some(key))
            .collect();
        // More than one candidate means same-key entries from different sources,
        // which only a dependency edge can tell apart. Without one the choice
        // would be a guess, so the whole name goes back to cargo.
        let version = match candidates.as_slice() {
            [] => {
                unsatisfied.push(floor);
                continue;
            }
            [only] => *only,
            many => match many.iter().copied().filter(is_direct).collect::<Vec<_>>()[..] {
                [only] => only,
                _ => return Vec::new(),
            },
        };
        if !matched.contains(&version) {
            matched.push(version);
        }
        if crate::version::compare::compare_versions(version, floor) == std::cmp::Ordering::Less
            && !stale.contains(&version)
        {
            stale.push(version);
        }
    }

    // Cargo has to add a version no locked entry can satisfy whatever it is
    // told, so the spec only has to name an entry unambiguously enough for cargo
    // to re-resolve from. The entry the rewritten declaration moved off is the
    // one the lockfile still records as a direct edge; failing that, the version
    // nearest below the floor, preferring one no other declaration still claims
    // so an unchanged sibling declaration keeps its own entry.
    for floor in unsatisfied {
        let unclaimed: Vec<&str> = below(floor)
            .into_iter()
            .filter(|version| !matched.contains(version))
            .collect();
        let anchor = highest(unclaimed.iter().copied().filter(is_direct).collect())
            .or_else(|| highest(unclaimed))
            .or_else(|| highest(below(floor)));
        if let Some(anchor) = anchor
            && !stale.contains(&anchor)
        {
            stale.push(anchor);
        }
    }

    if !stale.is_empty() {
        return stale;
    }
    // Nothing moved, so every entry a declaration claims is named anyway: the
    // spec is unambiguous and cargo treats it as a no-op, where the bare name
    // would be refused outright.
    matched
}

/// Build the `-p` specs for `cargo update` from the names `upd` just rewrote.
///
/// A bare name is ambiguous when the lockfile carries two semver-incompatible
/// versions of one crate, a direct `2.x` beside a transitive `1.x`, and cargo
/// refuses the entire command rather than guess which one is meant. Those names
/// are qualified with the locked version the manifest asks for, or, when a
/// cross-major bump has left the manifest asking for a version the lockfile
/// does not carry at all, with the entry the lockfile still records as a direct
/// edge. Every other name stays bare, and so does one whose entry cannot be
/// singled out, leaving cargo to report the ambiguity itself.
fn cargo_update_specs(manifest_path: &Path, changed: &[String]) -> Vec<String> {
    let dir = containing_dir(manifest_path);
    let manifest = std::fs::read_to_string(manifest_path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok());
    let owner = manifest
        .as_ref()
        .and_then(|doc| doc.get("package")?.get("name")?.as_str());
    let locked = locked_versions(&dir.join("Cargo.lock"), owner);

    changed
        .iter()
        .flat_map(|name| {
            let Some(entry) = locked.get(name) else {
                return vec![name.clone()];
            };
            if entry.versions.len() < 2 {
                return vec![name.clone()];
            }
            let requirements = manifest
                .as_ref()
                .map(|doc| manifest_requirements(doc, name))
                .unwrap_or_default();

            match locked_entries_to_update(&requirements, entry).as_slice() {
                [] => vec![name.clone()],
                entries => entries
                    .iter()
                    .map(|version| format!("{name}@{version}"))
                    .collect(),
            }
        })
        .collect()
}

/// Build the `cargo update -p {package}@{locked} --precise {precise}` args.
///
/// Extracted so the arg construction can be asserted without spawning
/// `cargo`, mirroring how [`LockfileType::command`] is tested.
fn cargo_precise_args(package: &str, locked: &str, precise: &str) -> Vec<String> {
    vec![
        "update".to_string(),
        "-p".to_string(),
        format!("{package}@{locked}"),
        "--precise".to_string(),
        precise.to_string(),
    ]
}

/// Pin one transitive crate to an exact version with
/// `cargo update -p {package}@{locked} --precise {precise}`, run in
/// `lock_dir`. The `name@version` spec disambiguates duplicate versions of
/// the same crate in the dependency graph. Returns the same [`RegenOutcome`]
/// shape as [`regenerate_lockfile`] so callers report tool-missing and
/// failure uniformly.
pub(crate) fn cargo_update_precise(
    lock_dir: &Path,
    package: &str,
    locked: &str,
    precise: &str,
    verbose: bool,
) -> RegenOutcome {
    let lockfile = LockfileType::CargoLock;
    let args = cargo_precise_args(package, locked, precise);

    if !tool_available("cargo") {
        return RegenOutcome::ToolMissing {
            lockfile,
            tool: "cargo",
        };
    }

    if verbose {
        println!(
            "{}",
            format!(
                "Running `cargo {}` in {}...",
                args.join(" "),
                crate::path_display::display_path(lock_dir)
            )
            .cyan()
        );
    }

    let output = match Command::new("cargo")
        .args(&args)
        .current_dir(lock_dir)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return RegenOutcome::Failed {
                lockfile,
                message: format!("Failed to run `cargo`: {e}"),
            };
        }
    };

    if output.status.success() {
        RegenOutcome::Ok(lockfile)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        RegenOutcome::Failed {
            lockfile,
            message: format!(
                "Failed to run `cargo {}`: {}",
                args.join(" "),
                stderr.trim()
            ),
        }
    }
}

/// The result of running lockfile regeneration for a single manifest path.
#[derive(Debug, Default)]
pub struct LockfileRegenResult {
    /// Outcomes for each lockfile that was attempted (or found missing).
    pub outcomes: Vec<RegenOutcome>,
    /// True if the manifest had no associated lockfiles to regenerate.
    pub no_lockfiles: bool,
}

impl LockfileRegenResult {
    /// Returns all hard-error messages (tool missing or command failed).
    pub fn error_messages(&self) -> Vec<String> {
        self.outcomes
            .iter()
            .filter_map(|o| o.error_message())
            .collect()
    }
}

/// Regenerate all lockfiles for a manifest, returning a structured result.
///
/// `changed` is the list of package names that `upd` just rewrote in the
/// corresponding manifest. It is forwarded to each [`regenerate_lockfile`]
/// call so ecosystems that support targeted commands only touch the packages
/// that actually changed.
///
/// If no lockfiles are detected the caller is responsible for emitting the
/// `note:` skip message; this function sets `no_lockfiles = true` to signal
/// that.
pub fn regenerate_lockfiles(
    manifest_path: &Path,
    changed: &[String],
    verbose: bool,
) -> LockfileRegenResult {
    let lockfiles = detect_lockfiles(manifest_path);

    if lockfiles.is_empty() {
        return LockfileRegenResult {
            outcomes: Vec::new(),
            no_lockfiles: true,
        };
    }

    let outcomes = lockfiles
        .into_iter()
        .map(|lf| regenerate_lockfile(manifest_path, lf, changed, verbose))
        .collect();

    LockfileRegenResult {
        outcomes,
        no_lockfiles: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Write a Cargo project whose lockfile holds two majors of `thiserror`
    /// and one `serde`, and answer the `-p` specs for updating both.
    fn specs_for(manifest_body: &str) -> Vec<String> {
        specs_for_lock(
            manifest_body,
            r#"version = 4

[[package]]
name = "serde"
version = "1.0.228"

[[package]]
name = "thiserror"
version = "1.0.69"

[[package]]
name = "thiserror"
version = "2.0.18"
"#,
        )
    }

    /// As [`specs_for`], with a lockfile the test writes itself.
    fn specs_for_lock(manifest_body: &str, lock_body: &str) -> Vec<String> {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, manifest_body).unwrap();
        fs::write(dir.path().join("Cargo.lock"), lock_body).unwrap();
        cargo_update_specs(&manifest, &["serde".to_string(), "thiserror".to_string()])
    }

    #[test]
    fn a_crate_locked_at_two_majors_is_qualified_by_the_one_its_manifest_admits() {
        let specs = specs_for("[dependencies]\nserde = \"1.0.229\"\nthiserror = \"2.0.20\"\n");
        // serde sits in the lockfile once, so the bare name is unambiguous.
        assert_eq!(specs, vec!["serde", "thiserror@2.0.18"]);
    }

    #[test]
    fn a_manifest_holding_the_older_major_qualifies_with_the_older_locked_entry() {
        // Guards against reading "ambiguous" as "take the newest": a manifest
        // deliberately on 1.x must move the 1.x entry, not the transitive 2.x.
        let specs = specs_for("[dependencies]\nserde = \"1.0.229\"\nthiserror = \"1.0.70\"\n");
        assert_eq!(specs, vec!["serde", "thiserror@1.0.69"]);
    }

    #[test]
    fn a_range_requirement_is_qualified_from_its_lower_bound() {
        let specs =
            specs_for("[dev-dependencies]\nserde = \"1.0.229\"\nthiserror = \">=1.0.70, <2.0\"\n");
        assert_eq!(specs, vec!["serde", "thiserror@1.0.69"]);
    }

    #[test]
    fn an_ambiguous_crate_absent_from_every_dependency_table_stays_bare() {
        let specs = specs_for("[dependencies]\nserde = \"1.0.229\"\n");
        assert_eq!(specs, vec!["serde", "thiserror"]);
    }

    #[test]
    fn a_crate_declared_at_two_majors_moves_only_the_declaration_the_lockfile_outgrew() {
        // `^1` is still satisfied by the locked 1.0.69, so the build-dependency
        // is the one that just moved and the only entry cargo should be given.
        let specs = specs_for(concat!(
            "[dependencies]\nserde = \"1.0.229\"\nthiserror = \"^1\"\n",
            "[build-dependencies]\nthiserror = \"2.0.20\"\n",
        ));
        assert_eq!(specs, vec!["serde", "thiserror@2.0.18"]);
    }

    #[test]
    fn a_crate_whose_every_declaration_moved_names_each_locked_entry() {
        let specs = specs_for(concat!(
            "[dependencies]\nserde = \"1.0.229\"\nthiserror = \"1.0.70\"\n",
            "[target.'cfg(unix)'.dependencies]\nthiserror = \"2.0.20\"\n",
        ));
        assert_eq!(specs, vec!["serde", "thiserror@1.0.69", "thiserror@2.0.18"]);
    }

    #[test]
    fn a_crate_whose_declarations_the_lockfile_already_satisfies_still_names_each_entry() {
        let specs = specs_for(concat!(
            "[dependencies]\nserde = \"1.0.228\"\nthiserror = \"1.0.69\"\n",
            "[build-dependencies]\nthiserror = \"2.0.18\"\n",
        ));
        assert_eq!(specs, vec!["serde", "thiserror@1.0.69", "thiserror@2.0.18"]);
    }

    #[test]
    fn an_unchanged_declaration_keeps_its_entry_when_a_sibling_crosses_a_major() {
        let specs = specs_for(concat!(
            "[dependencies]\nserde = \"1.0.228\"\nthiserror = \"2.0.18\"\n",
            "[build-dependencies]\nthiserror = \"3.0.1\"\n",
        ));
        assert_eq!(specs, vec!["serde", "thiserror@1.0.69"]);
    }

    /// A ceiling is left standing by every updater, so it cannot have outgrown
    /// its locked entry and must not name one. Read as a floor, `<3.0` asks for
    /// a compatibility key the lockfile does not carry, and the entry nearest
    /// below it gets updated on behalf of a declaration nothing rewrote.
    #[test]
    fn a_declaration_upd_cannot_rewrite_does_not_name_a_locked_entry() {
        let specs = specs_for(concat!(
            "[dependencies]\nserde = \"1.0.229\"\nthiserror = \"1.0.70\"\n",
            "[build-dependencies]\nthiserror = \"<3.0\"\n",
        ));
        assert_eq!(specs, vec!["serde", "thiserror@1.0.69"]);
    }

    /// Nor may one claim an entry, which would push a real cross-major jump onto
    /// a different entry: the `<2.0` build-dependency reads as claiming 2.0.18,
    /// leaving the 2.x -> 3.x bump anchored on the 1.x transitive copy.
    #[test]
    fn a_declaration_upd_cannot_rewrite_does_not_displace_a_major_jumps_anchor() {
        let specs = specs_for(concat!(
            "[dependencies]\nserde = \"1.0.229\"\nthiserror = \"3.0.1\"\n",
            "[build-dependencies]\nthiserror = \"<2.0\"\n",
        ));
        assert_eq!(specs, vec!["serde", "thiserror@2.0.18"]);
    }

    /// A lockfile whose workspace member depends on `thiserror` at the version
    /// `edge` names, beside a transitive copy of the other major.
    fn lock_with_direct_edge(edge: &str) -> String {
        format!(
            r#"version = 4

[[package]]
name = "fixture"
version = "0.1.0"
dependencies = [
 "serde",
 "thiserror {edge}",
]

[[package]]
name = "serde"
version = "1.0.228"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = [
 "thiserror 2.0.18",
]

[[package]]
name = "thiserror"
version = "1.0.69"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "thiserror"
version = "2.0.18"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#
        )
    }

    #[test]
    fn a_major_jump_anchors_on_the_entry_the_lockfile_calls_direct() {
        let specs = specs_for_lock(
            "[dependencies]\nserde = \"1.0.228\"\nthiserror = \"3.0.1\"\n",
            &lock_with_direct_edge("1.0.69"),
        );
        // 2.0.18 sits nearer below 3.0.1, but 1.0.69 is the copy the manifest
        // declared before the jump, so the transitive 2.x is left alone.
        assert_eq!(specs, vec!["serde", "thiserror@1.0.69"]);
    }

    #[test]
    fn same_key_entries_are_told_apart_by_the_direct_edge() {
        let lock = r#"version = 4

[[package]]
name = "fixture"
version = "0.1.0"
dependencies = [
 "serde",
 "thiserror 1.1.0",
]

[[package]]
name = "sibling"
version = "0.1.0"
dependencies = [
 "thiserror 1.0.69",
]

[[package]]
name = "serde"
version = "1.0.228"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "thiserror"
version = "1.0.69"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "thiserror"
version = "1.1.0"
source = "git+https://example.invalid/thiserror#0c0ffee"
"#;
        let specs = specs_for_lock(
            concat!(
                "[package]\nname = \"fixture\"\n",
                "[dependencies]\nserde = \"1.0.228\"\nthiserror = \"1.2.0\"\n",
            ),
            lock,
        );
        // The sibling member reaches the other copy, which is not this
        // manifest's edge to follow.
        assert_eq!(specs, vec!["serde", "thiserror@1.1.0"]);
    }

    #[test]
    fn a_sibling_members_edge_does_not_anchor_this_manifests_jump() {
        let lock = r#"version = 4

[[package]]
name = "fixture"
version = "0.1.0"
dependencies = [
 "serde",
 "thiserror 1.0.69",
]

[[package]]
name = "sibling"
version = "0.1.0"
dependencies = [
 "thiserror 2.0.18",
]

[[package]]
name = "serde"
version = "1.0.228"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "thiserror"
version = "1.0.69"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "thiserror"
version = "2.0.18"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
        let specs = specs_for_lock(
            concat!(
                "[package]\nname = \"fixture\"\n",
                "[dependencies]\nserde = \"1.0.228\"\nthiserror = \"3.0.1\"\n",
            ),
            lock,
        );
        assert_eq!(specs, vec!["serde", "thiserror@1.0.69"]);
    }

    #[test]
    fn same_key_entries_no_edge_tells_apart_stay_bare() {
        let lock = r#"version = 4

[[package]]
name = "serde"
version = "1.0.228"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "thiserror"
version = "1.0.69"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "thiserror"
version = "1.1.0"
source = "git+https://example.invalid/thiserror#0c0ffee"
"#;
        let specs = specs_for_lock(
            "[dependencies]\nserde = \"1.0.228\"\nthiserror = \"1.2.0\"\n",
            lock,
        );
        assert_eq!(specs, vec!["serde", "thiserror"]);
    }

    #[test]
    fn cargo_compat_key_separates_versions_that_can_share_a_lockfile() {
        assert_eq!(cargo_compat_key("1.0.69"), Some((0, 1)));
        assert_eq!(cargo_compat_key("2.0.18"), Some((0, 2)));
        assert_eq!(cargo_compat_key("0.39.6"), Some((1, 39)));
        assert_eq!(cargo_compat_key("0.38.1"), Some((1, 38)));
        assert_eq!(cargo_compat_key("0.0.5"), Some((2, 5)));
        assert_eq!(cargo_compat_key("2.0"), Some((0, 2)));
        assert_eq!(cargo_compat_key("1.0.0-rc.1"), Some((0, 1)));
        assert_eq!(cargo_compat_key("x.y.z"), None);
    }

    #[test]
    fn a_requirement_no_locked_major_satisfies_anchors_on_the_nearest_lower_entry() {
        let specs = specs_for(
            r#"[package]
name = "fixture"

[dependencies]
serde = "1.0.228"
thiserror = "3.0.1"
"#,
        );
        assert_eq!(specs, vec!["serde", "thiserror@2.0.18"]);
    }

    #[test]
    fn a_requirement_below_every_locked_entry_stays_bare() {
        let specs = specs_for(
            r#"[package]
name = "fixture"

[dependencies]
serde = "1.0.228"
thiserror = "0.9.1"
"#,
        );
        assert_eq!(specs, vec!["serde", "thiserror"]);
    }

    #[test]
    fn poetry_major_reads_both_version_line_shapes() {
        assert_eq!(poetry_major("Poetry (version 2.2.1)"), Some(2));
        assert_eq!(poetry_major("Poetry (version 1.8.5)\n"), Some(1));
        assert_eq!(poetry_major("Poetry version 1.1.15"), Some(1));
        assert_eq!(poetry_major(""), None);
        assert_eq!(poetry_major("poetry: command not found"), None);
        assert_eq!(poetry_major("Poetry (version unknown)"), None);
    }

    /// Poetry 2 removed `--no-update`, and no later major brings it back.
    #[test]
    fn poetry_2_and_later_lock_commands_drop_the_removed_no_update_flag() {
        for probe in ["Poetry (version 2.2.1)", "Poetry (version 3.0.0)"] {
            let (cmd, args) = LockfileType::PoetryLock.command_for(&[], Some(probe));
            assert_eq!(cmd, "poetry");
            assert_eq!(args, vec!["lock"], "{probe}");
        }
    }

    /// Poetry 1 needs `--no-update` to keep the lock minimal, and a version
    /// upd cannot read gets the same form: on Poetry 2 it fails loudly, where
    /// the bare `poetry lock` on Poetry 1 would silently refresh every package.
    #[test]
    fn poetry_1_and_an_unreadable_version_keep_no_update() {
        for probe in [Some("Poetry (version 1.8.5)"), Some("garbage"), None] {
            let (_, args) = LockfileType::PoetryLock.command_for(&[], probe);
            assert_eq!(args, vec!["lock", "--no-update"], "{probe:?}");
        }
    }

    #[test]
    fn bun_relock_only_writes_the_lockfile() {
        for lockfile in [LockfileType::BunLock, LockfileType::BunLockb] {
            let (cmd, args) = lockfile.command(&["react".to_string()]);
            assert_eq!(cmd, "bun", "{lockfile:?}");
            assert_eq!(args, vec!["install", "--lockfile-only"], "{lockfile:?}");
        }
    }

    #[test]
    fn detect_bun_text_lockfile() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        fs::write(&manifest, "{}").unwrap();
        fs::write(dir.path().join("bun.lock"), "{}").unwrap();

        assert_eq!(detect_lockfiles(&manifest), vec![LockfileType::BunLock]);
    }

    /// bun reads `bun.lock` when both formats exist, so only the text lockfile
    /// is reported and the refresh command runs once.
    #[test]
    fn detect_prefers_bun_text_lockfile_over_binary() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        fs::write(&manifest, "{}").unwrap();
        fs::write(dir.path().join("bun.lock"), "{}").unwrap();
        fs::write(dir.path().join("bun.lockb"), "").unwrap();

        assert_eq!(detect_lockfiles(&manifest), vec![LockfileType::BunLock]);
    }

    #[test]
    fn lockfile_paths_for_names_every_detected_lockfile_beside_the_manifest() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("pyproject.toml");
        fs::write(&manifest, "[project]\n").unwrap();
        fs::write(dir.path().join("poetry.lock"), "").unwrap();
        fs::write(dir.path().join("uv.lock"), "").unwrap();

        assert_eq!(
            lockfile_paths_for(&manifest),
            vec![dir.path().join("poetry.lock"), dir.path().join("uv.lock")]
        );
    }

    #[test]
    fn snapshot_restores_bytes_and_removes_files_it_captured_as_absent() {
        let dir = tempdir().unwrap();
        let present = dir.path().join("present");
        let absent = dir.path().join("absent");
        fs::write(&present, "before").unwrap();

        let snapshot = Snapshot::capture(&[present.clone(), absent.clone()]);
        fs::write(&present, "after").unwrap();
        fs::write(&absent, "created").unwrap();

        assert!(snapshot.restore().is_empty());
        assert_eq!(fs::read_to_string(&present).unwrap(), "before");
        assert!(!absent.exists(), "a file absent at capture is removed");
    }

    /// A file whose bytes never changed is not rewritten: restoring a
    /// group must not touch the manifests in it that the run left alone.
    /// A read-only file makes the difference observable, since a rewrite
    /// would fail on it.
    #[cfg(unix)]
    #[test]
    fn snapshot_does_not_rewrite_a_file_whose_bytes_did_not_change() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let untouched = dir.path().join("untouched");
        fs::write(&untouched, "same").unwrap();
        let snapshot = Snapshot::capture(std::slice::from_ref(&untouched));
        fs::set_permissions(&untouched, fs::Permissions::from_mode(0o444)).unwrap();

        let failures = snapshot.restore();

        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(fs::read_to_string(&untouched).unwrap(), "same");
    }

    /// A path already held keeps the bytes from its first capture: the
    /// manifest is captured before the updaters write and the lockfiles just
    /// before the refresh, and the later capture must not overwrite the
    /// earlier one with the rewritten manifest.
    #[test]
    fn snapshot_extend_keeps_the_first_capture_of_a_path() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("manifest");
        let lock = dir.path().join("lock");
        fs::write(&manifest, "v1").unwrap();
        fs::write(&lock, "lock-v1").unwrap();

        let mut snapshot = Snapshot::capture(std::slice::from_ref(&manifest));
        fs::write(&manifest, "v2").unwrap();
        snapshot.extend(&[manifest.clone(), lock.clone()]);
        fs::write(&manifest, "v3").unwrap();
        fs::write(&lock, "lock-v2").unwrap();

        assert!(snapshot.restore().is_empty());
        assert_eq!(fs::read_to_string(&manifest).unwrap(), "v1");
        assert_eq!(fs::read_to_string(&lock).unwrap(), "lock-v1");
    }

    /// A file that exists but cannot be read is neither deleted nor
    /// overwritten on restore; the failure is reported instead, because an
    /// unreadable file and an absent one are different facts.
    #[cfg(unix)]
    #[test]
    fn snapshot_leaves_a_file_it_could_not_read_alone_and_reports_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let secret = dir.path().join("secret");
        fs::write(&secret, "original").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&secret).is_ok() {
            // Running as root: the permission bits do not apply, so the
            // unreadable case cannot be produced here.
            return;
        }

        let snapshot = Snapshot::capture(std::slice::from_ref(&secret));
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&secret, "changed").unwrap();

        let failures = snapshot.restore();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].path, secret);
        let rendered = failures[0].to_string();
        assert!(
            rendered.contains("secret was not restored: ")
                && rendered.contains("could not be read before the run"),
            "{rendered}"
        );
        assert_eq!(fs::read_to_string(&secret).unwrap(), "changed");
    }

    #[test]
    fn test_lockfile_type_filename() {
        assert_eq!(LockfileType::PoetryLock.filename(), "poetry.lock");
        assert_eq!(LockfileType::UvLock.filename(), "uv.lock");
        assert_eq!(
            LockfileType::PackageLockJson.filename(),
            "package-lock.json"
        );
        assert_eq!(LockfileType::YarnLock.filename(), "yarn.lock");
        assert_eq!(LockfileType::PnpmLock.filename(), "pnpm-lock.yaml");
        assert_eq!(LockfileType::BunLock.filename(), "bun.lock");
        assert_eq!(LockfileType::BunLockb.filename(), "bun.lockb");
        assert_eq!(LockfileType::CargoLock.filename(), "Cargo.lock");
        assert_eq!(LockfileType::GoSum.filename(), "go.sum");
    }

    #[test]
    fn test_lockfile_type_command() {
        let (cmd, args) = LockfileType::PoetryLock.command(&[]);
        assert_eq!(cmd, "poetry");
        assert_eq!(args, &["lock", "--no-update"]);

        let (cmd, args) = LockfileType::UvLock.command(&[]);
        assert_eq!(cmd, "uv");
        assert_eq!(args, &["lock"]);
    }

    #[test]
    fn test_cargo_precise_args() {
        let args = cargo_precise_args("examplecrate", "1.0.0", "1.2.0");
        assert_eq!(
            args,
            vec!["update", "-p", "examplecrate@1.0.0", "--precise", "1.2.0"]
        );
    }

    #[test]
    fn test_package_lock_json_uses_package_lock_only_flag() {
        let (cmd, args) = LockfileType::PackageLockJson.command(&["react".to_string()]);
        assert_eq!(cmd, "npm");
        assert_eq!(
            args,
            vec!["install", "--package-lock-only", "--ignore-scripts"]
        );
    }

    #[test]
    fn npm_relock_commands_ignore_scripts() {
        let (cmd, args) = LockfileType::PackageLockJson.command(&[]);
        assert_eq!(cmd, "npm");
        assert!(args.contains(&"--ignore-scripts".to_string()));
        let (cmd, args) = LockfileType::NpmShrinkwrap.command(&[]);
        assert_eq!(cmd, "npm");
        assert!(args.contains(&"--package-lock-only".to_string()));
        assert!(args.contains(&"--ignore-scripts".to_string()));
    }

    #[test]
    fn test_pnpm_lock_uses_lockfile_only_flag() {
        let (cmd, args) = LockfileType::PnpmLock.command(&["react".to_string()]);
        assert_eq!(cmd, "pnpm");
        assert_eq!(args, vec!["install", "--lockfile-only"]);
    }

    #[test]
    fn test_yarn_lock_uses_mode_update_lockfile_flag() {
        // Yarn Berry (2+) supports --mode update-lockfile; it is the only
        // documented flag that refreshes the lockfile without running install
        // scripts.
        let (cmd, args) = LockfileType::YarnLock.command(&["react".to_string()]);
        assert_eq!(cmd, "yarn");
        assert_eq!(args, vec!["install", "--mode", "update-lockfile"]);
    }

    #[test]
    fn test_cargo_lock_passes_each_changed_package_to_update_p() {
        let changed = vec!["serde".to_string(), "tokio".to_string()];
        let (cmd, args) = LockfileType::CargoLock.command(&changed);
        assert_eq!(cmd, "cargo");
        assert_eq!(args, vec!["update", "-p", "serde", "-p", "tokio"]);
    }

    #[test]
    fn test_cargo_lock_with_empty_changed_list_stays_workspace_broad() {
        // Defensive: an empty changed list should never reach command() from the
        // update path, but if it does (e.g. the `upd lock` subcommand) we emit the
        // broad workspace update so nothing silently regresses.
        let (cmd, args) = LockfileType::CargoLock.command(&[]);
        assert_eq!(cmd, "cargo");
        assert_eq!(args, vec!["update", "--workspace"]);
    }

    #[test]
    fn test_gemfile_lock_uses_bundle_lock_update_with_changed_packages() {
        let changed = vec!["rails".to_string(), "pg".to_string()];
        let (cmd, args) = LockfileType::GemfileLock.command(&changed);
        assert_eq!(cmd, "bundle");
        assert_eq!(args, vec!["lock", "--update", "rails", "pg"]);
    }

    #[test]
    fn test_gemfile_lock_with_empty_changed_list_uses_plain_bundle_lock() {
        // Without targeted packages, `bundle lock --update` would bump every gem;
        // we emit plain `bundle lock` (refreshes against the current Gemfile
        // without bumping anything).
        let (cmd, args) = LockfileType::GemfileLock.command(&[]);
        assert_eq!(cmd, "bundle");
        assert_eq!(args, vec!["lock"]);
    }

    #[test]
    fn test_go_sum_falls_back_to_mod_tidy_regardless_of_changed_list() {
        let (cmd, args) = LockfileType::GoSum.command(&["golang.org/x/net".to_string()]);
        assert_eq!(cmd, "go");
        assert_eq!(args, vec!["mod", "tidy"]);
    }

    #[test]
    fn test_packages_lock_json_falls_back_to_dotnet_restore() {
        let (cmd, args) = LockfileType::PackagesLockJson.command(&["Newtonsoft.Json".to_string()]);
        assert_eq!(cmd, "dotnet");
        assert_eq!(args, vec!["restore"]);
    }

    #[test]
    fn test_terraform_lock_falls_back_to_providers_lock() {
        let (cmd, args) = LockfileType::TerraformLock.command(&["hashicorp/aws".to_string()]);
        assert_eq!(cmd, "terraform");
        assert_eq!(args, vec!["providers", "lock"]);
    }

    #[test]
    fn test_lockfile_type_manifest() {
        assert_eq!(LockfileType::PoetryLock.manifest(), "pyproject.toml");
        assert_eq!(LockfileType::UvLock.manifest(), "pyproject.toml");
        assert_eq!(LockfileType::PackageLockJson.manifest(), "package.json");
        assert_eq!(LockfileType::YarnLock.manifest(), "package.json");
        assert_eq!(LockfileType::PnpmLock.manifest(), "package.json");
        assert_eq!(LockfileType::BunLock.manifest(), "package.json");
        assert_eq!(LockfileType::BunLockb.manifest(), "package.json");
        assert_eq!(LockfileType::CargoLock.manifest(), "Cargo.toml");
        assert_eq!(LockfileType::GoSum.manifest(), "go.mod");
    }

    #[test]
    fn test_detect_lockfiles_poetry() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("pyproject.toml");
        let lockfile = dir.path().join("poetry.lock");

        fs::write(&manifest, "[tool.poetry]").unwrap();
        fs::write(&lockfile, "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0], LockfileType::PoetryLock);
    }

    #[test]
    fn test_detect_lockfiles_uv() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("pyproject.toml");
        let lockfile = dir.path().join("uv.lock");

        fs::write(&manifest, "[project]").unwrap();
        fs::write(&lockfile, "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0], LockfileType::UvLock);
    }

    #[test]
    fn test_detect_lockfiles_npm() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        let lockfile = dir.path().join("package-lock.json");

        fs::write(&manifest, "{}").unwrap();
        fs::write(&lockfile, "{}").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0], LockfileType::PackageLockJson);
    }

    #[test]
    fn detect_prefers_shrinkwrap_over_package_lock() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        std::fs::write(&manifest, "{}").unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        std::fs::write(dir.path().join("npm-shrinkwrap.json"), "{}").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert!(detected.contains(&LockfileType::NpmShrinkwrap));
        assert!(
            !detected.contains(&LockfileType::PackageLockJson),
            "npm ignores package-lock.json when a shrinkwrap exists"
        );
    }

    #[test]
    fn detect_shrinkwrap_alone() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        std::fs::write(&manifest, "{}").unwrap();
        std::fs::write(dir.path().join("npm-shrinkwrap.json"), "{}").unwrap();
        assert!(detect_lockfiles(&manifest).contains(&LockfileType::NpmShrinkwrap));
    }

    #[test]
    fn test_detect_lockfiles_yarn() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        let lockfile = dir.path().join("yarn.lock");

        fs::write(&manifest, "{}").unwrap();
        fs::write(&lockfile, "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0], LockfileType::YarnLock);
    }

    #[test]
    fn test_detect_lockfiles_pnpm() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        let lockfile = dir.path().join("pnpm-lock.yaml");

        fs::write(&manifest, "{}").unwrap();
        fs::write(&lockfile, "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0], LockfileType::PnpmLock);
    }

    #[test]
    fn detect_bun_binary_lockfile() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        let lockfile = dir.path().join("bun.lockb");

        fs::write(&manifest, "{}").unwrap();
        fs::write(&lockfile, "").unwrap();

        assert_eq!(detect_lockfiles(&manifest), vec![LockfileType::BunLockb]);
    }

    #[test]
    fn test_detect_lockfiles_cargo() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        let lockfile = dir.path().join("Cargo.lock");

        fs::write(&manifest, "[package]").unwrap();
        fs::write(&lockfile, "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0], LockfileType::CargoLock);
    }

    #[test]
    fn test_detect_lockfiles_go() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("go.mod");
        let lockfile = dir.path().join("go.sum");

        fs::write(&manifest, "module example").unwrap();
        fs::write(&lockfile, "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0], LockfileType::GoSum);
    }

    #[test]
    fn test_detect_lockfiles_multiple() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("package.json");

        fs::write(&manifest, "{}").unwrap();
        fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        fs::write(dir.path().join("yarn.lock"), "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected.len(), 2);
        assert!(detected.contains(&LockfileType::PackageLockJson));
        assert!(detected.contains(&LockfileType::YarnLock));
    }

    #[test]
    fn test_detect_lockfiles_none() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("pyproject.toml");
        fs::write(&manifest, "[project]").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert!(detected.is_empty());
    }

    #[test]
    fn test_detect_lockfiles_wrong_manifest() {
        // poetry.lock should only be detected for pyproject.toml, not package.json
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        let lockfile = dir.path().join("poetry.lock");

        fs::write(&manifest, "{}").unwrap();
        fs::write(&lockfile, "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert!(detected.is_empty());
    }

    #[test]
    fn test_lockfile_type_gemfile_filename() {
        assert_eq!(LockfileType::GemfileLock.filename(), "Gemfile.lock");
    }

    #[test]
    fn test_lockfile_type_gemfile_manifest() {
        assert_eq!(LockfileType::GemfileLock.manifest(), "Gemfile");
    }

    #[test]
    fn test_detect_lockfiles_gemfile() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Gemfile");
        let lockfile = dir.path().join("Gemfile.lock");

        fs::write(&manifest, "source 'https://rubygems.org'").unwrap();
        fs::write(&lockfile, "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0], LockfileType::GemfileLock);
    }

    #[test]
    fn test_detect_lockfiles_gemfile_no_lockfile() {
        // Gemfile without Gemfile.lock should not detect any lockfile
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Gemfile");

        fs::write(&manifest, "source 'https://rubygems.org'").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert!(detected.is_empty());
    }

    #[test]
    fn test_detect_lockfiles_gemfile_wrong_manifest() {
        // Gemfile.lock should only be detected for Gemfile, not for other manifests
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("pyproject.toml");
        let lockfile = dir.path().join("Gemfile.lock");

        fs::write(&manifest, "[project]").unwrap();
        fs::write(&lockfile, "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert!(detected.is_empty());
    }

    #[test]
    fn test_lockfile_type_packages_lock_json_filename() {
        assert_eq!(
            LockfileType::PackagesLockJson.filename(),
            "packages.lock.json"
        );
    }

    #[test]
    fn test_detect_lockfiles_packages_lock_json_for_csproj() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("App.csproj");
        fs::write(&manifest, "<Project/>").unwrap();
        fs::write(dir.path().join("packages.lock.json"), "{}").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected, vec![LockfileType::PackagesLockJson]);
    }

    #[test]
    fn test_detect_lockfiles_packages_lock_json_for_directory_packages_props() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Directory.Packages.props");
        fs::write(&manifest, "<Project/>").unwrap();
        fs::write(dir.path().join("packages.lock.json"), "{}").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected, vec![LockfileType::PackagesLockJson]);
    }

    #[test]
    fn test_detect_lockfiles_packages_lock_json_ignored_without_dotnet_manifest() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        fs::write(&manifest, "{}").unwrap();
        fs::write(dir.path().join("packages.lock.json"), "{}").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert!(detected.is_empty());
    }

    #[test]
    fn test_lockfile_type_terraform_lock_hcl_filename() {
        assert_eq!(
            LockfileType::TerraformLock.filename(),
            ".terraform.lock.hcl"
        );
    }

    #[test]
    fn test_detect_lockfiles_terraform_lock_hcl_for_tf_file() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("main.tf");
        fs::write(&manifest, "").unwrap();
        fs::write(dir.path().join(".terraform.lock.hcl"), "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected, vec![LockfileType::TerraformLock]);
    }

    #[test]
    fn test_detect_lockfiles_terraform_lock_hcl_ignored_without_tf() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, "[package]").unwrap();
        fs::write(dir.path().join(".terraform.lock.hcl"), "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert!(detected.is_empty());
    }

    // --- tool_available tests ---

    #[test]
    fn tool_available_returns_true_for_known_tools() {
        // These tools are reliably present in a standard Rust dev environment.
        assert!(
            tool_available("cargo"),
            "cargo should be on PATH in a Rust dev environment"
        );
    }

    #[test]
    fn tool_available_returns_false_for_nonexistent_tool() {
        assert!(
            !tool_available("__upd_nonexistent_tool_abc123__"),
            "a nonsense binary name should not be found on PATH"
        );
    }

    // --- containing_dir ---

    #[test]
    fn containing_dir_maps_bare_file_name_to_current_dir() {
        // Path::parent yields Some("") here; the empty path is not a usable
        // working directory and must come back as ".".
        assert_eq!(containing_dir(Path::new("Cargo.toml")), Path::new("."));
    }

    #[test]
    fn containing_dir_keeps_real_parents() {
        assert_eq!(
            containing_dir(Path::new("a/b/Cargo.toml")),
            Path::new("a/b")
        );
        assert_eq!(containing_dir(Path::new("./Cargo.toml")), Path::new("."));
        assert_eq!(
            containing_dir(Path::new("/tmp/Cargo.toml")),
            Path::new("/tmp")
        );
    }

    #[test]
    fn containing_dir_maps_parentless_paths_to_current_dir() {
        assert_eq!(containing_dir(Path::new("/")), Path::new("."));
    }

    // --- regenerate_lockfiles no-lockfile detection ---

    #[test]
    fn regenerate_lockfiles_no_lockfile_sets_flag() {
        // package.json with NO package-lock.json → no_lockfiles = true
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        fs::write(&manifest, "{}").unwrap();
        // Deliberately do NOT create package-lock.json

        let result = regenerate_lockfiles(&manifest, &[], false);
        assert!(
            result.no_lockfiles,
            "no_lockfiles should be true when no lockfile exists beside the manifest"
        );
        assert!(
            result.outcomes.is_empty(),
            "outcomes should be empty when no lockfile exists"
        );
    }

    #[test]
    fn regenerate_lockfiles_cargo_no_lockfile_sets_flag() {
        // Cargo.toml with NO Cargo.lock → no_lockfiles = true
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, "[package]").unwrap();
        // Deliberately do NOT create Cargo.lock

        let result = regenerate_lockfiles(&manifest, &[], false);
        assert!(
            result.no_lockfiles,
            "no_lockfiles should be true when Cargo.lock is absent"
        );
    }

    #[test]
    fn regen_outcome_tool_missing_is_error_with_message() {
        let outcome = RegenOutcome::ToolMissing {
            lockfile: LockfileType::PackageLockJson,
            tool: "npm",
        };
        assert!(outcome.is_error());
        let msg = outcome
            .error_message()
            .expect("ToolMissing should have an error message");
        assert!(
            msg.contains("npm"),
            "error message should mention the tool name"
        );
        assert!(
            msg.contains("package-lock.json"),
            "error message should mention the lockfile"
        );
    }

    #[test]
    fn regen_outcome_ok_is_not_error() {
        let outcome = RegenOutcome::Ok(LockfileType::CargoLock);
        assert!(!outcome.is_error());
        assert!(outcome.error_message().is_none());
    }

    #[test]
    fn regen_outcome_failed_is_error() {
        let outcome = RegenOutcome::Failed {
            lockfile: LockfileType::CargoLock,
            message: "exit status 1".to_string(),
        };
        assert!(outcome.is_error());
        assert!(outcome.error_message().is_some());
    }

    #[test]
    fn lockfile_regen_result_error_messages_collects_hard_errors() {
        let result = LockfileRegenResult {
            outcomes: vec![
                RegenOutcome::Ok(LockfileType::PoetryLock),
                RegenOutcome::ToolMissing {
                    lockfile: LockfileType::CargoLock,
                    tool: "cargo",
                },
                RegenOutcome::Failed {
                    lockfile: LockfileType::GoSum,
                    message: "exit 1".to_string(),
                },
            ],
            no_lockfiles: false,
        };
        let msgs = result.error_messages();
        assert_eq!(msgs.len(), 2, "only ToolMissing and Failed are errors");
        assert!(msgs[0].contains("cargo"));
        assert!(msgs[1].contains("exit 1"));
    }
}
