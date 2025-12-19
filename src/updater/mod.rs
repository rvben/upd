mod cargo_toml;
mod go_mod;
mod package_json;
mod pyproject;
mod requirements;

pub use cargo_toml::CargoTomlUpdater;
pub use go_mod::GoModUpdater;
pub use package_json::PackageJsonUpdater;
pub use pyproject::PyProjectUpdater;
pub use requirements::RequirementsUpdater;

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
}

/// Result of updating a single file
#[derive(Debug, Default)]
pub struct UpdateResult {
    /// Packages that were updated: (name, old_version, new_version, line_number)
    pub updated: Vec<(String, String, String, Option<usize>)>,
    /// Number of packages that were already at latest version
    pub unchanged: usize,
    /// Errors encountered during update
    pub errors: Vec<String>,
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
        self.ignored.extend(other.ignored);
        self.pinned.extend(other.pinned);
    }
}

/// Language/ecosystem type for filtering
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum Lang {
    Python,
    Node,
    Rust,
    Go,
}

/// Type of dependency file
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Requirements,
    PyProject,
    PackageJson,
    CargoToml,
    GoMod,
}

impl FileType {
    /// Get the language/ecosystem for this file type
    pub fn lang(&self) -> Lang {
        match self {
            FileType::Requirements | FileType::PyProject => Lang::Python,
            FileType::PackageJson => Lang::Node,
            FileType::CargoToml => Lang::Rust,
            FileType::GoMod => Lang::Go,
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
            // Walk directory respecting .gitignore
            let walker = WalkBuilder::new(path)
                .hidden(true)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .build();

            for entry in walker.flatten() {
                let entry_path = entry.path();
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
}
