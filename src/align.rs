//! Alignment module for ensuring consistent package versions across a repository.
//!
//! This module provides functionality to find the highest version of each package
//! used across multiple dependency files and update all occurrences to that version.

use crate::updater::{
    CargoTomlUpdater, CsprojUpdater, FileType, GemfileUpdater, GithubActionsUpdater, GoModUpdater,
    Lang, MiseUpdater, PackageJsonUpdater, ParsedDependency, PreCommitUpdater, PyProjectUpdater,
    RequirementsUpdater, TerraformUpdater, Updater,
};
use crate::version::is_stable_pep440;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Represents a single occurrence of a package in a file
#[derive(Debug, Clone)]
pub struct PackageOccurrence {
    /// Path to the file containing this package
    pub file_path: PathBuf,
    /// Type of the dependency file
    pub file_type: FileType,
    /// Version string as it appears in the file
    pub version: String,
    /// Line number where the package is defined (if available)
    pub line_number: Option<usize>,
    /// Whether this package has upper bound constraints (e.g., <3.0)
    pub has_upper_bound: bool,
    /// Package name with its original casing as it appears in the file.
    ///
    /// The HashMap key in `scan_packages` is lowercased for case-insensitive
    /// deduplication, but OSV NuGet lookups are case-sensitive. This field
    /// carries the un-normalized name so audit queries reach the right advisory.
    pub original_name: String,
    /// Whether this occurrence can be bumped to a newer version.
    ///
    /// `false` for commit-pinned references such as Go pseudo-versions.  These
    /// are still included so the audit path can inspect them, but they must not
    /// participate in alignment comparisons or update attempts.
    pub is_bumpable: bool,
}

/// Result of alignment analysis for a single package
#[derive(Debug, Clone)]
pub struct PackageAlignment {
    /// Name of the package
    pub package_name: String,
    /// The highest version found across all occurrences
    pub highest_version: String,
    /// All occurrences of this package
    pub occurrences: Vec<PackageOccurrence>,
    /// Language/ecosystem of this package
    pub lang: Lang,
}

impl PackageAlignment {
    /// Returns true if any occurrence is misaligned (not at highest version)
    pub fn has_misalignment(&self) -> bool {
        self.occurrences
            .iter()
            .any(|o| o.is_bumpable && !o.has_upper_bound && o.version != self.highest_version)
    }

    /// Returns only the misaligned occurrences (excluding those at highest version, with constraints, or not bumpable)
    pub fn misaligned_occurrences(&self) -> Vec<&PackageOccurrence> {
        self.occurrences
            .iter()
            .filter(|o| o.is_bumpable && !o.has_upper_bound && o.version != self.highest_version)
            .collect()
    }
}

/// Overall result of alignment analysis
#[derive(Debug, Default)]
pub struct AlignResult {
    /// All packages found with their alignments
    pub packages: Vec<PackageAlignment>,
    /// Total count of misaligned package occurrences
    pub misaligned_count: usize,
    /// Total number of files scanned
    pub total_files: usize,
}

/// Get the appropriate updater for a file type
fn get_updater(file_type: FileType) -> Box<dyn Updater> {
    match file_type {
        FileType::Requirements => Box::new(RequirementsUpdater::new()),
        FileType::PyProject => Box::new(PyProjectUpdater::new()),
        FileType::PackageJson => Box::new(PackageJsonUpdater::new()),
        FileType::CargoToml => Box::new(CargoTomlUpdater::new()),
        FileType::GoMod => Box::new(GoModUpdater::new()),
        FileType::Gemfile => Box::new(GemfileUpdater::new()),
        FileType::Csproj => Box::new(CsprojUpdater::new()),
        FileType::GithubActions => Box::new(GithubActionsUpdater::new()),
        FileType::PreCommitConfig => Box::new(PreCommitUpdater::new()),
        FileType::MiseToml | FileType::ToolVersions => Box::new(MiseUpdater::new()),
        FileType::TerraformTf => Box::new(TerraformUpdater::new()),
    }
}

/// Convert from updater's ParsedDependency to PackageOccurrence
fn to_occurrence(dep: &ParsedDependency, path: &Path, file_type: FileType) -> PackageOccurrence {
    PackageOccurrence {
        file_path: path.to_path_buf(),
        file_type,
        version: dep.version.clone(),
        line_number: dep.line_number,
        has_upper_bound: dep.has_upper_bound,
        original_name: dep.name.clone(),
        is_bumpable: dep.is_bumpable,
    }
}

