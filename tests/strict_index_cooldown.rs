//! Strict publication dates must stay on the index answering the version query.
use std::collections::HashMap;

use upd::registry::{
    DeclaredIndex, IndexChain, MultiPyPiRegistry, PyPiRegistry, Registry, VersionQuery,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn strict_metadata_never_falls_through_from_a_simple_only_or_failing_private_index() {
    for status in [404, 403] {
        for chain in [false, true] {
            let private = MockServer::start().await;
            let public = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/simple/demo/"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    "<a href='demo-2.0.0-py3-none-any.whl'>demo-2.0.0-py3-none-any.whl</a>",
                ))
                .mount(&private)
                .await;
            Mock::given(method("GET"))
                .and(path("/pypi/demo/json"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&private)
                .await;
            Mock::given(method("GET"))
                .and(path("/pypi/demo/json"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "releases": {"2.0.0": [{"yanked": false, "upload_time_iso_8601": "2020-01-01T00:00:00Z"}]}
                })))
                .mount(&public).await;
            let default = PyPiRegistry::with_index_url(public.uri());
            let multi = MultiPyPiRegistry::from_primary_and_extras(
                PyPiRegistry::with_index_url(private.uri()),
                vec![public.uri()],
            );
            let index_chain = IndexChain::new(
                vec![
                    DeclaredIndex::url(None, &private.uri()),
                    DeclaredIndex::default_registry(),
                ],
                &HashMap::new(),
                &default,
            )
            .unwrap();
            let registry: &dyn Registry = if chain { &index_chain } else { &multi };
            for query in [
                VersionQuery::Stable,
                VersionQuery::IncludingPrereleases,
                VersionQuery::Matching(">=2"),
            ] {
                assert_eq!(query.run(registry, "demo").await.unwrap(), "2.0.0");
                let answer = registry
                    .list_versions_for_cooldown_query("demo", true, query, Some("2.0.0"))
                    .await;
                if status == 404 {
                    assert!(answer.unwrap().is_empty());
                } else {
                    assert!(answer.is_err());
                }
                assert!(
                    public.received_requests().await.unwrap().is_empty(),
                    "strict dates must never consult the later public index"
                );
            }
            assert!(
                registry
                    .list_versions_for_cooldown_query(
                        "demo",
                        true,
                        VersionQuery::Stable,
                        Some("3.0.0")
                    )
                    .await
                    .is_err()
            );
            assert!(public.received_requests().await.unwrap().is_empty());
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("requirements.txt");
            let original = "demo==1.0.0\n";
            std::fs::write(&file, original).unwrap();
            let options = upd::updater::UpdateOptions::new(false, false).with_cooldown_policy(
                upd::cooldown::CooldownPolicy {
                    strict: true,
                    default: chrono::Duration::days(7),
                    ..Default::default()
                },
                chrono::Utc::now(),
            );
            let result = upd::updater::Updater::update(
                &upd::updater::RequirementsUpdater::new(),
                &file,
                registry,
                options,
            )
            .await
            .unwrap();
            assert_eq!(std::fs::read_to_string(file).unwrap(), original);
            if status == 404 {
                assert!(result.errors.is_empty(), "{result:?}");
                assert_eq!(result.skipped_by_cooldown.len(), 1);
            } else {
                assert_eq!(result.errors.len(), 1, "{result:?}");
            }
            assert!(public.received_requests().await.unwrap().is_empty());
            let normal = registry
                .list_versions_for_cooldown_query(
                    "demo",
                    false,
                    VersionQuery::Stable,
                    Some("2.0.0"),
                )
                .await
                .unwrap();
            assert_eq!(normal.len(), 1);
            assert!(normal[0].published_at.is_some());
        }
    }
}

/// Poetry's version query differs from its eligibility bounds; strict dates
/// must follow the actual query and never move to the later public index.
#[tokio::test]
async fn strict_poetry_keeps_publication_dates_on_its_resolution_index() {
    for status in [404, 403] {
        let private = MockServer::start().await;
        let public = MockServer::start().await;
        Mock::given(path("/simple/demo/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("<a href='demo-4.0.0.tar.gz'>demo-4.0.0.tar.gz</a>"),
            )
            .mount(&private)
            .await;
        Mock::given(path("/pypi/demo/json"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&private)
            .await;
        Mock::given(path("/simple/demo/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "<a href='demo-4.0.0.tar.gz'>demo-4.0.0.tar.gz</a><a href='demo-1.0.1.tar.gz'>demo-1.0.1.tar.gz</a>",
            )).mount(&public).await;
        Mock::given(path("/pypi/demo/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "releases": {
                    "4.0.0": [{"yanked": false, "upload_time_iso_8601": "2020-01-01T00:00:00Z"}],
                    "1.0.1": [{"yanked": false, "upload_time_iso_8601": "2020-01-01T00:00:00Z"}]
                }
            })))
            .mount(&public)
            .await;
        let registry = MultiPyPiRegistry::from_primary_and_extras(
            PyPiRegistry::with_index_url(private.uri()),
            vec![public.uri()],
        );
        for specifier in ["~1.0.0", "^1.0.0", "~=1.0.0", ">=1,<3"] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("pyproject.toml");
            let original = format!("[tool.poetry.dependencies]\ndemo = '{specifier}'\n");
            std::fs::write(&file, &original).unwrap();
            let options = upd::updater::UpdateOptions::new(false, true).with_cooldown_policy(
                upd::cooldown::CooldownPolicy {
                    strict: true,
                    default: chrono::Duration::days(7),
                    ..Default::default()
                },
                chrono::Utc::now(),
            );
            let result = upd::updater::Updater::update(
                &upd::updater::PyProjectUpdater::new(),
                &file,
                &registry,
                options,
            )
            .await
            .unwrap();
            assert_eq!(std::fs::read_to_string(file).unwrap(), original);
            if matches!(specifier, "~=1.0.0" | ">=1,<3") {
                // Existing Poetry parsing treats these as unparseable versions;
                // it keeps them with a downgrade warning before date lookup.
                assert!(result.errors.is_empty(), "{result:?}");
                assert_eq!(result.unchanged, 1, "{result:?}");
                assert_eq!(result.warnings.len(), 1, "{result:?}");
            } else if status == 404 {
                assert!(result.errors.is_empty(), "{result:?}");
                assert_eq!(result.skipped_by_cooldown.len(), 1, "{result:?}");
            } else {
                assert_eq!(result.errors.len(), 1, "{result:?}");
            }
            assert!(public.received_requests().await.unwrap().is_empty());
        }
    }
}
