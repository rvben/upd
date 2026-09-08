//! Adapt pre-commit's install arguments to the existing ecosystem updaters.
use super::*;
use crate::annotation::AnnotationSource;
use crate::updater::{CargoTomlUpdater, PackageJsonUpdater, PyProjectUpdater};
use anyhow::anyhow;

fn package_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

pub(super) fn supported(language: &str) -> bool {
    matches!(language, "python" | "node" | "rust")
}

pub(super) async fn update(
    value: &str,
    language: &str,
    registries: &RegistrySet,
    options: &UpdateOptions,
) -> Result<Option<(UpdateResult, String)>> {
    let dep = value.trim();
    let source = match language {
        "python" => AnnotationSource::PyPi,
        "node" => AnnotationSource::Npm,
        "rust" => AnnotationSource::Crates,
        _ => return Ok(None),
    };
    // Parse before requesting a registry: URLs, VCS refs, unpinned dependencies
    // and installer switches are intentionally left untouched.
    let (package, prefix, spec) = match language {
        "python" => {
            let Ok(requirement) = dep.parse::<pep508_rs::Requirement>() else {
                return Ok(None);
            };
            if !matches!(
                requirement.version_or_url,
                Some(pep508_rs::VersionOrUrl::VersionSpecifier(_))
            ) {
                return Ok(None);
            }
            let name_end = dep
                .find(|c: char| !(c.is_ascii_alphanumeric() || "-_.".contains(c)))
                .unwrap_or(dep.len());
            // Parenthesized requirements need a different coordinate mapping;
            // retain them rather than partially reading their specifier.
            if dep[..dep.find(';').unwrap_or(dep.len())].contains(['(', ')']) {
                return Ok(None);
            }
            (dep[..name_end].to_string(), String::new(), dep.to_string())
        }
        "node" => {
            let Some((name, spec)) = dep.rsplit_once('@') else {
                return Ok(None);
            };
            let valid_name = if let Some(scoped) = name.strip_prefix('@') {
                scoped
                    .split_once('/')
                    .is_some_and(|(scope, name)| package_name(scope) && package_name(name))
            } else {
                package_name(name)
            };
            if !valid_name
                || matches!(
                    crate::npm_range::classify(spec),
                    crate::npm_range::SpecShape::NoVersion
                        | crate::npm_range::SpecShape::Unsupported
                )
            {
                return Ok(None);
            }
            (name.to_string(), format!("{name}@"), spec.to_string())
        }
        "rust" => {
            let (cli, rest) = dep.strip_prefix("cli:").map_or(("", dep), |s| ("cli:", s));
            let Some((name, spec)) = rest.split_once(':') else {
                return Ok(None);
            };
            if !package_name(name) || spec.parse::<semver::VersionReq>().is_err() {
                return Ok(None);
            }
            (name.to_string(), format!("{cli}{name}:"), spec.to_string())
        }
        _ => return Ok(None),
    };
    let registry = registries.for_source(source)?;
    let (mut result, updated) = match source {
        AnnotationSource::PyPi => {
            PyProjectUpdater::new()
                .update_hook_requirement(&spec, registry, options)
                .await
        }
        AnnotationSource::Npm => {
            let content = serde_json::json!({"dependencies": { &package: &spec }}).to_string();
            let (result, updated) = PackageJsonUpdater::new()
                .update_content(content, registry, options.clone())
                .await?;
            let json: serde_json::Value = serde_json::from_str(&updated)?;
            (
                result,
                json["dependencies"][&package]
                    .as_str()
                    .ok_or_else(|| anyhow!("Missing updated npm requirement"))?
                    .to_string(),
            )
        }
        AnnotationSource::Crates => {
            CargoTomlUpdater::new()
                .update_hook_requirement(&package, &spec, registry, options)
                .await
        }
        _ => unreachable!(),
    };
    result.set_update_lang(source.lang());
    result
        .held_back_sources
        .extend((0..result.held_back.len()).map(|i| (i, source)));
    result
        .cooldown_skip_sources
        .extend((0..result.skipped_by_cooldown.len()).map(|i| (i, source)));
    result.entry_ecosystem.insert(package, source);
    let leading = value.len() - value.trim_start().len();
    let trailing = value.trim_end().len();
    Ok(Some((
        result,
        format!(
            "{}{prefix}{updated}{}",
            &value[..leading],
            &value[trailing..]
        ),
    )))
}

pub(super) fn relocate(result: &mut UpdateResult, line: Option<usize>, section: &str) {
    for (_, _, _, location) in result.updated.iter_mut().chain(&mut result.pinned) {
        *location = line;
    }
    for (_, _, location) in &mut result.ignored {
        *location = line;
    }
    for entry in &mut result.capped {
        entry.line_number = line;
    }
    for entry in &mut result.skipped {
        entry.line_number = line;
    }
    for context in result.update_context.values_mut() {
        context.section = Some(section.into());
    }
}
