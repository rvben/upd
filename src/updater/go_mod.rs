use super::{FileType, UpdateResult, Updater};
use crate::registry::Registry;
use anyhow::Result;
use regex::Regex;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub struct GoModUpdater {
    // Matches module path and version in require statements
    // e.g., "github.com/foo/bar v1.2.3" or "github.com/foo/bar v1.2.3 // indirect"
    require_re: Regex,
    // Matches replace directives to identify modules we should skip
    replace_re: Regex,
}

impl GoModUpdater {
    pub fn new() -> Self {
        // Match: module_path version [// comment]
        // Version format: v1.2.3, v1.2.3-alpha, v1.2.3+incompatible
        let require_re =
            Regex::new(r"^\s*([\w./-]+)\s+(v\d+\.\d+\.\d+(?:-[\w.]+)?(?:\+incompatible)?)")
                .expect("Invalid require regex");

        // Match replace directives: replace old => new
        // or replace ( ... ) blocks
        let replace_re = Regex::new(r"^\s*([\w./-]+)\s+=>\s+").expect("Invalid replace regex");

        Self {
            require_re,
            replace_re,
        }
    }

    /// Parse go.mod content and extract modules that have replace directives
    fn find_replaced_modules(&self, content: &str) -> HashSet<String> {
        let mut replaced = HashSet::new();
        let mut in_replace_block = false;

        for line in content.lines() {
            let trimmed = line.trim();

            // Handle replace block
            if trimmed.starts_with("replace (") || trimmed == "replace (" {
                in_replace_block = true;
                continue;
            }

            if in_replace_block {
                if trimmed == ")" {
                    in_replace_block = false;
                    continue;
                }
                // Inside replace block: "module => replacement"
                if let Some(caps) = self.replace_re.captures(line) {
                    replaced.insert(caps.get(1).unwrap().as_str().to_string());
                }
                continue;
            }

            // Single-line replace: "replace module => replacement"
            if let Some(rest) = trimmed.strip_prefix("replace ")
                && let Some(caps) = self.replace_re.captures(rest)
            {
                replaced.insert(caps.get(1).unwrap().as_str().to_string());
            }
        }

        replaced
    }

    /// Check if a version is a pre-release
    fn is_prerelease(version: &str) -> bool {
        let stripped = version.strip_prefix('v').unwrap_or(version);
        // Remove +incompatible suffix for parsing
        let without_incompatible = stripped.split('+').next().unwrap_or(stripped);

        semver::Version::parse(without_incompatible)
            .map(|v| !v.pre.is_empty())
            .unwrap_or(false)
    }
}

impl Default for GoModUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for GoModUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        dry_run: bool,
    ) -> Result<UpdateResult> {
        let content = fs::read_to_string(path)?;
        let mut result = UpdateResult::default();

        // Find modules with replace directives (we'll skip these)
        let replaced_modules = self.find_replaced_modules(&content);

        // Process the file line by line
        let mut new_lines: Vec<String> = Vec::new();
        let mut in_require_block = false;

        for line in content.lines() {
            let trimmed = line.trim();

            // Track require block state
            if trimmed.starts_with("require (") || trimmed == "require (" {
                in_require_block = true;
                new_lines.push(line.to_string());
                continue;
            }

            if in_require_block && trimmed == ")" {
                in_require_block = false;
                new_lines.push(line.to_string());
                continue;
            }

            // Check if this line is a require statement (inside block or single-line)
            let is_require_line = in_require_block
                || (trimmed.starts_with("require ") && !trimmed.starts_with("require ("));

            if !is_require_line {
                new_lines.push(line.to_string());
                continue;
            }

            // Try to parse as a module requirement
            let line_to_parse = if in_require_block {
                line
            } else {
                // Single-line require: "require github.com/foo/bar v1.0.0"
                &line[line.find("require").map(|i| i + 7).unwrap_or(0)..]
            };

            if let Some(caps) = self.require_re.captures(line_to_parse) {
                let module = caps.get(1).unwrap().as_str();
                let current_version = caps.get(2).unwrap().as_str();

                // Skip replaced modules
                if replaced_modules.contains(module) {
                    new_lines.push(line.to_string());
                    result.unchanged += 1;
                    continue;
                }

                // Fetch latest version
                let version_result = if Self::is_prerelease(current_version) {
                    registry
                        .get_latest_version_including_prereleases(module)
                        .await
                } else {
                    registry.get_latest_version(module).await
                };

                match version_result {
                    Ok(latest_version) => {
                        if latest_version != current_version {
                            // Replace version in the line, preserving everything else
                            let new_line = line.replace(current_version, &latest_version);
                            new_lines.push(new_line);
                            result.updated.push((
                                module.to_string(),
                                current_version.to_string(),
                                latest_version,
                            ));
                        } else {
                            new_lines.push(line.to_string());
                            result.unchanged += 1;
                        }
                    }
                    Err(e) => {
                        new_lines.push(line.to_string());
                        result.errors.push(format!("{}: {}", module, e));
                    }
                }
            } else {
                new_lines.push(line.to_string());
            }
        }

        if !result.updated.is_empty() && !dry_run {
            // Preserve original line ending
            let line_ending = if content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let new_content = new_lines.join(line_ending);

            // Preserve trailing newline if original had one
            let final_content = if content.ends_with('\n') && !new_content.ends_with('\n') {
                format!("{}{}", new_content, line_ending)
            } else {
                new_content
            };

            fs::write(path, final_content)?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::GoMod
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_require_regex() {
        let updater = GoModUpdater::new();

        // Standard version
        let caps = updater.require_re.captures("\tgithub.com/foo/bar v1.2.3");
        assert!(caps.is_some());
        let caps = caps.unwrap();
        assert_eq!(caps.get(1).unwrap().as_str(), "github.com/foo/bar");
        assert_eq!(caps.get(2).unwrap().as_str(), "v1.2.3");

        // With +incompatible
        let caps = updater
            .require_re
            .captures("\tgithub.com/foo/bar v2.0.0+incompatible");
        assert!(caps.is_some());
        let caps = caps.unwrap();
        assert_eq!(caps.get(2).unwrap().as_str(), "v2.0.0+incompatible");

        // Pre-release
        let caps = updater
            .require_re
            .captures("\tgithub.com/foo/bar v1.0.0-alpha.1");
        assert!(caps.is_some());
        let caps = caps.unwrap();
        assert_eq!(caps.get(2).unwrap().as_str(), "v1.0.0-alpha.1");
    }

    #[test]
    fn test_find_replaced_modules() {
        let updater = GoModUpdater::new();

        let content = r#"
module example.com/mymodule

require (
    github.com/foo/bar v1.0.0
    github.com/baz/qux v2.0.0
)

replace github.com/foo/bar => github.com/myfork/bar v1.0.1

replace (
    github.com/old/lib => ../local
)
"#;

        let replaced = updater.find_replaced_modules(content);
        assert!(replaced.contains("github.com/foo/bar"));
        assert!(replaced.contains("github.com/old/lib"));
        assert!(!replaced.contains("github.com/baz/qux"));
    }

    #[test]
    fn test_is_prerelease() {
        assert!(!GoModUpdater::is_prerelease("v1.0.0"));
        assert!(!GoModUpdater::is_prerelease("v1.0.0+incompatible"));
        assert!(GoModUpdater::is_prerelease("v1.0.0-alpha.1"));
        assert!(GoModUpdater::is_prerelease("v1.0.0-rc1"));
        assert!(GoModUpdater::is_prerelease("v1.0.0-beta"));
    }
}
