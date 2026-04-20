mod cargo_toml;
mod csproj;
mod gemfile;
mod github_actions;
mod go_mod;
mod mise;
mod package_json;
mod pre_commit;
mod pyproject;
mod requirements;
mod terraform;

pub use cargo_toml::CargoTomlUpdater;
pub use csproj::CsprojUpdater;
pub use gemfile::GemfileUpdater;
pub use github_actions::GithubActionsUpdater;
pub use go_mod::GoModUpdater;
pub use mise::MiseUpdater;
pub use package_json::PackageJsonUpdater;
pub use pre_commit::PreCommitUpdater;
pub use pyproject::PyProjectUpdater;
pub use requirements::RequirementsUpdater;
pub use terraform::TerraformUpdater;

use crate::config::UpdConfig;
use crate::registry::Registry;
use anyhow::{Result, anyhow};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Maximum file size allowed for dependency files (10 MB)
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// UTF-8 BOM character
const UTF8_BOM: char = '\u{feff}';

/// Read a file safely, handling BOM and enforcing size limits
pub fn read_file_safe(path: &Path) -> Result<String> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_FILE_SIZE {
        return Err(anyhow!(
            "File too large: {} bytes (max {} MB)",
            metadata.len(),
            MAX_FILE_SIZE / 1024 / 1024
        ));
    }

    let content = std::fs::read_to_string(path)?;
    // Strip UTF-8 BOM if present (common in Windows-created files)
    let content = content.strip_prefix(UTF8_BOM).unwrap_or(&content);
    Ok(content.to_string())
}

/// Build the standard warning message for a refused version downgrade.
///
/// Centralises the message format so all updaters emit identical text,
/// which makes it easy to grep logs and assert in tests.
pub(crate) fn downgrade_warning(pkg: &str, latest: &str, current: &str) -> String {
    format!("skipping {pkg}: latest \"{latest}\" is not greater than current \"{current}\"")
}

/// Write a file atomically (write to temp file, then rename)
pub fn write_file_atomic(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;

    // Create temp file in same directory to ensure atomic rename works
    let parent = path.parent().unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "temp".to_string());
    let tmp_path = parent.join(format!(".{}.upd.tmp", file_name));

    // Write to temporary file
    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;

    // Atomically rename to target path
    std::fs::rename(&tmp_path, path)?;

    Ok(())
}

/// Options for updating dependencies
#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    /// Dry run - don't write changes
    pub dry_run: bool,
    /// Use full version precision instead of matching original
    pub full_precision: bool,
    /// Configuration for ignoring/pinning packages
    pub config: Option<Arc<UpdConfig>>,
}

impl UpdateOptions {
    /// Create new options with the given dry_run and full_precision settings
    pub fn new(dry_run: bool, full_precision: bool) -> Self {
        Self {
            dry_run,
            full_precision,
            config: None,
        }
    }

    /// Set the configuration
    pub fn with_config(mut self, config: Arc<UpdConfig>) -> Self {
        self.config = Some(config);
        self
    }

    /// Check if a package should be ignored
    pub fn should_ignore(&self, package: &str) -> bool {
        self.config
            .as_ref()
            .map(|c| c.should_ignore(package))
            .unwrap_or(false)
    }

    /// Get the pinned version for a package (if any)
    pub fn get_pinned_version(&self, package: &str) -> Option<&str> {
        self.config
            .as_ref()
            .and_then(|c| c.get_pinned_version(package))
    }
}

/// A parsed dependency from a file (for alignment purposes)
#[derive(Debug, Clone)]
pub struct ParsedDependency {
    /// Package name
    pub name: String,
    /// Version string (the first/primary version number)
    pub version: String,
    /// Line number in the file (1-indexed)
    pub line_number: Option<usize>,
    /// Whether this dependency has upper bound constraints (e.g., <3.0)
    pub has_upper_bound: bool,
    /// Whether this dependency can be bumped to a newer version.
    ///
    /// Set to `false` for entries that reference a specific commit rather than
    /// a release tag (e.g. Go pseudo-versions like `v0.0.0-20200115085410-6d4e4cb37c7d`).
    /// Such entries are still included so that audit paths can see them, but the
    /// update path and alignment logic must not attempt to bump them.
    pub is_bumpable: bool,
}

