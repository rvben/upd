//! Lockfile regeneration support
//!
//! After updating manifest files, this module can regenerate lockfiles
//! by invoking the appropriate package manager.

use std::io;
use std::path::Path;
use std::process::Command;

use colored::Colorize;

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
                "{tool} not found on PATH — cannot regenerate {}\nhint: install {tool} or remove --lock",
                lockfile.filename()
            )),
            RegenOutcome::Failed { message, .. } => Some(message.clone()),
        }
    }
}

/// Lockfile types and their associated commands
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockfileType {
    /// poetry.lock - regenerated with `poetry lock --no-update`
    PoetryLock,
    /// uv.lock - regenerated with `uv lock`
    UvLock,
    /// package-lock.json - regenerated with `npm install`
    PackageLockJson,
    /// yarn.lock - regenerated with `yarn install`
    YarnLock,
    /// pnpm-lock.yaml - regenerated with `pnpm install`
    PnpmLock,
    /// bun.lockb - regenerated with `bun install`
    BunLock,
    /// Cargo.lock - regenerated with `cargo update`
    CargoLock,
    /// go.sum - regenerated with `go mod tidy`
    GoSum,
    /// Gemfile.lock - regenerated with `bundle install`
    GemfileLock,
    /// packages.lock.json - regenerated with `dotnet restore`
    PackagesLockJson,
    /// .terraform.lock.hcl - regenerated with `terraform providers lock`
    TerraformLock,
}

