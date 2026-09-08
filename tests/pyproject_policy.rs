//! End-to-end regressions for Python update policy and reporting.
use serde_json::{Value, json};
use std::process::{Command, Output};
use tempfile::TempDir;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

async fn serve(server: &MockServer, name: &str, files: Value) {
    Mock::given(path(format!("/simple/{name}/")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            json!({"files": files}).to_string(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(server)
        .await;
}
fn run(dir: &TempDir, server: &MockServer, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .env(
            "PIP_CONFIG_FILE",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("UV_INDEX_URL", server.uri())
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .current_dir(dir.path())
        .args(args)
        .args(["--no-cache", "--output", "json", "."])
        .output()
        .unwrap()
}
fn report(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn write(dir: &TempDir, name: &str, content: &str) {
    std::fs::write(dir.path().join(name), content).unwrap();
}

#[tokio::test]
async fn python_zero_patch_is_written_and_counted_under_patch_ceiling() {
    let server = MockServer::start().await;
    serve(&server, "demo", json!([{"filename":"demo-0.0.78.tar.gz"}])).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir,
        "pyproject.toml",
        "[project]\ndependencies = ['demo==0.0.77']\n",
    );
    write(&dir, "requirements.txt", "demo==0.0.77\n");
    let result = report(&run(&dir, &server, &["--apply", "--max-bump", "patch"]), 0);
    assert_eq!(result["summary"]["updates_patch"], 2, "{result}");
    assert_eq!(result["files"][0]["updates"][0]["bump"], "patch");
    assert!(
        std::fs::read_to_string(dir.path().join("pyproject.toml"))
            .unwrap()
            .contains("demo==0.0.78")
    );
}

#[tokio::test]
async fn report_preserves_full_constraints_sections_and_duplicate_locations() {
    let server = MockServer::start().await;
    serve(&server, "demo", json!([{"filename":"demo-1.5.tar.gz"}])).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir,
        "pyproject.toml",
        "[project]\ndependencies = [\n 'demo>=1.0,<2',\n 'demo>=1.0,<2',\n]\n[project.optional-dependencies]\ndocs = ['demo==1.0']\n[dependency-groups]\ndev = ['demo~=1.0']\n",
    );
    let result = report(&run(&dir, &server, &["--check"]), 1);
    let updates = result["files"][0]["updates"].as_array().unwrap();
    assert_eq!(updates.len(), 4);
    assert_eq!(updates[0]["line"], 3);
    assert_eq!(updates[1]["line"], 4);
    assert_eq!(updates[0]["previous_spec"], ">=1.0,<2");
    assert_eq!(updates[0]["new_spec"], ">=1.5,<2");
    assert_eq!(updates[2]["section"], "project.optional-dependencies.docs");
    assert_eq!(updates[3]["section"], "dependency-groups.dev");
}

#[tokio::test]
async fn exact_pin_opt_out_precedes_normalization_but_does_not_freeze_arbitrary_equality() {
    let server = MockServer::start().await;
    serve(&server, "demo", json!([{"filename":"demo-1.5.tar.gz"}])).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir,
        "pyproject.toml",
        "[project]\ndependencies = ['demo==1.0']\n[dependency-groups]\ndev = ['demo===1.0']\n",
    );
    write(
        &dir,
        "upd.toml",
        "[update.pyproject]\nexact-pins = false\n[normalize.pyproject]\ndependencies = 'at-least'\n",
    );
    let result = report(&run(&dir, &server, &["--apply"]), 0);
    let text = std::fs::read_to_string(dir.path().join("pyproject.toml")).unwrap();
    assert!(text.contains("demo==1.0"), "{text}");
    assert!(text.contains("demo===1.5"), "{text}");
    assert_eq!(
        result["files"][0]["skipped"][0]["reason"],
        "exact-pins-disabled"
    );
}

#[tokio::test]
async fn uv_arrays_are_updated_but_declared_non_registry_sources_are_preserved() {
    let server = MockServer::start().await;
    serve(&server, "demo", json!([{"filename":"demo-1.5.tar.gz"}])).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir,
        "pyproject.toml",
        "[project]\ndependencies = ['local==1.0']\n[tool.uv]\nconstraint-dependencies = ['demo>=1.0,<2']\nbuild-constraint-dependencies = ['demo>=1.0']\noverride-dependencies = ['demo==1.0']\ndev-dependencies = ['demo==1.0']\n[tool.uv.sources]\nlocal = { path = '../local' }\n",
    );
    let result = report(&run(&dir, &server, &["--apply"]), 0);
    assert_eq!(
        result["files"][0]["updates"].as_array().unwrap().len(),
        4,
        "{result}"
    );
    assert!(
        std::fs::read_to_string(dir.path().join("pyproject.toml"))
            .unwrap()
            .contains("local==1.0")
    );
    assert_eq!(
        result["files"][0]["updates"][0]["section"],
        "tool.uv.constraint-dependencies"
    );
}

#[tokio::test]
async fn ecosystem_empty_selection_is_none_and_cli_can_override_it() {
    let server = MockServer::start().await;
    serve(&server, "demo", json!([{"filename":"demo-1.5.tar.gz"}])).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir,
        "pyproject.toml",
        "[project]\ndependencies = ['demo==1.0']\n",
    );
    write(&dir, "Cargo.toml", "this is deliberately invalid TOML");
    write(&dir, "upd.toml", "[ecosystems]\nenable = []\n");
    let result = report(&run(&dir, &server, &["--check"]), 0);
    assert!(result["files"].as_array().unwrap().is_empty());
    let result = report(&run(&dir, &server, &["--check", "--lang", "python"]), 1);
    assert_eq!(result["files"].as_array().unwrap().len(), 1);
    write(
        &dir,
        "upd.toml",
        "[ecosystems]\nenable = ['python', 'rust']\ndisable = ['rust']\n",
    );
    let result = report(&run(&dir, &server, &["--check"]), 1);
    assert_eq!(result["files"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn uv_cutoff_filters_artifacts_before_python_coverage() {
    let server = MockServer::start().await;
    serve(&server, "demo", json!([
        {"filename":"demo-1.5.tar.gz", "requires-python":">=3.8", "upload-time":"2025-01-01T00:00:00Z"},
        {"filename":"demo-2.0-py3-none-any.whl", "requires-python":">=3.11", "upload-time":"2025-01-01T00:00:00Z"},
        {"filename":"demo-2.0.tar.gz", "requires-python":">=3.8", "upload-time":"2026-01-01T00:00:00Z"},
        {"filename":"demo-3.0.tar.gz", "requires-python":">=3.8"}
    ])).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir,
        "pyproject.toml",
        "[project]\nrequires-python = '>=3.10'\ndependencies = ['demo==1.0']\n[tool.uv]\nexclude-newer = '2025-06-01T00:00:00Z'\n",
    );
    let result = report(&run(&dir, &server, &["--check"]), 1);
    assert_eq!(
        result["files"][0]["updates"][0]["latest"], "1.5",
        "{result}"
    );
    // No declared interpreter range must not imply support for every Python.
    write(
        &dir,
        "pyproject.toml",
        "[project]\ndependencies = ['demo==1.0']\n[tool.uv]\nexclude-newer = '2025-06-01T00:00:00Z'\n",
    );
    let result = report(&run(&dir, &server, &["--check"]), 1);
    assert_eq!(
        result["files"][0]["updates"][0]["latest"], "2.0",
        "{result}"
    );
}

#[tokio::test]
async fn uv_cutoff_package_and_index_exemptions_follow_upstream_precedence() {
    let server = MockServer::start().await;
    serve(
        &server,
        "demo",
        json!([
            {"filename":"demo-1.5.tar.gz", "upload-time":"2025-01-01T00:00:00Z"},
            {"filename":"demo-2.0.tar.gz", "upload-time":"2026-01-01T00:00:00Z"},
            {"filename":"demo-3.0.tar.gz"}
        ]),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    for (package, index, expected) in [
        ("", "", "1.5"),
        ("exclude-newer-package = { DeMo = false }", "", "3.0"),
        ("", "exclude-newer = false", "3.0"),
        (
            "exclude-newer-package = { demo = '2025-06-01T00:00:00Z' }",
            "exclude-newer = false",
            "3.0",
        ),
        ("", "exclude-newer = '2025-06-01T00:00:00Z'", "3.0"),
    ] {
        // A configured index policy admits missing timestamps even when a
        // package has a stricter cutoff; known timestamps still obey it.
        write(
            &dir,
            "pyproject.toml",
            &format!(
                "[project]\ndependencies = ['demo==1.0']\n[tool.uv]\nexclude-newer = '2025-06-01T00:00:00Z'\n{package}\n[[tool.uv.index]]\nname = 'private'\nurl = '{}/simple/'\ndefault = true\n{index}\n",
                server.uri()
            ),
        );
        let result = report(&run(&dir, &server, &["--check"]), 1);
        assert_eq!(
            result["files"][0]["updates"][0]["latest"], expected,
            "{package} / {index}: {result}"
        );
    }
}

#[tokio::test]
async fn uv_cutoff_blocks_config_pins_and_honors_workspace_root() {
    let server = MockServer::start().await;
    serve(
        &server,
        "demo",
        json!([
            {"filename":"demo-1.5.tar.gz", "upload-time":"2025-01-01T00:00:00Z"},
            {"filename":"demo-2.0.tar.gz", "upload-time":"2026-01-01T00:00:00Z"}
        ]),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir,
        "pyproject.toml",
        "[project]\ndependencies = ['demo==1.0']\n[tool.uv]\nexclude-newer = '2025-06-01T00:00:00Z'\n[tool.uv.workspace]\nmembers = ['member']\n",
    );
    std::fs::create_dir(dir.path().join("member")).unwrap();
    write(
        &dir,
        "member/pyproject.toml",
        "[project]\nname = 'member'\ndependencies = ['demo==1.0']\n",
    );
    let result = report(&run(&dir, &server, &["--check"]), 1);
    for file in result["files"].as_array().unwrap() {
        assert_eq!(file["updates"][0]["latest"], "1.5", "{result}");
    }
    write(&dir, "upd.toml", "[pin]\ndemo = '2.0'\n");
    let result = report(&run(&dir, &server, &["--apply"]), 2);
    assert_eq!(result["summary"]["updates_total"], 0);
    assert!(
        std::fs::read_to_string(dir.path().join("pyproject.toml"))
            .unwrap()
            .contains("demo==1.0")
    );
}

#[tokio::test]
async fn python_minor_capping_and_text_reporting_use_the_same_policy() {
    let server = MockServer::start().await;
    serve(&server, "demo", json!([{"filename":"demo-0.78.0.tar.gz"}])).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir,
        "pyproject.toml",
        "[project]\ndependencies = ['demo>=0.77.0']\n",
    );
    let result = report(&run(&dir, &server, &["--check", "--max-bump", "patch"]), 0);
    assert_eq!(result["files"][0]["capped"][0]["bump"], "minor");
    let result = report(&run(&dir, &server, &["--check", "--only-bump", "minor"]), 1);
    assert_eq!(result["summary"]["updates_minor"], 1);
    let output = Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .env("HOME", dir.path())
        .env(
            "PIP_CONFIG_FILE",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("UV_INDEX_URL", server.uri())
        .current_dir(dir.path())
        .args([
            "--check",
            "--no-cache",
            "--output",
            "text",
            "--no-color",
            "pyproject.toml",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("demo >=0.77.0 → >=0.78.0 [project.dependencies]"),
        "{text}"
    );
    assert!(!text.contains("(MAJOR)"), "{text}");
}

#[tokio::test]
async fn cutoff_is_project_scoped_even_with_shared_cached_metadata() {
    use std::sync::{Arc, Mutex};
    use upd::cache::{Cache, CachedRegistry};
    use upd::registry::{MultiPyPiRegistry, PyPiRegistry};
    use upd::updater::{PyProjectUpdater, UpdateOptions, Updater};
    let server = MockServer::start().await;
    serve(
        &server,
        "demo",
        json!([
            {"filename":"demo-1.5.tar.gz", "upload-time":"2025-01-01T00:00:00Z"},
            {"filename":"demo-2.0.tar.gz", "upload-time":"2026-01-01T00:00:00Z"}
        ]),
    )
    .await;
    let registry = CachedRegistry::new(
        MultiPyPiRegistry::from_primary_and_extras(
            PyPiRegistry::with_index_url(server.uri()),
            vec![],
        ),
        Arc::new(Mutex::new(Cache::default())),
        true,
    );
    let dir = tempfile::tempdir().unwrap();
    for (name, cutoff) in [
        ("early", "2025-06-01T00:00:00Z"),
        ("late", "2026-06-01T00:00:00Z"),
    ] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
        write(
            &dir,
            &format!("{name}/pyproject.toml"),
            &format!(
                "[project]\ndependencies = ['demo==1.0']\n[tool.uv]\nexclude-newer = '{cutoff}'\n"
            ),
        );
    }
    let updater = PyProjectUpdater::new();
    let early = dir.path().join("early/pyproject.toml");
    let late = dir.path().join("late/pyproject.toml");
    let (early, late) = tokio::join!(
        updater.update(&early, &registry, UpdateOptions::new(true, true)),
        updater.update(&late, &registry, UpdateOptions::new(true, true))
    );
    assert_eq!(early.unwrap().updated[0].2, "1.5");
    assert_eq!(late.unwrap().updated[0].2, "2.0");
}

#[tokio::test]
async fn cutoff_and_cooldown_compose_using_legacy_artifact_metadata() {
    let server = MockServer::start().await;
    Mock::given(path("/simple/demo/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let recent = chrono::Utc::now().to_rfc3339();
    Mock::given(path("/pypi/demo/json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"releases": {
                "1.5": [{"upload_time_iso_8601":"2020-01-01T00:00:00Z"}],
                "2.0": [{"upload_time_iso_8601":recent}],
                "3.0": [{"upload_time_iso_8601":"2100-01-01T00:00:00Z"}]
            }})),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir,
        "pyproject.toml",
        "[project]\ndependencies = ['demo==1.0']\n[tool.uv]\nexclude-newer = '2099-01-01T00:00:00Z'\n",
    );
    let result = report(&run(&dir, &server, &["--check", "--min-age", "7d"]), 1);
    assert_eq!(
        result["files"][0]["updates"][0]["latest"], "1.5",
        "{result}"
    );
}

#[tokio::test]
async fn html_upload_times_and_explicit_pin_precedence_are_supported() {
    let server = MockServer::start().await;
    Mock::given(path("/simple/demo/")).respond_with(ResponseTemplate::new(200).set_body_raw(
        "<a href='demo-1.5.tar.gz' data-upload-time='2025-01-01T00:00:00Z'>old</a><a href='demo-2.0.tar.gz' data-upload-time='2026-01-01T00:00:00Z'>new</a>", "text/html"
    )).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir,
        "pyproject.toml",
        "[project]\ndependencies = ['demo==1.0']\n[tool.uv]\nexclude-newer = '2025-06-01T00:00:00Z'\n",
    );
    let result = report(&run(&dir, &server, &["--check"]), 1);
    assert_eq!(result["files"][0]["updates"][0]["latest"], "1.5");
    write(
        &dir,
        "upd.toml",
        "[update.pyproject]\nexact-pins = false\n[pin]\ndemo = '1.5'\n",
    );
    let result = report(&run(&dir, &server, &["--apply"]), 0);
    assert_eq!(
        result["files"][0]["pinned"][0]["pinned_to"], "1.5",
        "{result}"
    );
}

#[tokio::test]
async fn ecosystem_denylist_wins_over_the_annotated_wildcard() {
    let server = MockServer::start().await;
    serve(&server, "demo", json!([{"filename":"demo-1.5.tar.gz"}])).await;
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "versions.mk", "VERSION := 1.0  # upd: pypi demo\n");
    write(
        &dir,
        "upd.toml",
        "include = ['versions.mk']\n[ecosystems]\ndisable = ['python']\n",
    );
    let result = report(&run(&dir, &server, &["--check"]), 0);
    assert_eq!(result["summary"]["updates_total"], 0);
    assert!(server.received_requests().await.unwrap().is_empty());
    write(
        &dir,
        "upd.toml",
        "include = ['versions.mk']\n[ecosystems]\nenable = ['annotated']\ndisable = ['python']\n",
    );
    let result = report(&run(&dir, &server, &["--check"]), 0);
    assert_eq!(result["summary"]["updates_total"], 0);
    let result = report(&run(&dir, &server, &["--check", "--lang", "annotated"]), 1);
    assert_eq!(result["summary"]["updates_total"], 1);
}

