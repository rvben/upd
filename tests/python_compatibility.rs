use serde_json::json;
use std::sync::{Arc, Mutex};
use upd::cache::{Cache, CachedRegistry};
use upd::registry::{MultiPyPiRegistry, PyPiRegistry, Registry};
use upd::updater::{PyProjectUpdater, RequirementsUpdater, UpdateOptions, Updater};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

async fn index(files: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/simple/demo/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            json!({"files": files}).to_string(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&server)
        .await;
    server
}

async fn project(
    registry: &dyn Registry,
    requirement: &str,
    dependency: &str,
    options: UpdateOptions,
) -> (upd::updater::UpdateResult, String) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pyproject.toml");
    let text =
        format!("[project]\nrequires-python = '{requirement}'\ndependencies = ['{dependency}']\n");
    std::fs::write(&file, &text).unwrap();
    let result = PyProjectUpdater::new()
        .update(&file, registry, options)
        .await
        .unwrap();
    (result, std::fs::read_to_string(file).unwrap())
}

#[tokio::test]
async fn dependency_python_caps_do_not_downgrade_loguru_or_reject_aws_sso_util() {
    let server = MockServer::start().await;
    for (package, files) in [
        (
            "loguru",
            json!([
                {"filename":"loguru-0.7.2.tar.gz", "requires-python":">=3.5"},
                {"filename":"loguru-0.7.3.tar.gz", "requires-python":"<4.0,>=3.5"}
            ]),
        ),
        (
            "aws-sso-util",
            json!([
                {"filename":"aws_sso_util-4.32.0.tar.gz", "requires-python":">=3.7,<4.0"},
                {"filename":"aws_sso_util-4.33.0.tar.gz", "requires-python":"<4.0,>=3.7"}
            ]),
        ),
    ] {
        Mock::given(path(format!("/simple/{package}/")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                json!({"files": files}).to_string(),
                "application/vnd.pypi.simple.v1+json",
            ))
            .mount(&server)
            .await;
    }
    let registry = PyPiRegistry::with_index_url(server.uri());
    for (dependency, expected) in [
        ("loguru==0.7.2", "loguru==0.7.3"),
        ("loguru==0.7.3", "loguru==0.7.3"),
        ("aws_sso_util==4.32.0", "aws_sso_util==4.33.0"),
    ] {
        let (result, text) =
            project(&registry, ">=3.12", dependency, UpdateOptions::default()).await;
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert!(text.contains(expected), "{text}");
    }
}

#[tokio::test]
async fn project_selection_isolated_from_cached_latest_and_other_projects() {
    let server = index(json!([
        {"filename":"demo-1.5.tar.gz", "requires-python":">=3.8"},
        {"filename":"demo-2.0.tar.gz", "requires-python":">=3.11"},
        {"filename":"demo-3.0.tar.gz", "requires-python":">=3.8", "yanked":true},
        {"filename":"demo-4.0rc1.tar.gz", "requires-python":">=3.8"}
    ]))
    .await;
    let registry = CachedRegistry::new(
        MultiPyPiRegistry::from_primary_and_extras(
            PyPiRegistry::with_index_url(server.uri()),
            vec![],
        ),
        Arc::new(Mutex::new(Cache::default())),
        true,
    );
    assert_eq!(registry.get_latest_version("demo").await.unwrap(), "2.0");
    let (older, newer) = tokio::join!(
        project(&registry, ">=3.10", "demo==1.0", UpdateOptions::default()),
        project(&registry, ">=3.11", "demo==1.0", UpdateOptions::default())
    );
    assert!(older.0.errors.is_empty(), "{:?}", older.0.errors);
    assert!(older.1.contains("demo==1.5"), "{}", older.1);
    assert!(newer.1.contains("demo==2.0"), "{}", newer.1);
    let (_, prerelease) = project(
        &registry,
        ">=3.10",
        "demo==1.0rc1",
        UpdateOptions::default(),
    )
    .await;
    assert!(prerelease.contains("demo==4.0rc1"), "{prerelease}");
    let (_, constrained) = project(
        &registry,
        ">=3.11",
        "demo>=1.0,<2",
        UpdateOptions::default(),
    )
    .await;
    assert!(constrained.contains("demo>=1.5,<2"), "{constrained}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "one ordinary lookup and one shared metadata lookup"
    );
}

#[tokio::test]
async fn html_private_index_and_nested_requirements_use_project_range() {
    let server = MockServer::start().await;
    Mock::given(path("/simple/demo/")).respond_with(ResponseTemplate::new(200).set_body_string(
        "<html><a href='demo-1.5.tar.gz' data-requires-python='&#62;=3.8'>old</a><a\n data-requires-python=\"&gt;=3.11\" href=\"demo-2.0.tar.gz\">new</a></html>"
    )).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nrequires-python = '>=3.10'\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("requirements")).unwrap();
    let file = dir.path().join("requirements/dev.txt");
    std::fs::write(
        &file,
        format!(
            "--index-url {}/simple\ndemo==1.0 ; python_version >= \"3.8\"\n",
            server.uri()
        ),
    )
    .unwrap();
    let result = RequirementsUpdater::new()
        .update(
            &file,
            &PyPiRegistry::with_index_url("http://127.0.0.1:1".into()),
            UpdateOptions::default(),
        )
        .await
        .unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("demo==1.5 ; python_version >= \"3.8\"")
    );
}

