//! Exercise offline commands as users run them, including snippet round trips.
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

const ASSET: &str =
    "https://github.com/jdx/mise/releases/download/v2025.12.9/mise-v2025.12.9-linux-x64.tar.gz";
const SHA: &str = "afe7e9f2ea8e1704e9cc41e4b020798b8c60e5924ab4a313ccbf201a062f54d0";

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_upd"))
        .args(args)
        .current_dir(dir)
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .output()
        .unwrap()
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn scaffold_round_trips_all_syntaxes_and_preserves_files_even_with_apply() {
    let dir = tempfile::tempdir().unwrap();
    // Init must not read config, construct registries or require credentials.
    fs::write(dir.path().join(".updrc.toml"), "not valid TOML = [").unwrap();
    for (syntax, file) in [
        ("shell", "versions.sh"),
        ("docker", "Dockerfile"),
        ("toml", "pins.toml"),
        ("yaml", "pins.yaml"),
        ("javascript", "versions.js"),
    ] {
        let output = run(
            dir.path(),
            &[
                "annotations",
                "init",
                ASSET,
                "--checksum",
                SHA,
                "--syntax",
                syntax,
                "--checksums",
                "SHASUMS256.txt",
                "--apply",
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result = json(&output);
        assert_eq!(result["asset_template"], "mise-{tag}-linux-x64.tar.gz");
        assert_eq!(result["package"], "jdx/mise");
        assert_eq!(result["checksum_source"], "supplied");
        let snippet = result["snippet"].as_str().unwrap();
        assert!(snippet.contains("checksums=SHASUMS256.txt"));
        let path = dir.path().join(file);
        fs::write(&path, snippet).unwrap();
        // An explicit clean config lets validation use normal discovery config.
        fs::write(dir.path().join("clean.toml"), "").unwrap();
        let checked = run(
            dir.path(),
            &[
                "annotations",
                "validate",
                file,
                "--config",
                "clean.toml",
                "--apply",
            ],
        );
        assert!(
            checked.status.success(),
            "{}",
            String::from_utf8_lossy(&checked.stderr)
        );
        assert_eq!(json(&checked)["summary"]["versions"], 1);
        assert_eq!(json(&checked)["summary"]["checksums"], 1);
        assert_eq!(json(&checked)["valid"], true);
        assert_eq!(fs::read_to_string(path).unwrap(), snippet);
        let text = run(
            dir.path(),
            &[
                "annotations",
                "init",
                ASSET,
                "--checksum",
                SHA,
                "--syntax",
                syntax,
                "--checksums",
                "SHASUMS256.txt",
                "--output",
                "text",
            ],
        );
        assert!(text.status.success());
        assert_eq!(String::from_utf8(text.stdout).unwrap(), snippet);
    }
}

#[test]
fn validation_reports_line_errors_ambiguity_docker_scope_and_native_collisions() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        ("pins.toml", format!("version = '1.2.3' # upd: github-releases o/r\nversion = '2.3.4'\nsha = '{SHA}' # upd: checksum version asset=x.tar.gz\n"), 3, "use id="),
        ("versions.sh", format!("V=1.2.3 # upd: github-releases o/r\nSHA={SHA} # upd: checksum V asset=x-{{unknown}}.tar.gz\n"), 2, "invalid asset template"),
        ("Dockerfile", format!("# upd: github-releases o/r\nARG V=1.2.3\nFROM alpine\n# upd: checksum V asset=x.tar.gz\nARG SHA={SHA}\n"), 5, "consume a global ARG"),
        ("Cargo.toml", "version = '1.2.3' # upd: github-releases o/r\n".into(), 1, "native dependency parser"),
        ("duplicate.sh", "A=1.2.3 # upd: github-releases o/r id=tool\nB=1.2.3 # upd: github-releases o/r id=tool\n".into(), 1, "duplicate annotation id"),
        ("short.sh", "V=1.2.3 # upd: github-releases o/r\nSHA=abc # upd: checksum V asset=x.tar.gz\n".into(), 2, "SHA-256 token"),
    ];
    for (file, content, line, reason) in cases {
        fs::write(dir.path().join(file), &content).unwrap();
        let output = run(dir.path(), &["annotations", "validate", file]);
        assert_eq!(output.status.code(), Some(2), "{file}");
        let report = json(&output);
        assert_eq!(report["valid"], false);
        assert!(
            report["files"][0]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["lines"].as_array().unwrap().iter().any(|n| n == line)
                    && d["message"].as_str().unwrap().contains(reason)),
            "{file}: {report}"
        );
        assert_eq!(fs::read_to_string(dir.path().join(file)).unwrap(), content);
        let text = run(
            dir.path(),
            &[
                "annotations",
                "validate",
                file,
                "--output",
                "text",
                "--quiet",
            ],
        );
        assert_eq!(text.status.code(), Some(2));
        assert!(text.stdout.is_empty());
        assert!(String::from_utf8(text.stderr).unwrap().contains(reason));
    }
    fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
    let workflow = format!(
        "jobs:\n  test:\n    steps:\n      - uses: o/r@v1.2.3 # upd: github-releases o/r\n      - run: echo ok\n        env:\n          V: '1.2.3' # upd: github-releases o/tool\n          SHA: '{SHA}' # upd: checksum V asset=x.tar.gz\n"
    );
    fs::write(dir.path().join(".github/workflows/check.yml"), workflow).unwrap();
    let checked = run(
        dir.path(),
        &["annotations", "validate", ".github/workflows/check.yml"],
    );
    assert_eq!(checked.status.code(), Some(2));
    let report = json(&checked);
    assert_eq!(report["summary"]["versions"], 1);
    assert_eq!(report["summary"]["checksums"], 1);
    assert_eq!(report["files"][0]["diagnostics"][0]["lines"][0], 4);
    assert!(
        report["files"][0]["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("own updater")
    );

    fs::write(
        dir.path().join("unsupported.sh"),
        "A=1.2.3 # upd: imaginary o/r\nB=1.2.3 # upd: imaginary o/r\n",
    )
    .unwrap();
    let unsupported = run(dir.path(), &["annotations", "validate", "unsupported.sh"]);
    assert_eq!(unsupported.status.code(), Some(2));
    assert_eq!(json(&unsupported)["summary"]["errors"], 2);
}