/// Result of updating a single file
#[derive(Debug, Default, Clone)]
pub struct UpdateResult {
    /// Packages that were updated: (name, old_version, new_version, line_number)
    pub updated: Vec<(String, String, String, Option<usize>)>,
    /// Number of packages that were already at latest version
    pub unchanged: usize,
    /// Errors encountered during update
    pub errors: Vec<String>,
    /// Non-fatal warnings (e.g. lines with unparseable version tokens that were skipped)
    pub warnings: Vec<String>,
    /// Packages that were ignored due to config: (name, current_version, line_number)
    pub ignored: Vec<(String, String, Option<usize>)>,
    /// Packages that were pinned to a specific version: (name, current_version, pinned_version, line_number)
    pub pinned: Vec<(String, String, String, Option<usize>)>,
}

impl UpdateResult {
    pub fn merge(&mut self, other: UpdateResult) {
        self.updated.extend(other.updated);
        self.unchanged += other.unchanged;
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
        self.ignored.extend(other.ignored);
        self.pinned.extend(other.pinned);
    }
}

/// A version selected for a line in a dependency file, either resolved from a
/// registry fetch or supplied by user configuration (a pin).
pub(crate) enum PendingVersion {
    Registry(Result<String, anyhow::Error>),
    Pinned(String),
}

/// Language/ecosystem type for filtering
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum Lang {
    Python,
    Node,
    Rust,
    Go,
    Ruby,
    #[value(name = "dotnet")]
    DotNet,
    Actions,
    PreCommit,
    Mise,
    Terraform,
}

impl Lang {
    /// Canonical, stable identifier for this language (used by JSON output and CLI).
    pub fn as_str(&self) -> &'static str {
        match self {
            Lang::Python => "python",
            Lang::Node => "node",
            Lang::Rust => "rust",
            Lang::Go => "go",
            Lang::Ruby => "ruby",
            Lang::DotNet => "dotnet",
            Lang::Actions => "actions",
            Lang::PreCommit => "pre_commit",
            Lang::Mise => "mise",
            Lang::Terraform => "terraform",
        }
    }
}

/// Type of dependency file
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileType {
    Requirements,
    PyProject,
    PackageJson,
    CargoToml,
    GoMod,
    Gemfile,
    Csproj,
    GithubActions,
    PreCommitConfig,
    MiseToml,
    ToolVersions,
    TerraformTf,
}

impl FileType {
    /// Get the language/ecosystem for this file type
    pub fn lang(&self) -> Lang {
        match self {
            FileType::Requirements | FileType::PyProject => Lang::Python,
            FileType::PackageJson => Lang::Node,
            FileType::CargoToml => Lang::Rust,
            FileType::GoMod => Lang::Go,
            FileType::Gemfile => Lang::Ruby,
            FileType::Csproj => Lang::DotNet,
            FileType::GithubActions => Lang::Actions,
            FileType::PreCommitConfig => Lang::PreCommit,
            FileType::MiseToml | FileType::ToolVersions => Lang::Mise,
            FileType::TerraformTf => Lang::Terraform,
        }
    }

    /// Canonical, stable identifier for this file type (used by JSON output).
    pub fn as_str(&self) -> &'static str {
        match self {
            FileType::Requirements => "requirements",
            FileType::PyProject => "pyproject",
            FileType::PackageJson => "package_json",
            FileType::CargoToml => "cargo_toml",
            FileType::GoMod => "go_mod",
            FileType::Gemfile => "gemfile",
            FileType::Csproj => "csproj",
            FileType::GithubActions => "github_actions",
            FileType::PreCommitConfig => "pre_commit",
            FileType::MiseToml => "mise_toml",
            FileType::ToolVersions => "tool_versions",
            FileType::TerraformTf => "terraform_tf",
        }
    }
}

