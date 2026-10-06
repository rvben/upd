//! Offline annotation validation and snippet generation.
use crate::annotation::{expand_asset_template, is_version_token, valid_variable};
use crate::updater::{FileType, validate_annotations};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    /// One-based physical lines, empty for file-level I/O errors.
    pub lines: Vec<usize>,
    pub message: String,
}

impl Diagnostic {
    pub(crate) fn from_parser(text: &str) -> Self {
        let parsed = text.split_once(": ").and_then(|(location, message)| {
            let numbers = location
                .strip_prefix("line ")
                .or_else(|| location.strip_prefix("lines "))?;
            let lines = numbers
                .split(", ")
                .map(str::parse)
                .collect::<std::result::Result<Vec<usize>, _>>()
                .ok()?;
            Some(Self {
                lines,
                message: message.into(),
            })
        });
        parsed.unwrap_or_else(|| Self {
            lines: vec![],
            message: text.into(),
        })
    }
}

#[derive(Debug, Serialize)]
pub struct Validation {
    pub versions: usize,
    pub checksums: usize,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Serialize)]
pub struct FileValidation {
    pub path: String,
    #[serde(flatten)]
    pub validation: Validation,
}

#[derive(Debug, Default, Serialize)]
pub struct ValidationSummary {
    pub files: usize,
    pub versions: usize,
    pub checksums: usize,
    pub errors: usize,
}

#[derive(Debug, Serialize)]
pub struct ValidationReport {
    pub command: &'static str,
    pub valid: bool,
    pub files: Vec<FileValidation>,
    pub summary: ValidationSummary,
}

