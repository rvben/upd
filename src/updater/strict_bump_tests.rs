//! `--strict-bump` write gate, exercised through every updater's production
//! entry point.
//!
//! Each file type gets one fixture holding a real major update beside the
//! writes no bump level describes (a configured pin, and where the format has
//! them an unanchored rewrite, a reshape, the upd self-pin, an annotation or a
//! moved revision). Three runs per fixture:
//!
//! - `--only-bump major` alone writes every declared extra, proving the
//!   fixture reaches each write path and that existing behaviour is kept;
//! - `--only-bump major --strict-bump` changes only lines that belong to a
//!   reported `Major` update, and reports every declared extra as held;
//! - the same strict run against a registry with no major changes no bytes.
//!
//! The fixture match has no wildcard arm, so a new `FileType` does not compile
//! until it has a fixture here.

use super::*;
use crate::registry::{DockerRegistry, GradleRegistry, MockRegistry};
use tempfile::TempDir;

/// A write the fixture contains that is not a registry-selected bump.
struct Extra {
    package: &'static str,
    kind: HeldWrite,
}

enum Runner {
    Plain(Box<dyn Updater>),
    /// Dockerfiles and workflows run through `update_with_annotations`, as
    /// `main.rs` dispatches them.
    Dockerfile(DockerUpdater, AnnotatedUpdater),
    Workflow(Box<GithubActionsUpdater>, AnnotatedUpdater),
}

struct Case {
    file_name: &'static str,
    content: String,
    config: &'static str,
    runner: Runner,
    registry: Box<dyn Registry>,
    /// The dependency with a major release available, when `with_major`.
    /// `None` for a format with no major-version concept (Nix has no
    /// semver), where the strict run is expected to write nothing at all.
    major: Option<&'static str>,
    extras: Vec<Extra>,
    /// Enables the self-pin/SHA-pin-annotation machinery for GitHub Actions,
    /// which is gated behind `UpdateOptions::with_action_sha_updates` and
    /// off by default for every other file type.
    sha_updates: bool,
    /// Keeps any mock server backing the fixture alive for the run.
    _servers: Vec<wiremock::MockServer>,
    /// Keeps any scratch directory the fixture depends on (e.g. the fake
    /// `nix` script's own working directory) alive for the run.
    _temp_dirs: Vec<TempDir>,
}