impl FileType {
    pub fn detect(path: &Path) -> Option<Self> {
        let file_name = path.file_name()?.to_str()?;

        if file_name == "pyproject.toml" {
            return Some(FileType::PyProject);
        }

        if file_name == "package.json" {
            return Some(FileType::PackageJson);
        }

        if file_name == "Cargo.toml" {
            return Some(FileType::CargoToml);
        }

        if file_name == "go.mod" {
            return Some(FileType::GoMod);
        }

        if file_name == "Gemfile" {
            return Some(FileType::Gemfile);
        }

        // .csproj files (case-insensitive extension check)
        if file_name
            .rsplit('.')
            .next()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("csproj"))
        {
            return Some(FileType::Csproj);
        }

        // Directory.Packages.props and Directory.Build.props (central package management)
        if file_name.eq_ignore_ascii_case("Directory.Packages.props")
            || file_name.eq_ignore_ascii_case("Directory.Build.props")
        {
            return Some(FileType::Csproj);
        }

        if file_name == ".pre-commit-config.yaml" {
            return Some(FileType::PreCommitConfig);
        }

        if file_name == ".mise.toml" {
            return Some(FileType::MiseToml);
        }

        if file_name == ".tool-versions" {
            return Some(FileType::ToolVersions);
        }

        // Terraform .tf files (exclude files inside .terraform/ directories)
        if file_name.ends_with(".tf") {
            let path_str = path.to_string_lossy();
            if !path_str.contains("/.terraform/") && !path_str.contains("\\.terraform\\") {
                return Some(FileType::TerraformTf);
            }
        }

        // Requirements file patterns (.txt and .in extensions)
        let is_requirements = |name: &str| -> bool {
            // Exact matches
            if name == "requirements.txt" || name == "requirements.in" {
                return true;
            }

            // Pattern: requirements-*.txt, requirements-*.in
            if (name.starts_with("requirements-") || name.starts_with("requirements_"))
                && (name.ends_with(".txt") || name.ends_with(".in"))
            {
                return true;
            }

            // Pattern: *-requirements.txt, *_requirements.txt, *.requirements.txt
            if name.ends_with("-requirements.txt")
                || name.ends_with("_requirements.txt")
                || name.ends_with(".requirements.txt")
                || name.ends_with("-requirements.in")
                || name.ends_with("_requirements.in")
                || name.ends_with(".requirements.in")
            {
                return true;
            }

            false
        };

        if is_requirements(file_name) {
            return Some(FileType::Requirements);
        }

        None
    }
}

/// Trait for file updaters
#[async_trait::async_trait]
pub trait Updater: Send + Sync {
    /// Update the file at the given path
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult>;

    /// Check if this updater handles the given file type
    fn handles(&self, file_type: FileType) -> bool;

    /// Parse dependencies from a file (for alignment purposes)
    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>>;
}

/// Scan for GitHub Actions workflow files in .github/workflows/ directories.
/// This is a separate pass because WalkBuilder::hidden(true) skips dot-directories.
fn discover_github_actions(path: &Path, files: &mut Vec<(PathBuf, FileType)>) {
    let workflows_dir = path.join(".github").join("workflows");
    if !workflows_dir.is_dir() {
        return;
    }

    if let Ok(entries) = std::fs::read_dir(&workflows_dir) {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_file()
                && let Some(ext) = entry_path.extension().and_then(|e| e.to_str())
                && (ext == "yml" || ext == "yaml")
            {
                files.push((entry_path, FileType::GithubActions));
            }
        }
    }
}

/// Scan for .pre-commit-config.yaml in a directory.
/// This is a separate pass because WalkBuilder::hidden(true) skips dot-files.
fn discover_pre_commit_config(path: &Path, files: &mut Vec<(PathBuf, FileType)>) {
    let config_path = path.join(".pre-commit-config.yaml");
    if config_path.is_file() {
        files.push((config_path, FileType::PreCommitConfig));
    }
}

