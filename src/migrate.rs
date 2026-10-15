//! Conservative Renovate migration: only unconditional, exact-name rules are portable.
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct Migration {
    pub config: String,
    pub warnings: Vec<String>,
}

pub fn renovate(content: &str) -> Result<Migration> {
    let value: Value = serde_json::from_str(content)
        .context("Renovate config must be JSON; JSON5 and inherited presets need manual review")?;
    let object = value
        .as_object()
        .context("Renovate config must be an object")?;
    let mut warnings = Vec::new();
    let mut ignore = BTreeSet::new();
    let mut automation = toml::map::Map::new();
    for (key, value) in object {
        match key.as_str() {
            "$schema" => {}
            "automerge" if value.is_boolean() => {
                automation.insert(
                    "auto_merge".into(),
                    toml::Value::Boolean(value.as_bool().unwrap()),
                );
            }
            "lockFileMaintenance"
                if value.as_object().is_some_and(|o| {
                    o.len() == 1 && o.get("enabled").is_some_and(Value::is_boolean)
                }) =>
            {
                automation.insert(
                    "lock".into(),
                    toml::Value::Boolean(value["enabled"].as_bool().unwrap()),
                );
            }
            "packageRules" if value.is_array() => {
                for (i, rule) in value.as_array().unwrap().iter().enumerate() {
                    let translated = rule.as_object().is_some_and(|r| {
                        r.len() == 2
                            && r.get("enabled") == Some(&Value::Bool(false))
                            && r.get("matchPackageNames")
                                .and_then(Value::as_array)
                                .is_some_and(|names| {
                                    !names.is_empty()
                                        && names.iter().all(|v| v.as_str().is_some_and(exact_name))
                                })
                    });
                    if translated {
                        for name in rule["matchPackageNames"].as_array().unwrap() {
                            ignore.insert(name.as_str().unwrap().to_owned());
                        }
                    } else {
                        warnings.push(format!("packageRules[{i}] cannot be translated; review its selectors and actions manually"));
                    }
                }
            }
            _ => warnings.push(format!("{key} cannot be translated; review it manually")),
        }
    }
    let mut config = toml::map::Map::new();
    config.insert(
        "ignore".into(),
        toml::Value::Array(ignore.into_iter().map(toml::Value::String).collect()),
    );
    if !automation.is_empty() {
        config.insert("automation".into(), toml::Value::Table(automation));
    }
    Ok(Migration {
        config: toml::to_string_pretty(&config)?,
        warnings,
    })
}

fn exact_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@/._-".contains(c))
}

/// Never replace an existing upd config or remove the original Renovate config.
pub fn migrate_path(path: &Path, dry_run: bool) -> Result<Migration> {
    let source = if path.is_dir() {
        path.join("renovate.json")
    } else {
        path.to_owned()
    };
    let directory = source.parent().unwrap_or(Path::new("."));
    for name in crate::config::CONFIG_FILE_NAMES {
        if directory.join(name).exists() {
            bail!("{name} already exists; refusing to overwrite project policy");
        }
    }
    let result = renovate(
        &std::fs::read_to_string(&source)
            .with_context(|| format!("cannot read {}", source.display()))?,
    )?;
    if !dry_run {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(".updrc.toml"))?;
        file.write_all(result.config.as_bytes())?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migrates_exact_ignores_and_boolean_automation() {
        let migration = renovate(r#"{"packageRules":[{"matchPackageNames":["numpy","example-sdk","example-deployer"],"enabled":false}],"automerge":true,"lockFileMaintenance":{"enabled":true}}"#).unwrap();
        let (config, warnings) =
            crate::config::UpdConfig::parse_for_automation(&migration.config, "migration").unwrap();
        assert!(warnings.is_empty() && migration.warnings.is_empty());
        assert_eq!(config.ignore, ["example-deployer", "example-sdk", "numpy"]);
        assert_eq!(config.automation.auto_merge, Some(true));
        assert_eq!(config.automation.lock, Some(true));
        assert_eq!(config.automation.dependency_updates, None);
    }
    #[test]
    fn conditional_rules_patterns_presets_and_unknowns_are_reported() {
        let migration = renovate(r#"{"extends":["config:recommended"],"packageRules":[{"matchPackageNames":["numpy"],"matchManagers":["pip_requirements"],"enabled":false},{"matchPackageNames":["aws-*"],"enabled":false}],"schedule":["weekends"]}"#).unwrap();
        assert_eq!(migration.warnings.len(), 4);
        assert!(migration.config.contains("ignore = []"));
    }
}
