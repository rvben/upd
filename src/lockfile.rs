//! Lockfile regeneration support
//!
//! After updating manifest files, this module can regenerate lockfiles
//! by invoking the appropriate package manager.

use std::path::Path;
use std::process::Command;

use colored::Colorize;

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
        }
    }
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

    lockfiles
}

/// Regenerate a lockfile by running the appropriate package manager command
pub fn regenerate_lockfile(
    manifest_path: &Path,
    lockfile_type: LockfileType,
    verbose: bool,
) -> Result<(), String> {
    let dir = manifest_path.parent().unwrap_or(Path::new("."));
    let (cmd, args) = lockfile_type.command();

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

    let output = Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("Failed to run `{}`: {}", cmd, e))?;

    if output.status.success() {
        println!(
            "{} Regenerated {}",
            "✓".green(),
            lockfile_type.filename().bold()
        );
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "Failed to regenerate {}: {}",
            lockfile_type.filename(),
            stderr.trim()
        ))
    }
}

/// Regenerate all detected lockfiles for a manifest
pub fn regenerate_lockfiles(
    manifest_path: &Path,
    verbose: bool,
) -> Vec<Result<LockfileType, String>> {
    let lockfiles = detect_lockfiles(manifest_path);
    let mut results = Vec::new();

    for lockfile_type in lockfiles {
        match regenerate_lockfile(manifest_path, lockfile_type, verbose) {
            Ok(()) => results.push(Ok(lockfile_type)),
            Err(e) => results.push(Err(e)),
        }
    }

    results
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
}
