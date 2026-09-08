use super::*;
use crate::annotation::AnnotationSource;
use crate::config::UpdConfig;
use crate::registry::{GitHubReleasesRegistry, MockRegistry};
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{path, query_param},
};

fn updater() -> PreCommitUpdater {
    PreCommitUpdater::with_registries(RegistrySet::with_sources(vec![
        (
            AnnotationSource::PyPi,
            Arc::new(
                MockRegistry::new("pypi")
                    .with_version("demo", "2.0.0")
                    .with_version("flake8-docstrings", "1.7.0")
                    .with_constrained("demo", ">=1.0.0, <2.0", "1.9.0"),
            ),
        ),
        (
            AnnotationSource::Npm,
            Arc::new(
                MockRegistry::new("npm")
                    .with_version("@scope/demo", "2.0.0")
                    .with_constrained("@scope/demo", "^1.0.0", "1.5.0"),
            ),
        ),
        (
            AnnotationSource::Crates,
            Arc::new(MockRegistry::new("crates.io").with_version("demo", "3.0.0")),
        ),
    ]))
}

async fn run(name: &str, content: &str, options: UpdateOptions) -> (UpdateResult, String) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(name);
    std::fs::write(&file, content).unwrap();
    let result = updater()
        .update(
            &file,
            &MockRegistry::new("github-releases").with_version("owner/repo", "v2.0.0"),
            options,
        )
        .await
        .unwrap();
    (result, std::fs::read_to_string(file).unwrap())
}

#[tokio::test]
async fn toml_revisions_preserve_exact_bytes_and_ignore_special_repositories() {
    let original = "# café\r\n[[repos]] # remote\r\nrepo = 'https://github.com/owner/repo.git'\r\nrev = \"v1.0.0\" # keep\r\nhooks = [{ id = 'test' }]\r\n[[repos]]\r\nrepo = 'local'\r\nrev = 'v1.0.0'\r\n[[repos]]\r\nrepo = 'meta'\r\n[[repos]]\r\nrepo = 'builtin'\r\n[[repos]]\r\nrepo = 'https://gitlab.com/owner/repo'\r\nrev = 'v1.0.0'";
    let (result, written) = run("prek.toml", original, UpdateOptions::new(false, false)).await;
    assert_eq!(result.updated.len(), 1);
    assert_eq!(result.updated[0].3, Some(4));
    assert_eq!(
        written,
        original.replacen("rev = \"v1.0.0\"", "rev = \"v2.0.0\"", 1)
    );
    let (result, written) = run("prek.toml", original, UpdateOptions::new(true, false)).await;
    assert_eq!(result.updated.len(), 1);
    assert_eq!(written, original);
}

#[tokio::test]
async fn yaml_flow_and_quoted_repository_preserve_structure() {
    let original = "# café\nrepos: [{rev: 'v1.0.0', repo: 'https://github.com/owner/repo', hooks: []}, {repo: https://github.com/owner/repo, rev: v1.0.0}]\n";
    let (result, written) = run(
        ".pre-commit-config.yaml",
        original,
        UpdateOptions::new(false, false),
    )
    .await;
    assert_eq!(result.updated.len(), 2);
    assert_eq!(written, original.replace("v1.0.0", "v2.0.0"));
}

#[tokio::test]
async fn all_three_languages_work_in_both_formats_without_changing_other_content() {
    for (name, original) in [
        (
            ".pre-commit-config.yaml",
            "# café\nrepos:\n- repo: local\n  hooks:\n  - id: python\n    language: python\n    additional_dependencies: ['flake8-docstrings==1.6.0', 'demo>=1.0.0, <2.0'] # keep\n  - id: node\n    language: node\n    additional_dependencies: ['@scope/demo@^1.0.0']\n  - id: rust\n    language: rust\n    additional_dependencies: ['cli:demo:1.0.0', 'demo:1.0.0']\n",
        ),
        (
            "prek.toml",
            "# café\n[[repos]]\nrepo = 'local'\nhooks = [\n  { id = 'python', language = 'python', additional_dependencies = ['flake8-docstrings==1.6.0', 'demo>=1.0.0, <2.0'] }, # keep\n  { id = 'node', language = 'node', additional_dependencies = ['@scope/demo@^1.0.0'] },\n  { id = 'rust', language = 'rust', additional_dependencies = ['cli:demo:1.0.0', 'demo:1.0.0'] },\n]\n",
        ),
    ] {
        let (result, written) = run(name, original, UpdateOptions::new(false, false)).await;
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.updated.len(), 5, "{result:?}");
        assert_eq!(
            written,
            original
                .replace("flake8-docstrings==1.6.0", "flake8-docstrings==1.7.0")
                .replace("demo>=1.0.0", "demo>=1.9.0")
                .replace("@scope/demo@^1.0.0", "@scope/demo@^1.5.0")
                .replace("demo:1.0.0", "demo:3.0.0")
        );
        assert!(result.updated.iter().all(|e| e.3.is_some()));
        let (preview, written) = run(name, original, UpdateOptions::new(true, false)).await;
        assert_eq!(preview.updated.len(), 5);
        assert_eq!(written, original);
    }
}

