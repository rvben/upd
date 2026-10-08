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
        "description": "A fast dependency updater for Python, Node.js, Rust, Go, Ruby, .NET, Docker, Terraform, Nix flakes, GitHub Actions, pre-commit, and Mise/asdf projects",
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
                "description": "Prevent writes. Update commands report available changes; lock-refresh lists candidate lockfiles without resolving them",
                "type": "boolean"
            },
            {
                "name": "check",
                "description": "Exit 1 if updates are available, without writing any changes (CI use)",
                "type": "boolean"
            },
            {
                "name": "fail-on-blocked",
                "description": "For update or the default invocation only: with --check, also exit 1 when an unsafe or unverifiable dependency is blocked; cooldown, bump limits and unexamined pins do not count. Rejected on other subcommands, with --interactive or with --show-config",
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
                "name": "strict-bump",
                "description": "Requires --only-bump. Write only registry-selected versions at the selected levels; hold every other write (configured pins, flake revisions, unanchored and reshape rewrites, the rvben/upd self-pin, SHA-pin release comments) in capped[] with reason \"strict-bump\"",
                "type": "boolean"
            },
            {
                "name": "lang",
                "short": "-l",
                "description": "Filter by language/ecosystem (repeatable or comma-separated)",
                "type": "string[]",
                "enum": ["python", "node", "rust", "go", "ruby", "dotnet", "gradle", "actions", "pre-commit", "mise", "terraform", "docker", "nix", "github-releases", "annotated"]
            },
            {
                "name": "exclude-lang",
                "description": "Leave ecosystems out after --lang and the [ecosystems] configuration, annotated lines included (repeatable or comma-separated)",
                "type": "string[]",
                "enum": ["python", "node", "rust", "go", "ruby", "dotnet", "gradle", "actions", "pre-commit", "mise", "terraform", "docker", "nix", "github-releases", "annotated"]
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
                "description": "Regenerate lockfiles after applying changes. Honored by update and by audit --fix-audit --apply. Under update the refresh keeps to the cooldown and is read back (see lockfile_cooldown and lockfile_holds); audit --fix-audit moves a vulnerable package to the release that fixes it however young that release is, so its refreshes are not gated and nothing is held; under a cooldown they are read back and every young release a refresh locked besides the fixes themselves is reported (see audit's lockfile_cooldown). Implied by 'audit --fix-audit --apply'; see --no-lock. The refresh is a transaction per manifest group: when a lockfile cannot be refreshed, the manifest and every lockfile it owns are restored to their pre-run bytes, the file's errors[] carries an entry of kind \"lockfile\" naming what was put back, its updates[], pinned[] and normalized[] entries carry status \"rolled_back\" and drop out of the summary counts, and the exit code is 2. When a file in the group cannot be put back, the directory is neither at its pre-run bytes nor consistently updated, so those entries carry status \"failed\" instead, still outside the summary counts, and the error names the file that was not restored. Groups whose refresh succeeded keep their edits.",
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
                "name": "min-age-floor",
                "description": "Raise every configured cooldown window, and the zero default, to at least this release age; longer configured windows stay. Conflicts with --min-age",
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
                "name": "annotations validate",
                "description": "Validate annotation syntax and checksum relationships offline. Uses update discovery rules and include/exclude configuration; checks structural validity, not published release metadata. Exit 2 on validation or I/O errors",
                "effects": "read_only", "mutating": false, "cardinality": "single",
                "args": [{"name": "paths", "type": "path[]", "required": false}],
                "output_fields": [
                    {"name": "command", "type": "string"},
                    {"name": "valid", "type": "boolean"},
                    {"name": "files", "type": "array", "items": {"type": "object"}, "description": "File paths, version/checksum annotation counts and diagnostics with one-based lines and messages"},
                    {"name": "summary", "type": "object", "description": "files, versions, checksums and errors totals"}
                ],
                "example": {"args": ["annotations", "validate", "Dockerfile"]}
            },
            {
                "name": "annotations init",
                "description": "Print a linked version/checksum snippet from an exact GitHub release asset URL. Provide exactly one of --checksum (offline, supplied SHA-256) or --resolve-checksum (online, published digest or explicit --checksums manifest). Never writes files. Use --output text for a directly pasteable snippet",
                "effects": "read_only", "mutating": false, "cardinality": "single",
                "args": [
                    {"name": "asset_url", "type": "string", "required": true},
                    {"name": "checksum", "type": "string", "required": false, "description": "Published SHA-256 supplied offline; required unless resolve-checksum is set, mutually exclusive with it"},
                    {"name": "resolve-checksum", "type": "boolean", "default": false, "description": "Opt in to online checksum resolution for the exact URL tag and asset; mutually exclusive with checksum"},
                    {"name": "asset-template", "type": "string", "required": false, "description": "Override filename inference; only {version} and {tag}, and must expand exactly to the URL asset"},
                    {"name": "name", "type": "string", "required": false},
                    {"name": "syntax", "type": "string", "enum": ["shell", "docker", "toml", "yaml", "javascript"], "default": "shell"},
                    {"name": "checksums", "type": "string", "required": false}
                ],
                "output_fields": [
                    {"name": "command", "type": "string"},
                    {"name": "package", "type": "string"},
                    {"name": "version", "type": "string"},
                    {"name": "tag", "type": "string"},
                    {"name": "asset", "type": "string"},
                    {"name": "asset_template", "type": "string"},
                    {"name": "checksum", "type": "string"},
                    {"name": "checksum_source", "type": "string", "description": "supplied, github-asset-digest, or the explicitly selected manifest filename"},
                    {"name": "snippet", "type": "string"}
                ]
            },
            {
                "name": "lock-refresh",
                "description": "Refresh uv, npm, and Cargo lockfiles within existing manifest constraints. Without --apply, list candidates without running package managers",
                "effects": "non_idempotent",
                "mutating": true,
                "cardinality": "unbounded",
                "pagination": {"style": "offset", "limit_arg": "limit", "offset_arg": "offset"},
                "fields_arg": "fields",
                "args": [
                    {"name": "paths", "description": "Project directories or supported lockfiles", "type": "path[]", "required": false}
                ],
                "output_fields": [
                    {"name": "lockfile", "type": "string", "description": "Lockfile path"},
                    {"name": "status", "type": "string", "description": "planned, skipped, unchanged, refreshed, or failed"},
                    {"name": "changes", "type": "array", "items": {"type": "object"}, "description": "Resolved package version changes with from, to, and bump"},
                    {"name": "error", "type": "string", "description": "Failure reason, present only when status is failed"}
                ],
                "example": {"args": ["lock-refresh", "--apply", "."]}
            },
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
                    {"name": "files", "type": "array", "items": {"type": "object"}, "description": "Per-file update reports. Configured pyproject specifier-shape changes appear in normalized[] with their exact section identity. Verified GitHub Actions SHA updates include reference_kind, current_commit, and latest_commit; pins left alone appear in skipped[] with status (\"blocked\" for a failed safety condition, \"not-examined\" when SHA-pin updates are off) and reason. A SHA pin carrying no version comment has the release its commit belongs to read back from the repository: recovered and already current, it appears in annotations[] with package, version, commit and line, the comment being written beside the unchanged commit; recovered and behind, it is an ordinary entry in updates[]; not recoverable, it is blocked in skipped[] with reason \"unreleased-commit\" (the repository has no tag at that commit), \"floating-tag-only\" (only a moving alias such as v7 names it), \"no-releases\" (the repository publishes no release tag at all, so no pin to it can name one; ignoring the action is the remedy) or \"missing-version-comment\" (the registry has no tags to consult). A lookup that failed to answer is an error, never a skip. A pre-commit or prek rev upd cannot read as a version tag is left exactly as it is: a full 40-character commit SHA is not-examined in skipped[] with reason \"sha-pinned-rev\" because frozen commit-pin updates are unsupported; an abbreviated SHA, a branch such as main, a moving pointer such as 1.x or a prefixed tag such as black-24.3.0 is blocked with reason \"unrecognized-rev\". Neither costs a revision lookup, and a configured [pin] does not override it. A flake.lock input that moved upstream is an entry in updates[] with bump \"revision\": from and to are short commit hashes, and the --max-bump/--only-bump ceilings never hold it back. Under a cooldown an input moves only once its locked commit is older than the window, and then to the newest commit. A flake input upd cannot resolve (anything but github: and gitlab:) is left alone in skipped[] with status \"not-examined\" and reason \"unsupported-flake-input\". Updates that exist but exceed the --max-bump/--only-bump ceiling appear in capped[] with package, current, available and bump, lock-only version floors included; they are never counted as up to date and do not affect the exit code, so a run can exit 0 with work waiting in capped[]. Under --strict-bump, capped[] also holds every write that is not a registry-selected version at a selected level, each with reason \"strict-bump\" and write naming its kind (\"pin\", \"revision\", \"unanchored\", \"reshape\", \"self-pin\" or \"annotation\"); bump is \"revision\" for a flake revision and absent for a reshape, an annotation or an unanchored rewrite, none of which steps from one version to another. Without --strict-bump, capped entries carry neither reason nor write. A capped entry omits line when the update has no manifest line of its own. Each entry in updates[] may also carry method and status for lock-only version floors. Under --lock, a file whose lockfile refresh failed is restored to its pre-run bytes and reports every write it had received with status \"rolled_back\" (in updates[], pinned[] and normalized[]) beside an errors[] entry of kind \"lockfile\" that names the refresh error and what was put back. When a file in that directory could not be put back, every write in the directory carries status \"failed\" instead and the error names the file that was not restored; a failed write is not counted as applied either. Every file report also carries errors[] and warnings[]. A warning names a dependency that was checked and deliberately left as it was found, with something to say about it: a constraint that names no floor to raise (a bare ceiling, an exclusion, an npm OR range, a NuGet interval) and that the newest release has already outgrown. An error names a dependency that could not be checked at all, because its constraint could not be read or its registry lookup did not answer; any entry in errors[] exits 2"},
                    {"name": "summary", "type": "object", "description": "Aggregate counts (files_scanned, updates_total, normalized, etc.). updates_total counts only updates that were or would be written and are still in the file: a write that a failed lockfile refresh rolled back (status \"rolled_back\") or left in a directory it could not put back (status \"failed\") is not counted, and neither is its file in files_with_changes. normalized counts configured specifier-shape rewrites. updates_revision counts flake.lock input revisions and appears only when non-zero. \"Is anything waiting?\" also has to read capped (held back by the bump ceiling), unfixable (a newer release upd found but has no mechanism to write) and skipped_floors (a floor upd can write but was told not to, today only a cargo-precise floor under --no-lock); the latter two are detailed per package in files[].updates[] with status \"unfixable\"/\"skipped\" and an error. All three can be non-zero while updates_total is 0 and the exit code is 0. annotations counts SHA pins whose release was written beside them without their commit moving; it is disjoint from updates_total, but unlike capped it does affect the exit code, because --apply writes these and --check must report exactly what --apply would write"},
                    {"name": "warnings", "type": "array", "items": {"type": "object"}, "description": "Run-level selection or discovery warnings, including unmatched package globs and ancestor lockfiles outside the scanned paths, and lockfile refreshes that ran without the cooldown (a gated resolution that failed and was rerun without it, a uv that resolved again once the cutoff was removed, or a tool with no release-age setting whose lockfile upd cannot read back), and entries a refresh introduced that could not be checked against the cooldown (the registry lookup failed, the registry does not list the version or lists no publish date for it, or the entry comes from a registry upd does not read release dates from: a Python index or npm registry other than the configured ones, a Cargo registry other than crates.io, a gem server other than rubygems.org); warnings do not fail the command"},
                    {"name": "lockfile_cooldown", "type": "array", "items": {"type": "object"}, "description": "Present only when it has entries, which needs a lockfile refresh under a cooldown: a --lock refresh or a version floor's relock, under update. Releases a lockfile refresh locked although they were published inside the cooldown, each with lockfile, package, version, published_at, cooldown and, when upd tried to move it back and could not, note. Every refreshed lockfile upd can read (package-lock.json, uv.lock, poetry.lock, Cargo.lock, Gemfile.lock) has each entry the refresh introduced looked up, including one refreshed under the tool's own release-age gate, which exempts packages and admits releases with no publish date; a release moved to another Python index or npm registry, a crate moved to another Cargo registry and a gem moved to another gem server, each at the same version, count as introduced. A version floor upd wrote to Cargo.lock with cargo update --precise is never moved back, so one inside the cooldown appears here with the note that it is the version floor the run chose. Each entry is also counted in summary.warnings"},
                    {"name": "lockfile_holds", "type": "array", "items": {"type": "object"}, "description": "Present only when it has entries, which needs a lockfile refresh under a cooldown: a --lock refresh or a version floor's relock, under update. Crates.io entries a Cargo.lock refresh locked inside the cooldown and upd moved back with cargo update --precise, each with lockfile, package, from (the release the refresh locked), to (the release it is held at), published_at (when from was published) and cooldown. Companion crates a version floor's cargo update --precise locked are held the same way. Each hold is read back, and undone when cargo did not move the crate, moved another crate below the release the lockfile held before the run, or moved a crate below a version floor the run chose or a version [pin] sets exactly. A hold a later hold moved the crate away from is dropped the same way, since the lockfile the run leaves behind no longer carries it. The entry then appears in lockfile_cooldown with the reason as its note. When Cargo.lock cannot be put back after such a hold, upd stops holding in that lockfile, the errors[] of the Cargo.toml it belongs to carries the error, and the exit code is 2"}
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
                    {"name": "warnings", "type": "array", "items": {"type": "object"}, "description": "Coverage warnings (e.g. go.mod predating go 1.17): the audit ran but could not fully cover these inputs; status becomes \"incomplete\" without a nonzero exit. Under --fix-audit --apply this also names lockfile entries a fix refresh introduced whose age could not be checked against the cooldown; those do not change status"},
                    {"name": "fixes", "type": "array", "items": {"type": "object"}, "description": "Fix outcomes for each vulnerable pair targeted by --fix-audit, present only under --fix-audit. Each entry: package, ecosystem (the OSV ecosystem, as the package's vulnerabilities entries name it), dependency_key? (composite key disambiguating aliased or multi-section declarations), from_version, to_version? (absent when unfixable; for a PyPI, npm or crates.io package that resolves from a registry upd is configured with, as its lockfile records, that registry is asked first: the lowest installable release at or above the version the advisories name as fixed that none of them still covers, the fixed version itself when published or else a stable release above it, and unfixable when every such release is still affected or none is published; the version as named, with a stderr note, when the package resolves from a registry upd is not configured with or from more than one, an unlocked Python pin may resolve from several indexes, the registry cannot be asked, or the run is --offline), method? (manifest|uv-constraint|npm-override|cargo-precise), path?, status (planned|applied|pending_relock|skipped|unfixable|already_satisfied|failed|rolled_back|blocked; rolled_back means the directory's relock failed and every file in it is back at its pre-run bytes; failed means the write itself failed, or the relock failed and a file could not be restored, in which case every written fix in that directory is failed; blocked means a cargo-precise floor was rejected because another crate's own manifest requirement on the package excludes the version being pinned to - not an error, other fixes in the run still apply; a blocked, skipped or unfixable entry, or an already_satisfied one under --no-lock (the lockfile is not regenerated, so it still records the vulnerable release), leaves its vulnerability unresolved, so the process exits 6 unless a genuine error (2) or a dry run with planned fixes (1) outranks it, or --no-fail is given), error? (guidance for an unfixable floor, resolver/tool stderr for a failed or rolled-back floor followed by the name of any file that was not restored, or the constraining crate and its requirement for a blocked floor, e.g. \"ratatui-core requires lru ^0.16\")"},
                    {"name": "lockfile_cooldown", "type": "array", "items": {"type": "object"}, "description": "Present only when it has entries, which needs --fix-audit --apply refreshing a lockfile under a cooldown (--min-age, or the cooldown the configuration governing the fixed file sets). A fix refresh is never held to the cooldown, since holding it back could undo the fix; the lockfile is read back instead, and every release it locked inside the cooldown besides the fixes themselves appears here, each with lockfile, package, version, published_at and cooldown. The fixes are never listed, however young. These entries do not change the exit code. Entries that could not be checked are named in warnings, as is a run under --offline, which checks no lockfile"}
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
                "name": "gitlab run",
                "description": "Run one GitLab CI dependency update for the current project: update dependencies on the rolling automation branch, push it with a lease, and create, refresh or close its merge request. Configured by the GitLab CI job environment: requires UPD_GITLAB_TOKEN, CI_API_V4_URL, CI_DEFAULT_BRANCH, CI_PROJECT_DIR, CI_PROJECT_ID, CI_PROJECT_PATH and CI_SERVER_URL; reads UPD_BRANCH, UPD_PATHS, UPD_LANGS, UPD_PACKAGES, UPD_MIN_AGE, UPD_MAX_BUMP, UPD_LOCK, UPD_PREPARE_COMMAND, UPD_VALIDATION_COMMAND, UPD_COMMIT_MESSAGE, UPD_MR_TITLE, UPD_GIT_NAME, UPD_GIT_EMAIL, UPD_AUTO_MERGE, UPD_SECURITY_REMEDIATION, UPD_MAJOR_MR, UPD_MAJOR_BRANCH and UPD_MAJOR_COMMIT_MESSAGE. Unless UPD_SECURITY_REMEDIATION=false, the update is preceded by upd audit --fix-audit, which moves every dependency with a published advisory to the lowest release that resolves it whatever UPD_MIN_AGE, UPD_MAX_BUMP and UPD_PACKAGES allow (with UPD_LOCK=false fixes change manifests only and are reported pending a relock); a security fix that fails stops the run before the update, and one a requirement blocks, or an advisory no release resolves, is listed in the merge request as needing attention. With UPD_MAJOR_MR=true (which needs UPD_MAX_BUMP set to minor or patch) a second lane then runs on UPD_MAJOR_BRANCH, proposing only the major-version upgrades the ceiling held (updater run with --only-bump major --strict-bump) in a merge request upd never merges: auto-merge does not apply to it and is cancelled when found armed. A lane that fails does not stop the other; the exit code is that of the first failure, ordinary lane first. The token is passed only to upd's own git and API calls, never to the prepare command, the validation command or the updater. With --dry-run the update and its checks still run, but nothing is pushed and nothing is written to GitLab; the outcome names what would have happened. Progress goes to stderr",
                "effects": "non_idempotent",
                "mutating": true,
                "cardinality": "single",
                "args": [],
                "output_fields": [
                    {"name": "command", "type": "string", "description": "Always \"gitlab run\""},
                    {"name": "branch", "type": "string", "description": "The rolling automation branch"},
                    {"name": "outcome", "type": "string", "description": "\"clean\" (nothing to propose or clean up), \"closed\" (nothing to propose; the obsolete merge request and/or branch was removed), \"published\" (the update is on the branch and its merge request created or refreshed), \"paused\" (the branch holds commits automation did not write and was left untouched), or under --dry-run \"would_publish\", \"would_close\" or \"would_pause\" in place of the last three. With the major lane enabled, also \"failed\" (this lane failed; see error)"},
                    {"name": "merge_request", "type": "string", "description": "Web URL of the merge request; present for published and paused, and for closed and would_close when one was or would be closed (null otherwise)"},
                    {"name": "created", "type": "boolean", "description": "published only: whether the merge request was created by this run"},
                    {"name": "pushed", "type": "boolean", "description": "published only: whether this run pushed a new commit; false when the branch already held exactly this update, on the current default branch, and was left as it was"},
                    {"name": "commit", "type": "string", "description": "published only: the commit the merge request proposes"},
                    {"name": "auto_merge", "type": "string", "description": "published only: \"enabled\" (bound to that commit), \"disabled\" (an earlier auto-merge was cancelled) or \"off\""},
                    {"name": "branch_deleted", "type": "boolean", "description": "closed only: whether the rolling branch was deleted"},
                    {"name": "notice_added", "type": "boolean", "description": "paused only: whether this run added the pause notice to the merge request description"},
                    {"name": "title", "type": "string", "description": "would_publish only: the merge request title the update would be published under"},
                    {"name": "push", "type": "boolean", "description": "would_publish only: whether a commit would be pushed; false when the branch already holds this update"},
                    {"name": "delete_branch", "type": "boolean", "description": "would_close only: whether the rolling branch would be deleted"},
                    {"name": "error", "type": "object", "description": "failed only: the lane's error, with kind, message and exit_code"},
                    {"name": "security", "type": "object", "description": "Present when the security step ran (never on the major lane): fixes (dependencies moved to a release resolving their advisories, pending_relock included), pending_relock (fixes that changed a manifest and await lockfile regeneration), blocked (fixes a requirement excluded), skipped (fixes that need UPD_LOCK=true, including a manifest that already requires the fix while the lockfile is not regenerated), not_applied (fixes that exist but were not written, such as a package the configuration ignores or pins below the fix), unfixable (advisories no release resolves), advisories (distinct advisories resolved, not counting those of a reintroduced dependency), young (releases a fix's relock locked inside the freshness window besides the fixes, left locked and listed for review; present only when non-zero) and reintroduced (fixed dependencies an audit of the tree the update left finds vulnerable again; that audit runs only when the update changed the tree the fixes left, and for a fix awaiting lockfile regeneration does not count the vulnerable version the security step found; present only when non-zero) and audit_warnings (warnings the security step's audit and the audit of the updated tree reported about what they could not check, such as a release whose age the registry could not tell, each counted once and listed for review; present only when non-zero)"},
                    {"name": "major", "type": "object", "description": "Present only with the major lane enabled: the major lane's result, with the fields above except command, for UPD_MAJOR_BRANCH"}
                ]
            },
            {
                "name": "gitlab org run",
                "description": "Run the GitLab dependency update for every project in a group that opted in, from one central CI job. Lists the group's projects (subgroups included), skips (with reason) the central project itself, projects matching UPD_EXCLUDE, and archived, pending-deletion, empty, repository-disabled and branchless projects, and processes a project only when the configuration file on its default branch commit sets [automation] dependency_updates = true; each such project then gets the same rolling branch and merge request as gitlab run. Auto-merge is enabled only when both UPD_AUTO_MERGE and the project's [automation] auto_merge allow it. The ecosystem nix is always left out, and cannot be selected through UPD_LANGS. Configured by the environment: requires UPD_GITLAB_TOKEN, CI_SERVER_URL and UPD_GROUP; reads CI_API_V4_URL, CI_PROJECT_ID (the central project, which is skipped), UPD_EXCLUDE (space-separated path globs), UPD_LANGS, UPD_MIN_AGE (a floor under each project's configured cooldown), UPD_MAX_BUMP, UPD_BRANCH, UPD_COMMIT_MESSAGE, UPD_GIT_NAME, UPD_GIT_EMAIL, UPD_AUTO_MERGE, UPD_MAJOR_MR, UPD_MAJOR_BRANCH, UPD_MAJOR_COMMIT_MESSAGE, UPD_CONCURRENCY (1 to 16, default 4), UPD_SHARD (k/n: only the projects whose id modulo n is k-1, so n jobs with shards 1/n to n/n cover the group once) and UPD_LOCK_HANDED_OFF (comma-separated ids the lock pipeline gave to their own jobs, skipped with reason handed_off; set by gitlab org plan's child pipeline and needs UPD_LOCK=true). A project gets the gitlab run major lane only when both UPD_MAJOR_MR and the project's [automation] major_mr allow it; the lane runs even when the project's ordinary lane failed. The token is never passed to the updater. A project that fails is reported and the others still run; the command then exits 2. With --dry-run nothing is pushed and nothing is written to GitLab. Progress goes to stderr, and in JSON mode a one-line summary is written to stderr before the report",
                "effects": "non_idempotent",
                "mutating": true,
                "cardinality": "single",
                "args": [],
                "output_fields": [
                    {"name": "command", "type": "string", "description": "Always \"gitlab org run\""},
                    {"name": "group", "type": "string", "description": "The group that was listed"},
                    {"name": "branch", "type": "string", "description": "The rolling automation branch used in every project"},
                    {"name": "dry_run", "type": "boolean", "description": "Whether the run was a dry run"},
                    {"name": "shard", "type": "string", "description": "Present only with UPD_SHARD set: the shard this run covered, as k/n"},
                    {"name": "major_branch", "type": "string", "description": "Present only with the major lane enabled: the major lane's rolling branch in every project"},
                    {"name": "counts", "type": "object", "description": "Number of projects listed (projects) and in each state (skipped, not_opted_in, config_invalid, processed, deferred, failed). With the major lane enabled, also major_failed: projects whose major lane failed, which make the command exit 2"},
                    {"name": "projects", "type": "array", "items": {"type": "object"}, "description": "One entry per listed project with id, path and state. skipped carries reason (central_project, excluded, handed_off, archived, pending_deletion, repository_disabled, empty_repository or no_default_branch); deferred carries reason (in a lock run, a project that consented to lock = true after gitlab org plan planned the run: the next run gives it lock jobs) and not_opted_in carries reason; config_invalid carries config (the file) and message; processed carries the gitlab run outcome fields (outcome, merge_request, ...); failed carries error with kind, message and exit_code. config_invalid and failed entries make the command exit 2. A project whose major lane ran also carries major: that lane's gitlab run result, without command, including outcome \"failed\" with its error. A project that opted in also carries security_remediation: enabled (whether its ordinary lane applies security fixes, which needs both the organization and the project to allow them) and, when false, reason (the side that did not)"}
                ]
            },
            {
                "name": "gitlab org plan",
                "description": "Lock mode, planning job of an organization run: holds the token but runs no project code and clones nothing. Requires UPD_LOCK=true, CI_PIPELINE_ID, CI_JOB_NAME, UPD_ORGANIZATION_JOB (equal to CI_JOB_NAME), UPD_IMAGE and UPD_LOCK_RUNNER_TAGS (comma-separated tags of the runners reserved for lock jobs), and reads UPD_LOCK_IMAGE (default UPD_IMAGE), UPD_RUNNER_TAGS and UPD_ENVIRONMENT (the environment the token is scoped to, default upd-organization), besides everything gitlab org run reads. Unless --dry-run, refuses (exit 2) a central project (CI_PROJECT_ID, required) that lets CI/CD job tokens push to it, or that does not show the token whether it does (GitLab shows it only to the Maintainer role), since every lock job holds that project's job token. Lists the group (or its UPD_SHARD shard) as gitlab org run does and reads each candidate project's opt-in through the API. Writes .upd-ci/upd-lock-pipeline.yml, a child pipeline with gitlab org prepare, lock-worker and publish jobs for each lane of every project whose opt-in sets lock = true (the major lane too when both UPD_MAJOR_MR and the project allow it), and one organization-run job that runs gitlab org run for the rest with UPD_LOCK_HANDED_OFF naming those projects. Only prepare, publish and organization-run declare the environment; lock jobs run on the lock runners and lock image without it. Copies this executable to .upd-ci/bin/upd, which every child job runs. Each child job sets the organization settings the plan read in its own script, above any CI/CD variable of the central project or its groups, and unsets those the plan did not have; never UPD_GITLAB_TOKEN. With --dry-run, the child pipeline is a single organization-run job running gitlab org run --dry-run. A project whose opt-in cannot be read is left to organization-run and listed under unplanned. Refuses (exit 2, writing no child pipeline) one whose artifact archive would exceed the 5 MiB GitLab includes a pipeline from by default; UPD_SHARD splits such a group. Progress goes to stderr",
                "effects": "non_idempotent",
                "mutating": true,
                "cardinality": "single",
                "args": [],
                "output_fields": [
                    {"name": "command", "type": "string", "description": "Always \"gitlab org plan\""},
                    {"name": "group", "type": "string", "description": "The group that was listed"},
                    {"name": "dry_run", "type": "boolean", "description": "Whether the child pipeline is a dry run"},
                    {"name": "shard", "type": "string", "description": "Present only with UPD_SHARD set: the shard planned, as k/n"},
                    {"name": "pipeline", "type": "string", "description": "The child pipeline file, .upd-ci/upd-lock-pipeline.yml"},
                    {"name": "counts", "type": "object", "description": "projects (listed in the group or shard), handed_off (projects given lock jobs), lanes (their lanes), jobs (child pipeline jobs: three per lane and organization-run) and unplanned"},
                    {"name": "handed_off", "type": "array", "items": {"type": "object"}, "description": "One entry per project given lock jobs, with id, path and lanes (ordinary, and major when that lane runs)"},
                    {"name": "unplanned", "type": "array", "items": {"type": "object"}, "description": "One entry per project whose opt-in could not be read, with id, path and error (kind, message and exit_code); organization-run handles it"}
                ],
                "example": {"args": ["gitlab", "org", "plan", "--output", "json"]}
            },
            {
                "name": "gitlab org prepare",
                "description": "Lock mode, first of three jobs for one project lane: the job that holds the token. Opens the project as gitlab org run does (the project must be in UPD_GROUP and opted in), then applies the security fixes and the update to manifests only. The lane is finished here, as gitlab org run would, unless both UPD_LOCK=true and the project's [automation] lock = true allow relocking and a lockfile needs it; then the lockfile kinds are checked (uv.lock, package-lock.json, npm-shrinkwrap.json and Cargo.lock are supported; any other is refused and nothing is published), and the lock job's work (the base commit as a Git bundle, the planned manifest edits and the settings, sealed with the token for CI_PIPELINE_ID) is written into --dir. Nothing is pushed until gitlab org publish. Configured as gitlab org run is, plus CI_PIPELINE_ID. Progress goes to stderr",
                "effects": "non_idempotent",
                "mutating": true,
                "cardinality": "single",
                "args": [
                    {"name": "--project", "description": "The project's numeric id", "type": "integer", "required": true},
                    {"name": "--lane", "description": "ordinary (default) or major", "type": "string", "required": false},
                    {"name": "--dir", "description": "The directory the three jobs exchange their work through", "type": "path", "required": true}
                ],
                "output_fields": [
                    {"name": "command", "type": "string", "description": "Always \"gitlab org prepare\""},
                    {"name": "project", "type": "integer", "description": "The project id"},
                    {"name": "path", "type": "string", "description": "The project's full path"},
                    {"name": "lane", "type": "string", "description": "ordinary or major"},
                    {"name": "branch", "type": "string", "description": "The lane's rolling automation branch"},
                    {"name": "step", "type": "string", "description": "stopped (the project has not opted in, or its opt-in cannot be read; see state), lane_off (the major lane is not enabled for this project), finished (the lane published, closed or paused; see result), or locking (prepare handed lockfiles to the lock job)"},
                    {"name": "state", "type": "string", "description": "stopped only: not_opted_in (with reason) or config_invalid (with config and message; the command exits 2)"},
                    {"name": "reason", "type": "string", "description": "not_opted_in only: why the project does not count as opted in"},
                    {"name": "config", "type": "string", "description": "config_invalid only: the configuration file that failed to load"},
                    {"name": "message", "type": "string", "description": "config_invalid only: why it failed to load"},
                    {"name": "result", "type": "object", "description": "finished only: the gitlab run result without command and branch (outcome, merge_request, created, pushed, commit, auto_merge, ..., and security when the security step ran)"},
                    {"name": "lockfiles", "type": "array", "items": {"type": "string"}, "description": "locking only: the lockfiles the lock job regenerates"}
                ],
                "example": {"args": ["gitlab", "org", "prepare", "--project", "42", "--dir", "upd-work"]}
            },
            {
                "name": "gitlab org lock-worker",
                "description": "Lock mode, second job: regenerates the lockfiles without the token. Refuses to run with UPD_GITLAB_TOKEN set. Checks out the commit prepare bundled into --dir, confirms every lock tool the lockfiles need is on PATH, runs the same security fixes and update with lockfile regeneration (UV_NO_BUILD=1 unless the project sets [automation] lock_build = true), and writes the whole change as a patch with the reports into --dir/result, which must not exist yet. It pushes nothing and needs no GitLab access; gitlab org publish decides what of its result to trust. Progress goes to stderr",
                "effects": "non_idempotent",
                "mutating": true,
                "cardinality": "single",
                "args": [
                    {"name": "--dir", "description": "The directory gitlab org prepare wrote its work into", "type": "path", "required": true}
                ],
                "output_fields": [
                    {"name": "command", "type": "string", "description": "Always \"gitlab org lock-worker\""},
                    {"name": "project", "type": "integer", "description": "The project id"},
                    {"name": "path", "type": "string", "description": "The project's full path"},
                    {"name": "lane", "type": "string", "description": "ordinary or major"},
                    {"name": "branch", "type": "string", "description": "The lane's rolling automation branch"},
                    {"name": "step", "type": "string", "description": "locked (the lock job regenerated the lockfiles), or nothing_to_do (prepare already finished the lane)"},
                    {"name": "tools", "type": "object", "description": "locked only: each lock tool used, with the version it reported (null when it reported none)"}
                ],
                "example": {"args": ["gitlab", "org", "lock-worker", "--dir", "upd-work"]}
            },
            {
                "name": "gitlab org publish",
                "description": "Lock mode, third job: holds the token again. Verifies that the work in --dir carries this pipeline's seal and was prepared for this project and lane, and that the planned patch is the one prepare sealed. Finds the job --lock-job names in this pipeline with UPD_GITLAB_TOKEN (exactly one such job, and it succeeded) and downloads its artifacts archive with its own CI_JOB_TOKEN, reads only result/result.json and result/result.patch under --dir from it, in memory and at most 64 MiB each, and checks they answer this work. Takes no artifacts from the lock job as a dependency, so nothing the lock job uploaded is unpacked into this job or loaded as its variables. Admits from the lock job's patch only the planned manifest edits and in-place edits of lockfiles under the scanned paths that fetch from no place the original lockfile does not (no new registry or index, compared by its full URL, no new download host, repository, archive or local path, and no npm package left without a resolved URL unless the original lockfile has one); anything else (a created, deleted or binary file, another file, a manifest that differs from the plan) refuses the lane and nothing is published. Then publishes as gitlab run does, on the commit prepare started from, with the push lease prepare observed: a branch that moved since exits 5, as does a default branch that no longer contains that commit; a default branch that only moved on is noted in the merge request. Configured as gitlab org prepare is, plus CI_PROJECT_ID and CI_JOB_TOKEN. Progress goes to stderr",
                "effects": "non_idempotent",
                "mutating": true,
                "cardinality": "single",
                "args": [
                    {"name": "--project", "description": "The project's numeric id", "type": "integer", "required": true},
                    {"name": "--lane", "description": "ordinary (default) or major", "type": "string", "required": false},
                    {"name": "--dir", "description": "The directory the three jobs exchange their work through", "type": "path", "required": true},
                    {"name": "--lock-job", "description": "The name of the lock job in this pipeline whose result to publish", "type": "string", "required": true}
                ],
                "output_fields": [
                    {"name": "command", "type": "string", "description": "Always \"gitlab org publish\""},
                    {"name": "project", "type": "integer", "description": "The project id"},
                    {"name": "path", "type": "string", "description": "The project's full path"},
                    {"name": "lane", "type": "string", "description": "ordinary or major"},
                    {"name": "branch", "type": "string", "description": "The lane's rolling automation branch"},
                    {"name": "step", "type": "string", "description": "finished (the lane published, closed or paused; see result), or nothing_to_do (prepare already finished the lane)"},
                    {"name": "result", "type": "object", "description": "finished only: the gitlab run result without command and branch, and security when the security step ran"}
                ],
                "example": {"args": ["gitlab", "org", "publish", "--project", "42", "--dir", "upd-work", "--lock-job", "lock-42"]}
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
                "description": "Manifest changes are available in dry-run mode, or --check --fail-on-blocked found a dependency blocked by safety checks; the report is on stdout. Not an error. Use --apply for available changes; blocked dependencies require addressing the reported safety condition"
            },
            {
                "code": 6,
                "name": "vulnerabilities_found",
                "description": "Security vulnerabilities found during audit; the report is on stdout. Under --fix-audit, a vulnerability left unresolved (a fix blocked, skipped or unfixable, or a manifest that already requires the fix over a lockfile --no-lock leaves at the vulnerable release), even when other fixes applied. Not an error. Use --no-fail to exit 0 instead"
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
                "kind": "refused",
                "description": "gitlab run, gitlab org run: GitLab or the repository is in a state the run will not act on (more than one open merge request for the branch, or a response that does not identify a merge request). gitlab org prepare, lock-worker and publish: the job will not act on its input (a project outside UPD_GROUP, a lockfile lock mode does not support, the token in the lock job, work without this pipeline's seal, or a lock job change publish does not admit); nothing was published",
                "exit_code": 2,
                "retryable": false
            },
            {
                "kind": "api_error",
                "description": "gitlab run, gitlab org run, gitlab org plan, gitlab org prepare, gitlab org publish: the GitLab API rejected a request (a 4xx other than 429, e.g. an invalid or under-scoped token)",
                "exit_code": 2,
                "retryable": false
            },
            {
                "kind": "conflict",
                "description": "Version conflict detected between files, or (gitlab run, gitlab org run, gitlab org publish) the automation branch moved while the run was working, so its push lease was refused, or (gitlab org publish) the default branch no longer contains the commit prepare started from",
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
        let description = updates_available["description"].as_str().unwrap();
        assert!(
            description.contains("--check --fail-on-blocked")
                && description.contains("blocked by safety checks"),
            "exit 1 can report blocked dependencies even with no available updates"
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

    #[test]
    fn schema_declares_every_gitlab_error_as_reported() {
        use crate::gitlab::Error;
        let s = build_schema();
        let errors = s["errors"].as_array().expect("errors must be an array");
        for error in [
            Error::Input(String::new()),
            Error::Refused(String::new()),
            Error::Api(String::new()),
            Error::Network(String::new()),
            Error::Io(String::new()),
            Error::Conflict(String::new()),
        ] {
            let kind = error.kind();
            let declared = errors
                .iter()
                .find(|e| e["kind"].as_str() == Some(kind))
                .unwrap_or_else(|| panic!("gitlab error kind '{kind}' must be declared"));
            assert_eq!(
                declared["exit_code"].as_i64(),
                Some(i64::from(error.exit_code())),
                "{kind}"
            );
            assert_eq!(
                declared["retryable"].as_bool(),
                Some(matches!(error, Error::Network(_))),
                "{kind}"
            );
        }
    }

    #[test]
    fn schema_declares_gitlab_run() {
        let s = build_schema();
        let command = find_command(&s, "gitlab run");
        assert_eq!(command["mutating"], true);
        let fields = output_field_names(command);
        for field in [
            "command",
            "branch",
            "outcome",
            "merge_request",
            "error",
            "security",
            "major",
        ] {
            assert!(fields.iter().any(|f| f == field), "{field}");
        }
        let description = command["description"].as_str().unwrap();
        for variable in [
            "UPD_SECURITY_REMEDIATION",
            "UPD_MAJOR_MR",
            "UPD_MAJOR_BRANCH",
            "UPD_MAJOR_COMMIT_MESSAGE",
        ] {
            assert!(description.contains(variable), "{variable}");
        }
    }

    #[test]
    fn schema_declares_gitlab_org_run() {
        let s = build_schema();
        let command = find_command(&s, "gitlab org run");
        assert_eq!(command["mutating"], true);
        let fields = output_field_names(command);
        for field in [
            "command",
            "group",
            "branch",
            "dry_run",
            "counts",
            "projects",
            "major_branch",
        ] {
            assert!(fields.iter().any(|f| f == field), "{field}");
        }
        let description = command["description"].as_str().unwrap();
        for variable in [
            "UPD_MAJOR_MR",
            "UPD_MAJOR_BRANCH",
            "UPD_MAJOR_COMMIT_MESSAGE",
        ] {
            assert!(description.contains(variable), "{variable}");
        }
    }

    #[test]
    fn schema_declares_the_plan_output_it_writes() {
        use crate::gitlab::plan::{HandedOff, Plan, Unplanned};
        use crate::gitlab::{Error, org::Shard, run::Lane};

        let s = build_schema();
        let command = find_command(&s, "gitlab org plan");
        assert_eq!(command["mutating"], true);
        assert_eq!(command["args"], json!([]));
        let plan = Plan {
            group: "acme".to_string(),
            dry_run: false,
            shard: Some(Shard { index: 1, count: 2 }),
            projects: 2,
            handed_off: vec![HandedOff {
                id: 1,
                path: "acme/a".to_string(),
                lanes: vec![Lane::Ordinary],
            }],
            unplanned: vec![Unplanned {
                id: 3,
                path: "acme/b".to_string(),
                error: Error::Network("timed out".to_string()),
            }],
        };
        let mut written: Vec<String> = plan
            .to_json()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let mut declared = output_field_names(command);
        written.sort();
        declared.sort();
        assert_eq!(declared, written);
    }

    #[test]
    fn schema_declares_the_lock_mode_output_each_job_writes() {
        use crate::gitlab::run::{Lane, Outcome, Proposal};
        use crate::gitlab::split::{Step, StepReport, Stop};
        use std::collections::BTreeSet;

        let finished = || {
            Step::Finished(Proposal {
                outcome: Outcome::Clean,
                security: None,
            })
        };
        let s = build_schema();
        for (command, steps) in [
            (
                "gitlab org prepare",
                vec![
                    Step::Stopped(Stop::NotOptedIn("no opt-in".to_string())),
                    Step::Stopped(Stop::ConfigInvalid {
                        config: ".upd.toml".to_string(),
                        message: "bad".to_string(),
                    }),
                    Step::LaneOff,
                    finished(),
                    Step::Locking {
                        lockfiles: vec!["uv.lock".to_string()],
                    },
                ],
            ),
            (
                "gitlab org lock-worker",
                vec![
                    Step::Locked {
                        tools: Default::default(),
                    },
                    Step::NothingToDo,
                ],
            ),
            ("gitlab org publish", vec![finished(), Step::NothingToDo]),
        ] {
            let mut written = BTreeSet::new();
            for step in steps {
                let report = StepReport {
                    command,
                    project: 1,
                    path: "acme/a".to_string(),
                    lane: Lane::Ordinary,
                    branch: "upd/update".to_string(),
                    step,
                };
                written.extend(report.to_json().as_object().unwrap().keys().cloned());
            }
            let declared: BTreeSet<String> = output_field_names(find_command(&s, command))
                .into_iter()
                .collect();
            assert_eq!(declared, written, "{command}");
        }
    }

    #[test]
    fn schema_declares_the_lock_mode_jobs() {
        let s = build_schema();
        for (name, args) in [
            ("gitlab org prepare", &["--project", "--lane", "--dir"][..]),
            ("gitlab org lock-worker", &["--dir"][..]),
            (
                "gitlab org publish",
                &["--project", "--lane", "--dir", "--lock-job"][..],
            ),
        ] {
            let command = find_command(&s, name);
            assert_eq!(command["mutating"], true, "{name}");
            let declared: Vec<&str> = command["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|arg| arg["name"].as_str().unwrap())
                .collect();
            assert_eq!(declared, args, "{name}");
            let fields = output_field_names(command);
            assert!(fields.iter().any(|f| f == "command"), "{name}");
            assert!(fields.iter().any(|f| f == "step"), "{name}");
        }
        for (name, described) in [
            (
                "gitlab org prepare",
                &["stopped", "lane_off", "finished", "locking"][..],
            ),
            ("gitlab org lock-worker", &["locked", "nothing_to_do"][..]),
            ("gitlab org publish", &["finished", "nothing_to_do"][..]),
        ] {
            let steps = find_command(&s, name)["output_fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|field| field["name"] == "step")
                .unwrap()["description"]
                .as_str()
                .unwrap()
                .to_string();
            for step in described {
                assert!(
                    steps.contains(step),
                    "{name}: {step} is not described: {steps}"
                );
            }
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

        for flag in ["lang", "exclude-lang"] {
            let arg = find_global_arg(&s, flag);
            let mut values: Vec<String> = arg["enum"]
                .as_array()
                .unwrap_or_else(|| panic!("--{flag} must have an enum of valid ecosystems"))
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            values.sort();
            assert_eq!(
                values, expected,
                "the --{flag} enum in the schema must list exactly the Lang variants clap accepts"
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
