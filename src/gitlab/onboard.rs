//! Onboarding uses repository API edits only; it never runs project code.
use super::{
    Error,
    api::Client,
    org::{OrgSettings, Project, State},
    run::{Log, Outcome},
};
const BRANCH: &str = "automation/upd-onboarding";

pub(super) async fn project(
    settings: &OrgSettings,
    api: &Client,
    project: &Project,
    log: &Log,
) -> Result<State, Error> {
    let done = |reason: String, merge_request: Option<String>| {
        State::Processed(Outcome::Onboarding {
            reason,
            merge_request,
        })
    };
    let api = api.for_project(project.id);
    let base = api.branch_head(project.id, &project.default_branch).await?;
    for name in crate::config::CONFIG_FILE_NAMES {
        if api.raw_file(project.id, name, &base).await?.is_some() {
            return Ok(done(
                format!("{name} already exists; project policy left for manual review"),
                None,
            ));
        }
    }
    let history = api.merge_request_history(BRANCH).await?;
    if let Some(previous) = history.first() {
        return Ok(done(
            format!(
                "invitation already {}",
                previous["state"].as_str().unwrap_or("exists")
            ),
            previous["web_url"].as_str().map(str::to_owned),
        ));
    }
    // Avoid deleting a configuration when another Renovate config or a preset could
    // still carry policy that this translator does not understand.
    for name in [
        "renovate.jsonc",
        "renovate.json5",
        ".renovaterc",
        ".renovaterc.json",
        ".renovaterc.jsonc",
        ".renovaterc.json5",
        ".github/renovate.json",
        ".github/renovate.jsonc",
        ".github/renovate.json5",
        ".gitlab/renovate.json",
        ".gitlab/renovate.jsonc",
        ".gitlab/renovate.json5",
    ] {
        if api.raw_file(project.id, name, &base).await?.is_some() {
            return Err(Error::Refused(format!(
                "{name} needs manual migration before onboarding"
            )));
        }
    }
    let mut warnings = Vec::new();
    if let Some(package) = api.raw_file(project.id, "package.json", &base).await? {
        let content = package.strip_prefix('\u{feff}').unwrap_or(&package);
        if content.trim().is_empty() {
            let warning = "warning: package.json is empty; no embedded Renovate policy was found";
            log.line(warning);
            warnings.push(warning);
        } else {
            let package: serde_json::Value = serde_json::from_str(content).map_err(|_| {
                Error::Refused(
                    "cannot inspect invalid package.json for embedded Renovate policy".into(),
                )
            })?;
            if package.get("renovate").is_some() {
                return Err(Error::Refused(
                    "package.json renovate settings need manual migration before onboarding".into(),
                ));
            }
        }
    }
    let source = api.raw_file(project.id, "renovate.json", &base).await?;
    let mut migration = crate::migrate::renovate(source.as_deref().unwrap_or("{}"))
        .map_err(|e| Error::Input(e.to_string()))?;
    if !migration.allowed(settings.allow_dropped) {
        use crate::migrate::Classification::{Blocking, Dropped};
        let blocked: Vec<_> = migration
            .diagnostics
            .iter()
            .filter(|item| item.classification == Blocking)
            .map(|item| item.path.escape_debug().to_string())
            .collect();
        let dropped: Vec<_> = migration
            .diagnostics
            .iter()
            .filter(|item| item.classification == Dropped)
            .map(|item| item.path.escape_debug().to_string())
            .collect();
        let mut reasons = Vec::new();
        if !blocked.is_empty() {
            reasons.push(format!("blocking: {}", blocked.join(", ")));
        }
        if !settings.allow_dropped && !dropped.is_empty() {
            reasons.push(format!(
                "dropped: {} (--allow-dropped=false)",
                dropped.join(", ")
            ));
        }
        return Err(Error::MigrationRefused {
            message: reasons.join("; "),
            diagnostics: migration.diagnostics,
        });
    }
    let mut config: toml::Value =
        toml::from_str(&migration.config).map_err(|e| Error::Io(e.to_string()))?;
    let table = config.as_table_mut().expect("generated config is a table");
    let automation = table
        .entry("automation")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .unwrap();
    automation.insert("dependency_updates".into(), toml::Value::Boolean(true));
    if !automation.contains_key("lock") {
        let files = api.repository_files(project.id, &base).await?;
        let locks = supported_lockfiles(&files);
        if !locks.is_empty() {
            automation.insert("lock".into(), toml::Value::Boolean(true));
            migration.diagnostics.push(crate::migrate::Diagnostic {
                path: "automation.lock".into(),
                classification: crate::migrate::Classification::Added,
                explanation: format!("proposed lock regeneration for {}; requires project CI or isolated organization lock jobs", locks.iter().map(|p| p.escape_debug().to_string()).collect::<Vec<_>>().join(", ")),
            });
        }
    }
    migration.config = toml::to_string_pretty(&config).map_err(|e| Error::Io(e.to_string()))?;
    let mut migration_report: String = if migration.diagnostics.is_empty() {
        if source.is_some() {
            "Renovate configuration contains no settings; nothing was migrated.\n".into()
        } else {
            "No Renovate configuration was found; nothing was migrated.\n".into()
        }
    } else {
        migration.report()
    };
    for warning in warnings {
        migration_report.push_str(&format!("{warning}\n"));
    }
    let lock_consent = if config["automation"]
        .get("lock")
        .and_then(toml::Value::as_bool)
        == Some(true)
    {
        "\n\nThis invitation proposes `automation.lock = true`: merging it consents to running project code during lock regeneration. Configure either `ci/gitlab-project-lock.yml` in this project or the organization's isolated lock jobs in `ci/gitlab-organization-update.yml`; the setting alone does not install either workflow."
    } else {
        ""
    };
    if settings.dry_run {
        return Ok(done(
            format!(
                "would open invitation with .updrc.toml{}\n{}\n{}{}",
                if source.is_some() {
                    " and remove renovate.json"
                } else {
                    ""
                },
                migration.config,
                migration_report,
                lock_consent
            ),
            None,
        ));
    }
    api.onboarding_commit(
        project.id,
        BRANCH,
        &base,
        &migration.config,
        source.is_some(),
    )
    .await?;
    // Indented Markdown code keeps source-controlled setting paths inert in the MR.
    let report = if migration.diagnostics.is_empty() {
        migration_report
    } else {
        let indented = migration_report
            .lines()
            .map(|line| format!("    {line}\n"))
            .collect::<String>();
        format!(
            "Migration report (dropped settings adopt upd behaviour and may change policy):\n\n{indented}"
        )
    };
    let description = format!(
        "Opt this project in to organization dependency updates. Review the migrated ignores and automation settings before merging. Close this invitation to decline; upd will not reopen it. Lockfile regeneration needs the project CI template or the organization's isolated lock jobs.\n\nNo project code ran during onboarding.{lock_consent}\n\n{report}"
    );
    let mr = api
        .create(
            BRANCH,
            &project.default_branch,
            "chore(deps): opt in to upd dependency updates",
            &description,
        )
        .await?;
    Ok(done(
        "invitation opened".into(),
        mr["web_url"].as_str().map(str::to_owned),
    ))
}