#[tokio::test]
async fn legacy_metadata_missing_metadata_and_no_compatible_release() {
    let server = MockServer::start().await;
    Mock::given(path("/pypi/demo/json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"releases": {
                "1.5": [{"requires_python": null}], "2.0": [{"requires_python": ">=3.11"}],
                "3.0": [{"requires_python": "invalid"}]
            }})),
        )
        .mount(&server)
        .await;
    let registry = PyPiRegistry::with_index_url(server.uri());
    let (result, text) = project(&registry, ">=3.10", "demo==1.0", UpdateOptions::default()).await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(text.contains("demo==1.5"));
    let server = index(json!([{"filename":"demo-2.0.tar.gz", "requires-python":">=3.11"}])).await;
    let (result, text) = project(
        &PyPiRegistry::with_index_url(server.uri()),
        ">=3.10",
        "demo==1.0",
        UpdateOptions::default(),
    )
    .await;
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.contains("Python requirement")),
        "{:?}",
        result.errors
    );
    assert!(text.contains("demo==1.0"));
}

#[tokio::test]
async fn cooldown_cannot_reintroduce_an_incompatible_release() {
    let server = index(json!([
        {"filename":"demo-1.5.tar.gz", "requires-python":">=3.8"},
        {"filename":"demo-2.0.tar.gz", "requires-python":">=3.11"},
        {"filename":"demo-3.0.tar.gz", "requires-python":">=3.8"}
    ]))
    .await;
    Mock::given(path("/pypi/demo/json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"releases": {
                "1.5": [{"upload_time_iso_8601":"2026-08-01T00:00:00Z"}],
                "2.0": [{"upload_time_iso_8601":"2026-08-01T00:00:00Z"}],
                "3.0": [{"upload_time_iso_8601":"2026-09-07T00:00:00Z"}]
            }})),
        )
        .mount(&server)
        .await;
    let options = UpdateOptions::default().with_cooldown_policy(
        upd::cooldown::CooldownPolicy {
            default: chrono::Duration::days(7),
            ..Default::default()
        },
        "2026-09-07T12:00:00Z".parse().unwrap(),
    );
    let (result, text) = project(
        &PyPiRegistry::with_index_url(server.uri()),
        ">=3.10",
        "demo==1.0",
        options,
    )
    .await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(text.contains("demo==1.5"), "{text}");
}

