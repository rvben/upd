//! Dockerfile dispatch and source selection, using configured pins to avoid network calls.
use std::process::Command;
use tempfile::TempDir;

const DOCKERFILE: &str = "FROM alpine:3.22\r\n # upd: pypi uv\r\nARG UV_VERSION=0.9.30\r\n# renovate: datasource=npm depName=tool\r\nENV TOOL_VERSION=1.0.0\r\nARG UNANNOTATED=1.0.0\r\n";

fn fixture(name: &str, content: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join(name), content).unwrap();
    std::fs::write(
        dir.path().join(".updrc.toml"),
        "[pin]\nalpine = \"3.23\"\nuv = \"0.10.0\"\ntool = \"2.0.0\"\n",
    )
    .unwrap();
    dir
}

fn run(dir: &TempDir, target: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_upd"))
        .env_clear()
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path())
        .env("UPD_CACHE_DIR", dir.path().join("cache"))
        .current_dir(dir.path())
        .arg(target)
        .args(["--no-cache", "--output", "json"])
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn dockerfile_annotations_obey_source_selection_and_discovery() {
    for name in ["Dockerfile", "Dockerfile.ci"] {
        for target in [name, "."] {
            for (lang, image, pypi, npm) in [
                (None, true, true, true),
                (Some("annotated"), false, true, true),
                (Some("python"), false, true, false),
                (Some("docker"), true, false, false),
            ] {
                let dir = fixture(name, DOCKERFILE);
                let mut args = vec!["--apply"];
                if let Some(lang) = lang {
                    args.extend(["--lang", lang]);
                }
                let output = run(&dir, target, &args);
                assert!(
                    output.status.success(),
                    "{name} {target} {lang:?}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let mut expected = DOCKERFILE.to_string();
                if image {
                    expected = expected.replace("alpine:3.22", "alpine:3.23");
                }
                if pypi {
                    expected = expected.replace("0.9.30", "0.10.0");
                }
                if npm {
                    expected = expected.replace("TOOL_VERSION=1.0.0", "TOOL_VERSION=2.0.0");
                }
                assert_eq!(
                    std::fs::read_to_string(dir.path().join(name)).unwrap(),
                    expected,
                    "{name} {target} {lang:?}"
                );
                let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(report["files"][0]["file_type"], "dockerfile");
            }
        }
    }
}

#[test]
fn variable_from_warning_requires_verbose_but_inline_annotation_warning_does_not() {
    let content = "ARG BASE=alpine:3.22\nFROM $BASE\nARG UV_VERSION=0.9.30 # upd: pypi uv\n";
    for verbose in [false, true] {
        let dir = fixture("Dockerfile", content);
        std::fs::remove_file(dir.path().join(".updrc.toml")).unwrap();
        let mut args = vec!["--apply"];
        if verbose {
            args.push("--verbose");
        }
        let output = run(&dir, "Dockerfile", &args);
        assert!(output.status.success(), "{:?}", output);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{error}: {output:?}"));
        let warnings = report["files"][0]["warnings"].as_array().unwrap();
        assert_eq!(
            warnings
                .iter()
                .any(|warning| warning.as_str().unwrap().contains("variable-based FROM")),
            verbose,
            "{report}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.as_str().unwrap().contains("inline # text")),
            "{report}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Dockerfile")).unwrap(),
            content
        );
    }
}

#[test]
fn dockerfile_annotation_check_does_not_write() {
    let dir = fixture("Dockerfile", DOCKERFILE);
    let output = run(&dir, "Dockerfile", &["--check", "--lang", "annotated"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("Dockerfile")).unwrap(),
        DOCKERFILE
    );
}

#[test]
fn dockerfile_from_annotation_is_refused() {
    let dir = fixture("Dockerfile", "FROM alpine:3.22 # upd: pypi uv\n");
    let output = run(&dir, "Dockerfile", &["--apply"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("Dockerfile")).unwrap(),
        "FROM alpine:3.23 # upd: pypi uv\n"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("annotation ignored"));
}

#[test]
fn dockerfile_invalid_annotations_never_change_values() {
    for content in [
        "ARG UV_VERSION=0.9.30 # upd: pypi uv\n",
        "# upd: pypi uv\n\nARG UV_VERSION=0.9.30\n",
        "# upd: pypi uv\n# another comment\nARG UV_VERSION=0.9.30\n",
        "# upd: pypi uv\nENV UV_VERSION=0.9.30 OTHER=0.9.30\n",
        "# upd: pypi uv\nARG UV_VERSION=${DEFAULT:-0.9.30}\n",
        "# upd: pypi uv\nARG UV_VERSION=\\\n0.9.30\n",
        "RUN echo hi \\\n# upd: pypi uv\nARG UV_VERSION=0.9.30\n",
        "# escape=`\nRUN echo hi `\n# upd: pypi uv\nARG UV_VERSION=0.9.30\n",
        "# upd: pypi\nARG UV_VERSION=0.9.30\n",
        "# upd: pypi uv\n",
    ] {
        let dir = fixture("Dockerfile", content);
        let output = run(&dir, "Dockerfile", &["--apply", "--lang", "annotated"]);
        assert!(
            output.status.success(),
            "{content}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Dockerfile")).unwrap(),
            content
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            !report["files"][0]["warnings"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{content}: {report}"
        );
    }
}

#[test]
fn dockerfile_heredoc_content_is_not_an_assignment() {
    let content = "FROM scratch\nCOPY <<'EOF' /file\n# upd: pypi uv\nARG UV_VERSION=0.9.30\nEOF\n# upd: pypi uv\nARG UV_VERSION=\"0.9.30\"\n";
    let dir = fixture("Dockerfile", content);
    let output = run(&dir, "Dockerfile", &["--apply"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("Dockerfile")).unwrap(),
        content.replace("\"0.9.30\"", "\"0.10.0\"")
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["files"][0]["pinned"][0]["line"], 7);
}

#[test]
fn dockerfile_heredoc_delimiters_keep_body_annotations_untouched() {
    for (opening, closing) in [
        ("123", "123"),
        ("'END FILE'", "END FILE"),
        ("-EOF", "\tEOF"),
    ] {
        let content = format!(
            "FROM scratch\nCOPY <<{opening} /file\n# upd: pypi uv\nARG UV_VERSION=0.9.30\n{closing}\n# upd: pypi uv\nARG UV_VERSION='0.9.30'\n"
        );
        let dir = fixture("Dockerfile", &content);
        let output = run(&dir, "Dockerfile", &["--apply"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Dockerfile")).unwrap(),
            content.replace("'0.9.30'", "'0.10.0'")
        );
    }
}