#[tokio::test]
async fn package_filters_pins_ignores_and_bump_limits_apply_to_hooks_and_revisions() {
    let original = "[[repos]]\nrepo = 'https://github.com/owner/repo'\nrev = 'v1.0.0'\nhooks = [{ id = 'python', language = 'python', additional_dependencies = ['demo==1.0.0', 'flake8-docstrings==1.6.0'] }]\n";
    let options = UpdateOptions::new(false, false).with_packages(vec!["flake8-*".into()]);
    let (result, written) = run("prek.toml", original, options).await;
    assert_eq!(result.updated.len(), 1);
    assert_eq!(written, original.replace("1.6.0", "1.7.0"));
    let mut options = UpdateOptions::new(false, false);
    options.bump_filter.major = false;
    let (result, written) = run("prek.toml", original, options.clone()).await;
    assert_eq!(result.capped.len(), 2);
    assert_eq!(written, original.replace("1.6.0", "1.7.0"));
    options.config = Some(Arc::new(UpdConfig {
        ignore: vec!["flake8-docstrings".into()],
        pin: HashMap::from([
            ("demo".into(), "3.0.0".into()),
            ("owner/repo".into(), "v3.0.0".into()),
        ]),
        ..Default::default()
    }));
    let (result, written) = run("prek.toml", original, options).await;
    assert_eq!(result.pinned.len(), 2);
    assert_eq!(result.ignored.len(), 1);
    assert_eq!(written, original.replace("1.0.0", "3.0.0"));
}

#[tokio::test]
async fn unsafe_and_unversioned_install_arguments_are_unchanged() {
    let original = "repos:\n- repo: local\n  hooks:\n  - id: python\n    language: python\n    additional_dependencies: ['demo', 'demo @ https://example.com/demo.whl', '--index-url=https://example.com', 'demo==1.0.0 garbage', \"demo==1.0.\\u0030\"]\n  - id: node\n    language: node\n    additional_dependencies: ['demo', '@scope/demo@latest', '@scope/demo@file:../demo', 'https://example.com/pkg']\n  - id: rust\n    language: rust\n    additional_dependencies: ['cli:https://github.com/owner/repo', 'demo', 'demo:bad']\n  - id: shell\n    language: system\n    additional_dependencies: ['demo==1.0.0']\n";
    let (result, written) = run(
        ".pre-commit-config.yaml",
        original,
        UpdateOptions::new(false, false),
    )
    .await;
    assert!(result.updated.is_empty(), "{result:?}");
    assert!(result.errors.is_empty(), "{result:?}");
    assert_eq!(written, original);
    assert_eq!(result.warnings.len(), 2);
}

#[tokio::test]
async fn remote_language_uses_selected_revision_and_manifest_is_fetched_once() {
    use base64::Engine;
    let server = MockServer::start().await;
    Mock::given(path("/repos/owner/repo/releases/latest"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"tag_name":"v2.0.0"})),
        )
        .mount(&server)
        .await;
    let manifest = "- id: one\n  language: rust\n- id: two\n  language: python\n";
    Mock::given(path("/repos/owner/repo/contents/.pre-commit-hooks.yaml"))
        .and(query_param("ref", "v2.0.0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"encoding":"base64", "content":base64::engine::general_purpose::STANDARD.encode(manifest)})))
        .expect(1).mount(&server).await;
    let original = "repos:\n- repo: https://github.com/owner/repo\n  rev: v1.0.0\n  hooks:\n  - id: one\n    additional_dependencies: ['demo:1.0.0']\n  - id: two\n    additional_dependencies: ['demo==1.0.0']\n";
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(".pre-commit-config.yaml");
    std::fs::write(&file, original).unwrap();
    let registry = GitHubReleasesRegistry::with_api_url(server.uri());
    let result = updater()
        .update(&file, &registry, UpdateOptions::new(false, false))
        .await
        .unwrap();
    assert_eq!(result.updated.len(), 3, "{result:?}");
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        original
            .replace("rev: v1.0.0", "rev: v2.0.0")
            .replace("demo:1.0.0", "demo:3.0.0")
            .replace("demo==1.0.0", "demo==2.0.0")
    );
}