#[tokio::test]
async fn incompatible_primary_does_not_select_from_another_index() {
    let primary = index(json!([{"filename":"demo-2.0.tar.gz", "requires-python":">=3.11"}])).await;
    let secondary = index(json!([{"filename":"demo-9.0.tar.gz", "requires-python":">=3.8"}])).await;
    let registry = MultiPyPiRegistry::from_primary_and_extras(
        PyPiRegistry::with_index_url(primary.uri()),
        vec![secondary.uri()],
    );
    let (result, text) = project(&registry, ">=3.10", "demo==1.0", UpdateOptions::default()).await;
    assert!(!result.errors.is_empty());
    assert!(text.contains("demo==1.0"));
    assert!(secondary.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn poetry_projects_and_unconstrained_requirements() {
    let server = index(json!([
        {"filename":"demo-1.5.tar.gz", "requires-python":">=3.8"},
        {"filename":"demo-2.0.tar.gz", "requires-python":">=3.11"}
    ]))
    .await;
    let registry = PyPiRegistry::with_index_url(server.uri());
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("pyproject.toml");
    std::fs::write(
        &project,
        "[tool.poetry.dependencies]\npython = '^3.10'\ndemo = '1.0'\n",
    )
    .unwrap();
    let result = PyProjectUpdater::new()
        .update(&project, &registry, UpdateOptions::default())
        .await
        .unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let updated: toml::Value = toml::from_str(&std::fs::read_to_string(&project).unwrap()).unwrap();
    assert_eq!(
        updated["tool"]["poetry"]["dependencies"]["demo"].as_str(),
        Some("1.5")
    );
    std::fs::write(&project, "[project]\nname = 'no-python-constraint'\n").unwrap();
    let requirements = dir.path().join("requirements.txt");
    std::fs::write(&requirements, "demo==1.0\n").unwrap();
    let result = RequirementsUpdater::new()
        .update(&requirements, &registry, UpdateOptions::default())
        .await
        .unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(
        std::fs::read_to_string(requirements)
            .unwrap()
            .contains("demo==2.0")
    );
}

#[tokio::test]
async fn each_marker_branch_selects_its_own_version_and_reports_the_hold() {
    let server = index(json!([
        {"filename":"demo-1.5.tar.gz", "requires-python":">=3.8"},
        {"filename":"demo-2.0.tar.gz", "requires-python":">=3.11"}
    ]))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pyproject.toml");
    std::fs::write(
        &file,
        r#"[project]
requires-python = ">=3.10"
dependencies = [
  'demo==1.0 ; python_version < "3.11"',
  'demo==1.0 ; python_version >= "3.11"',
]
"#,
    )
    .unwrap();
    let result = PyProjectUpdater::new()
        .update(
            &file,
            &PyPiRegistry::with_index_url(server.uri()),
            UpdateOptions::default(),
        )
        .await
        .unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let updated = std::fs::read_to_string(file).unwrap();
    assert!(
        updated.contains(r#"demo==1.5 ; python_version < "3.11""#),
        "{updated}"
    );
    assert!(
        updated.contains(r#"demo==2.0 ; python_version >= "3.11""#),
        "{updated}"
    );
    assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
    let note = &result.warnings[0];
    assert!(
        note.contains("selects 1.5 instead of 2.0")
            && note.contains("Requires-Python '>=3.11'")
            && note.contains("project supports >=3.10")
            && note.contains("dependency marker"),
        "{note}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn requirement_markers_preserve_comments_and_cooldown_uses_the_narrowed_range() {
    let server = index(json!([
        {"filename":"demo-1.5.tar.gz", "requires-python":">=3.8"},
        {"filename":"demo-2.0.tar.gz", "requires-python":">=3.11"},
        {"filename":"demo-3.0.tar.gz", "requires-python":">=3.11"}
    ]))
    .await;
    Mock::given(path("/pypi/demo/json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"releases": {
                "1.5": [{"upload_time_iso_8601":"2026-08-01T00:00:00Z"}],
                "2.0": [{"upload_time_iso_8601":"2026-08-01T00:00:00Z"}],
                "3.0": [{"upload_time_iso_8601":"2026-09-07T00:00:00Z"}]
            }})),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nrequires-python = '>=3.10'\n",
    )
    .unwrap();
    let file = dir.path().join("requirements.txt");
    std::fs::write(
        &file,
        "demo==1.0 ; python_version >= '3.11' # modern Python\n",
    )
    .unwrap();
    let options = UpdateOptions::default().with_cooldown_policy(
        upd::cooldown::CooldownPolicy {
            default: chrono::Duration::days(7),
            ..Default::default()
        },
        "2026-09-07T12:00:00Z".parse().unwrap(),
    );
    let result = RequirementsUpdater::new()
        .update(&file, &PyPiRegistry::with_index_url(server.uri()), options)
        .await
        .unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        "demo==2.0 ; python_version >= '3.11' # modern Python\n"
    );
}

#[tokio::test]
async fn inactive_and_invalid_markers_leave_dependencies_unchanged_without_lookups() {
    let server = index(json!([])).await;
    for (marker, invalid) in [
        ("python_version < \"3.10\"", false),
        ("python_version >=", true),
        ("python_version >= \"nonsense\"", true),
    ] {
        let (result, text) = project(
            &PyPiRegistry::with_index_url(server.uri()),
            ">=3.10",
            &format!("demo==1.0 ; {marker}"),
            UpdateOptions::default(),
        )
        .await;
        assert_eq!(
            !result.errors.is_empty(),
            invalid,
            "{marker}: {:?}",
            result.errors
        );
        assert!(text.contains("demo==1.0"));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn normalization_respects_markers_for_unversioned_dependencies() {
    let server = index(json!([
        {"filename":"demo-1.5.tar.gz", "requires-python":">=3.8"},
        {"filename":"demo-2.0.tar.gz", "requires-python":">=3.11"}
    ]))
    .await;
    let config =
        toml::from_str::<upd::config::UpdConfig>("[normalize.pyproject]\ndependencies = 'exact'\n")
            .unwrap();
    let options = UpdateOptions::default().with_config(Arc::new(config));
    let (result, text) = project(
        &PyPiRegistry::with_index_url(server.uri()),
        ">=3.10",
        "demo; python_version >= \"3.11\"",
        options.clone(),
    )
    .await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(
        text.contains("demo==2.0; python_version >= \"3.11\""),
        "{text}"
    );
    let (result, text) = project(
        &PyPiRegistry::with_index_url(server.uri()),
        ">=3.10",
        "demo; python_version < \"3.10\"",
        options,
    )
    .await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.normalized.is_empty());
    assert!(text.contains("demo; python_version < \"3.10\""));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn compatibility_holds_appear_in_json_even_when_nothing_can_update() {
    let server = index(json!([
        {"filename":"demo-1.5.tar.gz", "requires-python":">=3.8"},
        {"filename":"demo-2.0.tar.gz", "requires-python":">=3.11"}
    ]))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pyproject.toml");
    let content = "[project]\nrequires-python = '>=3.10'\ndependencies = ['demo==1.5']\n";
    std::fs::write(&file, content).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .env("UV_INDEX_URL", server.uri())
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .args([
            "--dry-run",
            "--format",
            "json",
            "--no-cache",
            "pyproject.toml",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let warnings = report["files"][0]["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|s| s.contains("selects 1.5 instead of 2.0")
                && s.contains("project supports >=3.10"))),
        "{report}"
    );
    assert_eq!(std::fs::read_to_string(file).unwrap(), content);
}
