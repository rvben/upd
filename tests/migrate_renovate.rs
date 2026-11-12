use std::{fs, process::Command};

#[test]
fn cli_migration_preserves_the_source_and_refuses_existing_policy() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"{"packageRules":[{"matchPackageNames":["numpy"],"enabled":false}],"extends":["config:recommended"]}"#;
    fs::write(dir.path().join("renovate.json"), source).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_upd"))
            .current_dir(dir.path())
            .args(["migrate", "renovate", "--output", "json"])
            .args(args)
            .output()
            .unwrap()
    };
    let preview = run(&["--dry-run"]);
    assert!(preview.status.success());
    assert!(!dir.path().join(".updrc.toml").exists());
    let report: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert!(report["config"].as_str().unwrap().contains("numpy"));
    assert_eq!(report["diagnostics"][0]["path"], "extends[0]");
    assert_eq!(report["diagnostics"][0]["classification"], "dropped");
    let result = run(&[]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let policy = fs::read_to_string(dir.path().join(".updrc.toml")).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("renovate.json")).unwrap(),
        source
    );
    assert!(!run(&[]).status.success());
    assert_eq!(
        fs::read_to_string(dir.path().join(".updrc.toml")).unwrap(),
        policy
    );
}

#[test]
fn common_template_prints_config_and_classifications_without_opting_in() {
    let dir = tempfile::tempdir().unwrap();
    let source = include_str!("fixtures/renovate-common.json");
    fs::write(dir.path().join("renovate.json"), source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_upd"))
        .current_dir(dir.path())
        .args(["migrate", "renovate", "--output", "text"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("ignore = [\"some-internal-pkg\"]"));
    assert!(stdout.contains("# translated: automerge:"));
    assert!(stdout.contains("# dropped: extends[0]:"));
    assert!(stdout.contains("# dropped: packageRules[1].matchPackagePatterns:"));
    let config: toml::Value = toml::from_str(&stdout).unwrap();
    assert_eq!(config["automation"]["auto_merge"].as_bool(), Some(true));
    assert_eq!(
        config["automation"]["ignore_approvals"].as_bool(),
        Some(true)
    );
    assert_eq!(config["automation"]["lock"].as_bool(), Some(true));
    assert!(config["automation"].get("dependency_updates").is_none());
    assert_eq!(
        fs::read_to_string(dir.path().join("renovate.json")).unwrap(),
        source
    );
}

#[test]
fn refused_migrations_return_a_json_preview_without_writing_policy() {
    for (source, args, classification) in [
        (
            r#"{"packageRules":[{"matchPackagePatterns":["^@scope/"],"enabled":false}]}"#,
            vec![],
            "blocking",
        ),
        (
            r#"{"lockFileMaintenance":{"enabled":true,"schedule":["weekends"]}}"#,
            vec![],
            "blocking",
        ),
        (
            r#"{"extends":["config:recommended"]}"#,
            vec!["--allow-dropped=false"],
            "dropped",
        ),
        (
            r#"{"lockFileMaintenance":{"enabled":true}}"#,
            vec!["--allow-dropped=false"],
            "dropped",
        ),
        (
            r#"{"packageRules":[{"matchPackageNames":["/numpy/"],"enabled":false}]}"#,
            vec![],
            "blocking",
        ),
        (
            include_str!("fixtures/renovate-common.json"),
            vec!["--allow-dropped=false"],
            "dropped",
        ),
    ] {
        for dry_run in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("renovate.json"), source).unwrap();
            let mut command = Command::new(env!("CARGO_BIN_EXE_upd"));
            command
                .current_dir(dir.path())
                .args(["migrate", "renovate", "--output", "json"])
                .args(&args);
            if dry_run {
                command.arg("--dry-run");
            }
            let output = command.output().unwrap();
            assert_eq!(output.status.code(), Some(2), "{source}");
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert!(report["config"].is_string());
            assert!(
                report["diagnostics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["classification"] == classification)
            );
            assert!(!dir.path().join(".updrc.toml").exists());
            assert_eq!(
                fs::read_to_string(dir.path().join("renovate.json")).unwrap(),
                source
            );
        }
    }
}

#[test]
fn strict_mode_still_accepts_exact_translations() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("renovate.json"),
        r#"{"automerge":false,"lockFileMaintenance":{"enabled":false}}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_upd"))
        .current_dir(dir.path())
        .args([
            "migrate",
            "renovate",
            "--allow-dropped=false",
            "--output",
            "json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(dir.path().join(".updrc.toml").exists());
}

#[test]
fn conventional_nested_config_paths_write_to_root_and_protect_existing_policy() {
    for folder in [".github", ".gitlab"] {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(folder)).unwrap();
        let source = dir.path().join(folder).join("renovate.json");
        fs::write(&source, r#"{"automerge":false}"#).unwrap();
        let run = || {
            Command::new(env!("CARGO_BIN_EXE_upd"))
                .current_dir(dir.path())
                .args(["migrate", "renovate"])
                .arg(format!("{folder}/renovate.json"))
                .output()
                .unwrap()
        };
        assert!(run().status.success());
        assert!(dir.path().join(".updrc.toml").exists());
        assert!(!dir.path().join(folder).join(".updrc.toml").exists());
        let policy = "ignore = [\"protected\"]\n";
        fs::write(dir.path().join(".updrc.toml"), policy).unwrap();
        assert!(!run().status.success());
        assert_eq!(
            fs::read_to_string(dir.path().join(".updrc.toml")).unwrap(),
            policy
        );
        assert_eq!(
            fs::read_to_string(source).unwrap(),
            r#"{"automerge":false}"#
        );
    }
}