/// Build the fixture for `file_type`. `with_major` false gives the same file
/// with the major dependency's registry answering its current version.
async fn case(file_type: FileType, with_major: bool) -> Case {
    match file_type {
        FileType::CargoToml => Case {
            file_name: "Cargo.toml",
            content: "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n\
                      [dependencies]\nbig = \"1.2.0\"\nsmall = \"1.2.0\"\npinned = \"1.2.0\"\n"
                .to_string(),
            config: "[pin]\npinned = \"1.2.5\"\n",
            runner: Runner::Plain(Box::new(CargoTomlUpdater::new())),
            registry: Box::new(
                MockRegistry::new("crates.io")
                    .with_version("big", if with_major { "2.0.0" } else { "1.2.0" })
                    .with_version("small", "1.3.0")
                    .with_version("pinned", "3.0.0"),
            ),
            major: Some("big"),
            extras: vec![Extra {
                package: "pinned",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::Requirements => Case {
            file_name: "requirements.txt",
            content: "big==1.2.0\nsmall==1.2.0\npinned==1.2.0\n".to_string(),
            config: "[pin]\npinned = \"1.2.5\"\n",
            runner: Runner::Plain(Box::new(RequirementsUpdater::new())),
            registry: Box::new(
                MockRegistry::new("PyPI")
                    .with_version("big", if with_major { "2.0.0" } else { "1.2.0" })
                    .with_version("small", "1.3.0")
                    .with_version("pinned", "3.0.0"),
            ),
            major: Some("big"),
            extras: vec![Extra {
                package: "pinned",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::PyProject => Case {
            file_name: "pyproject.toml",
            // `[normalize.pyproject]` below normalizes the whole section to the
            // `==` shape, so `big`/`small`/`pinned` are written already in that
            // shape: `classify_rewrite` then calls them `SameShape` and routes
            // them through the ordinary bump/pin paths (`result.updated` /
            // `result.pinned`) instead of `result.normalized`. Only the two
            // deliberately-mis-shaped entries (`unanchored`, `reshaped`) are
            // meant to hit the normalization paths.
            content: "[project]\nname = \"fixture\"\ndependencies = [\n    \"big==1.2.0\",\n    \
                      \"small==1.2.0\",\n    \"pinned==1.2.0\",\n    \"unanchored\",\n    \
                      \"reshaped>=2.0.0\",\n]\n"
                .to_string(),
            config: "[pin]\npinned = \"1.2.5\"\n\n[normalize.pyproject]\ndependencies = \"exact\"\n",
            runner: Runner::Plain(Box::new(PyProjectUpdater::new())),
            registry: Box::new(
                MockRegistry::new("PyPI")
                    .with_version("big", if with_major { "2.0.0" } else { "1.2.0" })
                    .with_version("small", "1.3.0")
                    .with_version("pinned", "3.0.0")
                    .with_version("unanchored", "1.0.0")
                    .with_version("reshaped", "2.0.0"),
            ),
            major: Some("big"),
            extras: vec![
                Extra {
                    package: "pinned",
                    kind: HeldWrite::Pin,
                },
                Extra {
                    package: "unanchored",
                    kind: HeldWrite::Unanchored,
                },
                Extra {
                    package: "reshaped",
                    kind: HeldWrite::Reshape,
                },
            ],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::PackageJson => Case {
            file_name: "package.json",
            content: "{\n  \"name\": \"fixture\",\n  \"dependencies\": {\n    \
                      \"big\": \"^1.2.0\",\n    \"small\": \"^1.2.0\",\n    \
                      \"pinned\": \"^1.2.0\"\n  }\n}\n"
                .to_string(),
            config: "[pin]\npinned = \"1.2.5\"\n",
            runner: Runner::Plain(Box::new(PackageJsonUpdater::new())),
            registry: Box::new(
                MockRegistry::new("npm")
                    .with_version("big", if with_major { "2.0.0" } else { "1.2.0" })
                    .with_version("small", "1.3.0")
                    .with_version("pinned", "3.0.0"),
            ),
            major: Some("big"),
            extras: vec![Extra {
                package: "pinned",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::GoMod => Case {
            file_name: "go.mod",
            content: "module example.com/fixture\n\nrequire (\n\t\
                      example.com/big v1.2.0\n\texample.com/small v1.2.0\n\t\
                      example.com/pinned v1.2.0\n)\n"
                .to_string(),
            config: "[pin]\n\"example.com/pinned\" = \"v1.2.5\"\n",
            runner: Runner::Plain(Box::new(GoModUpdater::new())),
            registry: Box::new(
                MockRegistry::new("go-proxy")
                    .with_version("example.com/big", if with_major { "v2.0.0" } else { "v1.2.0" })
                    .with_version("example.com/small", "v1.3.0")
                    .with_version("example.com/pinned", "v3.0.0"),
            ),
            major: Some("example.com/big"),
            extras: vec![Extra {
                package: "example.com/pinned",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::Gemfile => Case {
            file_name: "Gemfile",
            content: "gem 'big', '1.2.0'\ngem 'small', '1.2.0'\ngem 'pinned', '1.2.0'\n"
                .to_string(),
            config: "[pin]\npinned = \"1.2.5\"\n",
            runner: Runner::Plain(Box::new(GemfileUpdater::new())),
            registry: Box::new(
                MockRegistry::new("rubygems")
                    .with_version("big", if with_major { "2.0.0" } else { "1.2.0" })
                    .with_version("small", "1.3.0")
                    .with_version("pinned", "3.0.0"),
            ),
            major: Some("big"),
            extras: vec![Extra {
                package: "pinned",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::Csproj => Case {
            file_name: "fixture.csproj",
            content: "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <ItemGroup>\n    \
                      <PackageReference Include=\"Big\" Version=\"1.2.0\" />\n    \
                      <PackageReference Include=\"Small\" Version=\"1.2.0\" />\n    \
                      <PackageReference Include=\"Pinned\" Version=\"1.2.0\" />\n  \
                      </ItemGroup>\n</Project>\n"
                .to_string(),
            // Unlike most updaters, CsprojUpdater runs a configured pin through
            // the same `allows_bump` ceiling as a registry bump (csproj.rs
            // applies the check before it knows whether the line is pinned), so
            // a patch-level pin here would be capped rather than written and the
            // fixture would never reach the pin's write path at all. The pin
            // target is a major-level jump so it clears the same `--only-bump
            // major` ceiling the fixture selects.
            config: "[pin]\nPinned = \"2.5.0\"\n",
            runner: Runner::Plain(Box::new(CsprojUpdater::new())),
            registry: Box::new(
                MockRegistry::new("nuget")
                    .with_version("Big", if with_major { "2.0.0" } else { "1.2.0" })
                    .with_version("Small", "1.3.0")
                    .with_version("Pinned", "3.0.0"),
            ),
            major: Some("Big"),
            extras: vec![Extra {
                package: "Pinned",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::TerraformTf => Case {
            file_name: "main.tf",
            content: "terraform {\n  required_providers {\n    big = {\n      \
                      source  = \"example/big\"\n      version = \"1.2.0\"\n    }\n    \
                      small = {\n      source  = \"example/small\"\n      version = \"1.2.0\"\n    }\n    \
                      pinned = {\n      source  = \"example/pinned\"\n      version = \"1.2.0\"\n    }\n  \
                      }\n}\n"
                .to_string(),
            config: "[pin]\n\"example/pinned\" = \"1.2.5\"\n",
            runner: Runner::Plain(Box::new(TerraformUpdater::new())),
            registry: Box::new(
                MockRegistry::new("terraform")
                    .with_version("example/big", if with_major { "2.0.0" } else { "1.2.0" })
                    .with_version("example/small", "1.3.0")
                    .with_version("example/pinned", "3.0.0"),
            ),
            major: Some("example/big"),
            extras: vec![Extra {
                package: "example/pinned",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::GradleCatalog => Case {
            file_name: "libs.versions.toml",
            content: "[libraries]\nbig = { module = 'example:big', version = '1.2.0' }\n\
                      small = { module = 'example:small', version = '1.2.0' }\n\
                      pinned = { module = 'example:pinned', version = '1.2.0' }\n"
                .to_string(),
            config: "[pin]\n\"example:pinned\" = \"1.2.5\"\n",
            runner: Runner::Plain(Box::new(GradleUpdater::new())),
            registry: Box::new(
                MockRegistry::new("maven")
                    .with_version("example:big", if with_major { "2.0.0" } else { "1.2.0" })
                    .with_version("example:small", "1.3.0")
                    .with_version("example:pinned", "3.0.0"),
            ),
            major: Some("example:big"),
            extras: vec![Extra {
                package: "example:pinned",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::GradleScript => Case {
            file_name: "build.gradle",
            content: "dependencies {\n    implementation(\"example:big:1.2.0\")\n    \
                      implementation(\"example:small:1.2.0\")\n    \
                      implementation(\"example:pinned:1.2.0\")\n}\n"
                .to_string(),
            config: "[pin]\n\"example:pinned\" = \"1.2.5\"\n",
            runner: Runner::Plain(Box::new(GradleUpdater::new())),
            registry: Box::new(
                MockRegistry::new("maven")
                    .with_version("example:big", if with_major { "2.0.0" } else { "1.2.0" })
                    .with_version("example:small", "1.3.0")
                    .with_version("example:pinned", "3.0.0"),
            ),
            major: Some("example:big"),
            extras: vec![Extra {
                package: "example:pinned",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::GradleWrapper => {
            let stable = if with_major { "9.0.0" } else { "8.13.0" };
            let checksum = "b".repeat(64);
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/versions/all"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {"version": stable, "snapshot": false, "nightly": false},
                ])))
                .mount(&server)
                .await;
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path(format!(
                    "/distributions/gradle-{stable}-bin.zip.sha256"
                )))
                .respond_with(
                    wiremock::ResponseTemplate::new(200).set_body_string(format!("{checksum}\n")),
                )
                .mount(&server)
                .await;
            Case {
                file_name: "gradle-wrapper.properties",
                content: format!(
                    "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.13.0-bin.zip\n\
                     distributionSha256Sum={}\nnetworkTimeout=10000\n",
                    "a".repeat(64)
                ),
                config: "",
                runner: Runner::Plain(Box::new(GradleUpdater::new())),
                registry: Box::new(GradleRegistry::new().with_distribution_url(server.uri())),
                major: Some("gradle-wrapper"),
                // A wrapper file declares exactly one `distributionUrl`, so there is no
                // room for a second, ordinary-ceiling-capped dependency, and no way to
                // express a `[pin]` for it either.
                extras: vec![],
                sha_updates: false,
                _servers: vec![server],
                _temp_dirs: Vec::new(),
            }
        }
        FileType::GithubActions => {
            // A bare commit pin already at the newest release: the only write
            // upd would make is the version comment it recovers.
            //
            // The self-pin moves 0.14 -> 0.15, a major step before 1.0, so it
            // is at the selected level. The ordinary lane writes the self-pin
            // past any ceiling, so the strict run must hold it anyway or both
            // lanes would carry it.
            const SHA_ANN: &str = "1111111111111111111111111111111111111111";
            const SELF_PIN_OLD: &str = "3333333333333333333333333333333333333333";
            const SELF_PIN_NEW: &str = "4444444444444444444444444444444444444444";
            let old_checksum = "a".repeat(64);
            let new_checksum = "b".repeat(64);
            let registry = MockRegistry::new("github-releases")
                .with_version("example/major-action", if with_major { "v2.0.0" } else { "v1.2.0" })
                .with_version("example/minor-action", "v1.3.0")
                .with_version("example/pinned-action", "v3.0.0")
                .with_version("example/sha-action", "v1.2.0")
                .with_resolved_ref("example/sha-action", "v1.2.0", SHA_ANN)
                .with_version("rvben/upd", "v0.15.0")
                .with_resolved_ref("rvben/upd", "v0.14.1", SELF_PIN_OLD)
                .with_resolved_ref("rvben/upd", "v0.15.0", SELF_PIN_NEW)
                .with_release_asset(
                    "rvben/upd",
                    "v0.15.0",
                    "upd-v0.15.0-x86_64-unknown-linux-gnu.tar.gz.sha256",
                    format!("{new_checksum}  upd-v0.15.0-x86_64-unknown-linux-gnu.tar.gz\n")
                        .into_bytes(),
                );
            Case {
                file_name: "ci.yml",
                content: format!(
                    "jobs:\n  build:\n    steps:\n      \
                     - uses: example/major-action@v1.2.0\n      \
                     - uses: example/minor-action@v1.2.0\n      \
                     - uses: example/pinned-action@v1.2.0\n      \
                     - uses: example/sha-action@{SHA_ANN}\n  \
                     health:\n    \
                     uses: rvben/upd/.github/workflows/dependency-health.yml@{SELF_PIN_OLD} # v0.14.1\n    \
                     with:\n      \
                     upd-version: v0.14.1\n      \
                     upd-target: x86_64-unknown-linux-gnu\n      \
                     upd-sha256: {old_checksum}\n"
                ),
                config: "[pin]\n\"example/pinned-action\" = \"v1.2.5\"\n",
                runner: Runner::Workflow(
                    Box::default(),
                    AnnotatedUpdater::new_parse_only(ParseWarnings::Suppress),
                ),
                registry: Box::new(registry),
                major: Some("example/major-action"),
                extras: vec![
                    Extra {
                        package: "example/pinned-action",
                        kind: HeldWrite::Pin,
                    },
                    Extra {
                        package: "example/sha-action",
                        kind: HeldWrite::Annotation,
                    },
                    Extra {
                        package: "rvben/upd",
                        kind: HeldWrite::SelfPin,
                    },
                ],
                sha_updates: true,
                _servers: Vec::new(),
                _temp_dirs: Vec::new(),
            }
        }
        FileType::PreCommitConfig => Case {
            file_name: ".pre-commit-config.yaml",
            content: "repos:\n  - repo: https://github.com/example/major-hook\n    rev: v1.2.0\n    \
                      hooks:\n      - id: major-hook\n  \
                      - repo: https://github.com/example/minor-hook\n    rev: v1.2.0\n    \
                      hooks:\n      - id: minor-hook\n  \
                      - repo: https://github.com/example/pinned-hook\n    rev: v1.2.0\n    \
                      hooks:\n      - id: pinned-hook\n"
                .to_string(),
            config: "[pin]\n\"example/pinned-hook\" = \"v1.2.5\"\n",
            runner: Runner::Plain(Box::new(PreCommitUpdater::new())),
            registry: Box::new(
                MockRegistry::new("github-releases")
                    .with_version("example/major-hook", if with_major { "v2.0.0" } else { "v1.2.0" })
                    .with_version("example/minor-hook", "v1.3.0")
                    .with_version("example/pinned-hook", "v3.0.0"),
            ),
            major: Some("example/major-hook"),
            extras: vec![Extra {
                package: "example/pinned-hook",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::MiseToml => Case {
            file_name: ".mise.toml",
            content: "[tools]\nnode = \"20.11.0\"\nzig = \"0.13.0\"\nrust = \"1.80.0\"\n".to_string(),
            config: "[pin]\nrust = \"1.81.0\"\n",
            runner: Runner::Plain(Box::new(MiseUpdater::new(RegistrySet::with_single(
                AnnotationSource::GitHubReleases,
                Arc::new(
                    MockRegistry::new("github-releases")
                        .with_version("nodejs/node", if with_major { "v22.0.0" } else { "v20.11.0" })
                        .with_version("ziglang/zig", "0.13.1")
                        .with_version("rust-lang/rust", "v1.85.0"),
                ) as Arc<dyn Registry>,
            )))),
            registry: Box::new(MockRegistry::new("unused")),
            major: Some("node"),
            extras: vec![Extra {
                package: "rust",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::ToolVersions => Case {
            file_name: ".tool-versions",
            content: "node 20.11.0\nzig 0.13.0\nrust 1.80.0\n".to_string(),
            config: "[pin]\nrust = \"1.81.0\"\n",
            runner: Runner::Plain(Box::new(MiseUpdater::new(RegistrySet::with_single(
                AnnotationSource::GitHubReleases,
                Arc::new(
                    MockRegistry::new("github-releases")
                        .with_version("nodejs/node", if with_major { "v22.0.0" } else { "v20.11.0" })
                        .with_version("ziglang/zig", "0.13.1")
                        .with_version("rust-lang/rust", "v1.85.0"),
                ) as Arc<dyn Registry>,
            )))),
            registry: Box::new(MockRegistry::new("unused")),
            major: Some("node"),
            extras: vec![Extra {
                package: "rust",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::Dockerfile => Case {
            file_name: "Dockerfile",
            content: "FROM example/major-image:1.2.0\nFROM example/minor-image:1.2.0\n\
                      FROM example/pinned-image:1.2.0\n"
                .to_string(),
            config: "[pin]\n\"example/pinned-image\" = \"1.2.5\"\n",
            runner: Runner::Dockerfile(
                DockerUpdater::new(),
                AnnotatedUpdater::new_parse_only(ParseWarnings::Suppress),
            ),
            registry: Box::new(
                MockRegistry::new("docker")
                    .with_version(
                        &DockerRegistry::lookup_key("example/major-image", "1.2.0"),
                        if with_major { "2.0.0" } else { "1.2.0" },
                    )
                    .with_version(&DockerRegistry::lookup_key("example/minor-image", "1.2.0"), "1.3.0")
                    .with_version(
                        &DockerRegistry::lookup_key("example/pinned-image", "1.2.0"),
                        "3.0.0",
                    ),
            ),
            major: Some("example/major-image"),
            extras: vec![Extra {
                package: "example/pinned-image",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::DockerCompose => Case {
            file_name: "docker-compose.yml",
            content: "services:\n  major:\n    image: example/major-image:1.2.0\n  \
                      minor:\n    image: example/minor-image:1.2.0\n  \
                      pinned:\n    image: example/pinned-image:1.2.0\n"
                .to_string(),
            config: "[pin]\n\"example/pinned-image\" = \"1.2.5\"\n",
            runner: Runner::Dockerfile(
                DockerUpdater::new(),
                AnnotatedUpdater::new_parse_only(ParseWarnings::Suppress),
            ),
            registry: Box::new(
                MockRegistry::new("docker")
                    .with_version(
                        &DockerRegistry::lookup_key("example/major-image", "1.2.0"),
                        if with_major { "2.0.0" } else { "1.2.0" },
                    )
                    .with_version(&DockerRegistry::lookup_key("example/minor-image", "1.2.0"), "1.3.0")
                    .with_version(
                        &DockerRegistry::lookup_key("example/pinned-image", "1.2.0"),
                        "3.0.0",
                    ),
            ),
            major: Some("example/major-image"),
            extras: vec![Extra {
                package: "example/pinned-image",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
        FileType::FlakeLock => {
            let old_rev = "0".repeat(40);
            let new_rev = "beef".repeat(10);
            let head = if with_major { new_rev.clone() } else { old_rev.clone() };
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/repos/example/flake/commits/main"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(head))
                .mount(&server)
                .await;
            let lock = format!(
                "{{\n  \"nodes\": {{\n    \"beef\": {{\n      \"locked\": {{\n        \
                 \"lastModified\": 1788220800,\n        \
                 \"narHash\": \"sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\",\n        \
                 \"owner\": \"example\",\n        \"repo\": \"flake\",\n        \
                 \"rev\": \"{old_rev}\",\n        \"type\": \"github\"\n      }},\n      \
                 \"original\": {{\n        \"owner\": \"example\",\n        \"repo\": \"flake\",\n        \
                 \"ref\": \"main\",\n        \"type\": \"github\"\n      }}\n    }},\n    \
                 \"root\": {{\n      \"inputs\": {{\n        \"beef\": \"beef\"\n      }}\n    }}\n  }},\n  \
                 \"root\": \"root\",\n  \"version\": 7\n}}\n"
            );
            let replacement = lock.replace(&old_rev, &new_rev);
            let scratch = TempDir::new().unwrap();
            let replacement_path = scratch.path().join("replacement-flake.lock");
            std::fs::write(&replacement_path, &replacement).unwrap();
            let script_path = scratch.path().join("fake-nix");
            std::fs::write(
                &script_path,
                format!("#!/bin/sh\ncp '{}' flake.lock\n", replacement_path.display()),
            )
            .unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            Case {
                file_name: "flake.lock",
                content: lock,
                config: "",
                runner: Runner::Plain(Box::new(
                    FlakeLockUpdater::with_endpoints(server.uri(), None)
                        .with_nix_program(script_path.clone().into_os_string()),
                )),
                // Nix has no [pin]/version-bump concept of its own; the registry
                // parameter is unused by this updater regardless.
                registry: Box::new(MockRegistry::new("unused")),
                major: None,
                extras: vec![Extra {
                    package: "beef",
                    kind: HeldWrite::Revision,
                }],
                sha_updates: false,
                _servers: vec![server],
                _temp_dirs: vec![scratch],
            }
        }
        FileType::Annotated => Case {
            file_name: "versions.env",
            content: "BIG_VERSION ?= 1.2.0  # upd: pypi big\n\
                      SMALL_VERSION ?= 1.2.0  # upd: pypi small\n\
                      PINNED_VERSION ?= 1.2.0  # upd: pypi pinned\n"
                .to_string(),
            config: "[pin]\npinned = \"1.2.5\"\n",
            runner: Runner::Plain(Box::new(AnnotatedUpdater::new(RegistrySet::with_single(
                AnnotationSource::PyPi,
                Arc::new(
                    MockRegistry::new("pypi")
                        .with_version("big", if with_major { "2.0.0" } else { "1.2.0" })
                        .with_version("small", "1.3.0")
                        .with_version("pinned", "3.0.0"),
                ) as Arc<dyn Registry>,
            )))),
            registry: Box::new(MockRegistry::new("unused")),
            major: Some("big"),
            extras: vec![Extra {
                package: "pinned",
                kind: HeldWrite::Pin,
            }],
            sha_updates: false,
            _servers: Vec::new(),
            _temp_dirs: Vec::new(),
        },
    }
}

/// Declares `ALL_FILE_TYPES` and an exhaustive match over the same list, so a
/// new `FileType` fails to compile here until it is listed and has a fixture.
macro_rules! every_file_type {
    ($($variant:ident),* $(,)?) => {
        const ALL_FILE_TYPES: &[FileType] = &[$(FileType::$variant),*];

        fn is_listed(file_type: FileType) -> bool {
            match file_type {
                $(FileType::$variant)|* => true,
            }
        }
    };
}

every_file_type!(
    Requirements,
    PyProject,
    PackageJson,
    CargoToml,
    GoMod,
    Gemfile,
    Csproj,
    GradleCatalog,
    GradleScript,
    GradleWrapper,
    GithubActions,
    PreCommitConfig,
    MiseToml,
    ToolVersions,
    TerraformTf,
    Dockerfile,
    DockerCompose,
    FlakeLock,
    Annotated,
);

struct Run {
    before: String,
    after: String,
    result: UpdateResult,
}

async fn run(file_type: FileType, with_major: bool, strict: bool) -> Run {
    let case = case(file_type, with_major).await;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(case.file_name);
    std::fs::write(&path, &case.content).unwrap();
    let (config, _) = UpdConfig::parse_with_warnings(case.config, "fixture").unwrap();
    let options = UpdateOptions::new(false, false)
        .with_config(Arc::new(config))
        .with_bump_filter(BumpFilter {
            major: true,
            minor: false,
            patch: false,
        })
        .with_strict_bump(strict)
        .with_action_sha_updates(case.sha_updates)
        .with_cooldown_lang(file_type.lang());
    let registry = case.registry.as_ref();
    let mut result = match &case.runner {
        Runner::Plain(updater) => updater.update(&path, registry, options).await,
        Runner::Dockerfile(primary, annotated) => {
            update_with_annotations(primary, annotated, &path, registry, options).await
        }
        Runner::Workflow(primary, annotated) => {
            update_with_annotations(primary.as_ref(), annotated, &path, registry, options).await
        }
    }
    .unwrap_or_else(|e| panic!("{file_type:?}: update failed: {e:#}"));
    if file_type != FileType::Annotated {
        result.set_update_lang(file_type.lang());
    }
    assert!(
        result.errors.is_empty(),
        "{file_type:?}: fixture produced errors: {:?}",
        result.errors
    );
    let after = std::fs::read_to_string(&path).unwrap();
    Run {
        before: case.content,
        after,
        result,
    }
}

/// Line numbers (1-based) whose text differs between `before` and `after`.
/// Every fixture edit rewrites lines in place, so a line-count change is itself
/// a failure the caller reports.
fn changed_lines(file_type: FileType, before: &str, after: &str) -> Vec<(usize, String)> {
    let before: Vec<&str> = before.lines().collect();
    let after: Vec<&str> = after.lines().collect();
    assert_eq!(
        before.len(),
        after.len(),
        "{file_type:?}: the write changed the line count"
    );
    before
        .iter()
        .zip(&after)
        .enumerate()
        .filter(|(_, (b, a))| b != a)
        .map(|(i, (_, a))| (i + 1, (*a).to_string()))
        .collect()
}

/// Lines a format rewrites as part of a version update without naming the
/// package on them: the Gradle wrapper's distribution checksum follows its
/// `distributionUrl`.
fn companion_line(file_type: FileType, text: &str) -> bool {
    file_type == FileType::GradleWrapper && text.starts_with("distributionSha256Sum=")
}

/// Whether a changed line is accounted for by a reported `Major` update: at
/// the update's recorded line when it has one, otherwise by naming the
/// package and its new version. A format's companion line counts once a
/// major update is reported.
fn line_is_major_update(
    file_type: FileType,
    result: &UpdateResult,
    number: usize,
    text: &str,
) -> bool {
    let major = |index: usize| result.update_bump(index) == BumpKind::Major;
    if companion_line(file_type, text) {
        return (0..result.updated.len()).any(major);
    }
    (0..result.updated.len()).any(|index| {
        let (package, _, new, line) = &result.updated[index];
        result.update_bump(index) == BumpKind::Major
            && match line {
                Some(line) => *line == number,
                None => text.contains(package.as_str()) && text.contains(new.as_str()),
            }
    })
}

async fn assert_strict_gate(file_type: FileType) {
    // Positive control: without the gate every declared extra is written, so
    // the fixture really reaches each write path the strict run must hold.
    let open = run(file_type, true, false).await;
    let changed = changed_lines(file_type, &open.before, &open.after);
    let extras = case(file_type, true).await.extras;
    for extra in &extras {
        // A format that keeps the version on its own line (a pre-commit
        // `rev:`, a Terraform `version =`) is matched by the line the write
        // was reported at instead of by name.
        let reported_lines: Vec<usize> = open
            .result
            .pinned
            .iter()
            .chain(&open.result.updated)
            .filter(|(package, ..)| package == extra.package)
            .filter_map(|(.., line)| *line)
            .collect();
        assert!(
            changed.iter().any(|(number, text)| {
                text.contains(extra.package) || reported_lines.contains(number)
            }),
            "{file_type:?}: without --strict-bump the {:?} write for {} must still happen; \
             changed lines: {changed:?}",
            extra.kind,
            extra.package,
        );
    }
    let major = case(file_type, true).await.major;
    if let Some(major) = major {
        assert!(
            (0..open.result.updated.len()).any(|i| open.result.updated[i].0 == major
                && open.result.update_bump(i) == BumpKind::Major),
            "{file_type:?}: the fixture's major update for {major} must be found; updated: {:?}",
            open.result.updated
        );
    }

    // Invariant: under the gate every changed line is a reported major. A
    // format with no major-version concept (`major` is `None`) has nothing
    // the gate would ever allow, so the strict run must change no bytes.
    let strict = run(file_type, true, true).await;
    let changed = changed_lines(file_type, &strict.before, &strict.after);
    match major {
        Some(_) => {
            assert!(
                !changed.is_empty(),
                "{file_type:?}: the strict run must still write the major update"
            );
            for (number, text) in &changed {
                assert!(
                    line_is_major_update(file_type, &strict.result, *number, text),
                    "{file_type:?}: line {number} changed under --strict-bump without a \
                     reported major update: {text:?}\nupdated: {:?}",
                    strict.result.updated
                );
            }
        }
        None => {
            assert!(
                changed.is_empty(),
                "{file_type:?}: a format with no major-version concept must change no bytes \
                 under --strict-bump; changed: {changed:?}"
            );
        }
    }
    for index in 0..strict.result.updated.len() {
        assert_eq!(
            strict.result.update_bump(index),
            BumpKind::Major,
            "{file_type:?}: non-major update reported under --strict-bump: {:?}",
            strict.result.updated[index]
        );
    }
    assert!(
        strict.result.pinned.is_empty()
            && strict.result.normalized.is_empty()
            && strict.result.annotations.is_empty(),
        "{file_type:?}: a held write was still reported as made: pinned {:?}, normalized {:?}, \
         annotations {:?}",
        strict.result.pinned,
        strict.result.normalized,
        strict.result.annotations
    );
    for extra in &extras {
        assert!(
            strict
                .result
                .capped
                .iter()
                .any(|c| c.package == extra.package && c.strict == Some(extra.kind)),
            "{file_type:?}: {} must be reported held as {:?}; capped: {:?}",
            extra.package,
            extra.kind,
            strict.result.capped
        );
    }

    // With nothing at the selected level, the gate leaves the file untouched.
    let idle = run(file_type, false, true).await;
    assert_eq!(
        idle.before, idle.after,
        "{file_type:?}: --strict-bump with no major available must change no bytes"
    );
}

#[tokio::test]
async fn strict_bump_writes_only_selected_registry_bumps_for_every_file_type() {
    for &file_type in ALL_FILE_TYPES {
        assert!(is_listed(file_type));
        assert_strict_gate(file_type).await;
    }
}

#[test]
fn without_strict_bump_the_gate_answers_as_before() {
    let open = UpdateOptions::new(false, false).with_bump_filter(BumpFilter {
        major: true,
        minor: false,
        patch: false,
    });
    for kind in [
        WriteKind::Revision,
        WriteKind::Pin,
        WriteKind::Unanchored,
        WriteKind::Reshape,
        WriteKind::SelfPin,
        WriteKind::Annotation,
    ] {
        assert!(open.allows_write(kind), "{kind:?}");
        assert!(
            !open.clone().with_strict_bump(true).allows_write(kind),
            "{kind:?}"
        );
    }
    assert!(open.allows_bump_for(Lang::Nix, "a", "b"));
    assert!(
        !open
            .clone()
            .with_strict_bump(true)
            .allows_bump_for(Lang::Nix, "a", "b")
    );
    let strict = open.with_strict_bump(true);
    assert!(strict.allows_write(WriteKind::Bump {
        lang: Lang::Rust,
        current: "1.0.0",
        new: "2.0.0",
    }));
    assert!(!strict.allows_write(WriteKind::Bump {
        lang: Lang::Rust,
        current: "1.0.0",
        new: "1.1.0",
    }));
}
