//! Renovate migration with explicit reporting of translations and policy losses.
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Translated,
    Dropped,
    Blocking,
}

#[derive(Debug, Serialize)]
pub struct Diagnostic {
    pub path: String,
    pub classification: Classification,
    pub explanation: String,
}

#[derive(Debug, Serialize)]
pub struct Migration {
    pub config: String,
    pub diagnostics: Vec<Diagnostic>,
}

impl Migration {
    pub fn allowed(&self, allow_dropped: bool) -> bool {
        self.diagnostics
            .iter()
            .all(|item| match item.classification {
                Classification::Translated => true,
                Classification::Dropped => allow_dropped,
                Classification::Blocking => false,
            })
    }

    /// Plain text, also embedded as indented code in onboarding invitations.
    pub fn report(&self) -> String {
        self.diagnostics
            .iter()
            .map(|item| {
                let kind = match item.classification {
                    Classification::Translated => "translated",
                    Classification::Dropped => "dropped",
                    Classification::Blocking => "blocking",
                };
                format!(
                    "{kind}: {}: {}\n",
                    item.path.escape_debug(),
                    item.explanation
                )
            })
            .collect()
    }
}

fn note(
    items: &mut Vec<Diagnostic>,
    path: &str,
    classification: Classification,
    explanation: &str,
) {
    items.push(Diagnostic {
        path: path.into(),
        classification,
        explanation: explanation.into(),
    });
}

pub fn renovate(content: &str) -> Result<Migration> {
    use Classification::{Blocking, Dropped, Translated};
    let value: Value = serde_json::from_str(content)
        .context("Renovate config must be JSON; JSON5 needs manual review")?;
    let object = value
        .as_object()
        .context("Renovate config must be an object")?;
    let mut diagnostics = Vec::new();
    let mut ignore = BTreeSet::new();
    let mut automation = toml::map::Map::new();
    // Evaluate inherited rule actions against the global value, independent of key order.
    // Known presets do not grant automerge; absent global consent defaults to false.
    let auto_merge = object
        .get("automerge")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    for (key, value) in object {
        match key.as_str() {
            "$schema" if value.is_string() => note(
                &mut diagnostics,
                key,
                Dropped,
                "editor schema metadata is not needed by upd",
            ),
            "automerge" | "gitLabIgnoreApprovals" if value.is_boolean() => {
                let target = if key == "automerge" {
                    "auto_merge"
                } else {
                    "ignore_approvals"
                };
                automation.insert(
                    target.into(),
                    toml::Value::Boolean(value.as_bool().unwrap()),
                );
                note(
                    &mut diagnostics,
                    key,
                    Translated,
                    &format!("automation.{target}; organization settings must also permit it"),
                );
            }
            "extends" if value.is_array() => {
                for (i, preset) in value.as_array().unwrap().iter().enumerate() {
                    let path = format!("extends[{i}]");
                    match preset.as_str() {
                        Some("config:base" | "config:recommended") => note(
                            &mut diagnostics,
                            &path,
                            Dropped,
                            "standard preset replaced by upd defaults; Renovate grouping, path exclusions, replacements and workarounds are not preserved",
                        ),
                        Some("config:best-practices") => note(
                            &mut diagnostics,
                            &path,
                            Dropped,
                            "standard preset replaced by upd defaults; grouping, exclusions, workarounds, digest/version pinning, release-age policy and weekly lock maintenance are not preserved",
                        ),
                        _ => note(
                            &mut diagnostics,
                            &path,
                            Blocking,
                            "custom, unknown or malformed preset needs manual translation",
                        ),
                    }
                }
                if value.as_array().unwrap().is_empty() {
                    note(
                        &mut diagnostics,
                        key,
                        Dropped,
                        "empty preset list has no effect",
                    );
                }
            }
            "automergeType" if value.as_str() == Some("pr") => note(
                &mut diagnostics,
                key,
                Dropped,
                "upd uses merge requests; Renovate PR merge mechanics are replaced by upd's platform auto-merge",
            ),
            "platformAutomerge" if value.is_boolean() => note(
                &mut diagnostics,
                key,
                Dropped,
                "upd uses platform auto-merge when permitted; Renovate's choice of merge mechanism is not preserved",
            ),
            "separateMinorPatch" if value.is_boolean() => note(
                &mut diagnostics,
                key,
                Dropped,
                "upd combines ordinary dependency updates in a rolling merge request; separate minor and patch branches are not preserved",
            ),
            "rebaseWhen" if valid_rebase(value) => note(
                &mut diagnostics,
                key,
                Dropped,
                "upd manages its rolling branch; Renovate's rebase timing is not preserved",
            ),
            "lockFileMaintenance" if value.is_object() => {
                let lock = value.as_object().unwrap();
                if lock.is_empty() {
                    note(
                        &mut diagnostics,
                        key,
                        Dropped,
                        "empty lock maintenance settings do not enable lock regeneration",
                    );
                }
                for (field, value) in lock {
                    let path = format!("{key}.{field}");
                    match field.as_str() {
                        "enabled" if value.is_boolean() => {
                            automation.insert(
                                "lock".into(),
                                toml::Value::Boolean(value.as_bool().unwrap()),
                            );
                            note(
                                &mut diagnostics,
                                &path,
                                Translated,
                                "automation.lock controls lock regeneration; requires project CI or isolated organization lock jobs",
                            );
                            if value.as_bool() == Some(true) {
                                note(
                                    &mut diagnostics,
                                    key,
                                    Dropped,
                                    "upd regenerates locks during dependency updates; Renovate's standalone scheduled maintenance is not preserved",
                                );
                            }
                        }
                        "automerge" if value.as_bool() == Some(auto_merge) => note(
                            &mut diagnostics,
                            &path,
                            Dropped,
                            "agrees with global auto_merge; upd has no separate lock-maintenance merge permission",
                        ),
                        _ => note(
                            &mut diagnostics,
                            &path,
                            Blocking,
                            "unsupported lock-maintenance setting or conflicting merge permission needs manual translation",
                        ),
                    }
                }
            }
            "packageRules" if value.is_array() => {
                for (i, rule) in value.as_array().unwrap().iter().enumerate() {
                    translate_rule(rule, i, auto_merge, &mut ignore, &mut diagnostics);
                }
                if value.as_array().unwrap().is_empty() {
                    note(
                        &mut diagnostics,
                        key,
                        Dropped,
                        "empty rule list has no effect",
                    );
                }
            }
            _ => note(
                &mut diagnostics,
                key,
                Blocking,
                "unsupported setting or invalid value needs manual translation",
            ),
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
        diagnostics,
    })
}