impl ValidationReport {
    /// Same discovery rules as updates; validation checks every annotation
    /// regardless of version selection, ignore/pin or release-age policy.
    pub fn scan(paths: &[PathBuf], options: crate::updater::DiscoverOptions<'_>) -> Self {
        let mut files = Vec::new();
        let mut existing = Vec::new();
        for path in paths {
            if path.exists() {
                existing.push(path.clone());
            } else {
                files.push(Self::io_error(path, "path does not exist".into()));
            }
        }
        for (path, file_type) in crate::updater::discover_files_with(&existing, &[], options) {
            let validation = match crate::updater::read_file_safe(&path) {
                Ok(content) => validate_annotations(&content, file_type),
                Err(error) => {
                    files.push(Self::io_error(&path, error.to_string()));
                    continue;
                }
            };
            if file_type != FileType::Annotated
                && !file_type.scans_annotations()
                && validation.versions == 0
                && validation.checksums == 0
                && validation.diagnostics.is_empty()
            {
                continue;
            }
            files.push(FileValidation {
                path: crate::path_display::display_path(&path),
                validation,
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut summary = ValidationSummary {
            files: files.len(),
            ..Default::default()
        };
        for file in &files {
            summary.versions += file.validation.versions;
            summary.checksums += file.validation.checksums;
            summary.errors += file.validation.diagnostics.len();
        }
        Self {
            command: "annotations validate",
            valid: summary.errors == 0,
            files,
            summary,
        }
    }

    fn io_error(path: &Path, message: String) -> FileValidation {
        FileValidation {
            path: crate::path_display::display_path(path),
            validation: Validation {
                versions: 0,
                checksums: 0,
                diagnostics: vec![Diagnostic {
                    lines: vec![],
                    message,
                }],
            },
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SnippetSyntax {
    Shell,
    Docker,
    Toml,
    Yaml,
    Javascript,
}

#[derive(Debug, Serialize)]
pub struct Scaffold {
    pub command: &'static str,
    pub package: String,
    pub version: String,
    pub tag: String,
    pub asset: String,
    pub asset_template: String,
    /// Supplied by the user or resolved from published release metadata.
    pub checksum: String,
    /// `supplied`, `github-asset-digest`, or the explicitly selected manifest name.
    pub checksum_source: String,
    pub snippet: String,
}

fn decode_segment(raw: &str) -> Result<String> {
    let mut bytes = Vec::new();
    let mut remaining = raw.as_bytes();
    while let Some((&first, rest)) = remaining.split_first() {
        if first == b'%' {
            ensure!(rest.len() >= 2, "invalid percent escape in asset URL");
            let hex = std::str::from_utf8(&rest[..2])?;
            bytes.push(u8::from_str_radix(hex, 16).context("invalid percent escape in asset URL")?);
            remaining = &rest[2..];
        } else {
            bytes.push(first);
            remaining = rest;
        }
    }
    String::from_utf8(bytes).context("asset URL must contain UTF-8 path segments")
}

fn safe_filename(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-+@{}".contains(&b))
}

/// A validated, exact release asset request. Parsing happens before any registry
/// construction or network I/O; the supplied SHA and online paths share rendering.
#[derive(Debug)]
pub struct ScaffoldRequest {
    package: String,
    version: String,
    tag: String,
    asset: String,
    asset_template: String,
    name: String,
    syntax: SnippetSyntax,
    checksums: Option<String>,
}

impl ScaffoldRequest {
    pub fn new(
        asset_url: &str,
        name: Option<&str>,
        syntax: SnippetSyntax,
        checksums: Option<&str>,
        asset_template: Option<&str>,
    ) -> Result<Self> {
        let url = url::Url::parse(asset_url).context("invalid release asset URL")?;
        ensure!(
            url.scheme() == "https"
                && url.host_str() == Some("github.com")
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "use a plain HTTPS github.com release asset URL without credentials, query, or fragment"
        );
        let parts = url
            .path_segments()
            .context("release asset URL has no path")?
            .map(decode_segment)
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            parts.len() == 6 && parts[2] == "releases" && parts[3] == "download",
            "expected https://github.com/<owner>/<repo>/releases/download/<tag>/<asset>"
        );
        ensure!(
            parts[..2].iter().all(|part| !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
            "invalid repository in release asset URL"
        );
        let tag = &parts[4];
        let version = tag.strip_prefix('v').unwrap_or(tag);
        ensure!(
            is_version_token(version)
                && version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b)),
            "release tag must be a concrete version, such as v1.2.3"
        );
        let asset = &parts[5];
        ensure!(
            safe_filename(asset) && !asset.contains(['{', '}']) && asset != "." && asset != "..",
            "asset filename cannot be represented safely in an annotation; use a filename with letters, digits, dots, underscores, hyphens, plus signs, or @"
        );
        let derived = parts[1]
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let derived = if derived.starts_with(|c: char| c.is_ascii_digit()) {
            format!("TOOL_{derived}")
        } else {
            derived
        };
        let name = name.unwrap_or(&derived);
        ensure!(
            valid_variable(name),
            "--name must be a valid variable prefix (letters, digits, underscores; no leading digit)"
        );
        // Refuse partial numeric matches: 1.2.3 is not the version in 1.2.30.
        let tokens = regex::Regex::new(&format!(
            "{}|{}",
            regex::escape(tag),
            regex::escape(version)
        ))?;
        let mut template = String::new();
        let mut end = 0;
        for token in tokens.find_iter(asset) {
            let start = token.start();
            let after = token.end();
            if asset.as_bytes().get(after).is_some_and(u8::is_ascii_digit)
                || (asset.as_bytes().get(after) == Some(&b'.')
                    && asset
                        .as_bytes()
                        .get(after + 1)
                        .is_some_and(u8::is_ascii_digit))
                || (start > 0 && asset.as_bytes()[start - 1].is_ascii_digit())
            {
                continue;
            }
            template.push_str(&asset[end..start]);
            template.push_str(if token.as_str() == tag && tag != version {
                "{tag}"
            } else {
                "{version}"
            });
            end = after;
        }
        template.push_str(&asset[end..]);
        if let Some(explicit) = asset_template {
            ensure!(
                safe_filename(explicit),
                "--asset-template must be a safe release asset filename or template"
            );
            template = explicit.to_string();
        }
        ensure!(
            expand_asset_template(&template, version, tag)? == *asset,
            "asset template must expand exactly to the URL asset {asset}"
        );
        if let Some(value) = checksums {
            ensure!(
                safe_filename(value),
                "--checksums must be a safe release asset filename or template"
            );
            let manifest = expand_asset_template(value, version, tag)?;
            ensure!(
                manifest != *asset,
                "--checksums must select a different release asset from the binary being pinned"
            );
        }
        let package = format!("{}/{}", parts[0], parts[1]);
        Ok(Self {
            package,
            version: version.into(),
            tag: tag.clone(),
            asset: asset.clone(),
            asset_template: template,
            name: name.into(),
            syntax,
            checksums: checksums.map(str::to_string),
        })
    }

    /// Offline generation: this records the SHA as supplied, without authenticating it.
    pub fn with_checksum(self, checksum: &str) -> Result<Scaffold> {
        self.finish(checksum, "supplied".into())
    }

    /// Query only the URL's exact tag and asset. No latest-version lookup, binary
    /// download, guessed sidecar, or fallback from missing metadata.
    pub async fn resolve(self, registry: &dyn crate::registry::Registry) -> Result<Scaffold> {
        let manifest = self
            .checksums
            .as_deref()
            .map(|template| expand_asset_template(template, &self.version, &self.tag))
            .transpose()?;
        let result = crate::updater::resolve_release_checksum(
            registry,
            &self.package,
            &self.tag,
            &self.asset,
            manifest.as_deref(),
        )
        .await;
        let resolved = match result {
            Err(error)
                if self.checksums.is_none()
                    && error
                        .chain()
                        .any(|cause| cause.to_string().contains("SHA-256 digest unavailable")) =>
            {
                return Err(error.context("published digest unavailable; use --resolve-checksum --checksums <release manifest filename>, or supply --checksum <published SHA-256> offline"));
            }
            result => result?,
        };
        self.finish(&resolved.sha256, resolved.source)
    }

    fn finish(self, checksum: &str, checksum_source: String) -> Result<Scaffold> {
        ensure!(
            checksum.len() == 64 && checksum.bytes().all(|b| b.is_ascii_hexdigit()),
            "--checksum must contain exactly 64 hexadecimal digits from the published asset checksum"
        );
        let Self {
            package,
            version,
            tag,
            asset,
            asset_template: template,
            name,
            syntax,
            checksums,
        } = self;
        let manifest = checksums
            .map(|value| format!(" checksums={value}"))
            .unwrap_or_default();
        let version_name = format!("{name}_VERSION");
        let checksum_name = format!("{name}_CHECKSUM");
        let version_comment = format!("upd: github-releases {package}");
        let checksum_comment = format!("upd: checksum {version_name} asset={template}{manifest}");
        let checksum = checksum.to_ascii_lowercase();
        let snippet = match syntax {
            SnippetSyntax::Shell => format!(
                "{version_name}={version} # {version_comment}\n{checksum_name}={checksum} # {checksum_comment}\n"
            ),
            SnippetSyntax::Docker => format!(
                "# {version_comment}\nARG {version_name}={version}\n\n# {checksum_comment}\nARG {checksum_name}={checksum}\n"
            ),
            SnippetSyntax::Toml => format!(
                "{version_name} = \"{version}\" # {version_comment}\n{checksum_name} = \"{checksum}\" # {checksum_comment}\n"
            ),
            SnippetSyntax::Yaml => format!(
                "{version_name}: \"{version}\" # {version_comment}\n{checksum_name}: \"{checksum}\" # {checksum_comment}\n"
            ),
            SnippetSyntax::Javascript => format!(
                "const {version_name} = \"{version}\"; // {version_comment}\nconst {checksum_name} = \"{checksum}\"; // {checksum_comment}\n"
            ),
        };
        let file_type = if matches!(syntax, SnippetSyntax::Docker) {
            FileType::Dockerfile
        } else {
            FileType::Annotated
        };
        let validation = validate_annotations(&snippet, file_type);
        ensure!(
            validation.diagnostics.is_empty()
                && validation.versions == 1
                && validation.checksums == 1,
            "generated annotations failed validation: {:?}",
            validation.diagnostics
        );
        Ok(Scaffold {
            command: "annotations init",
            package,
            version,
            tag,
            asset,
            asset_template: template,
            checksum,
            checksum_source,
            snippet,
        })
    }
}