/// Scan all dependency files and collect package versions grouped by package name and language
pub fn scan_packages(
    files: &[(PathBuf, FileType)],
) -> Result<HashMap<(String, Lang), Vec<PackageOccurrence>>> {
    let mut packages: HashMap<(String, Lang), Vec<PackageOccurrence>> = HashMap::new();

    for (path, file_type) in files {
        let updater = get_updater(*file_type);
        let deps = updater.parse_dependencies(path)?;
        let lang = file_type.lang();

        for dep in deps {
            let key = (dep.name.to_lowercase(), lang);
            packages
                .entry(key)
                .or_default()
                .push(to_occurrence(&dep, path, *file_type));
        }
    }

    Ok(packages)
}

/// Find the highest version for each package and identify misalignments
pub fn find_alignments(packages: HashMap<(String, Lang), Vec<PackageOccurrence>>) -> AlignResult {
    let mut result = AlignResult::default();

    for ((package_name, lang), occurrences) in packages {
        // Skip packages that only appear once (already "aligned")
        if occurrences.len() <= 1 {
            continue;
        }

        // Find highest version, only considering non-constrained occurrences
        let highest = find_highest_version(&occurrences, lang);

        if let Some(highest_version) = highest {
            let alignment = PackageAlignment {
                package_name: package_name.clone(),
                highest_version: highest_version.clone(),
                occurrences,
                lang,
            };

            if alignment.has_misalignment() {
                result.misaligned_count += alignment.misaligned_occurrences().len();
            }

            result.packages.push(alignment);
        }
    }

    // Sort by package name for consistent output
    result
        .packages
        .sort_by(|a, b| a.package_name.cmp(&b.package_name));

    result
}

/// Find the highest stable version among occurrences
fn find_highest_version(occurrences: &[PackageOccurrence], lang: Lang) -> Option<String> {
    occurrences
        .iter()
        .filter(|o| o.is_bumpable) // Skip commit-pinned refs (e.g. Go pseudo-versions)
        .filter(|o| !o.has_upper_bound) // Skip constrained versions
        .filter(|o| is_stable_version(&o.version, lang)) // Skip pre-releases
        .max_by(|a, b| compare_versions(&a.version, &b.version, lang))
        .map(|o| o.version.clone())
}

/// Check if a version is stable (not a pre-release)
fn is_stable_version(version: &str, lang: Lang) -> bool {
    match lang {
        Lang::Python => is_stable_pep440(version),
        Lang::Node | Lang::Rust | Lang::Go | Lang::DotNet => {
            // Semver pre-release indicator: hyphen followed by identifier
            !version.contains('-')
        }
        Lang::Ruby => {
            let v = version.to_lowercase();
            !v.contains(".pre")
                && !v.contains(".rc")
                && !v.contains(".beta")
                && !v.contains(".alpha")
        }
        Lang::Actions | Lang::PreCommit | Lang::Mise | Lang::Terraform => {
            let v = version.strip_prefix('v').unwrap_or(version);
            !v.contains('-')
        }
    }
}

/// Compare two versions within the same ecosystem.
///
/// Returns `Ordering::Greater` if `a` is newer than `b`, `Ordering::Less` if older,
/// and `Ordering::Equal` if they are the same version. Used by updaters to enforce
/// the no-downgrade invariant before writing any version change.
pub(crate) fn compare_versions(a: &str, b: &str, lang: Lang) -> std::cmp::Ordering {
    match lang {
        Lang::Python => compare_pep440(a, b),
        Lang::Node | Lang::Rust | Lang::Ruby | Lang::DotNet => compare_semver(a, b),
        Lang::Go => compare_go_version(a, b),
        Lang::Actions | Lang::PreCommit | Lang::Mise | Lang::Terraform => {
            let clean_a = a.trim_start_matches('v');
            let clean_b = b.trim_start_matches('v');
            compare_semver(clean_a, clean_b)
        }
    }
}

/// Compare PEP 440 versions
fn compare_pep440(a: &str, b: &str) -> std::cmp::Ordering {
    match (
        pep440_rs::Version::from_str(a),
        pep440_rs::Version::from_str(b),
    ) {
        (Ok(va), Ok(vb)) => va.cmp(&vb),
        _ => a.cmp(b), // Fallback to string comparison
    }
}