#[tokio::test]
async fn empty_ecosystem_selection_also_applies_to_align_and_audit() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "pyproject.toml", "not valid TOML");
    write(&dir, "uv.lock", "not a valid lockfile");
    write(&dir, "upd.toml", "[ecosystems]\nenable = []\n");
    report(&run(&dir, &server, &["align"]), 0);
    report(&run(&dir, &server, &["audit", "--offline"]), 0);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn lock_only_package_selection_also_honors_the_uv_cutoff() {
    let server = MockServer::start().await;
    serve(
        &server,
        "demo",
        json!([
            {"filename":"demo-1.5.tar.gz", "upload-time":"2025-01-01T00:00:00Z"},
            {"filename":"demo-2.0.tar.gz", "upload-time":"2026-01-01T00:00:00Z"}
        ]),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir,
        "pyproject.toml",
        "[project]\nname='project'\nversion='0.1.0'\ndependencies=[]\n[tool.uv]\nexclude-newer='2025-06-01T00:00:00Z'\n",
    );
    write(
        &dir,
        "uv.lock",
        "version = 1\n[[package]]\nname = 'demo'\nversion = '1.0'\nsource = {registry = 'https://pypi.org/simple'}\n",
    );
    let result = report(&run(&dir, &server, &["--check", "--package", "demo"]), 1);
    let updates: Vec<_> = result["files"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|file| file["updates"].as_array().unwrap())
        .collect();
    assert_eq!(updates.len(), 1, "{result}");
    assert_eq!(updates[0]["latest"], "1.5", "{result}");
}

