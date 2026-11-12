//! Onboarding uses repository API edits only; it never runs project code.
use super::{
    Error,
    api::Client,
    org::{OrgSettings, Project, State},
    run::Outcome,
};
const BRANCH: &str = "automation/upd-onboarding";

pub(super) async fn project(
    settings: &OrgSettings,
    api: &Client,
    project: &Project,
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
    if let Some(package) = api.raw_file(project.id, "package.json", &base).await? {
        let package: serde_json::Value = serde_json::from_str(&package).map_err(|_| {
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
    let source = api.raw_file(project.id, "renovate.json", &base).await?;
    let mut migration = crate::migrate::renovate(source.as_deref().unwrap_or("{}"))
        .map_err(|e| Error::Input(e.to_string()))?;
    if !migration.allowed(settings.allow_dropped) {
        return Err(Error::Refused(format!(
            "migration needs manual review{}:\n{}",
            if settings.allow_dropped {
                ""
            } else {
                " (--allow-dropped=false disallows policy losses)"
            },
            migration.report()
        )));
    }
    let mut config: toml::Value =
        toml::from_str(&migration.config).map_err(|e| Error::Io(e.to_string()))?;
    let table = config.as_table_mut().expect("generated config is a table");
    table
        .entry("automation")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .unwrap()
        .insert("dependency_updates".into(), toml::Value::Boolean(true));
    migration.config = toml::to_string_pretty(&config).map_err(|e| Error::Io(e.to_string()))?;
    let migration_report = if migration.diagnostics.is_empty() {
        if source.is_some() {
            "Renovate configuration contains no settings; nothing was migrated.\n".into()
        } else {
            "No Renovate configuration was found; nothing was migrated.\n".into()
        }
    } else {
        migration.report()
    };
    if settings.dry_run {
        return Ok(done(
            format!(
                "would open invitation with .updrc.toml{}\n{}\n{}",
                if source.is_some() {
                    " and remove renovate.json"
                } else {
                    ""
                },
                migration.config,
                migration_report
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
        "Opt this project in to organization dependency updates. Review the migrated ignores and automation settings before merging. Close this invitation to decline; upd will not reopen it. Lockfile regeneration needs the project CI template or the organization's isolated lock jobs.\n\nNo project code ran during onboarding.\n\n{report}"
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