#[test]
fn validation_respects_discovery_config_and_fails_on_missing_paths() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("upd.toml"),
        "include = ['**/*.pins']\nexclude = ['**/bad.pins']\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("tool.pins"),
        "V=1.2.3 # upd: github-releases o/r\n",
    )
    .unwrap();
    fs::write(dir.path().join("bad.pins"), "# upd: checksum V asset=x\n").unwrap();
    let output = run(
        dir.path(),
        &["annotations", "validate", ".", "--config", "upd.toml"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(&output)["summary"]["versions"], 1);
    let explicit = run(
        dir.path(),
        &[
            "annotations",
            "validate",
            "bad.pins",
            "--config",
            "upd.toml",
        ],
    );
    assert_eq!(explicit.status.code(), Some(2));
    let missing = run(dir.path(), &["annotations", "validate", "absent.sh"]);
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(
        json(&missing)["files"][0]["diagnostics"][0]["message"],
        "path does not exist"
    );
}

#[test]
fn init_rejects_unsafe_or_nonrelease_inputs_and_never_emits_a_snippet_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    for asset in [
        "http://github.com/o/r/releases/download/v1.2.3/x.tar.gz",
        "https://example.com/o/r/releases/download/v1.2.3/x.tar.gz",
        "https://github.com/o/r/releases/latest/download/x.tar.gz",
        "https://github.com/o/r/releases/download/latest/x.tar.gz",
        "https://github.com/o/r/releases/download/v1.2.3/x%22%3B.tar.gz",
        "https://github.com/o/r/releases/download/v1.2.3/x%2Ffile.tar.gz",
        "https://github.com/o/r/releases/download/v1.2.3/x.tar.gz?token=secret",
    ] {
        let output = run(
            dir.path(),
            &["annotations", "init", asset, "--checksum", SHA],
        );
        assert!(!output.status.success(), "accepted {asset}");
        assert!(output.stdout.is_empty(), "{asset}");
    }
    for extra in [
        vec!["--checksum", "abc"],
        vec!["--checksum", SHA, "--name", "9bad"],
        vec!["--checksum", SHA, "--checksums", "x asset=evil"],
        vec!["--checksum", SHA, "--checksums", "{unknown}.txt"],
    ] {
        let mut args = vec!["annotations", "init", ASSET];
        args.extend(extra);
        let output = run(dir.path(), &args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn init_handles_unprefixed_encoded_tags_fixed_assets_and_numeric_repository_names() {
    let dir = tempfile::tempdir().unwrap();
    for (url, template, prefix) in [
        (
            "https://github.com/o/r/releases/download/1.2.3/tool-1.2.3.tar.gz",
            "tool-{version}.tar.gz",
            "R",
        ),
        (
            "https://github.com/o/9-tool/releases/download/v1.2.3/tool.tar.gz",
            "tool.tar.gz",
            "TOOL_9_TOOL",
        ),
        (
            "https://github.com/o/r/releases/download/v1%2E2%2E3/tool-v1.2.3.tar.gz",
            "tool-{tag}.tar.gz",
            "R",
        ),
        (
            "https://github.com/o/r/releases/download/v1.2.3/tool-1.2.30.tar.gz",
            "tool-1.2.30.tar.gz",
            "R",
        ),
        (
            "https://github.com/o/r/releases/download/v1.2.3/tool-v1.2.3-1.2.3.tar.gz",
            "tool-{tag}-{version}.tar.gz",
            "R",
        ),
        (
            "https://github.com/o/r/releases/download/v1.2.3/tool-1.2.3.4.tar.gz",
            "tool-1.2.3.4.tar.gz",
            "R",
        ),
    ] {
        let output = run(dir.path(), &["annotations", "init", url, "--checksum", SHA]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result = json(&output);
        assert_eq!(result["asset_template"], template);
        assert!(
            result["snippet"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{prefix}_VERSION="))
        );
    }
}

#[test]
fn explicit_templates_round_trip_and_checksum_modes_are_required_and_exclusive() {
    let dir = tempfile::tempdir().unwrap();
    for template in [
        "mise-{tag}-linux-x64.tar.gz",
        "mise-v{version}-linux-x64.tar.gz",
        "mise-v2025.12.9-linux-x64.tar.gz",
    ] {
        for syntax in ["shell", "docker", "toml", "yaml", "javascript"] {
            let output = run(
                dir.path(),
                &[
                    "annotations",
                    "init",
                    ASSET,
                    "--checksum",
                    SHA,
                    "--asset-template",
                    template,
                    "--syntax",
                    syntax,
                ],
            );
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let result = json(&output);
            assert_eq!(result["asset_template"], template);
            assert!(
                result["snippet"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("asset={template}"))
            );
        }
    }
    for template in [
        "mise-{tag}-linux-arm64.tar.gz",
        "mise-{version}-linux-x64.tar.gz",
        "https://example.com/{tag}",
        "../mise-{tag}-linux-x64.tar.gz",
        "mise-{unknown}-linux-x64.tar.gz",
        "mise-{tag}-linux-x64.tar.gz extra=value",
        "",
        "mise-{tag}-linux-x64.tar.gz#comment",
    ] {
        let output = run(
            dir.path(),
            &[
                "annotations",
                "init",
                ASSET,
                "--checksum",
                SHA,
                "--asset-template",
                template,
            ],
        );
        assert!(!output.status.success(), "accepted {template}");
        assert!(output.stdout.is_empty());
    }
    for modes in [vec![], vec!["--checksum", SHA, "--resolve-checksum"]] {
        let mut args = vec!["annotations", "init", ASSET];
        args.extend(modes);
        let output = run(dir.path(), &args);
        assert_eq!(output.status.code(), Some(4));
        assert!(output.stdout.is_empty());
    }
}
