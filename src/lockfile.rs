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

/// Lockfile variants supported by `upd --lock`. See `command()` for the
/// concrete invocation used per variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockfileType {
    PoetryLock,
    UvLock,
    PackageLockJson,
    YarnLock,
    PnpmLock,
    BunLock,
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

    /// Returns the command + args to regenerate this lockfile.
    ///
    /// `changed` is the list of package names that `upd` just rewrote in the
    /// corresponding manifest. Ecosystems whose CLI supports a targeted form use
    /// it (`cargo update -p …`, `bundle lock --update …`). Ecosystems whose CLI
    /// supports a lockfile-only flag prefer that over a full install. Everything
    /// else falls back to the manifest-wide refresh command.
    pub fn command(&self, changed: &[String]) -> (&'static str, Vec<String>) {
        match self {
            LockfileType::PoetryLock => (
                "poetry",
                vec!["lock".to_string(), "--no-update".to_string()],
            ),
            LockfileType::UvLock => ("uv", vec!["lock".to_string()]),
            LockfileType::PackageLockJson => (
                "npm",
                vec!["install".to_string(), "--package-lock-only".to_string()],
            ),
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
            LockfileType::BunLock => ("bun", vec!["install".to_string()]),
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
    let dir = manifest_path.parent().unwrap_or(Path::new("."));
    let (cmd, args) = lockfile_type.command(changed);

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
        let (cmd, args) = LockfileType::PoetryLock.command(&[]);
        assert_eq!(cmd, "poetry");
        assert_eq!(args, &["lock", "--no-update"]);

        let (cmd, args) = LockfileType::UvLock.command(&[]);
        assert_eq!(cmd, "uv");
        assert_eq!(args, &["lock"]);
    }

    #[test]
    fn test_package_lock_json_uses_package_lock_only_flag() {
        let (cmd, args) = LockfileType::PackageLockJson.command(&["react".to_string()]);
        assert_eq!(cmd, "npm");
        assert_eq!(args, vec!["install", "--package-lock-only"]);
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
    fn test_bun_lock_uses_bun_install() {
        // Bun does not have a stable lockfile-only mode; plain `install` is the
        // minimum reliable form. Keeping the test pins the decision so changes
        // here are intentional.
        let (cmd, args) = LockfileType::BunLock.command(&["react".to_string()]);
        assert_eq!(cmd, "bun");
        assert_eq!(args, vec!["install"]);
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
