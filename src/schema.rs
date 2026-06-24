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
        "clispec": "0.2",
        "name": "upd",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "A fast dependency updater for Python, Node.js, Rust, Go, Ruby, .NET, Terraform, GitHub Actions, pre-commit, and Mise/asdf projects",
        "global_args": [
            {
                "name": "paths",
                "description": "Paths to update (files or directories; default: nearest git root)",
                "type": "path[]",
                "required": false
            },
            {
                "name": "output",
                "short": "o",
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
                "short": "n",
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
                "short": "l",
                "description": "Filter by language/ecosystem (repeatable or comma-separated)",
                "type": "string[]",
                "enum": ["python", "node", "rust", "go", "ruby", "dotnet", "actions", "pre-commit", "mise", "terraform"]
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
                "description": "Update only the named package(s). Comma-separated or repeatable",
                "type": "string[]"
            },
            {
                "name": "full-precision",
                "description": "Use full version precision (e.g. 3.1.5 instead of 3.1)",
                "type": "boolean"
            },
            {
                "name": "interactive",
                "short": "i",
                "description": "Prompt before applying each update",
                "type": "boolean"
            },
            {
                "name": "lock",
                "description": "Regenerate lockfiles after updating",
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
                "short": "v",
                "description": "Verbose output",
                "type": "boolean"
            },
            {
                "name": "quiet",
                "short": "q",
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
                "short": "c",
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
                "mutating": true,
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
                    {"name": "files", "type": "array", "description": "Per-file update reports"},
                    {"name": "summary", "type": "object", "description": "Aggregate counts (files_scanned, updates_total, etc.)"}
                ]
            },
            {
                "name": "align",
                "description": "Align all packages to the highest version found in the repository. Dry-run by default; pass --apply to write",
                "mutating": true,
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
                    {"name": "packages", "type": "array", "description": "Per-package alignment records (name, highest_version, occurrences with file/line/is_misaligned)"},
                    {"name": "summary", "type": "object", "description": "Aggregate counts (files_scanned, misaligned_packages, misaligned_occurrences, packages)"}
                ]
            },
            {
                "name": "audit",
                "description": "Check dependencies for known security vulnerabilities",
                "mutating": false,
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
                        "description": "Bump vulnerable packages to the minimum version that clears all known CVEs. Read-only on its own; combined with --apply this makes `audit` MUTATING (it writes to dependency files), despite the command-level mutating:false default",
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
                    {"name": "status", "type": "string", "description": "\"ok\", \"vulnerable\", or \"incomplete\" (e.g. an offline cache miss)"},
                    {"name": "vulnerabilities", "type": "array", "description": "Vulnerable packages, each with package, ecosystem, version, and a vulnerabilities list (id, severity, fixed_version, url)"},
                    {"name": "summary", "type": "object", "description": "Aggregate counts (packages_checked, vulnerabilities, vulnerable_packages, errors)"},
                    {"name": "errors", "type": "array", "description": "Per-package audit errors (e.g. unreachable registry, offline cache miss)"}
                ]
            },
            {
                "name": "clean-cache",
                "description": "Clear the version cache",
                "mutating": true
            },
            {
                "name": "self-update",
                "description": "Update upd itself to the latest release",
                "mutating": true
            },
            {
                "name": "schema",
                "description": "Print machine-readable schema (clispec v0.2 JSON). Works offline with no config required",
                "mutating": false,
                "output_fields": [
                    {"name": "clispec", "type": "string", "description": "Spec version"},
                    {"name": "name", "type": "string", "description": "Tool name"},
                    {"name": "version", "type": "string", "description": "Tool version"},
                    {"name": "commands", "type": "array", "description": "Available commands"},
                    {"name": "global_args", "type": "array", "description": "Global flags accepted by every command"},
                    {"name": "errors", "type": "array", "description": "Error kinds with exit codes"}
                ]
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
                "description": "File read or write failed, or a required path does not exist",
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

    // The clispec v0.2 JSON Schema, vendored from clispec.dev/schema/v0.2.json.
    const CLISPEC_V0_2_SCHEMA: &str = include_str!("../fixtures/clispec-v0.2.json");

    #[test]
    fn schema_output_validates_against_clispec_v0_2() {
        let meta_schema: Value =
            serde_json::from_str(CLISPEC_V0_2_SCHEMA).expect("vendored schema must be valid JSON");
        let validator = jsonschema::draft202012::new(&meta_schema)
            .expect("vendored schema must be a valid Draft 2020-12 schema");

        let instance = build_schema();
        let errors: Vec<_> = validator.iter_errors(&instance).collect();
        assert!(
            errors.is_empty(),
            "schema output must validate against clispec v0.2: {:?}",
            errors
                .iter()
                .map(|e| format!("{}: {}", e.instance_path(), e))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn schema_has_required_top_level_fields() {
        let s = build_schema();
        assert_eq!(s["clispec"], "0.2");
        assert_eq!(s["name"], "upd");
        assert!(s["version"].is_string());
        assert!(s["commands"].is_array());
        assert!(s["global_args"].is_array());
        assert!(s["errors"].is_array());
    }

    #[test]
    fn schema_all_commands_have_mutating_marker() {
        let s = build_schema();
        let commands = s["commands"].as_array().expect("commands must be an array");
        for cmd in commands {
            let name = cmd["name"].as_str().unwrap_or("<unnamed>");
            assert!(
                cmd.get("mutating").is_some_and(|m| m.is_boolean()),
                "command '{}' must have an explicit mutating marker",
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
        let s = build_schema();
        let arg = find_global_arg(&s, "lang");
        let values: Vec<String> = arg["enum"]
            .as_array()
            .expect("--lang must have an enum of valid ecosystems")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        for eco in [
            "python",
            "node",
            "rust",
            "go",
            "ruby",
            "dotnet",
            "actions",
            "pre-commit",
            "mise",
            "terraform",
        ] {
            assert!(
                values.iter().any(|v| v == eco),
                "--lang enum must include '{eco}'; got {values:?}"
            );
        }
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
