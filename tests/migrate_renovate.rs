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
    assert!(report["warnings"][0].as_str().unwrap().contains("extends"));
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
