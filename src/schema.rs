use serde_json::{Value, json};

pub fn print_schema() {
    let schema = build_schema();
    println!("{}", serde_json::to_string_pretty(&schema).unwrap());
}

pub fn build_schema_value() -> Value {
    build_schema()
}

fn build_schema() -> Value {
    json!({
        "clispec": "0.3",
        "name": "upd",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "A fast dependency updater for Python, Node.js, Rust, Go, Ruby, .NET, Docker, Terraform, GitHub Actions, pre-commit, and Mise/asdf projects",
        "output": {"tty": "text", "piped": "json"},
        "global_args": [
            {
                "name": "paths",
                "description": "Paths to update (files or directories; default: nearest git root)",
                "type": "path[]",
                "required": false
            },
            {
                "name": "output",
                "short": "-o",
                "description": "Output format. auto emits JSON when stdout is not a TTY, explicit value always wins",
                "type": "string",
                "enum": ["auto", "text", "json"],
                "default": "auto"
            },
            {
                "name": "apply",
                "description": "Apply updates to files. Without --apply (and without --interactive), runs in dry-run mode",
                "type": "boolean"
            },
            {
                "name": "yes",
                "description": "Alias for --apply: apply updates non-interactively (for scripted use)",
                "type": "boolean"
            },
            {
                "name": "dry-run",
                "short": "-n",
                "description": "Show what would change without writing any files",
                "type": "boolean"
            },
            {
                "name": "check",
                "description": "Exit 1 if updates are available, without writing any changes (CI use)",
                "type": "boolean"
            },
            {
                "name": "max-bump",
                "description": "Include updates up to and including the given bump level",
                "type": "string",
                "enum": ["patch", "minor", "major"]
            },
            {
                "name": "only-bump",
                "description": "Include only updates whose bump level exactly matches. Repeatable or comma-separated. Mutually exclusive with --max-bump",
                "type": "string[]",
                "enum": ["patch", "minor", "major"]
            },
            {
                "name": "lang",
                "short": "-l",
                "description": "Filter by language/ecosystem (repeatable or comma-separated)",
                "type": "string[]",
                "enum": ["python", "node", "rust", "go", "ruby", "dotnet", "actions", "pre-commit", "mise", "terraform", "docker", "github-releases", "annotated"]
            },
            {
                "name": "limit",
                "description": "Limit output to N items",
                "type": "integer"
            },
            {
                "name": "offset",
                "description": "Skip first N items",
                "type": "integer",
                "default": 0
            },
            {
                "name": "fields",
                "description": "Comma-separated list of fields to include in JSON output",
                "type": "string"
            },
            {
                "name": "format",
                "description": "Set output format: text (default), json, or sarif. Use --output/-o for auto-detection",
                "type": "string",
                "enum": ["text", "json", "sarif"]
            },
            {
                "name": "package",
                "description": "Update only matching packages. Accepts case-sensitive shell-style globs (*, ?, [abc]); quote globs to prevent shell expansion. Comma-separated or repeatable",
                "type": "string[]"
            },
            {
                "name": "full-precision",
                "description": "Use full version precision (e.g. 3.1.5 instead of 3.1)",
                "type": "boolean"
            },
            {
                "name": "update-action-shas",
                "description": "Update full GitHub Actions SHA pins with verified concrete version comments while preserving immutable refs. On by default; pass this only to override update_action_shas = false in .updrc.toml.",
                "type": "boolean",
                "default": true
            },
            {
                "name": "no-update-action-shas",
                "description": "Leave GitHub Actions SHA pins alone, overriding update_action_shas in .updrc.toml and the default. The pins are still reported in skipped[] with status \"not-examined\". Conflicts with --update-action-shas.",
                "type": "boolean"
            },
            {
                "name": "interactive",
                "short": "-i",
                "description": "Prompt before applying each update",
                "type": "boolean"
            },
            {
                "name": "lock",
                "description": "Regenerate lockfiles after applying changes. Honored by update and by audit --fix-audit --apply. Implied by 'audit --fix-audit --apply'; see --no-lock.",
                "type": "boolean"
            },
            {
                "name": "no-lock",
                "description": "Do not regenerate lockfiles after fixing; floor and manifest edits are reported as pending_relock, cargo-precise floors as skipped. Conflicts with --lock.",
                "type": "boolean"
            },
            {
                "name": "no-cache",
                "description": "Disable version caching",
                "type": "boolean"
            },
            {
                "name": "no-color",
                "description": "Disable colored output",
                "type": "boolean"
            },
            {
                "name": "no-ignore",
                "description": "Disable .gitignore filtering and walk every dependency file",
                "type": "boolean"
            },
            {
                "name": "verbose",
                "short": "-v",
                "description": "Verbose output",
                "type": "boolean"
            },
            {
                "name": "quiet",
                "short": "-q",
                "description": "Suppress all output except errors and warnings",
                "type": "boolean"
            },
            {
                "name": "min-age",
                "description": "Minimum release age before a version is eligible for update (e.g. 72h, 7d, 2w)",
                "type": "string"
            },
            {
                "name": "config",
                "short": "-c",
                "description": "Path to config file (default: auto-discover .updrc.toml, upd.toml, or .updrc)",
                "type": "path"
            },
            {
                "name": "show-config",
                "description": "Print the effective configuration and exit",
                "type": "boolean"
            },
            {
                "name": "insecure",
                "description": "Disable TLS certificate verification for all HTTPS requests",
                "type": "boolean"
            }
        ],
        "commands": [
            {
                "name": "update",
                "description": "Update dependencies (default when no subcommand is given). Dry-run by default; pass --apply to write",
                "effects": "non_idempotent",
                "mutating": true,
                "cardinality": "unbounded",
                "pagination": {"style": "offset", "limit_arg": "limit", "offset_arg": "offset"},
                "fields_arg": "fields",
                "args": [
                    {
                        "name": "paths",
                        "description": "Paths to update (files or directories)",
                        "type": "path[]",
                        "required": false
                    }
                ],
                "output_fields": [
                    {"name": "command", "type": "string", "description": "Always \"update\""},
                    {"name": "mode", "type": "string", "description": "\"dry-run\" or \"applied\""},
                    {"name": "files", "type": "array", "items": {"type": "object"}, "description": "Per-file update reports. Configured pyproject specifier-shape changes appear in normalized[] with their exact section identity. Verified GitHub Actions SHA updates include reference_kind, current_commit, and latest_commit; pins left alone appear in skipped[] with status (\"blocked\" for a failed safety condition, \"not-examined\" when SHA-pin updates are off) and reason. A SHA pin carrying no version comment has the release its commit belongs to read back from the repository: recovered and already current, it appears in annotations[] with package, version, commit and line, the comment being written beside the unchanged commit; recovered and behind, it is an ordinary entry in updates[]; not recoverable, it is blocked in skipped[] with reason \"unreleased-commit\" (the repository has no tag at that commit), \"floating-tag-only\" (only a moving alias such as v7 names it) or \"missing-version-comment\" (the registry has no tags to consult). A lookup that failed to answer is an error, never a skip. Updates that exist but exceed the --max-bump/--only-bump ceiling appear in capped[] with package, current, available and bump, lock-only version floors included; they are never counted as up to date and do not affect the exit code, so a run can exit 0 with work waiting in capped[]. A capped entry omits line when the update has no manifest line of its own. Each entry in updates[] may also carry method and status for lock-only version floors. Every file report also carries errors[] and warnings[]. A warning names a dependency that was checked and deliberately left as it was found, with something to say about it: a constraint that names no floor to raise (a bare ceiling, an exclusion, an npm OR range, a NuGet interval) and that the newest release has already outgrown. An error names a dependency that could not be checked at all, because its constraint could not be read or its registry lookup did not answer; any entry in errors[] exits 2"},
                    {"name": "summary", "type": "object", "description": "Aggregate counts (files_scanned, updates_total, normalized, etc.). updates_total counts only updates that were or would be written, while normalized counts configured specifier-shape rewrites. \"Is anything waiting?\" also has to read capped (held back by the bump ceiling), unfixable (a newer release upd found but has no mechanism to write) and skipped_floors (a floor upd can write but was told not to, today only a cargo-precise floor under --no-lock); the latter two are detailed per package in files[].updates[] with status \"unfixable\"/\"skipped\" and an error. All three can be non-zero while updates_total is 0 and the exit code is 0. annotations counts SHA pins whose release was written beside them without their commit moving; it is disjoint from updates_total, but unlike capped it does affect the exit code, because --apply writes these and --check must report exactly what --apply would write"},
                    {"name": "warnings", "type": "array", "items": {"type": "object"}, "description": "Run-level selection or discovery warnings, including unmatched package globs and ancestor lockfiles outside the scanned paths; warnings do not fail the command"}
                ]
            },
            {
                "name": "align",
                "description": "Align all packages to the highest version found in the repository. Dry-run by default; pass --apply to write",
                "effects": "non_idempotent",
                "mutating": true,
                "cardinality": "unbounded",
                "pagination": {"style": "offset", "limit_arg": "limit", "offset_arg": "offset"},
                "fields_arg": "fields",
                "args": [
                    {
                        "name": "paths",
                        "description": "Paths to scan and align",
                        "type": "path[]",
                        "required": false
                    }
                ],
                "output_fields": [
                    {"name": "command", "type": "string", "description": "Always \"align\""},
                    {"name": "packages", "type": "array", "items": {"type": "object"}, "description": "Per-package alignment records (name, highest_version, occurrences with file/line/is_misaligned)"},
                    {"name": "summary", "type": "object", "description": "Aggregate counts (files_scanned, misaligned_packages, misaligned_occurrences, packages)"}
                ],
                "example": {"args": ["align", "--dry-run", "Cargo.toml"]}
            },
            {
                "name": "audit",
                "description": "Check dependencies for known security vulnerabilities",
                "effects": "non_idempotent",
                "mutating": true,
                "cardinality": "unbounded",
                "pagination": {"style": "offset", "limit_arg": "limit", "offset_arg": "offset"},
                "fields_arg": "fields",
                "args": [
                    {
                        "name": "paths",
                        "description": "Paths to scan",
                        "type": "path[]",
                        "required": false
                    },
                    {
                        "name": "no-fail",
                        "description": "Exit 0 even when vulnerabilities are found",
                        "type": "boolean"
                    },
                    {
                        "name": "fix-audit",
                        "description": "Bump vulnerable packages to the minimum version that clears all known CVEs. Read-only on its own; combined with --apply this makes `audit` MUTATING (it writes to dependency files), despite the command-level mutating:false default. Implies --lock (regenerates the lockfiles of fixed manifests); pass --no-lock to skip",
                        "type": "boolean"
                    },
                    {
                        "name": "offline",
                        "description": "Use local audit cache only; do not contact OSV",
                        "type": "boolean"
                    }
                ],
                "output_fields": [
                    {"name": "command", "type": "string", "description": "Always \"audit\""},
                    {"name": "status", "type": "string", "description": "\"complete\" or \"incomplete\" (an offline cache miss or coverage warning)"},
                    {"name": "vulnerabilities", "type": "array", "items": {"type": "object"}, "description": "Vulnerable packages, each with package, ecosystem, version, id, severity, fixed_version, url, aliases (alternate ids such as CVEs, omitted when empty), and source (advisory database prefix of id, e.g. GHSA/PYSEC/GO)"},
                    {"name": "summary", "type": "object", "description": "Aggregate counts (packages_checked, vulnerabilities, vulnerable_packages, errors)"},
                    {"name": "errors", "type": "array", "items": {"type": "object"}, "description": "Per-package audit errors (e.g. unreachable registry, offline cache miss)"},
                    {"name": "warnings", "type": "array", "items": {"type": "object"}, "description": "Coverage warnings (e.g. go.mod predating go 1.17): the audit ran but could not fully cover these inputs; status becomes \"incomplete\" without a nonzero exit"},
                    {"name": "fixes", "type": "array", "items": {"type": "object"}, "description": "Fix outcomes for each vulnerable pair targeted by --fix-audit, present only under --fix-audit. Each entry: package, dependency_key? (composite key disambiguating aliased or multi-section declarations), from_version, to_version? (absent when unfixable), method? (manifest|uv-constraint|npm-override|cargo-precise), path?, status (planned|applied|pending_relock|skipped|unfixable|already_satisfied|failed|rolled_back), error? (guidance for an unfixable floor, or resolver/tool stderr for a failed or rolled-back floor)"}
                ]
            },
            {
                "name": "clean-cache",
                "description": "Clear the version cache",
                "effects": "non_idempotent",
                "mutating": true,
                "cardinality": "single",
                "stdout_schema": {}
            },
            {
                "name": "self-update",
                "description": "Update upd itself to the latest release",
                "effects": "non_idempotent",
                "mutating": true,
                "cardinality": "single",
                "stdout_schema": {}
            },
            {
                "name": "capabilities",
                "description": "Describe offline-safe CLI capabilities",
                "effects": "read_only",
                "mutating": false,
                "cardinality": "single",
                "args": [],
                "example": {"args": ["capabilities"]},
                "output_fields": [
                    {"name": "name", "type": "string"},
                    {"name": "version", "type": "string"},
                    {"name": "clispec", "type": "string"},
                    {"name": "output", "type": "array", "items": {"type": "string"}},
                    {"name": "features", "type": "array", "items": {"type": "string"}}
                ]
            },
            {
                "name": "schema",
                "description": "Print machine-readable schema (clispec v0.3 JSON). Works offline with no config required",
                "effects": "read_only",
                "mutating": false,
                "cardinality": "single",
                "stdout_schema": {"$ref": "https://clispec.dev/schema/v0.3.json"}
            }
        ],
        "outcomes": [
            {
                "code": 1,
                "name": "updates_available",
                "description": "Updates are available (dry-run mode only); the report is on stdout. Not an error. Run with --apply to write changes"
            },
            {
                "code": 6,
                "name": "vulnerabilities_found",
                "description": "Security vulnerabilities found during audit; the report is on stdout. Not an error. Use --no-fail to exit 0 instead"
            }
        ],
        "errors": [
            {
                "kind": "io_error",
                "description": "A file could not be read or written, a required path does not exist, a lockfile refresh failed, or a dependency could not be checked. A dependency-level failure (a version constraint that cannot be read, a registry lookup that did not answer) is listed in files[].errors and exits 2 without an error envelope. Exit 2 takes precedence over every other exit code, including the outcome codes",
                "exit_code": 2,
                "retryable": false
            },
            {
                "kind": "confirmation_required",
                "description": "--interactive needs a terminal on stdin to prompt with, and stdin is not one. Use --check or --dry-run to preview the updates instead",
                "exit_code": 2,
                "retryable": false
            },
            {
                "kind": "network_error",
                "description": "Network request failed (registry unreachable, timeout, etc.)",
                "exit_code": 3,
                "retryable": true
            },
            {
                "kind": "parse_error",
                "description": "Failed to parse a dependency file, a config file (.updrc.toml), or a CLI argument",
                "exit_code": 4,
                "retryable": false
            },
            {
                "kind": "conflict",
                "description": "Version conflict detected between files",
                "exit_code": 5,
                "retryable": false
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The clispec v0.3 JSON Schema, vendored from clispec.dev/schema/v0.3.json.
    const CLISPEC_V0_3_SCHEMA: &str = include_str!("../fixtures/clispec-v0.3.json");

    #[test]
    fn schema_output_validates_against_clispec_v0_3() {
        let meta_schema: Value =
            serde_json::from_str(CLISPEC_V0_3_SCHEMA).expect("vendored schema must be valid JSON");
        let validator = jsonschema::draft202012::new(&meta_schema)
            .expect("vendored schema must be a valid Draft 2020-12 schema");

        let instance = build_schema();
        let errors: Vec<_> = validator.iter_errors(&instance).collect();
        assert!(
            errors.is_empty(),
            "schema output must validate against clispec v0.3: {:?}",
            errors
                .iter()
                .map(|e| format!("{}: {}", e.instance_path(), e))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn schema_has_required_top_level_fields() {
        let s = build_schema();
        assert_eq!(s["clispec"], "0.3");
        assert_eq!(s["name"], "upd");
        assert!(s["version"].is_string());
        assert!(s["commands"].is_array());
        assert!(s["global_args"].is_array());
        assert!(s["errors"].is_array());
    }

    #[test]
    fn schema_all_commands_have_effects_and_cardinality() {
        let s = build_schema();
        let commands = s["commands"].as_array().expect("commands must be an array");
        for cmd in commands {
            let name = cmd["name"].as_str().unwrap_or("<unnamed>");
            assert!(
                cmd.get("mutating").is_some_and(|m| m.is_boolean()),
                "command '{}' must have an explicit mutating marker",
                name
            );
            assert!(
                cmd.get("effects").is_some_and(|e| e.is_string()),
                "command '{}' must declare effects",
                name
            );
            assert!(
                cmd.get("cardinality").is_some_and(|c| c.is_string()),
                "command '{}' must declare cardinality",
                name
            );
        }
    }

    #[test]
    fn schema_all_errors_have_exit_code() {
        let s = build_schema();
        let errors = s["errors"].as_array().expect("errors must be an array");
        for err in errors {
            let kind = err["kind"].as_str().unwrap_or("<unnamed>");
            assert!(
                err.get("exit_code").is_some_and(|c| c.is_u64()),
                "error kind '{}' must have an exit_code",
                kind
            );
        }
    }

    #[test]
    fn schema_declares_updates_available_outcome_with_code_1() {
        let s = build_schema();
        let outcomes = s["outcomes"].as_array().expect("outcomes must be an array");
        let updates_available = outcomes
            .iter()
            .find(|o| o["name"].as_str() == Some("updates_available"))
            .expect("must declare an 'updates_available' outcome");
        assert_eq!(
            updates_available["code"].as_u64(),
            Some(1),
            "updates_available must map to exit code 1 (the dry-run signal)"
        );
        let errors = s["errors"].as_array().expect("errors must be an array");
        assert!(
            !errors
                .iter()
                .any(|e| e["kind"].as_str() == Some("updates_available")),
            "updates_available is an outcome, not an error kind"
        );
        for outcome in outcomes {
            let code = outcome["code"].as_u64().expect("outcome must have a code");
            assert!(
                !errors.iter().any(|e| e["exit_code"].as_u64() == Some(code)),
                "outcome code {code} must not overlap with error exit codes"
            );
        }
    }

    #[test]
    fn schema_declares_conflict_error_kind() {
        let s = build_schema();
        let errors = s["errors"].as_array().expect("errors must be an array");
        assert!(
            errors
                .iter()
                .any(|e| e["kind"].as_str() == Some("conflict")),
            "schema must declare a 'conflict' error kind"
        );
    }

    /// `errors[]` is the finite set of kinds a consumer writes handlers
    /// against, so a kind the binary emits without declaring here reaches that
    /// consumer as a failure it has no branch for. The literal envelopes are
    /// what drift; the three kinds the fatal classifier picks between reach the
    /// envelope through a variable and are declared with them.
    #[test]
    fn schema_declares_every_error_kind_the_binary_emits() {
        const MAIN_SOURCE: &str = include_str!("main.rs");
        let s = build_schema();
        let declared: Vec<&str> = s["errors"]
            .as_array()
            .expect("errors must be an array")
            .iter()
            .filter_map(|e| e["kind"].as_str())
            .collect();

        let mut emitted: Vec<&str> = MAIN_SOURCE
            .split("\"kind\": \"")
            .skip(1)
            .filter_map(|tail| tail.split('"').next())
            .filter(|kind| !kind.is_empty())
            .collect();
        emitted.sort_unstable();
        emitted.dedup();
        assert!(
            !emitted.is_empty(),
            "the scan must find the error envelopes it is guarding"
        );

        for kind in emitted {
            assert!(
                declared.contains(&kind),
                "error kind '{kind}' is emitted by the binary but not declared in errors[]; declared: {declared:?}"
            );
        }
    }

    /// Helper: find a command by name.
    fn find_command<'a>(s: &'a Value, name: &str) -> &'a Value {
        s["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("command '{name}' must exist"))
    }

    fn find_global_arg<'a>(s: &'a Value, name: &str) -> &'a Value {
        s["global_args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("global arg '{name}' must exist"))
    }

    fn output_field_names(cmd: &Value) -> Vec<String> {
        cmd["output_fields"]
            .as_array()
            .map(|fs| {
                fs.iter()
                    .filter_map(|f| f["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn schema_audit_output_fields_match_actual_json() {
        // The audit JSON document has top-level keys: command, status, errors,
        // vulnerabilities (the list), summary. The schema must describe these and
        // must NOT advertise the non-existent items/changed/packages_checked keys.
        let s = build_schema();
        let cmd = find_command(&s, "audit");
        let names = output_field_names(cmd);
        for expected in ["command", "status", "vulnerabilities", "summary"] {
            assert!(
                names.iter().any(|n| n == expected),
                "audit output_fields must include '{expected}'; got {names:?}"
            );
        }
        for stale in ["items", "changed", "packages_checked"] {
            assert!(
                !names.iter().any(|n| n == stale),
                "audit output_fields must not advertise the non-existent '{stale}' key; got {names:?}"
            );
        }
    }

    #[test]
    fn schema_align_has_output_fields() {
        let s = build_schema();
        let cmd = find_command(&s, "align");
        let names = output_field_names(cmd);
        for expected in ["command", "packages", "summary"] {
            assert!(
                names.iter().any(|n| n == expected),
                "align output_fields must include '{expected}'; got {names:?}"
            );
        }
    }

    #[test]
    fn schema_lang_arg_enumerates_valid_ecosystems() {
        use crate::updater::Lang;
        use clap::ValueEnum;

        let s = build_schema();
        let arg = find_global_arg(&s, "lang");
        let mut values: Vec<String> = arg["enum"]
            .as_array()
            .expect("--lang must have an enum of valid ecosystems")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        values.sort();

        let mut expected: Vec<String> = Lang::value_variants()
            .iter()
            .map(|lang| {
                lang.to_possible_value()
                    .expect("every Lang variant must be selectable on the command line")
                    .get_name()
                    .to_string()
            })
            .collect();
        expected.sort();

        assert_eq!(
            values, expected,
            "the --lang enum in the schema must list exactly the Lang variants clap accepts"
        );
    }

    #[test]
    fn schema_only_bump_arg_has_enum() {
        let s = build_schema();
        let arg = find_global_arg(&s, "only-bump");
        let values: Vec<String> = arg["enum"]
            .as_array()
            .expect("--only-bump must have an enum")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert_eq!(
            values,
            vec!["patch", "minor", "major"],
            "--only-bump enum must match --max-bump"
        );
    }

    #[test]
    fn schema_global_args_include_output_flag() {
        let s = build_schema();
        let global_args = s["global_args"]
            .as_array()
            .expect("global_args must be an array");
        let output_arg = global_args
            .iter()
            .find(|a| a["name"].as_str() == Some("output"))
            .expect("global_args must include 'output' flag");
        assert_eq!(
            output_arg["default"].as_str(),
            Some("auto"),
            "output flag must default to 'auto'"
        );
    }
}
