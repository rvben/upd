use super::{FileType, UpdateResult, Updater};
use crate::registry::Registry;
use anyhow::Result;
use regex::Regex;
use std::fs;
use std::path::Path;

pub struct RequirementsUpdater {
    // Regex to match package specifications
    // Matches: package==1.0.0, package>=1.0.0, package[extra]==1.0.0, etc.
    package_re: Regex,
}

impl RequirementsUpdater {
    pub fn new() -> Self {
        // Match package name (with optional extras), operator, and version
        // Captures: 1=package_name, 2=extras (optional), 3=operator, 4=version
        let package_re = Regex::new(
            r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)(\[[^\]]+\])?\s*(==|>=|<=|~=|!=|>|<)\s*([^\s,;#]+)",
        )
        .expect("Invalid regex");

        Self { package_re }
    }

    fn parse_line(&self, line: &str) -> Option<(String, String, String, String)> {
        // Skip comments and empty lines
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            return None;
        }

        // Handle inline comments
        let code_part = line.split('#').next().unwrap_or(line);

        self.package_re.captures(code_part).map(|caps| {
            let package = caps.get(1).unwrap().as_str().to_string();
            let extras = caps.get(2).map_or("", |m| m.as_str()).to_string();
            let operator = caps.get(3).unwrap().as_str().to_string();
            let version = caps.get(4).unwrap().as_str().to_string();
            (package, extras, operator, version)
        })
    }

    fn update_line(&self, line: &str, new_version: &str) -> String {
        if let Some(caps) = self.package_re.captures(line) {
            let full_match = caps.get(0).unwrap();
            let package = caps.get(1).unwrap().as_str();
            let extras = caps.get(2).map_or("", |m| m.as_str());
            let operator = caps.get(3).unwrap().as_str();

            // Reconstruct the package spec with new version
            let new_spec = format!("{}{}{}{}", package, extras, operator, new_version);

            // Replace in original line to preserve trailing comments and whitespace
            let mut result = line.to_string();
            result.replace_range(full_match.range(), &new_spec);
            result
        } else {
            line.to_string()
        }
    }
}

impl Default for RequirementsUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for RequirementsUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        dry_run: bool,
    ) -> Result<UpdateResult> {
        let content = fs::read_to_string(path)?;
        let mut result = UpdateResult::default();
        let mut new_lines = Vec::new();
        let mut modified = false;

        for line in content.lines() {
            if let Some((package, _extras, _operator, current_version)) = self.parse_line(line) {
                match registry.get_latest_version(&package).await {
                    Ok(latest_version) => {
                        if latest_version != current_version {
                            result.updated.push((
                                package.clone(),
                                current_version.clone(),
                                latest_version.clone(),
                            ));
                            new_lines.push(self.update_line(line, &latest_version));
                            modified = true;
                        } else {
                            result.unchanged += 1;
                            new_lines.push(line.to_string());
                        }
                    }
                    Err(e) => {
                        result.errors.push(format!("{}: {}", package, e));
                        new_lines.push(line.to_string());
                    }
                }
            } else {
                new_lines.push(line.to_string());
            }
        }

        if modified && !dry_run {
            // Preserve original line ending style
            let line_ending = if content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };

            let mut new_content = new_lines.join(line_ending);

            // Preserve trailing newline if present
            if content.ends_with('\n') || content.ends_with("\r\n") {
                new_content.push_str(line_ending);
            }

            fs::write(path, new_content)?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::Requirements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_line() {
        let updater = RequirementsUpdater::new();

        let (pkg, extras, op, ver) = updater.parse_line("requests==2.28.0").unwrap();
        assert_eq!(pkg, "requests");
        assert_eq!(extras, "");
        assert_eq!(op, "==");
        assert_eq!(ver, "2.28.0");

        let (pkg, extras, _, ver) = updater.parse_line("uvicorn[standard]==0.20.0").unwrap();
        assert_eq!(pkg, "uvicorn");
        assert_eq!(extras, "[standard]");
        assert_eq!(ver, "0.20.0");

        let (pkg, _, op, ver) = updater.parse_line("django>=4.0.0").unwrap();
        assert_eq!(pkg, "django");
        assert_eq!(op, ">=");
        assert_eq!(ver, "4.0.0");

        assert!(updater.parse_line("# comment").is_none());
        assert!(updater.parse_line("").is_none());
        assert!(updater.parse_line("-r other.txt").is_none());
    }

    #[test]
    fn test_update_line() {
        let updater = RequirementsUpdater::new();

        assert_eq!(
            updater.update_line("requests==2.28.0", "2.31.0"),
            "requests==2.31.0"
        );

        assert_eq!(
            updater.update_line("requests==2.28.0  # HTTP library", "2.31.0"),
            "requests==2.31.0  # HTTP library"
        );

        assert_eq!(
            updater.update_line("uvicorn[standard]==0.20.0", "0.24.0"),
            "uvicorn[standard]==0.24.0"
        );
    }
}