#[tokio::test]
async fn missing_remote_language_is_a_warning_and_explicit_override_needs_no_manifest() {
    let original = "repos:\n- repo: https://github.com/owner/repo\n  rev: v1.0.0\n  hooks:\n  - id: missing\n    additional_dependencies: ['demo==1.0.0']\n  - id: explicit\n    language: python\n    additional_dependencies: ['demo==1.0.0']\n";
    let (result, written) = run(
        ".pre-commit-config.yaml",
        original,
        UpdateOptions::new(false, false).with_packages(vec!["demo".into()]),
    )
    .await;
    assert_eq!(result.updated.len(), 1);
    assert_eq!(result.warnings.len(), 1);
    assert!(written.contains("id: missing\n    additional_dependencies: ['demo==1.0.0']"));
    assert!(written.contains(
        "id: explicit\n    language: python\n    additional_dependencies: ['demo==2.0.0']"
    ));
}

#[tokio::test]
async fn shared_yaml_values_are_never_modified_and_duplicates_are_rejected() {
    let original = "defaults: &hooks [{id: test, language: python, additional_dependencies: ['demo==1.0.0']}]\nrepos:\n- repo: local\n  hooks: *hooks\n";
    let (result, written) = run(
        ".pre-commit-config.yaml",
        original,
        UpdateOptions::new(false, false),
    )
    .await;
    assert!(result.updated.is_empty());
    assert_eq!(written, original);
    assert!(config::yaml("repos: []\nrepos: []\n").is_err());
}

#[test]
fn toml_discovery_and_both_aliases_select_precommit() {
    use clap::ValueEnum;
    assert_eq!(Lang::from_str("prek", false).unwrap(), Lang::PreCommit);
    assert_eq!(
        Lang::from_str("pre-commit", false).unwrap(),
        Lang::PreCommit
    );
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prek.toml"), "repos = []").unwrap();
    std::fs::write(dir.path().join(".pre-commit-config.yaml"), "repos: []").unwrap();
    let found = crate::updater::discover_files(&[dir.path().to_path_buf()], &[Lang::PreCommit]);
    assert_eq!(found.len(), 2);
}

#[tokio::test]
async fn cooldown_uses_the_dependency_ecosystem_and_python_patch_classification() {
    use crate::cooldown::CooldownPolicy;
    use chrono::{Duration, Utc};
    let now = Utc::now();
    let pypi = MockRegistry::new("pypi")
        .with_version("demo", "0.0.3")
        .with_version_meta("demo", "0.0.3", Some(now - Duration::days(1)), false, false)
        .with_version_meta(
            "demo",
            "0.0.2",
            Some(now - Duration::days(20)),
            false,
            false,
        );
    let updater = PreCommitUpdater::with_registries(RegistrySet::with_single(
        AnnotationSource::PyPi,
        Arc::new(pypi),
    ));
    let options = UpdateOptions::new(false, false).with_cooldown_policy(
        CooldownPolicy {
            default: Duration::zero(),
            per_ecosystem: HashMap::from([("pypi".into(), Duration::days(7))]),
            force_override: None,
        },
        now,
    );
    let mut options = options;
    options.bump_filter.major = false;
    options.bump_filter.minor = false;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("prek.toml");
    let original = "[[repos]]\nrepo = 'local'\n[[repos.hooks]]\nid = 'demo'\nlanguage = 'python'\nadditional_dependencies = ['demo==0.0.1']\n";
    std::fs::write(&file, original).unwrap();
    let result = updater
        .update(&file, &MockRegistry::new("github-releases"), options)
        .await
        .unwrap();
    assert_eq!(result.updated.len(), 1, "{result:?}");
    assert_eq!(result.updated[0].2, "0.0.2");
    assert_eq!(result.held_back.len(), 1);
    assert_eq!(result.update_bump(0), crate::updater::BumpKind::Patch);
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        original.replace("0.0.1", "0.0.2")
    );
}

#[tokio::test]
async fn revision_cooldown_and_precision_also_apply_to_toml() {
    use crate::cooldown::CooldownPolicy;
    use chrono::{Duration, Utc};
    let now = Utc::now();
    let registry = MockRegistry::new("github-releases")
        .with_version("owner/repo", "v3.0.0")
        .with_version_meta(
            "owner/repo",
            "v3.0.0",
            Some(now - Duration::days(1)),
            false,
            false,
        )
        .with_version_meta(
            "owner/repo",
            "v2.5.0",
            Some(now - Duration::days(20)),
            false,
            false,
        );
    let options = UpdateOptions::new(false, false).with_cooldown_policy(
        CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: HashMap::new(),
            force_override: None,
        },
        now,
    );
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("prek.toml");
    let original = "repos = [{repo = 'https://github.com/owner/repo', rev = 'v1'}]\n";
    std::fs::write(&file, original).unwrap();
    let result = updater().update(&file, &registry, options).await.unwrap();
    assert_eq!(result.updated[0].2, "v2");
    assert_eq!(result.held_back.len(), 1);
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        original.replace("'v1'", "'v2'")
    );
}