fn valid_rebase(value: &Value) -> bool {
    matches!(
        value.as_str(),
        Some("auto" | "automerging" | "behind-base-branch" | "conflicted" | "never")
    )
}

fn translate_rule(
    rule: &Value,
    index: usize,
    auto_merge: bool,
    ignore: &mut BTreeSet<String>,
    items: &mut Vec<Diagnostic>,
) {
    use Classification::{Blocking, Dropped, Translated};
    let path = format!("packageRules[{index}]");
    let Some(rule) = rule.as_object() else {
        note(items, &path, Blocking, "rule must be an object");
        return;
    };
    let matcher = if rule.contains_key("matchPackagePatterns") {
        "matchPackagePatterns"
    } else {
        "matchPackageNames"
    };
    let ignore_rule = rule.get("enabled") == Some(&Value::Bool(false))
        && rule
            .keys()
            .all(|k| k == matcher || k == "enabled" || k == "rebaseWhen")
        && rule.get("rebaseWhen").is_none_or(valid_rebase)
        && rule
            .get(matcher)
            .and_then(Value::as_array)
            .is_some_and(|names| {
                !names.is_empty()
                    && names.iter().all(|name| {
                        name.as_str()
                            .is_some_and(|name| exact_name(name, matcher == "matchPackagePatterns"))
                    })
            });
    if ignore_rule {
        for name in rule[matcher].as_array().unwrap() {
            ignore.insert(name.as_str().unwrap().to_owned());
        }
        note(
            items,
            &format!("{path}.{matcher}"),
            if matcher == "matchPackagePatterns" {
                Dropped
            } else {
                Translated
            },
            if matcher == "matchPackagePatterns" {
                "converted to named ignores; legacy regex substring and wildcard matching is lost (numpy no longer ignores numpy-financial); upd normalizes case and runs of '.', '_' and '-'"
            } else {
                "ignore; upd normalizes case and runs of '.', '_' and '-' in package names"
            },
        );
        note(
            items,
            &format!("{path}.enabled"),
            Translated,
            "false becomes an unconditional ignore",
        );
        if rule.contains_key("rebaseWhen") {
            note(
                items,
                &format!("{path}.rebaseWhen"),
                Dropped,
                "ignored packages do not receive upd update branches",
            );
        }
        return;
    }
    let no_op = !rule.is_empty()
        && rule.keys().all(|key| {
            matches!(
                key.as_str(),
                "matchUpdateTypes" | "matchCurrentVersion" | "rebaseWhen" | "automerge"
            )
        })
        && rule.get("matchUpdateTypes").is_none_or(|value| {
            value.as_array().is_some_and(|types| {
                !types.is_empty()
                    && types.iter().all(|t| {
                        matches!(
                            t.as_str(),
                            Some(
                                "major"
                                    | "minor"
                                    | "patch"
                                    | "pin"
                                    | "digest"
                                    | "pinDigest"
                                    | "rollback"
                                    | "replacement"
                                    | "bump"
                                    | "lockFileMaintenance"
                                    | "lockfileUpdate"
                            )
                        )
                    })
            })
        })
        && rule
            .get("matchCurrentVersion")
            .is_none_or(|value| value.as_str().is_some_and(|s| !s.is_empty()))
        && rule.get("rebaseWhen").is_none_or(valid_rebase)
        && rule
            .get("automerge")
            .is_none_or(|value| value.as_bool() == Some(auto_merge));
    if no_op {
        for key in rule.keys() {
            note(
                items,
                &format!("{path}.{key}"),
                Dropped,
                match key.as_str() {
                    "automerge" => {
                        "agrees with global auto_merge; no conditional merge permission is lost"
                    }
                    "rebaseWhen" => {
                        "upd manages its rolling branch; Renovate's rebase timing is not preserved"
                    }
                    _ => {
                        "selector has no remaining action after redundant merge and rebase settings are removed"
                    }
                },
            );
        }
    } else {
        note(
            items,
            &path,
            Blocking,
            "unsupported selectors/actions, regex patterns, invalid values or conflicting automerge need manual translation; no ignores from this rule were applied",
        );
    }
}