impl LockfileType {
    /// Get the lockfile filename
    pub fn filename(&self) -> &'static str {
        match self {
            LockfileType::PoetryLock => "poetry.lock",
            LockfileType::UvLock => "uv.lock",
            LockfileType::PackageLockJson => "package-lock.json",
            LockfileType::YarnLock => "yarn.lock",
            LockfileType::PnpmLock => "pnpm-lock.yaml",
            LockfileType::BunLock => "bun.lockb",
            LockfileType::CargoLock => "Cargo.lock",
            LockfileType::GoSum => "go.sum",
            LockfileType::GemfileLock => "Gemfile.lock",
            LockfileType::PackagesLockJson => "packages.lock.json",
            LockfileType::TerraformLock => ".terraform.lock.hcl",
        }
    }

    /// Get the command to regenerate this lockfile
    pub fn command(&self) -> (&'static str, &'static [&'static str]) {
        match self {
            LockfileType::PoetryLock => ("poetry", &["lock", "--no-update"]),
            LockfileType::UvLock => ("uv", &["lock"]),
            LockfileType::PackageLockJson => ("npm", &["install"]),
            LockfileType::YarnLock => ("yarn", &["install"]),
            LockfileType::PnpmLock => ("pnpm", &["install"]),
            LockfileType::BunLock => ("bun", &["install"]),
            LockfileType::CargoLock => ("cargo", &["update"]),
            LockfileType::GoSum => ("go", &["mod", "tidy"]),
            LockfileType::GemfileLock => ("bundle", &["install"]),
            LockfileType::PackagesLockJson => ("dotnet", &["restore"]),
            LockfileType::TerraformLock => ("terraform", &["providers", "lock"]),
        }
    }

    /// Get the manifest file this lockfile corresponds to
    pub fn manifest(&self) -> &'static str {
        match self {
            LockfileType::PoetryLock | LockfileType::UvLock => "pyproject.toml",
            LockfileType::PackageLockJson
            | LockfileType::YarnLock
            | LockfileType::PnpmLock
            | LockfileType::BunLock => "package.json",
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

/// Detect lockfiles in the directory containing the given manifest file
pub fn detect_lockfiles(manifest_path: &Path) -> Vec<LockfileType> {
    let dir = manifest_path.parent().unwrap_or(Path::new("."));
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
        if dir.join("package-lock.json").exists() {
            lockfiles.push(LockfileType::PackageLockJson);
        }
        if dir.join("yarn.lock").exists() {
            lockfiles.push(LockfileType::YarnLock);
        }
        if dir.join("pnpm-lock.yaml").exists() {
            lockfiles.push(LockfileType::PnpmLock);
        }
        if dir.join("bun.lockb").exists() {
            lockfiles.push(LockfileType::BunLock);
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

/// Returns `true` if `tool` is found on PATH.
///
/// Uses a lightweight probe: attempt to spawn `tool --version` and check
/// whether the OS reports `NotFound`. Any other result (including non-zero
/// exit from `--version`) means the binary exists.
pub fn tool_available(tool: &str) -> bool {
    match Command::new(tool).arg("--version").output() {
        Ok(_) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        // Unexpected OS error — assume the tool exists to avoid a false error.
        Err(_) => true,
    }
}

/// Regenerate a single lockfile by running the appropriate package manager.
///
/// Returns a [`RegenOutcome`] distinguishing success, missing tool, and
/// command failure.
pub fn regenerate_lockfile(
    manifest_path: &Path,
    lockfile_type: LockfileType,
    verbose: bool,
) -> RegenOutcome {
    let dir = manifest_path.parent().unwrap_or(Path::new("."));
    let (cmd, args) = lockfile_type.command();

    if !tool_available(cmd) {
        return RegenOutcome::ToolMissing {
            lockfile: lockfile_type,
            tool: cmd,
        };
    }

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

    let output = match Command::new(cmd).args(args).current_dir(dir).output() {
        Ok(o) => o,
        Err(e) => {
            return RegenOutcome::Failed {
                lockfile: lockfile_type,
                message: format!("Failed to run `{cmd}`: {e}"),
            };
        }
    };

    if output.status.success() {
        println!(
            "{} Regenerated {}",
            "✓".green(),
            lockfile_type.filename().bold()
        );
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
/// If no lockfiles are detected the caller is responsible for emitting the
/// `note:` skip message; this function sets `no_lockfiles = true` to signal
/// that.
pub fn regenerate_lockfiles(manifest_path: &Path, verbose: bool) -> LockfileRegenResult {
    let lockfiles = detect_lockfiles(manifest_path);

    if lockfiles.is_empty() {
        return LockfileRegenResult {
            outcomes: Vec::new(),
            no_lockfiles: true,
        };
    }

    let outcomes = lockfiles
        .into_iter()
        .map(|lf| regenerate_lockfile(manifest_path, lf, verbose))
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
        assert_eq!(LockfileType::BunLock.filename(), "bun.lockb");
        assert_eq!(LockfileType::CargoLock.filename(), "Cargo.lock");
        assert_eq!(LockfileType::GoSum.filename(), "go.sum");
    }

    #[test]
    fn test_lockfile_type_command() {
        let (cmd, args) = LockfileType::PoetryLock.command();
        assert_eq!(cmd, "poetry");
        assert_eq!(args, &["lock", "--no-update"]);

        let (cmd, args) = LockfileType::UvLock.command();
        assert_eq!(cmd, "uv");
        assert_eq!(args, &["lock"]);

        let (cmd, args) = LockfileType::PackageLockJson.command();
        assert_eq!(cmd, "npm");
        assert_eq!(args, &["install"]);

        let (cmd, args) = LockfileType::BunLock.command();
        assert_eq!(cmd, "bun");
        assert_eq!(args, &["install"]);

        let (cmd, args) = LockfileType::CargoLock.command();
        assert_eq!(cmd, "cargo");
        assert_eq!(args, &["update"]);

        let (cmd, args) = LockfileType::GoSum.command();
        assert_eq!(cmd, "go");
        assert_eq!(args, &["mod", "tidy"]);
    }

    #[test]
    fn test_lockfile_type_manifest() {
        assert_eq!(LockfileType::PoetryLock.manifest(), "pyproject.toml");
        assert_eq!(LockfileType::UvLock.manifest(), "pyproject.toml");
        assert_eq!(LockfileType::PackageLockJson.manifest(), "package.json");
        assert_eq!(LockfileType::YarnLock.manifest(), "package.json");
        assert_eq!(LockfileType::PnpmLock.manifest(), "package.json");
        assert_eq!(LockfileType::BunLock.manifest(), "package.json");
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
    fn test_detect_lockfiles_bun() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        let lockfile = dir.path().join("bun.lockb");

        fs::write(&manifest, "{}").unwrap();
        fs::write(&lockfile, "").unwrap();

        let detected = detect_lockfiles(&manifest);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0], LockfileType::BunLock);
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
    fn test_lockfile_type_gemfile_command() {
        let (cmd, args) = LockfileType::GemfileLock.command();
        assert_eq!(cmd, "bundle");
        assert_eq!(args, &["install"]);
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
    fn test_lockfile_type_packages_lock_json_command() {
        // .NET lockfiles are regenerated via `dotnet restore` (which
        // rewrites `packages.lock.json` next to each .csproj when
        // `RestorePackagesWithLockFile` is enabled).
        let (cmd, args) = LockfileType::PackagesLockJson.command();
        assert_eq!(cmd, "dotnet");
        assert_eq!(args, &["restore"]);
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
    fn test_lockfile_type_terraform_lock_hcl_command() {
        // Terraform's dependency-lock file is regenerated with
        // `terraform providers lock -platform=...`. We use the bare form
        // which updates the existing lock in-place for all platforms
        // currently pinned.
        let (cmd, args) = LockfileType::TerraformLock.command();
        assert_eq!(cmd, "terraform");
        assert_eq!(args, &["providers", "lock"]);
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

    // --- regenerate_lockfiles no-lockfile detection ---

    #[test]
    fn regenerate_lockfiles_no_lockfile_sets_flag() {
        // package.json with NO package-lock.json → no_lockfiles = true
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        fs::write(&manifest, "{}").unwrap();
        // Deliberately do NOT create package-lock.json

        let result = regenerate_lockfiles(&manifest, false);
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

        let result = regenerate_lockfiles(&manifest, false);
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