#[tokio::test]
async fn sibling_lock_only_updates_use_each_projects_cutoff() {
    let server = MockServer::start().await;
    serve(
        &server,
        "demo",
        json!([
            {"filename":"demo-1.5.tar.gz", "upload-time":"2025-01-01T00:00:00Z"},
            {"filename":"demo-2.0.tar.gz", "upload-time":"2026-01-01T00:00:00Z"}
        ]),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    for (project, cutoff) in [("early", "2025-06-01"), ("late", "2026-06-01")] {
        std::fs::create_dir(dir.path().join(project)).unwrap();
        write(
            &dir,
            &format!("{project}/pyproject.toml"),
            &format!(
                "[project]\nname='{project}'\nversion='0.1.0'\ndependencies=[]\n[tool.uv]\nexclude-newer='{cutoff}T00:00:00Z'\n"
            ),
        );
        write(
            &dir,
            &format!("{project}/uv.lock"),
            "version=1\n[[package]]\nname='demo'\nversion='1.0'\nsource={registry='https://pypi.org/simple'}\n",
        );
    }
    let result = report(&run(&dir, &server, &["--check", "--package", "demo"]), 1);
    for (project, expected) in [("early", "1.5"), ("late", "2.0")] {
        let file = result["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| {
                f["path"].as_str().unwrap().contains(project)
                    && !f["updates"].as_array().unwrap().is_empty()
            })
            .unwrap();
        assert_eq!(file["updates"][0]["latest"], expected, "{result}");
    }
}