fn exact_name(name: &str, legacy_pattern: bool) -> bool {
    // A leading slash introduces a Renovate regex, even without metacharacters.
    // Keep scoped/path-like package names, but reject regex delimiters and negation.
    let bytes = name.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    let Some(last) = bytes.last() else {
        return false;
    };
    (first.is_ascii_alphanumeric() || (!legacy_pattern && *first == b'@'))
        && last.is_ascii_alphanumeric()
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || if legacy_pattern { "._-" } else { "@/._-" }.contains(c)
        })
}

/// Never replace an existing upd config or remove the original Renovate config.
pub fn migrate_path(path: &Path, dry_run: bool) -> Result<Migration> {
    migrate_path_with_options(path, dry_run, true)
}

/// Blocked or strict-mode migrations only return a preview, never write policy.
pub fn migrate_path_with_options(
    path: &Path,
    dry_run: bool,
    allow_dropped: bool,
) -> Result<Migration> {
    let source = if path.is_dir() {
        path.join("renovate.json")
    } else {
        path.to_owned()
    };
    let parent = source.parent().unwrap_or(Path::new("."));
    // Conventional nested Renovate files configure the repository, not .github/.gitlab.
    let directory = if matches!(
        parent.file_name().and_then(|s| s.to_str()),
        Some(".github" | ".gitlab")
    ) && matches!(
        source.file_name().and_then(|s| s.to_str()),
        Some("renovate.json" | "renovate.jsonc" | "renovate.json5")
    ) {
        parent.parent().unwrap_or(Path::new("."))
    } else {
        parent
    };
    for name in crate::config::CONFIG_FILE_NAMES {
        if directory.join(name).exists() {
            bail!("{name} already exists; refusing to overwrite project policy");
        }
    }
    let result = renovate(
        &std::fs::read_to_string(&source)
            .with_context(|| format!("cannot read {}", source.display()))?,
    )?;
    if !dry_run && result.allowed(allow_dropped) {
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
    use serde_json::json;

    #[test]
    fn common_template_migrates_with_explicit_policy_losses() {
        let migration = renovate(include_str!("../tests/fixtures/renovate-common.json")).unwrap();
        assert!(migration.allowed(true));
        assert!(!migration.allowed(false));
        let (config, warnings) =
            crate::config::UpdConfig::parse_for_automation(&migration.config, "migration").unwrap();
        assert!(warnings.is_empty());
        assert_eq!(config.ignore, ["some-internal-pkg"]);
        assert_eq!(config.automation.auto_merge, Some(true));
        assert_eq!(config.automation.ignore_approvals, Some(true));
        assert_eq!(config.automation.lock, Some(true));
        assert_eq!(config.automation.dependency_updates, None);
        assert!(
            migration
                .report()
                .contains("legacy regex substring and wildcard matching is lost")
        );
        assert!(
            migration
                .report()
                .contains("packageRules[0].matchCurrentVersion")
        );
        assert!(!migration.report().contains("blocking:"));
    }

    #[test]
    fn exact_ignores_preserve_scoped_names_and_do_not_grant_merge_consent() {
        for enabled in [true, false] {
            let migration = renovate(&json!({"gitLabIgnoreApprovals":enabled,"packageRules":[{"matchPackageNames":["numpy","@scope/pkg"],"enabled":false}]}).to_string()).unwrap();
            assert!(migration.allowed(false));
            let (config, _) =
                crate::config::UpdConfig::parse_for_automation(&migration.config, "migration")
                    .unwrap();
            assert_eq!(config.ignore, ["@scope/pkg", "numpy"]);
            assert_eq!(config.automation.ignore_approvals, Some(enabled));
            assert!(!config.auto_merge_enabled());
        }
    }

    #[test]
    fn presets_are_losses_and_unknown_or_mixed_presets_block() {
        for preset in ["config:base", "config:recommended", "config:best-practices"] {
            let migration = renovate(&json!({"extends":[preset]}).to_string()).unwrap();
            assert!(migration.allowed(true));
            assert!(!migration.allowed(false));
        }
        for presets in [
            json!(["local>group/custom"]),
            json!(["config:recommended", "group:all"]),
            json!([null]),
            json!("config:recommended"),
        ] {
            assert!(
                !renovate(&json!({"extends":presets}).to_string())
                    .unwrap()
                    .allowed(true)
            );
        }
    }

    #[test]
    fn lock_settings_never_hide_unknown_keys_or_widen_merge_consent() {
        for source in [
            json!({"lockFileMaintenance":{"enabled":true,"schedule":["weekends"]}}),
            json!({"lockFileMaintenance":{"enabled":true,"automerge":true}}),
            json!({"automerge":true,"lockFileMaintenance":{"enabled":true,"automerge":false}}),
            json!({"lockFileMaintenance":{"enabled":"true"}}),
            json!({"lockFileMaintenance":{"enabled":true,"registryUrls":[]}}),
        ] {
            let migration = renovate(&source.to_string()).unwrap();
            assert!(!migration.allowed(true), "{source}");
            assert!(
                migration
                    .report()
                    .contains("blocking: lockFileMaintenance.")
            );
        }
        for enabled in [true, false] {
            let migration = renovate(
                &json!({"lockFileMaintenance":{"enabled":enabled,"automerge":false}}).to_string(),
            )
            .unwrap();
            assert!(migration.allowed(true));
            let (config, _) =
                crate::config::UpdConfig::parse_for_automation(&migration.config, "migration")
                    .unwrap();
            assert_eq!(config.automation.lock, Some(enabled));
            assert!(!config.auto_merge_enabled());
        }
    }

    #[test]
    fn unsupported_rules_are_atomic_and_never_add_ignores() {
        for rule in [
            json!({"matchPackagePatterns":["^@scope/"],"enabled":false}),
            json!({"matchPackageNames":["/numpy/"],"enabled":false}),
            json!({"matchPackageNames":["/numpy/i"],"enabled":false}),
            json!({"matchPackageNames":["numpy/"],"enabled":false}),
            json!({"matchPackageNames":["!numpy"],"enabled":false}),
            json!({"matchPackagePatterns":["."],"enabled":false}),
            json!({"matchPackagePatterns":["..."],"enabled":false}),
            json!({"matchPackagePatterns":["-"],"enabled":false}),
            json!({"matchPackagePatterns":["_"],"enabled":false}),
            json!({"matchPackagePatterns":["numpy","aws-.*"],"enabled":false}),
            json!({"matchPackageNames":["numpy"],"matchManagers":["pip_requirements"],"enabled":false}),
            json!({"matchPackageNames":["numpy"],"allowedVersions":"<2","enabled":false}),
            json!({"matchPackageNames":["numpy"],"matchPackagePatterns":["numpy"],"enabled":false}),
            json!({"matchPackagePatterns":[],"enabled":false}),
            json!({"matchPackagePatterns":[null],"enabled":false}),
            json!({"matchPackageNames":["numpy"],"enabled":"false"}),
            json!({"matchPackageNames":["numpy"],"enabled":true}),
            json!({}),
            json!(null),
        ] {
            let migration = renovate(&json!({"packageRules":[rule]}).to_string()).unwrap();
            assert!(!migration.allowed(true), "{rule}");
            assert!(migration.config.contains("ignore = []"), "{rule}");
        }
    }

    #[test]
    fn redundant_rules_use_the_global_default_and_validate_values() {
        for global in [None, Some(false), Some(true)] {
            let effective = global.unwrap_or(false);
            let mut source = json!({"packageRules":[{"matchUpdateTypes":["minor","patch"],"matchCurrentVersion":"!/^0/","automerge":effective,"rebaseWhen":"auto"}]});
            if let Some(global) = global {
                source["automerge"] = json!(global);
            }
            assert!(renovate(&source.to_string()).unwrap().allowed(true));
            source["packageRules"][0]["automerge"] = json!(!effective);
            assert!(!renovate(&source.to_string()).unwrap().allowed(true));
        }
        for rule in [
            json!({"matchUpdateTypes":"minor","rebaseWhen":"auto"}),
            json!({"matchUpdateTypes":["nonsense"],"rebaseWhen":"auto"}),
            json!({"matchCurrentVersion":false,"rebaseWhen":"auto"}),
            json!({"matchCurrentVersion":"!/^0/","rebaseWhen":"nonsense"}),
            json!({"matchUpdateTypes":["minor"],"automerge":"false"}),
        ] {
            assert!(
                !renovate(&json!({"packageRules":[rule]}).to_string())
                    .unwrap()
                    .allowed(true),
                "{rule}"
            );
        }
    }

    #[test]
    fn malformed_and_unrecognized_settings_remain_blocking() {
        for source in [
            json!({"automerge":"true"}),
            json!({"gitLabIgnoreApprovals":0}),
            json!({"automergeType":"branch"}),
            json!({"automergeType":"pr-comment"}),
            json!({"platformAutomerge":"false"}),
            json!({"separateMinorPatch":null}),
            json!({"rebaseWhen":"invalid"}),
            json!({"rebaseWhen":"always"}),
            json!({"$schema":false}),
            json!({"packageRules":{}}),
            json!({"lockFileMaintenance":false}),
            json!({"schedule":["weekends"]}),
            json!({"registryUrls":[]}),
            json!({"allowedVersions":"<2"}),
            json!({"groupName":"all"}),
        ] {
            assert!(
                !renovate(&source.to_string()).unwrap().allowed(true),
                "{source}"
            );
        }
        for source in ["[]", "null", "{ /* JSON5 */ }"] {
            assert!(renovate(source).is_err());
        }
    }

    #[test]
    fn supported_rebase_timings_and_new_update_type_can_be_dropped() {
        for timing in [
            "auto",
            "never",
            "conflicted",
            "behind-base-branch",
            "automerging",
        ] {
            let source = json!({"rebaseWhen":timing,"packageRules":[{"matchUpdateTypes":["lockfileUpdate"],"rebaseWhen":timing}]});
            let migration = renovate(&source.to_string()).unwrap();
            assert!(migration.allowed(true));
            assert!(!migration.allowed(false));
        }
        let migration = renovate(r#"{"lockFileMaintenance":{"enabled":true}}"#).unwrap();
        assert!(migration.allowed(true));
        assert!(!migration.allowed(false));
        assert!(
            migration
                .report()
                .contains("standalone scheduled maintenance is not preserved")
        );
    }
    #[test]
    fn setting_paths_cannot_inject_report_lines() {
        let migration = renovate(&json!({"unknown\ntranslated: fake":true}).to_string()).unwrap();
        assert_eq!(migration.report().lines().count(), 1);
        assert!(migration.report().starts_with("blocking:"));
    }
}