#[tokio::test]
async fn same_package_in_different_ecosystems_keeps_correct_update_sources() {
    let original = "repos = [{repo = 'local', hooks = [{id = 'python', language = 'python', additional_dependencies = ['demo==1.0.0']}, {id = 'rust', language = 'rust', additional_dependencies = ['demo:1.0.0']}]}]";
    let (result, _) = run("prek.toml", original, UpdateOptions::new(true, false)).await;
    let report = crate::output::build_update_file_report(
        Path::new("prek.toml"),
        FileType::PreCommitConfig,
        &result,
        None,
        |_, _| "major",
    );
    let json = serde_json::to_value(report).unwrap();
    assert_eq!(json["updates"][0]["source"], "pypi");
    assert_eq!(json["updates"][1]["source"], "crates");
}

#[tokio::test]
async fn pins_keep_each_ecosystem_source_even_for_identical_names_and_lines() {
    let original = "repos = [{repo = 'local', hooks = [{id = 'python', language = 'python', additional_dependencies = ['demo==1.0.0']}, {id = 'rust', language = 'rust', additional_dependencies = ['demo:1.0.0']}, {id = 'node', language = 'node', additional_dependencies = ['@scope/demo@^1.0.0']}]}]";
    let options = UpdateOptions::new(false, false).with_config(Arc::new(UpdConfig {
        pin: HashMap::from([
            ("demo".into(), "1.5.0".into()),
            ("@scope/demo".into(), "1.8.0".into()),
        ]),
        ..Default::default()
    }));
    let (result, written) = run("prek.toml", original, options).await;
    assert_eq!(result.pinned.len(), 3);
    let report = crate::output::build_update_file_report(
        Path::new("prek.toml"),
        FileType::PreCommitConfig,
        &result,
        None,
        |_, _| "minor",
    );
    let json = serde_json::to_value(report).unwrap();
    assert_eq!(json["pinned"][0]["source"], "pypi");
    assert_eq!(json["pinned"][1]["source"], "crates");
    assert_eq!(json["pinned"][2]["source"], "npm");
    assert_eq!(
        written,
        original
            .replace("demo==1.0.0", "demo==1.5.0")
            .replace("demo:1.0.0", "demo:1.5.0")
            .replace("@scope/demo@^1.0.0", "@scope/demo@^1.8.0")
    );
}

#[tokio::test]
async fn same_named_cooldown_entries_report_their_own_registry_and_policy() {
    use crate::cooldown::CooldownPolicy;
    use chrono::{Duration, Utc};
    let now = Utc::now();
    for fallback in [true, false] {
        let registry = |name| {
            let registry = MockRegistry::new(name)
                .with_version("demo", "3.0.0")
                .with_version_meta("demo", "3.0.0", Some(now - Duration::days(1)), false, false);
            if fallback {
                registry.with_version_meta(
                    "demo",
                    "2.0.0",
                    Some(now - Duration::days(30)),
                    false,
                    false,
                )
            } else {
                registry
            }
        };
        let updater = PreCommitUpdater::with_registries(RegistrySet::with_sources(vec![
            (AnnotationSource::PyPi, Arc::new(registry("pypi"))),
            (AnnotationSource::Crates, Arc::new(registry("crates.io"))),
        ]));
        let policy = CooldownPolicy {
            default: Duration::zero(),
            per_ecosystem: HashMap::from([
                ("pypi".into(), Duration::days(7)),
                ("crates.io".into(), Duration::days(10)),
            ]),
            force_override: None,
        };
        let options = UpdateOptions::new(true, false).with_cooldown_policy(policy.clone(), now);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("prek.toml");
        std::fs::write(&file, "repos = [{repo = 'local', hooks = [{id = 'python', language = 'python', additional_dependencies = ['demo==1.0.0']}, {id = 'rust', language = 'rust', additional_dependencies = ['demo:1.0.0']}]}]").unwrap();
        let result = updater
            .update(&file, &MockRegistry::new("github-releases"), options)
            .await
            .unwrap();
        let report = crate::output::build_update_file_report(
            &file,
            FileType::PreCommitConfig,
            &result,
            Some(&policy),
            |_, _| "major",
        );
        let json = serde_json::to_value(report).unwrap();
        let entries = &json[if fallback {
            "held_back"
        } else {
            "skipped_by_cooldown"
        }];
        assert_eq!(entries[0]["source"], "pypi", "{json}");
        assert_eq!(entries[1]["source"], "crates", "{json}");
        assert_eq!(entries[0]["cooldown_seconds"], 7 * 86400);
        assert_eq!(entries[1]["cooldown_seconds"], 10 * 86400);
    }
}