/// Compare semver versions
fn compare_semver(a: &str, b: &str) -> std::cmp::Ordering {
    // Clean up version strings (remove ^, ~, = prefixes)
    let clean_a = a.trim_start_matches(['^', '~', '=', 'v']);
    let clean_b = b.trim_start_matches(['^', '~', '=', 'v']);

    match (
        semver::Version::parse(clean_a),
        semver::Version::parse(clean_b),
    ) {
        (Ok(va), Ok(vb)) => va.cmp(&vb),
        _ => clean_a.cmp(clean_b), // Fallback to string comparison
    }
}

/// Compare Go module versions
fn compare_go_version(a: &str, b: &str) -> std::cmp::Ordering {
    // Go uses semver with 'v' prefix
    let clean_a = a.trim_start_matches('v');
    let clean_b = b.trim_start_matches('v');
    compare_semver(clean_a, clean_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_stable_version_python() {
        assert!(is_stable_version("1.0.0", Lang::Python));
        assert!(is_stable_version("2.31.0", Lang::Python));
        assert!(!is_stable_version("1.0.0a1", Lang::Python));
        assert!(!is_stable_version("1.0.0b2", Lang::Python));
        assert!(!is_stable_version("1.0.0rc1", Lang::Python));
        assert!(!is_stable_version("1.0.0dev1", Lang::Python));
        assert!(!is_stable_version("1.0.0alpha", Lang::Python));
        assert!(!is_stable_version("1.0.0beta", Lang::Python));
    }

    #[test]
    fn test_is_stable_version_semver() {
        assert!(is_stable_version("1.0.0", Lang::Node));
        assert!(is_stable_version("4.17.21", Lang::Rust));
        assert!(!is_stable_version("1.0.0-alpha", Lang::Node));
        assert!(!is_stable_version("1.0.0-beta.1", Lang::Rust));
        assert!(!is_stable_version("1.0.0-rc.1", Lang::Go));
    }

    #[test]
    fn test_compare_versions_semver() {
        use std::cmp::Ordering;
        assert_eq!(compare_semver("1.0.0", "2.0.0"), Ordering::Less);
        assert_eq!(compare_semver("2.0.0", "1.0.0"), Ordering::Greater);
        assert_eq!(compare_semver("1.0.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_semver("1.5.0", "1.10.0"), Ordering::Less);
        assert_eq!(compare_semver("^1.0.0", "^2.0.0"), Ordering::Less);
    }

    #[test]
    fn test_package_alignment_has_misalignment() {
        let alignment = PackageAlignment {
            package_name: "requests".to_string(),
            highest_version: "2.31.0".to_string(),
            lang: Lang::Python,
            occurrences: vec![
                PackageOccurrence {
                    file_path: PathBuf::from("requirements.txt"),
                    file_type: FileType::Requirements,
                    version: "2.28.0".to_string(),
                    line_number: Some(1),
                    has_upper_bound: false,
                    original_name: "requests".to_string(),
                    is_bumpable: true,
                },
                PackageOccurrence {
                    file_path: PathBuf::from("requirements-dev.txt"),
                    file_type: FileType::Requirements,
                    version: "2.31.0".to_string(),
                    line_number: Some(1),
                    has_upper_bound: false,
                    original_name: "requests".to_string(),
                    is_bumpable: true,
                },
            ],
        };

        assert!(alignment.has_misalignment());
        assert_eq!(alignment.misaligned_occurrences().len(), 1);
    }

    #[test]
    fn test_package_alignment_skips_constrained() {
        let alignment = PackageAlignment {
            package_name: "django".to_string(),
            highest_version: "4.2.0".to_string(),
            lang: Lang::Python,
            occurrences: vec![
                PackageOccurrence {
                    file_path: PathBuf::from("requirements.txt"),
                    file_type: FileType::Requirements,
                    version: "3.2.0".to_string(),
                    line_number: Some(1),
                    has_upper_bound: true, // Has constraint, should be skipped
                    original_name: "django".to_string(),
                    is_bumpable: true,
                },
                PackageOccurrence {
                    file_path: PathBuf::from("requirements-dev.txt"),
                    file_type: FileType::Requirements,
                    version: "4.2.0".to_string(),
                    line_number: Some(1),
                    has_upper_bound: false,
                    original_name: "django".to_string(),
                    is_bumpable: true,
                },
            ],
        };

        // No misalignment because the lower version has upper bound constraint
        assert!(!alignment.has_misalignment());
    }

    /// scan_packages must preserve the original package-name casing in each
    /// PackageOccurrence so that OSV audit queries use the correct name.
    ///
    /// OSV's NuGet ecosystem is case-sensitive: querying with "newtonsoft.json"
    /// returns no results, but "Newtonsoft.Json" returns CVE advisories. The
    /// HashMap key is intentionally lowercased for deduplication; the
    /// original_name field carries the un-lowercased form for audit queries.
    #[test]
    fn test_scan_packages_preserves_original_name_casing() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a minimal .csproj with a mixed-case NuGet package name
        let mut f = NamedTempFile::with_suffix(".csproj").unwrap();
        write!(
            f,
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="Newtonsoft.Json" Version="12.0.1" />
  </ItemGroup>
</Project>"#
        )
        .unwrap();

        let path = f.path().to_path_buf();
        let files = vec![(path, FileType::Csproj)];

        let packages = scan_packages(&files).unwrap();

        // The HashMap key must be lowercased for deduplication
        let key = ("newtonsoft.json".to_string(), Lang::DotNet);
        let occurrences = packages
            .get(&key)
            .expect("package not found under lowercased key");
        assert_eq!(occurrences.len(), 1);

        // The occurrence must carry the original, un-lowercased name for OSV queries
        assert_eq!(
            occurrences[0].original_name, "Newtonsoft.Json",
            "original_name must preserve casing; lowercased name fails OSV NuGet lookups"
        );
    }

    /// When the same Go module appears as a pseudo-version in one go.mod and as a
    /// semver release in another, `find_alignments` must NOT report a misalignment.
    /// Pseudo-versions are commit pins, not version choices, so they must be excluded
    /// from alignment comparisons.
    #[test]
    fn test_find_alignments_no_misalignment_for_pseudo_version() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file_a = NamedTempFile::with_suffix(".mod").unwrap();
        write!(
            file_a,
            "module example.com/a\n\nrequire golang.org/x/crypto v0.0.0-20200115085410-6d4e4cb37c7d\n"
        )
        .unwrap();

        let mut file_b = NamedTempFile::with_suffix(".mod").unwrap();
        write!(
            file_b,
            "module example.com/b\n\nrequire golang.org/x/crypto v0.17.0\n"
        )
        .unwrap();

        // We use scan_packages on actual go.mod files to get real ParsedDependency values.
        // The file extension is .mod, not go.mod, so we specify the FileType directly.
        let files = vec![
            (file_a.path().to_path_buf(), FileType::GoMod),
            (file_b.path().to_path_buf(), FileType::GoMod),
        ];
        let packages = scan_packages(&files).unwrap();
        let result = find_alignments(packages);

        // There must be no misaligned occurrences — the pseudo-version is a pin, not
        // a version choice that should be aligned to v0.17.0.
        assert_eq!(
            result.misaligned_count, 0,
            "pseudo-version occurrence must not be counted as misaligned"
        );

        // The package may appear in result.packages (with highest_version = v0.17.0),
        // but has_misalignment must be false.
        if let Some(alignment) = result
            .packages
            .iter()
            .find(|p| p.package_name == "golang.org/x/crypto")
        {
            assert!(
                !alignment.has_misalignment(),
                "has_misalignment must be false when the only lower occurrence is a pseudo-version"
            );
        }
    }

    /// scan_packages must include pseudo-version Go dependencies so that the audit path
    /// can see them. The occurrence must carry `is_bumpable: false`.
    #[test]
    fn test_scan_packages_includes_pseudo_version_with_is_bumpable_false() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut f = NamedTempFile::with_suffix(".mod").unwrap();
        write!(
            f,
            "module example.com/mymodule\n\nrequire golang.org/x/crypto v0.0.0-20200115085410-6d4e4cb37c7d\n"
        )
        .unwrap();

        let files = vec![(f.path().to_path_buf(), FileType::GoMod)];
        let packages = scan_packages(&files).unwrap();

        let key = ("golang.org/x/crypto".to_string(), Lang::Go);
        let occurrences = packages
            .get(&key)
            .expect("pseudo-version package not found by scan_packages");
        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].version, "v0.0.0-20200115085410-6d4e4cb37c7d");
        assert!(
            !occurrences[0].is_bumpable,
            "pseudo-version occurrence must have is_bumpable == false"
        );
    }
}
