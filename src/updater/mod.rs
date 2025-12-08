mod package_json;
mod pyproject;
mod requirements;

pub use package_json::PackageJsonUpdater;
pub use pyproject::PyProjectUpdater;
pub use requirements::RequirementsUpdater;

use crate::registry::Registry;
use anyhow::Result;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Result of updating a single file
#[derive(Debug, Default)]
pub struct UpdateResult {
    /// Packages that were updated: (name, old_version, new_version)
    pub updated: Vec<(String, String, String)>,
    /// Number of packages that were already at latest version
    pub unchanged: usize,
    /// Errors encountered during update
    pub errors: Vec<String>,
}

impl UpdateResult {
    pub fn merge(&mut self, other: UpdateResult) {
        self.updated.extend(other.updated);
        self.unchanged += other.unchanged;
        self.errors.extend(other.errors);
    }
}

/// Type of dependency file
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Requirements,
    PyProject,
    PackageJson,
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

        // Requirements file patterns
        if file_name == "requirements.txt"
            || file_name.starts_with("requirements-")
            || file_name.starts_with("requirements_")
            || file_name.ends_with("-requirements.txt")
            || file_name.ends_with("_requirements.txt")
            || file_name.ends_with(".requirements.txt")
        {
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
        dry_run: bool,
    ) -> Result<UpdateResult>;

    /// Check if this updater handles the given file type
    fn handles(&self, file_type: FileType) -> bool;
}

/// Discover dependency files in the given paths
pub fn discover_files(paths: &[PathBuf]) -> Vec<(PathBuf, FileType)> {
    let mut files = Vec::new();

    for path in paths {
        if path.is_file() {
            if let Some(file_type) = FileType::detect(path) {
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
                if entry_path.is_file() {
                    if let Some(file_type) = FileType::detect(entry_path) {
                        files.push((entry_path.to_path_buf(), file_type));
                    }
                }
            }
        }
    }

    files
}