/// Scan for .mise.toml and .tool-versions in a directory.
/// These are dot-files, so WalkBuilder::hidden(true) skips them.
fn discover_mise_files(path: &Path, files: &mut Vec<(PathBuf, FileType)>) {
    let mise_path = path.join(".mise.toml");
    if mise_path.is_file() {
        files.push((mise_path, FileType::MiseToml));
    }

    let tool_versions_path = path.join(".tool-versions");
    if tool_versions_path.is_file() {
        files.push((tool_versions_path, FileType::ToolVersions));
    }
}

fn discover_hidden_dependency_files(
    path: &Path,
    langs: &[Lang],
    files: &mut Vec<(PathBuf, FileType)>,
) {
    if langs.is_empty() || langs.contains(&Lang::Actions) {
        discover_github_actions(path, files);
    }

    if langs.is_empty() || langs.contains(&Lang::PreCommit) {
        discover_pre_commit_config(path, files);
    }

    if langs.is_empty() || langs.contains(&Lang::Mise) {
        discover_mise_files(path, files);
    }
}

/// Discover dependency files in the given paths, optionally filtered by language
pub fn discover_files(paths: &[PathBuf], langs: &[Lang]) -> Vec<(PathBuf, FileType)> {
    let mut files = Vec::new();

    for path in paths {
        if path.is_file()
            && let Some(file_type) = FileType::detect(path)
        {
            if langs.is_empty() || langs.contains(&file_type.lang()) {
                files.push((path.clone(), file_type));
            }
        } else if path.is_dir() {
            discover_hidden_dependency_files(path, langs, &mut files);

            // Walk directory respecting .gitignore
            let walker = WalkBuilder::new(path)
                .hidden(true)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .build();

            for entry in walker.flatten() {
                let entry_path = entry.path();
                if entry_path.is_dir() && entry_path != path {
                    discover_hidden_dependency_files(entry_path, langs, &mut files);
                }

                if entry_path.is_file()
                    && let Some(file_type) = FileType::detect(entry_path)
                    && (langs.is_empty() || langs.contains(&file_type.lang()))
                {
                    files.push((entry_path.to_path_buf(), file_type));
                }
            }
        }
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_update_result_merge() {
        let mut result1 = UpdateResult {
            updated: vec![(
                "pkg1".to_string(),
                "1.0".to_string(),
                "2.0".to_string(),
                Some(1),
            )],
            unchanged: 5,
            errors: vec!["error1".to_string()],
            warnings: vec!["warn1".to_string()],
            ignored: vec![("ignored1".to_string(), "1.0".to_string(), Some(3))],
            pinned: vec![(
                "pinned1".to_string(),
                "1.0".to_string(),
                "1.5".to_string(),
                Some(4),
            )],
        };

        let result2 = UpdateResult {
            updated: vec![(
                "pkg2".to_string(),
                "2.0".to_string(),
                "3.0".to_string(),
                Some(2),
            )],
            unchanged: 3,
            errors: vec!["error2".to_string()],
            warnings: vec!["warn2".to_string()],
            ignored: vec![("ignored2".to_string(), "2.0".to_string(), Some(5))],
            pinned: vec![(
                "pinned2".to_string(),
                "2.0".to_string(),
                "2.5".to_string(),
                Some(6),
            )],
        };

        result1.merge(result2);

        assert_eq!(result1.updated.len(), 2);
        assert_eq!(result1.unchanged, 8);
        assert_eq!(result1.errors.len(), 2);
        assert_eq!(result1.warnings.len(), 2);
        assert_eq!(result1.ignored.len(), 2);
        assert_eq!(result1.pinned.len(), 2);
        assert_eq!(result1.updated[0].0, "pkg1");
        assert_eq!(result1.updated[1].0, "pkg2");
        assert_eq!(result1.ignored[0].0, "ignored1");
        assert_eq!(result1.ignored[1].0, "ignored2");
        assert_eq!(result1.pinned[0].0, "pinned1");
        assert_eq!(result1.pinned[1].0, "pinned2");
    }

    #[test]
    fn test_update_result_default() {
        let result = UpdateResult::default();
        assert!(result.updated.is_empty());
        assert_eq!(result.unchanged, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_filetype_as_str_is_unique_and_stable() {
        let variants = [
            FileType::Requirements,
            FileType::PyProject,
            FileType::PackageJson,
            FileType::CargoToml,
            FileType::GoMod,
            FileType::Gemfile,
            FileType::Csproj,
            FileType::GithubActions,
            FileType::PreCommitConfig,
            FileType::MiseToml,
            FileType::ToolVersions,
            FileType::TerraformTf,
        ];
        let mut seen = std::collections::HashSet::new();
        for ft in variants {
            let name = ft.as_str();
            assert!(
                seen.insert(name),
                "duplicate FileType::as_str value: {name}"
            );
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "FileType::as_str must be snake_case ASCII: {name}"
            );
        }
        assert_eq!(FileType::PackageJson.as_str(), "package_json");
        assert_eq!(FileType::TerraformTf.as_str(), "terraform_tf");
    }

    #[test]
    fn test_lang_as_str_is_unique_and_stable() {
        let variants = [
            Lang::Python,
            Lang::Node,
            Lang::Rust,
            Lang::Go,
            Lang::Ruby,
            Lang::DotNet,
            Lang::Actions,
            Lang::PreCommit,
            Lang::Mise,
            Lang::Terraform,
        ];
        let mut seen = std::collections::HashSet::new();
        for lang in variants {
            let name = lang.as_str();
            assert!(seen.insert(name), "duplicate Lang::as_str value: {name}");
        }
        assert_eq!(Lang::DotNet.as_str(), "dotnet");
        assert_eq!(Lang::PreCommit.as_str(), "pre_commit");
    }

    #[test]
    fn test_discover_files_single_file() {
        let temp = tempdir().unwrap();
        let req_path = temp.path().join("requirements.txt");
        fs::write(&req_path, "flask>=2.0").unwrap();

        let files = discover_files(std::slice::from_ref(&req_path), &[]);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, req_path);
        assert_eq!(files[0].1, FileType::Requirements);
    }

    #[test]
    fn test_discover_files_directory() {
        let temp = tempdir().unwrap();

        // Create various dependency files
        fs::write(temp.path().join("requirements.txt"), "flask>=2.0").unwrap();
        fs::write(
            temp.path().join("pyproject.toml"),
            "[project]\nname = \"test\"",
        )
        .unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        // Create a non-matching file
        fs::write(temp.path().join("README.md"), "# Test").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[]);

        assert_eq!(files.len(), 3);

        // Check that all expected file types are present
        let types: Vec<_> = files.iter().map(|(_, ft)| *ft).collect();
        assert!(types.contains(&FileType::Requirements));
        assert!(types.contains(&FileType::PyProject));
        assert!(types.contains(&FileType::PackageJson));
    }

    #[test]
    fn test_discover_files_multiple_requirements() {
        let temp = tempdir().unwrap();

        fs::write(temp.path().join("requirements.txt"), "flask>=2.0").unwrap();
        fs::write(temp.path().join("requirements-dev.txt"), "pytest>=7.0").unwrap();
        fs::write(temp.path().join("requirements.in"), "django>=4.0").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[]);

        assert_eq!(files.len(), 3);
        assert!(files.iter().all(|(_, ft)| *ft == FileType::Requirements));
    }

    #[test]
    fn test_discover_files_empty_directory() {
        let temp = tempdir().unwrap();
        let files = discover_files(&[temp.path().to_path_buf()], &[]);
        assert!(files.is_empty());
    }

    #[test]
    fn test_discover_files_nonexistent_path() {
        let files = discover_files(&[PathBuf::from("/nonexistent/path")], &[]);
        assert!(files.is_empty());
    }

    #[test]
    fn test_discover_files_mixed_paths() {
        let temp = tempdir().unwrap();

        // Create a file directly in temp
        let direct_file = temp.path().join("requirements.txt");
        fs::write(&direct_file, "flask>=2.0").unwrap();

        // Create a subdirectory with a file
        let subdir = temp.path().join("subdir");
        fs::create_dir(&subdir).unwrap();
        fs::write(subdir.join("package.json"), "{}").unwrap();

        // Discover from both paths
        let files = discover_files(&[direct_file.clone(), subdir.clone()], &[]);

        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_file_type_detection() {
        // PyProject
        assert_eq!(
            FileType::detect(Path::new("pyproject.toml")),
            Some(FileType::PyProject)
        );
        assert_eq!(
            FileType::detect(Path::new("/some/path/pyproject.toml")),
            Some(FileType::PyProject)
        );

        // Package.json
        assert_eq!(
            FileType::detect(Path::new("package.json")),
            Some(FileType::PackageJson)
        );
        assert_eq!(
            FileType::detect(Path::new("/some/path/package.json")),
            Some(FileType::PackageJson)
        );

        // Requirements.txt patterns
        assert_eq!(
            FileType::detect(Path::new("requirements.txt")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("requirements.in")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("requirements-dev.txt")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("requirements_dev.txt")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("requirements-dev.in")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("dev-requirements.txt")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("dev_requirements.txt")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("dev.requirements.txt")),
            Some(FileType::Requirements)
        );

        // Cargo.toml
        assert_eq!(
            FileType::detect(Path::new("Cargo.toml")),
            Some(FileType::CargoToml)
        );
        assert_eq!(
            FileType::detect(Path::new("/some/path/Cargo.toml")),
            Some(FileType::CargoToml)
        );

        // go.mod
        assert_eq!(FileType::detect(Path::new("go.mod")), Some(FileType::GoMod));
        assert_eq!(
            FileType::detect(Path::new("/some/path/go.mod")),
            Some(FileType::GoMod)
        );

        // Pre-commit config
        assert_eq!(
            FileType::detect(Path::new(".pre-commit-config.yaml")),
            Some(FileType::PreCommitConfig)
        );

        // Gemfile
        assert_eq!(
            FileType::detect(Path::new("Gemfile")),
            Some(FileType::Gemfile)
        );

        // Mise
        assert_eq!(
            FileType::detect(Path::new(".mise.toml")),
            Some(FileType::MiseToml)
        );

        // Tool versions
        assert_eq!(
            FileType::detect(Path::new(".tool-versions")),
            Some(FileType::ToolVersions)
        );

        // Non-matching patterns
        assert_eq!(FileType::detect(Path::new("requirements")), None);
        assert_eq!(FileType::detect(Path::new("requirements-dev")), None);
        assert_eq!(FileType::detect(Path::new("setup.py")), None);
        assert_eq!(FileType::detect(Path::new("cargo.toml")), None); // lowercase doesn't match
    }

    #[test]
    fn test_file_type_lang_mapping() {
        assert_eq!(FileType::Requirements.lang(), Lang::Python);
        assert_eq!(FileType::PyProject.lang(), Lang::Python);
        assert_eq!(FileType::PackageJson.lang(), Lang::Node);
        assert_eq!(FileType::CargoToml.lang(), Lang::Rust);
        assert_eq!(FileType::GoMod.lang(), Lang::Go);
        assert_eq!(FileType::Gemfile.lang(), Lang::Ruby);
        assert_eq!(FileType::GithubActions.lang(), Lang::Actions);
        assert_eq!(FileType::PreCommitConfig.lang(), Lang::PreCommit);
        assert_eq!(FileType::MiseToml.lang(), Lang::Mise);
        assert_eq!(FileType::ToolVersions.lang(), Lang::Mise);
    }

    #[test]
    fn test_discover_files_with_lang_filter() {
        let temp = tempdir().unwrap();

        // Create files for different ecosystems
        fs::write(temp.path().join("requirements.txt"), "flask>=2.0").unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();
        fs::write(temp.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        fs::write(temp.path().join("go.mod"), "module test").unwrap();

        // No filter - should get all 4
        let files = discover_files(&[temp.path().to_path_buf()], &[]);
        assert_eq!(files.len(), 4);

        // Filter for Python only
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Python]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::Requirements);

        // Filter for Node only
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Node]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::PackageJson);

        // Filter for Rust only
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Rust]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::CargoToml);

        // Filter for Go only
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Go]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::GoMod);

        // Filter for Python and Rust
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Python, Lang::Rust]);
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_discover_github_actions_files() {
        let temp = tempdir().unwrap();
        let workflows_dir = temp.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();
        fs::write(workflows_dir.join("ci.yml"), "name: CI\non: push").unwrap();
        fs::write(workflows_dir.join("release.yaml"), "name: Release").unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[]);
        assert_eq!(files.len(), 3);
        let types: Vec<_> = files.iter().map(|(_, ft)| *ft).collect();
        assert!(types.contains(&FileType::PackageJson));
        assert_eq!(
            types
                .iter()
                .filter(|ft| **ft == FileType::GithubActions)
                .count(),
            2
        );
    }

    #[test]
    fn test_discover_github_actions_respects_lang_filter() {
        let temp = tempdir().unwrap();
        let workflows_dir = temp.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();
        fs::write(workflows_dir.join("ci.yml"), "name: CI").unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Node]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::PackageJson);

        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Actions]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::GithubActions);
    }

    #[test]
    fn test_discover_pre_commit_config() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join(".pre-commit-config.yaml"), "repos: []").unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[]);
        let types: Vec<_> = files.iter().map(|(_, ft)| *ft).collect();
        assert!(types.contains(&FileType::PreCommitConfig));
        assert!(types.contains(&FileType::PackageJson));
    }

    #[test]
    fn test_discover_mise_files() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join(".mise.toml"), "[tools]\nnode = \"20\"").unwrap();
        fs::write(temp.path().join(".tool-versions"), "node 20").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Mise]);
        assert_eq!(files.len(), 2);
        let types: Vec<_> = files.iter().map(|(_, ft)| *ft).collect();
        assert!(types.contains(&FileType::MiseToml));
        assert!(types.contains(&FileType::ToolVersions));
    }

    #[test]
    fn test_discover_mise_respects_lang_filter() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join(".mise.toml"), "[tools]").unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        // Node filter should not include mise files
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Node]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::PackageJson);
    }

    #[test]
    fn test_discover_nested_hidden_ecosystem_files() {
        let temp = tempdir().unwrap();
        let nested = temp.path().join("apps").join("api");

        fs::create_dir_all(nested.join(".github").join("workflows")).unwrap();
        fs::write(
            nested.join(".github").join("workflows").join("ci.yml"),
            "name: CI",
        )
        .unwrap();
        fs::write(nested.join(".pre-commit-config.yaml"), "repos: []").unwrap();
        fs::write(nested.join(".mise.toml"), "[tools]\nnode = \"20\"").unwrap();
        fs::write(nested.join(".tool-versions"), "node 20").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[]);
        let paths: Vec<_> = files.iter().map(|(path, _)| path.clone()).collect();

        assert!(paths.contains(&nested.join(".github").join("workflows").join("ci.yml")));
        assert!(paths.contains(&nested.join(".pre-commit-config.yaml")));
        assert!(paths.contains(&nested.join(".mise.toml")));
        assert!(paths.contains(&nested.join(".tool-versions")));
    }

    #[test]
    fn test_discover_no_github_dir() {
        let temp = tempdir().unwrap();
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Actions]);
        assert!(files.is_empty());
    }
}