/// Propose consent only for regular lockfiles with a supported manifest beside them.
/// This is repository inspection, not execution or a promise that a lock runner is configured.
fn supported_lockfiles(files: &[String]) -> Vec<&str> {
    use crate::lockfile::LockfileType;
    let paths: std::collections::BTreeSet<_> = files.iter().map(String::as_str).collect();
    paths
        .iter()
        .copied()
        .filter(|path| {
            let (dir, name) = path
                .rsplit_once('/')
                .map_or(("", *path), |(dir, name)| (dir, name));
            let Some(kind) = LockfileType::from_filename(name) else {
                return false;
            };
            let beside = |name: &str| {
                if dir.is_empty() {
                    name.to_owned()
                } else {
                    format!("{dir}/{name}")
                }
            };
            match kind {
                LockfileType::DenoLock => {
                    paths.contains(beside("deno.json").as_str())
                        || paths.contains(beside("deno.jsonc").as_str())
                }
                LockfileType::PackagesLockJson | LockfileType::TerraformLock => {
                    paths.iter().any(|candidate| {
                        let (parent, name) = candidate.rsplit_once('/').unwrap_or(("", candidate));
                        parent == dir
                            && match kind {
                                LockfileType::PackagesLockJson => {
                                    name.ends_with(".csproj")
                                        || matches!(
                                            name,
                                            "Directory.Packages.props" | "Directory.Build.props"
                                        )
                                }
                                _ => name.ends_with(".tf"),
                            }
                    })
                }
                _ => paths.contains(beside(kind.manifest()).as_str()),
            }
        })
        .collect()
}
