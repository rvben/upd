mod annotated;
mod cargo_toml;
mod csproj;
mod gemfile;
mod github_actions;
mod go_mod;
mod mise;
mod package_json;
mod pre_commit;
mod pyproject;
mod requirements;
mod terraform;

pub use annotated::{AnnotatedUpdater, ParseWarnings, RegistrySet, selection_reaches_annotations};
pub use cargo_toml::CargoTomlUpdater;
pub use csproj::CsprojUpdater;
pub use gemfile::GemfileUpdater;
pub use github_actions::GithubActionsUpdater;
pub use go_mod::GoModUpdater;
pub use mise::MiseUpdater;

pub use package_json::PackageJsonUpdater;
pub use pre_commit::PreCommitUpdater;
pub use pyproject::PyProjectUpdater;
pub use requirements::RequirementsUpdater;
pub use terraform::TerraformUpdater;

use crate::annotation::AnnotationSource;
use crate::config::UpdConfig;
use crate::cooldown::CooldownPolicy;
use crate::registry::{Registry, VersionQuery};
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Maximum file size allowed for dependency files (10 MB)
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// UTF-8 BOM character
const UTF8_BOM: char = '\u{feff}';

/// Read a file safely, handling BOM and enforcing size limits
pub fn read_file_safe(path: &Path) -> Result<String> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_FILE_SIZE {
        return Err(anyhow!(
            "File too large: {} bytes (max {} MB)",
            metadata.len(),
            MAX_FILE_SIZE / 1024 / 1024
        ));
    }

    let content = std::fs::read_to_string(path)?;
    // Strip UTF-8 BOM if present (common in Windows-created files)
    let content = content.strip_prefix(UTF8_BOM).unwrap_or(&content);
    Ok(content.to_string())
}

/// Build the standard warning message for a refused version downgrade.
///
/// Centralises the message format so all updaters emit identical text,
/// which makes it easy to grep logs and assert in tests.
pub(crate) fn downgrade_warning(pkg: &str, latest: &str, current: &str) -> String {
    format!("skipping {pkg}: latest \"{latest}\" is not greater than current \"{current}\"")
}

/// Run a Python latest-version query and distrust an answer below the version
/// already present in the manifest.
///
/// A lower cached value cannot safely establish what is latest. Revalidating
/// through the registry preserves its index-selection rules, and cache
/// decorators collapse concurrent retries for duplicate manifests into one
/// live request. If the live answer is still lower, callers retain their normal
/// downgrade warning rather than assuming the manifest version exists.
pub(crate) async fn python_version_with_revalidation(
    registry: &dyn Registry,
    package: &str,
    current: &str,
    query: VersionQuery<'_>,
) -> Result<String> {
    let latest = query.run(registry, package).await?;
    if crate::align::compare_versions(&latest, current, Lang::Python) == std::cmp::Ordering::Less {
        registry.revalidate_version(package, query, &latest).await
    } else {
        Ok(latest)
    }
}

/// One clause of a version specifier: an operator and the version it bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Clause<'a> {
    /// The comparison operator, empty for a bare version.
    pub(crate) op: &'a str,
    /// The version the operator bounds, without surrounding whitespace.
    pub(crate) version: &'a str,
    /// Byte range of `version` within the string the caller passed a slice of,
    /// so a rewrite lands on this clause and not on a look-alike elsewhere.
    pub(crate) range: std::ops::Range<usize>,
}

/// Read one clause of a specifier, with `version`'s byte range offset by `base`.
///
/// `None` when the clause holds no digit-led version, which is how a wildcard
/// (`*`) and any other clause with nothing to rewrite answer.
pub(crate) fn parse_clause(clause: &str, base: usize) -> Option<Clause<'_>> {
    let after_space = clause.trim_start();
    let op_at = clause.len() - after_space.len();
    let op_len = after_space
        .bytes()
        .take_while(|b| matches!(b, b'=' | b'<' | b'>' | b'!' | b'~' | b'^'))
        .count();
    let (op, rest) = after_space.split_at(op_len);

    let version = rest.trim_start();
    let version_at = op_at + op_len + (rest.len() - version.len());
    let version = version.trim_end();
    if !version.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }

    Some(Clause {
        op,
        version,
        range: base + version_at..base + version_at + version.len(),
    })
}

/// Read a comma-separated specifier as the clause set it is.
///
/// Clauses that hold no version are dropped, so the iterator yields only what a
/// caller could read or rewrite.
pub(crate) fn comma_clauses(constraint: &str, base: usize) -> impl Iterator<Item = Clause<'_>> {
    let mut offset = 0usize;
    constraint.split(',').filter_map(move |clause| {
        let clause_at = offset;
        offset += clause.len() + 1;
        parse_clause(clause, base + clause_at)
    })
}

/// Read a comma-separated specifier as the clause set it is, or `None` when any
/// part of it holds no version.
///
/// The strict counterpart to [`comma_clauses`], for an ecosystem whose own
/// parser refuses a whole constraint when one clause of it is malformed. Such a
/// caller cannot use a reading that leaves the malformed clause out, because
/// what remains parses cleanly and describes a file the ecosystem will not
/// load: `">= 5.0, banana"` reads as `">= 5.0"` and answers as an ordinary
/// constraint, while `terraform init` on that file fails before it looks up
/// anything.
pub(crate) fn all_comma_clauses(constraint: &str, base: usize) -> Option<Vec<Clause<'_>>> {
    let mut offset = 0usize;
    constraint
        .split(',')
        .map(|clause| {
            let clause_at = offset;
            offset += clause.len() + 1;
            parse_clause(clause, base + clause_at)
        })
        .collect()
}

/// Whether `op` names the release the manifest is on, so an update may move it.
///
/// A bare version is Cargo's `serde = "1.0"`, which means `^1.0`, and RubyGems'
/// `gem 'puma', '5.1.0'`, which means `= 5.1.0`; both are the release in use and
/// both are the commonest floor there is. `>` names the one version the author
/// ruled out, and a ceiling or an exclusion names none at all.
pub(crate) fn operator_is_raisable(op: &str) -> bool {
    matches!(op, "" | ">=" | "==" | "===" | "=" | "~=" | "~" | "~>" | "^")
}

/// Whether any clause can exclude the newest release, so the release to raise a
/// floor to has to be looked up against the specifier rather than taken as the
/// newest one published.
///
/// A pure lower bound never excludes it, and neither does an exact pin, whose
/// whole purpose is to be replaced by the newest release.
pub(crate) fn caps_from_above(clauses: &[Clause<'_>]) -> bool {
    clauses
        .iter()
        .any(|c| matches!(c.op, "<" | "<=" | "~>" | "!="))
}

/// The clause naming the highest version among `floors`, or `None` when they
/// cannot all be ranked against each other.
///
/// Ranking is by release segments so it holds across ecosystems, and a version
/// spelled in a way that carries no such segments (a PEP 440 epoch or
/// post-release, a Cargo build suffix) makes the whole set unrankable rather
/// than sorting to one end of it: naming the wrong clause the floor is what
/// this exists to prevent.
fn greatest_floor<'c>(floors: &[&'c Clause<'c>]) -> Option<&'c Clause<'c>> {
    let mut best = *floors.first()?;
    let mut best_version = crate::version::TagVersion::parse(best.version)?;
    for clause in &floors[1..] {
        let version = crate::version::TagVersion::parse(clause.version)?;
        if version > best_version {
            best = clause;
            best_version = version;
        }
    }
    Some(best)
}

/// The clause an update may rewrite, or the first readable one when there is none.
///
/// Reported for both so that a caller which only reads a specifier, like the
/// lockfile anchor, still gets a position; `raisable` is what says whether
/// writing to that position is allowed.
///
/// A specifier may carry more than one lower bound, and every resolver reads
/// their conjunction: `requests>=1.0,>=2.30` installs 2.30 or better, so 2.30
/// is the release in use and 1.0 names nothing anyone is on. Taking whichever
/// came first made the same requirement answer two ways depending on the order
/// it was typed in, reporting a major bump from 1.0 in one spelling and a minor
/// one from 2.30 in the other, which then decided differently under
/// `--max-bump`. When the bounds cannot be ranked the first is still used, so a
/// specifier upd cannot fully read keeps the position it always had.
pub(crate) fn floor_of(clauses: &[Clause<'_>]) -> Option<SpecifierFloor> {
    let floors: Vec<&Clause<'_>> = clauses
        .iter()
        .filter(|c| operator_is_raisable(c.op))
        .collect();
    if let Some(first) = floors.first() {
        let clause = greatest_floor(&floors).unwrap_or(first);
        return Some(SpecifierFloor {
            range: clause.range.clone(),
            raisable: true,
        });
    }
    clauses.first().map(|clause| SpecifierFloor {
        range: clause.range.clone(),
        raisable: false,
    })
}

/// The version a specifier is anchored at, and whether an update may move it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpecifierFloor {
    /// Byte range of that version within the string the caller passed a slice
    /// of, so a rewrite lands on the right clause of the whole line.
    pub(crate) range: std::ops::Range<usize>,
    /// Whether the clause the version was taken from admits it, which is what
    /// makes it the release the author is on rather than one they ruled out.
    pub(crate) raisable: bool,
}

/// Where a version specifier's floor is written.
///
/// A specifier is a set of clauses and carries no order: `botocore<1.35.0,>=1.34.0`
/// is what setuptools and pip write, and it means exactly what
/// `botocore>=1.34.0,<1.35.0` means. The clause that says which release is
/// installed today, and the one an update rewrites, is the lower bound wherever
/// the author put it. Reading the first clause instead makes one requirement
/// answer two ways depending on how it was typed: against an upper bound the
/// latest release compares as a downgrade, so the package is passed over as
/// already ahead of the registry and never updated.
///
/// Only an *inclusive* lower bound is `raisable`. A `>` names the one version
/// the author has ruled out, not one they are on, so there is nothing under it
/// to carry forward and writing the newest release into it produces a specifier
/// that excludes that very release: `urllib3>2.0` became `urllib3>2.7` with 2.7
/// the newest release, and the next run could not resolve the file upd had just
/// written. A ceiling (`<6`) and an exclusion (`!=1.5`) name no floor at all and
/// answer the same way, so a caller cannot mistake either for a version to move.
/// The range is still reported for all of them, because reading a specifier and
/// rewriting it are different questions and the callers that only read one, like
/// the lockfile anchor, still need the position.
///
/// A clause whose version does not start with a digit is passed over entirely,
/// so a wildcard (`*`) or otherwise unreadable requirement answers `None`.
pub(crate) fn specifier_floor(constraint: &str, base: usize) -> Option<SpecifierFloor> {
    floor_of(&comma_clauses(constraint, base).collect::<Vec<_>>())
}

/// Whether `constraint` admits `version`, read with the ecosystem's own parser.
///
/// `None` when that parser cannot read the constraint, which is not the same
/// answer as `false`: one says the dependency is behind, the other says nothing
/// looked at it.
pub(crate) fn pep440_admits(constraint: &str, version: &str) -> Option<bool> {
    use std::str::FromStr;
    let specifiers = pep440_rs::VersionSpecifiers::from_str(constraint).ok()?;
    let version = pep440_rs::Version::from_str(version).ok()?;
    Some(specifiers.contains(&version))
}

/// Whether `requirement` admits `version` under Cargo's reading of it.
///
/// Cargo's partial bounds are not PEP 440's: `>1.0` there means `>=1.1.0`, so
/// `serde = ">1.0"` admits no 1.0.x release at all. Answering that question with
/// the wrong ecosystem's parser would report such a requirement as satisfied.
pub(crate) fn cargo_admits(requirement: &str, version: &str) -> Option<bool> {
    let req = semver::VersionReq::parse(requirement).ok()?;
    let version = semver::Version::parse(version).ok()?;
    Some(req.matches(&version))
}

/// Build the standard warning for a spec upd can read but must not rewrite.
///
/// Centralised beside [`downgrade_warning`] so every ecosystem says this the
/// same way: the release exists, upd looked, and the manifest is the reason
/// nothing moved.
pub(crate) fn unrewritable_warning(pkg: &str, latest: &str, spec: &str) -> String {
    format!("{pkg}: {latest} is available, but '{spec}' is a range upd does not rewrite")
}

/// Build the standard error for a spec upd cannot read at all.
///
/// An error rather than a warning: a warning leaves the run exiting 0 with a
/// green tick over a dependency nothing has looked at.
pub(crate) fn unreadable_error(pkg: &str, spec: &str) -> String {
    format!("cannot check '{pkg}': '{spec}' is not a version range upd can read")
}

/// Build the standard error for a pin that cannot be written into a specifier.
///
/// A specifier with no raisable floor has nowhere to put the pinned version, and
/// writing it into a ceiling produces a specifier the pin does not satisfy. An
/// error rather than a silent skip: the user asked for that exact version.
pub(crate) fn unpinnable_error(pkg: &str, pinned: &str, spec: &str) -> String {
    format!("cannot pin '{pkg}' to '{pinned}': '{spec}' has no lower bound that version fits")
}

/// UTF-8 byte-order mark, as bytes.
const UTF8_BOM_BYTES: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Re-apply the original file's byte-level encoding (UTF-8 BOM and dominant line
/// ending) to rewritten `content`.
///
/// In-memory edits canonicalize to LF and drop any BOM; without this a CRLF or
/// BOM-prefixed manifest would be silently reformatted on `--apply`, producing
/// noisy diffs on cross-platform repos. New files (no original) are written as-is.
fn apply_original_encoding(original: Option<&[u8]>, content: &str) -> Vec<u8> {
    let Some(original) = original else {
        return content.as_bytes().to_vec();
    };

    let had_bom = original.starts_with(&UTF8_BOM_BYTES);
    let original_body = original.strip_prefix(&UTF8_BOM_BYTES).unwrap_or(original);
    let uses_crlf = original_body.windows(2).any(|w| w == b"\r\n");

    // An updater that retained the original terminator sequence has already
    // done the more precise job, including for mixed LF/CRLF files.
    let body = if line_ending_sequence(original_body) == line_ending_sequence(content.as_bytes()) {
        content.to_string()
    } else {
        // Other updaters work on logical lines. Keep their established
        // behavior of re-applying the original file's line-ending style.
        let normalized = content.replace("\r\n", "\n");
        if uses_crlf {
            normalized.replace('\n', "\r\n")
        } else {
            normalized
        }
    };

    let mut out = Vec::with_capacity(body.len() + 3);
    if had_bom && !body.as_bytes().starts_with(&UTF8_BOM_BYTES) {
        out.extend_from_slice(&UTF8_BOM_BYTES);
    }
    out.extend_from_slice(body.as_bytes());
    out
}

fn line_ending_sequence(bytes: &[u8]) -> Vec<bool> {
    bytes
        .iter()
        .enumerate()
        .filter_map(|(idx, byte)| (*byte == b'\n').then_some(idx > 0 && bytes[idx - 1] == b'\r'))
        .collect()
}

/// Write a file atomically (write to temp file, then rename)
pub fn write_file_atomic(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;

    // Create temp file in same directory to ensure atomic rename works
    let parent = path.parent().unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "temp".to_string());
    let tmp_path = parent.join(format!(".{}.upd.tmp", file_name));

    // Capture the original file's bytes (to preserve BOM + line endings) and
    // permissions so the atomic rename does not silently change them. Without
    // the permission capture, a read-only (0o444) manifest is rewritten as
    // 0o644: the rename replaces the inode with the temp file, which was
    // created with the umask default.
    let original_bytes = std::fs::read(path).ok();
    let original_perms = std::fs::metadata(path).ok().map(|m| m.permissions());
    let final_bytes = apply_original_encoding(original_bytes.as_deref(), content);

    // Write to temporary file
    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(&final_bytes)?;
    file.sync_all()?;

    // Atomically rename to target path
    std::fs::rename(&tmp_path, path)?;

    // Restore the original permissions onto the renamed file so updating a
    // manifest never changes its mode (read-only stays read-only).
    if let Some(perms) = original_perms {
        let _ = std::fs::set_permissions(path, perms);
    }

    Ok(())
}

/// Coarse bump classification of a version change, used to honor the
/// `--only-bump` / `--max-bump` ceiling at write time and to label the change
/// in reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BumpKind {
    Major,
    Minor,
    Patch,
}

/// Classify a version change as major / minor / patch.
///
/// Parses the leading `major.minor.patch`, tolerating a leading `v` and
/// missing segments, and falls back to `Patch` for anything unparseable or
/// non-increasing.
///
/// Below `1.0.0` the compatible range is narrower than the version numbers
/// suggest. SemVer leaves a zero major version unstable, and Cargo and npm
/// both read `^0.12` as `>=0.12, <0.13`, so moving a dependency from `0.12` to
/// `0.13` breaks callers exactly the way `1.0` to `2.0` does. Such a step is
/// therefore `Major`, which is what holds it behind a `--max-bump minor`
/// ceiling instead of applying it unattended. The same reasoning goes one
/// digit further down, where `^0.0.3` means `>=0.0.3, <0.0.4` and every
/// release is breaking.
///
/// This is the single classifier behind both the write-time gate and the
/// printed labels, so the two cannot disagree about what a change is.
pub fn classify_bump(old: &str, new: &str) -> BumpKind {
    /// Read the version numbers out of a version or a range spec. A spec
    /// (`^1.2.3`, `>=1.0`, `~=1.4`, `>=1.0.0 <2.0.0`) reaches here verbatim
    /// wherever an updater records the string it found in the manifest, and
    /// the numbers that decide its bump level are the lower bound's - the
    /// same anchor the ceiling compares. Leading operators are dropped and
    /// everything from the first character that is neither a digit nor a dot
    /// is ignored, which also trims a prerelease or build suffix.
    fn parse(v: &str) -> Option<(u64, u64, u64)> {
        let v = v.trim_start_matches(|c: char| {
            matches!(c, '^' | '~' | '>' | '<' | '=' | '!' | 'v' | 'V') || c.is_whitespace()
        });
        let end = v
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(v.len());
        let mut parts = v[..end].split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        Some((major, minor, patch))
    }
    let (Some((om, oi, op)), Some((nm, ni, np))) = (parse(old), parse(new)) else {
        return BumpKind::Patch;
    };
    if nm > om {
        return BumpKind::Major;
    }
    if om == 0 {
        if ni > oi || (oi == 0 && np > op) {
            return BumpKind::Major;
        }
        return BumpKind::Patch;
    }
    if ni > oi {
        return BumpKind::Minor;
    }
    BumpKind::Patch
}

/// Which bump levels are permitted to be written.
///
/// The default permits everything, so updaters that are unaware of the filter
/// (and the no-flag case) behave exactly as before.
#[derive(Debug, Clone, Copy)]
pub struct BumpFilter {
    pub major: bool,
    pub minor: bool,
    pub patch: bool,
}

impl Default for BumpFilter {
    fn default() -> Self {
        Self {
            major: true,
            minor: true,
            patch: true,
        }
    }
}

impl BumpFilter {
    /// Returns `true` when a change from `old` to `new` is permitted and may be
    /// written.
    ///
    /// A missing/blank `old` or `new` is never writable: an empty current
    /// version means the updater failed to extract a real version, and
    /// substituting would corrupt the file (for example a non-version string
    /// gaining an appended digit, `"hello"` -> `"hello1"`).
    pub fn allows(&self, old: &str, new: &str) -> bool {
        if old.trim().is_empty() || new.trim().is_empty() {
            return false;
        }
        match classify_bump(old, new) {
            BumpKind::Major => self.major,
            BumpKind::Minor => self.minor,
            BumpKind::Patch => self.patch,
        }
    }
}

/// Options for updating dependencies
#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    /// Dry run - don't write changes
    pub dry_run: bool,
    /// Use full version precision instead of matching original
    pub full_precision: bool,
    /// Configuration for ignoring/pinning packages
    pub config: Option<Arc<UpdConfig>>,
    /// When non-empty, only packages whose name is in this set are processed.
    /// An empty set means "process all packages" (no filter active).
    pub packages: Vec<String>,
    /// Active cooldown policy, if configured. None => cooldown disabled.
    pub cooldown_policy: Option<Arc<CooldownPolicy>>,
    /// Wall-clock used for cooldown decisions. None => `Utc::now()` at call time.
    /// Injected by tests for deterministic behaviour.
    pub cooldown_now: Option<DateTime<Utc>>,
    /// Notes emitted when a registry cannot supply publish dates, keyed by the
    /// condition they describe. Shared across updaters so a single run reports
    /// each condition once however many packages ran into it.
    pub cooldown_unavailable_notes: Arc<Mutex<BTreeMap<String, String>>>,
    /// Bump-level ceiling enforced at write time. Defaults to permitting every
    /// level, so updates are only skipped when `--only-bump` / `--max-bump`
    /// narrow it.
    pub bump_filter: BumpFilter,
    /// Ecosystems selected by `--lang`. Empty means every ecosystem.
    ///
    /// Only `AnnotatedUpdater` reads this. Every other file type was already
    /// filtered at discovery, where the same test was applied to the file's own
    /// `FileType::lang()`; an annotated file is admitted by discovery
    /// unconditionally because its ecosystems are not known until its lines are
    /// read.
    pub langs: Vec<Lang>,
    /// Update fully SHA-pinned GitHub Actions when they carry a verified,
    /// concrete version comment (for example `# v4.2.2`). Resolved from the
    /// command line, then the config file, then `DEFAULT_UPDATE_ACTION_SHAS`.
    pub update_action_shas: bool,
}

/// Whether GitHub Actions SHA pins are updated when neither the command line
/// nor the config file says. A SHA pin that is not updated is still reported,
/// so this decides the default behaviour but never whether the pin is visible.
///
/// On, because pinning an action to a commit is the hardening GitHub itself
/// recommends, and a default that declines to update those pins leaves the
/// hardened repository on stale action code while the unhardened one gets
/// patched. Every rewrite is gated on a full 40-character SHA, a concrete
/// version comment, and that comment resolving to the pinned commit, so the
/// pins this default touches are exactly the ones whose provenance is
/// verifiable; anything ambiguous is reported and left alone.
pub const DEFAULT_UPDATE_ACTION_SHAS: bool = true;

impl UpdateOptions {
    /// Create new options with the given dry_run and full_precision settings
    pub fn new(dry_run: bool, full_precision: bool) -> Self {
        Self {
            dry_run,
            full_precision,
            config: None,
            packages: Vec::new(),
            cooldown_policy: None,
            cooldown_now: None,
            cooldown_unavailable_notes: Arc::default(),
            bump_filter: BumpFilter::default(),
            langs: Vec::new(),
            update_action_shas: DEFAULT_UPDATE_ACTION_SHAS,
        }
    }

    /// Restrict which bump levels may be written.
    pub fn with_bump_filter(mut self, filter: BumpFilter) -> Self {
        self.bump_filter = filter;
        self
    }

    /// Returns `true` when an update from `current` to `new` is within the
    /// permitted bump levels. Updaters consult this immediately before recording
    /// and writing a change so a capped-out update never reaches disk.
    pub fn allows_bump(&self, current: &str, new: &str) -> bool {
        self.bump_filter.allows(current, new)
    }

    /// Set the configuration
    pub fn with_config(mut self, config: Arc<UpdConfig>) -> Self {
        self.config = Some(config);
        self
    }

    /// Restrict processing to the named packages.
    pub fn with_packages(mut self, packages: Vec<String>) -> Self {
        self.packages = packages;
        self
    }

    /// Restrict processing to the named ecosystems. Empty means every one.
    pub fn with_langs(mut self, langs: Vec<Lang>) -> Self {
        self.langs = langs;
        self
    }

    /// Opt in to verified GitHub Actions SHA-pin updates.
    pub fn with_action_sha_updates(mut self, enabled: bool) -> Self {
        self.update_action_shas = enabled;
        self
    }

    /// Returns `true` when this package should be skipped because a `--package`
    /// filter is active and the name is not in the allowed set.
    pub fn is_package_filtered_out(&self, package: &str) -> bool {
        !self.packages.is_empty() && !self.packages.iter().any(|p| p == package)
    }

    /// Check if a package should be ignored
    pub fn should_ignore(&self, package: &str) -> bool {
        self.config
            .as_ref()
            .map(|c| c.should_ignore(package))
            .unwrap_or(false)
    }

    /// Get the pinned version for a package (if any)
    pub fn get_pinned_version(&self, package: &str) -> Option<&str> {
        self.config
            .as_ref()
            .and_then(|c| c.get_pinned_version(package))
    }

    /// Activate a cooldown policy with a fixed reference time for decisions.
    pub fn with_cooldown_policy(mut self, policy: CooldownPolicy, now: DateTime<Utc>) -> Self {
        self.cooldown_policy = Some(Arc::new(policy));
        self.cooldown_now = Some(now);
        self
    }

    /// Returns `true` when the cooldown policy is active for `ecosystem`.
    pub fn cooldown_is_enabled_for(&self, ecosystem: &str) -> bool {
        self.cooldown_policy
            .as_ref()
            .map(|p| p.is_enabled_for(ecosystem))
            .unwrap_or(false)
    }

    /// Record a note that cooldown metadata was unavailable for an ecosystem.
    /// One condition is one note however many packages meet it, so the packages
    /// after the first add nothing the user does not already know.
    ///
    /// Which of them arrives first is not fixed, since packages resolve
    /// concurrently. The lowest-ordered message wins rather than the earliest,
    /// so the same repository state reports the same cause every run.
    pub fn note_cooldown_unavailable(&self, note: &CooldownNote) {
        if let Ok(mut guard) = self.cooldown_unavailable_notes.lock() {
            guard
                .entry(note.key.clone())
                .and_modify(|reported| {
                    if note.message < *reported {
                        reported.clone_from(&note.message);
                    }
                })
                .or_insert_with(|| note.message.clone());
        }
    }
}

/// A parsed dependency from a file (for alignment purposes)
#[derive(Debug, Clone)]
pub struct ParsedDependency {
    /// Package name
    pub name: String,
    /// Version string (the first/primary version number)
    pub version: String,
    /// Line number in the file (1-indexed)
    pub line_number: Option<usize>,
    /// Whether this dependency has upper bound constraints (e.g., <3.0)
    pub has_upper_bound: bool,
    /// Whether this dependency can be bumped to a newer version.
    ///
    /// Set to `false` for entries that reference a specific commit rather than
    /// a release tag (e.g. Go pseudo-versions like `v0.0.0-20200115085410-6d4e4cb37c7d`).
    /// Such entries are still included so that audit paths can see them, but the
    /// update path and alignment logic must not attempt to bump them.
    pub is_bumpable: bool,
}

/// Result of updating a single file
#[derive(Debug, Default, Clone)]
pub struct UpdateResult {
    /// Packages that were updated: (name, old_version, new_version, line_number)
    pub updated: Vec<(String, String, String, Option<usize>)>,
    /// Number of packages that were already at latest version
    pub unchanged: usize,
    /// Errors encountered during update
    pub errors: Vec<String>,
    /// Non-fatal warnings (e.g. lines with unparseable version tokens that were skipped)
    pub warnings: Vec<String>,
    /// Packages that were ignored due to config: (name, current_version, line_number)
    pub ignored: Vec<(String, String, Option<usize>)>,
    /// Packages that were pinned to a specific version: (name, current_version, pinned_version, line_number)
    pub pinned: Vec<(String, String, String, Option<usize>)>,
    /// Packages where cooldown forced us to a safer-older version than the
    /// absolute latest. Tuple: (name, old_version, chosen_version,
    /// skipped_latest_version, skipped_latest_published_at).
    pub held_back: Vec<(String, String, String, String, DateTime<Utc>)>,
    /// Packages where every newer version sits inside the cooldown window and
    /// we kept the current version. Tuple: (name, current_version,
    /// skipped_latest_version, skipped_latest_published_at). The publish date
    /// is `None` when the registry does not report one for the skipped version.
    pub skipped_by_cooldown: Vec<(String, String, String, Option<DateTime<Utc>>)>,
    /// Source of an entry that does not inherit its ecosystem from the file,
    /// keyed by package name. Populated only by `AnnotatedUpdater`.
    pub entry_ecosystem: HashMap<String, AnnotationSource>,
    /// Extra immutable-ref details for GitHub Actions updates. The matching
    /// entry also appears in `updated`, using semantic versions for bump
    /// classification and human-readable reporting.
    pub action_sha_updates: Vec<ActionShaUpdate>,
    /// Dependencies deliberately left alone, each carrying the reason why. This
    /// is distinct from `ignored` (user policy) and `errors` (operation
    /// failure), allowing automation to identify blocked and unchecked pins.
    pub skipped: Vec<SkippedUpdate>,
    /// Updates that exist but were not written because the bump exceeds the
    /// `--only-bump` / `--max-bump` ceiling.
    pub capped: Vec<CappedUpdate>,
    /// Dependencies whose recorded identity was completed without their version
    /// changing.
    pub annotations: Vec<Annotation>,
}

/// An available update held back by the bump ceiling.
///
/// Distinct from `unchanged`, which counts dependencies that are genuinely
/// current. A capped dependency has a newer release waiting and needs a human
/// to decide on it, so reporting the two together would answer "is anything
/// waiting for me?" with a confident no.
#[derive(Debug, Clone)]
pub struct CappedUpdate {
    pub package: String,
    pub current: String,
    pub available: String,
    pub line_number: Option<usize>,
}

/// A dependency whose identity was written into the file without its version
/// changing.
///
/// A GitHub Actions SHA pin carries no version of its own: the release it names
/// lives in a comment beside it, and a pin without that comment is one nothing
/// can safely move. Recovering the release from the commit and writing it down
/// changes the file while running exactly the same commit as before, so it is
/// neither an update (no version moved) nor unchanged (bytes were written), and
/// reporting it as either would misstate one of the two.
#[derive(Debug, Clone)]
pub struct Annotation {
    pub package: String,
    /// The release the dependency was found to be at.
    pub version: String,
    /// The immutable reference the annotation describes.
    pub commit: String,
    pub line_number: Option<usize>,
}

/// A verified GitHub Actions SHA-pin update.
#[derive(Debug, Clone)]
pub struct ActionShaUpdate {
    pub package: String,
    pub current_version: String,
    pub new_version: String,
    pub current_commit: String,
    pub new_commit: String,
    pub line_number: Option<usize>,
}

/// A dependency left at its current version, with the reason why.
#[derive(Debug, Clone)]
pub struct SkippedUpdate {
    pub package: String,
    pub current: String,
    pub status: SkipStatus,
    pub reason: &'static str,
    pub message: String,
    pub line_number: Option<usize>,
}

/// Why a `SkippedUpdate` exists, which decides how it is reported.
///
/// Both statuses leave the line untouched, but they answer different questions.
/// `Blocked` means the dependency was examined and a safety condition refused
/// the change. `NotExamined` means it was never looked at, because the feature
/// that reads that kind of pin is off. Reporting the second as the first would
/// describe a configuration choice as a safety problem, and reporting either as
/// up to date would claim a check that never happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipStatus {
    Blocked,
    NotExamined,
}

impl SkipStatus {
    /// Stable token for machine-readable output.
    pub fn token(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::NotExamined => "not-examined",
        }
    }

    /// The word a human-readable line opens with.
    pub fn label(self) -> &'static str {
        match self {
            Self::Blocked => "Blocked",
            Self::NotExamined => "Not checked",
        }
    }
}

impl UpdateResult {
    pub fn merge(&mut self, other: UpdateResult) {
        self.updated.extend(other.updated);
        self.unchanged += other.unchanged;
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
        self.ignored.extend(other.ignored);
        self.pinned.extend(other.pinned);
        self.held_back.extend(other.held_back);
        self.skipped_by_cooldown.extend(other.skipped_by_cooldown);
        self.entry_ecosystem.extend(other.entry_ecosystem);
        self.action_sha_updates.extend(other.action_sha_updates);
        self.skipped.extend(other.skipped);
        self.capped.extend(other.capped);
        self.annotations.extend(other.annotations);
    }

    /// Record an update that the bump ceiling refused to write.
    ///
    /// Every updater routes its ceiling rejection through here so the reason a
    /// dependency stayed put is recorded once, in one shape, rather than being
    /// added to the up-to-date tally at thirteen separate call sites.
    pub fn record_capped(
        &mut self,
        package: &str,
        current: &str,
        available: &str,
        line_number: Option<usize>,
    ) {
        self.capped.push(CappedUpdate {
            package: package.to_string(),
            current: current.to_string(),
            available: available.to_string(),
            line_number,
        });
    }
}

/// A version selected for a line in a dependency file, either resolved from a
/// registry fetch or supplied by user configuration (a pin).
pub(crate) enum PendingVersion {
    Registry(Result<String, anyhow::Error>),
    Pinned(String),
}

/// Language/ecosystem type for filtering
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum Lang {
    Python,
    Node,
    Rust,
    Go,
    Ruby,
    #[value(name = "dotnet")]
    DotNet,
    Actions,
    PreCommit,
    Mise,
    Terraform,
    GithubReleases,
    Annotated,
}

impl Lang {
    /// Canonical, stable identifier for this language (used by JSON output and CLI).
    pub fn as_str(&self) -> &'static str {
        match self {
            Lang::Python => "python",
            Lang::Node => "node",
            Lang::Rust => "rust",
            Lang::Go => "go",
            Lang::Ruby => "ruby",
            Lang::DotNet => "dotnet",
            Lang::Actions => "actions",
            Lang::PreCommit => "pre_commit",
            Lang::Mise => "mise",
            Lang::Terraform => "terraform",
            Lang::GithubReleases => "github_releases",
            Lang::Annotated => "annotated",
        }
    }
}

/// Type of dependency file
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileType {
    Requirements,
    PyProject,
    PackageJson,
    CargoToml,
    GoMod,
    Gemfile,
    Csproj,
    GithubActions,
    PreCommitConfig,
    MiseToml,
    ToolVersions,
    TerraformTf,
    /// A file whose dependencies declare their own ecosystem in a trailing
    /// comment. Unlike every other variant, the file name does not decide the
    /// registry; each annotated line does.
    Annotated,
}

impl FileType {
    /// Get the language/ecosystem for this file type
    pub fn lang(&self) -> Lang {
        match self {
            FileType::Requirements | FileType::PyProject => Lang::Python,
            FileType::PackageJson => Lang::Node,
            FileType::CargoToml => Lang::Rust,
            FileType::GoMod => Lang::Go,
            FileType::Gemfile => Lang::Ruby,
            FileType::Csproj => Lang::DotNet,
            FileType::GithubActions => Lang::Actions,
            FileType::PreCommitConfig => Lang::PreCommit,
            FileType::MiseToml | FileType::ToolVersions => Lang::Mise,
            FileType::TerraformTf => Lang::Terraform,
            FileType::Annotated => Lang::Annotated,
        }
    }

    /// Whether this recognized type is also read for `upd:` annotations.
    ///
    /// A type answering yes here runs through [`update_with_annotations`]
    /// instead of its own updater alone, so it holds two kinds of dependency:
    /// the ones its own grammar describes, and annotated ones belonging to any
    /// ecosystem. `--lang` therefore cannot be answered from
    /// [`FileType::lang`] alone for these, and `file_type_selected` asks this
    /// before turning a file away.
    ///
    /// Adding a type to the composed dispatch in `main.rs` means adding it
    /// here; `github_actions_is_scanned_for_annotations` holds the two together
    /// for the one type that composes today.
    pub fn scans_annotations(&self) -> bool {
        matches!(self, FileType::GithubActions)
    }

    /// Canonical, stable identifier for this file type (used by JSON output).
    pub fn as_str(&self) -> &'static str {
        match self {
            FileType::Requirements => "requirements",
            FileType::PyProject => "pyproject",
            FileType::PackageJson => "package_json",
            FileType::CargoToml => "cargo_toml",
            FileType::GoMod => "go_mod",
            FileType::Gemfile => "gemfile",
            FileType::Csproj => "csproj",
            FileType::GithubActions => "github_actions",
            FileType::PreCommitConfig => "pre_commit",
            FileType::MiseToml => "mise_toml",
            FileType::ToolVersions => "tool_versions",
            FileType::TerraformTf => "terraform_tf",
            FileType::Annotated => "annotated",
        }
    }
}

/// Registry name for a file type's ecosystem, in the vocabulary
/// `CooldownPolicy::effective_for` and the `[cooldown.ecosystem]` config keys
/// use. `None` means the file has no single ecosystem: its entries carry their
/// own (see `UpdateResult::entry_ecosystem`).
pub fn ecosystem_key(file_type: FileType) -> Option<&'static str> {
    Some(match file_type {
        FileType::Requirements | FileType::PyProject => "pypi",
        FileType::PackageJson => "npm",
        FileType::CargoToml => "crates.io",
        FileType::GoMod => "go-proxy",
        FileType::Gemfile => "rubygems",
        FileType::GithubActions
        | FileType::PreCommitConfig
        | FileType::MiseToml
        | FileType::ToolVersions => "github-releases",
        FileType::Csproj => "nuget",
        FileType::TerraformTf => "terraform",
        // An annotated file has no ecosystem of its own. Every entry carries
        // its own, which is what `UpdateResult::entry_ecosystem` is for.
        FileType::Annotated => return None,
    })
}

impl FileType {
    pub fn detect(path: &Path) -> Option<Self> {
        let file_name = path.file_name()?.to_str()?;

        if file_name == "pyproject.toml" {
            return Some(FileType::PyProject);
        }

        if file_name == "package.json" {
            return Some(FileType::PackageJson);
        }

        if file_name == "Cargo.toml" {
            return Some(FileType::CargoToml);
        }

        if file_name == "go.mod" {
            return Some(FileType::GoMod);
        }

        if file_name == "Gemfile" {
            return Some(FileType::Gemfile);
        }

        // .csproj files (case-insensitive extension check)
        if file_name
            .rsplit('.')
            .next()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("csproj"))
        {
            return Some(FileType::Csproj);
        }

        // Directory.Packages.props and Directory.Build.props (central package management)
        if file_name.eq_ignore_ascii_case("Directory.Packages.props")
            || file_name.eq_ignore_ascii_case("Directory.Build.props")
        {
            return Some(FileType::Csproj);
        }

        if file_name == ".pre-commit-config.yaml" {
            return Some(FileType::PreCommitConfig);
        }

        if file_name == ".mise.toml" {
            return Some(FileType::MiseToml);
        }

        if file_name == ".tool-versions" {
            return Some(FileType::ToolVersions);
        }

        // GitHub Actions workflows: *.yml or *.yaml inside .github/workflows/
        if (file_name.ends_with(".yml") || file_name.ends_with(".yaml"))
            && let Some(parent) = path.parent()
            && parent.file_name().and_then(|n| n.to_str()) == Some("workflows")
            && parent
                .parent()
                .and_then(|gp| gp.file_name())
                .and_then(|n| n.to_str())
                == Some(".github")
        {
            return Some(FileType::GithubActions);
        }

        // Terraform .tf files (exclude files inside .terraform/ directories)
        if file_name.ends_with(".tf") {
            let path_str = path.to_string_lossy();
            if !path_str.contains("/.terraform/") && !path_str.contains("\\.terraform\\") {
                return Some(FileType::TerraformTf);
            }
        }

        // Requirements file patterns (.txt and .in extensions)
        let is_requirements = |name: &str| -> bool {
            // Exact matches
            if name == "requirements.txt" || name == "requirements.in" {
                return true;
            }

            // Pattern: requirements-*.txt, requirements-*.in
            if (name.starts_with("requirements-") || name.starts_with("requirements_"))
                && (name.ends_with(".txt") || name.ends_with(".in"))
            {
                return true;
            }

            // Pattern: *-requirements.txt, *_requirements.txt, *.requirements.txt
            if name.ends_with("-requirements.txt")
                || name.ends_with("_requirements.txt")
                || name.ends_with(".requirements.txt")
                || name.ends_with("-requirements.in")
                || name.ends_with("_requirements.in")
                || name.ends_with(".requirements.in")
            {
                return true;
            }

            false
        };

        if is_requirements(file_name) {
            return Some(FileType::Requirements);
        }

        None
    }

    /// File names a directory walk opens looking for version annotations.
    /// Deliberately small, and in v1 the only set: no Markdown (it would
    /// rewrite this project's own README and every fixture in this repo), no
    /// `Dockerfile*` (those names are reserved for Docker support), no YAML.
    const ANNOTATED_FILE_NAMES: &'static [&'static str] = &[
        "Makefile",
        "makefile",
        "GNUmakefile",
        "justfile",
        "Justfile",
    ];

    /// Extensions a directory walk opens, matched against the file name.
    const ANNOTATED_FILE_EXTENSIONS: &'static [&'static str] = &[".mk", ".sh", ".bash"];

    /// `detect`, extended with the annotated set. Every earlier rule wins by
    /// construction. `explicit` is true for a file named directly on the
    /// command line, which bypasses the set entirely: `upd README.md` works
    /// with no configuration while a walk never touches Markdown.
    pub(crate) fn detect_with_annotated(path: &Path, explicit: bool) -> Option<Self> {
        if let Some(file_type) = Self::detect(path) {
            return Some(file_type);
        }

        if explicit {
            return Some(FileType::Annotated);
        }

        let file_name = path.file_name()?.to_str()?;
        if Self::ANNOTATED_FILE_NAMES.contains(&file_name)
            || Self::ANNOTATED_FILE_EXTENSIONS
                .iter()
                .any(|ext| file_name.ends_with(ext))
        {
            return Some(FileType::Annotated);
        }

        None
    }
}

/// Trait for file updaters
#[async_trait::async_trait]
pub trait Updater: Send + Sync {
    /// Update the file at the given path
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult>;

    /// Check if this updater handles the given file type
    fn handles(&self, file_type: FileType) -> bool;

    /// Parse dependencies from a file (for alignment purposes)
    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>>;
}

/// A recognized file's updater declaring which of its lines it rewrites itself.
///
/// Implementing this is what makes a file type eligible for
/// [`update_with_annotations`]. Both updaters then run over the same file in
/// one invocation, and this predicate is the boundary between them: a line the
/// file's own parser understands is never also rewritten from an `upd:`
/// annotation, so no line is written twice in a single run.
pub trait OwnsLines: Send + Sync {
    /// Whether this line carries a version this updater resolves itself.
    ///
    /// Answered from the line alone, without the surrounding file structure,
    /// so it is deliberately conservative. A line it claims is refused by the
    /// annotation pass with a warning rather than silently skipped, and a
    /// false claim therefore costs a diagnostic rather than a wrong write.
    fn owns_line(&self, line: &str) -> bool;

    /// The `--lang` selector this updater answers to.
    ///
    /// Its file is admitted by a selection naming only annotations, so this is
    /// what tells [`update_with_annotations`] to leave the file's own
    /// dependencies alone on such a run.
    fn lang(&self) -> Lang;
}

/// Run a recognized file's own updater over `path`, then the annotation pass
/// over the same file, and merge the two reports into one.
///
/// A file `upd` recognizes never reaches the annotated updater on its own.
/// `FileType::detect_with_annotated` gives a real detected type precedence, so
/// `FileType::Annotated` names only a file no other updater claimed - the rule
/// that keeps `main.tf` Terraform. Correct as far as it goes, but it also means
/// a version pinned in a recognized file that the file's own parser has no
/// concept of, such as a tool version passed to an action through a `with:`
/// input, is invisible to `upd` with no way to opt in. Composing the two passes
/// is that opt-in, and `OwnsLines` keeps them from colliding.
///
/// Order matters. The primary updater runs first and, outside a dry run, has
/// already written its changes by the time the annotation pass re-reads the
/// file, so the second pass sees the first pass's output rather than a stale
/// buffer.
pub async fn update_with_annotations<P>(
    primary: &P,
    annotated: &AnnotatedUpdater,
    path: &Path,
    registry: &dyn Registry,
    options: UpdateOptions,
) -> Result<UpdateResult>
where
    P: Updater + OwnsLines,
{
    // `--lang annotated`, or any single ecosystem, admits this file for the sake
    // of its annotations alone. The primary pass is declined here rather than at
    // the walk, because turning the file away there would take the annotations
    // with it. An empty selection means everything, as everywhere else.
    let mut result = if options.langs.is_empty() || options.langs.contains(&primary.lang()) {
        primary.update(path, registry, options.clone()).await?
    } else {
        UpdateResult::default()
    };

    // Recorded as an error rather than propagated: the primary pass may already
    // have written to the file, and returning `Err` here would drop everything
    // it reported along with the record of that write.
    match annotated.update_alongside(path, options, primary).await {
        Ok(annotated_result) => result.merge(annotated_result),
        Err(e) => result.errors.push(e.to_string()),
    }

    Ok(result)
}

/// Outcome of applying the cooldown layer to a resolved `(current -> latest)`
/// transition. See `apply_cooldown`.
pub enum CooldownOutcome {
    /// No cooldown policy active, or the latest is already old enough.
    /// The caller proceeds with this version.
    Unchanged(String),
    /// Cooldown held the update back to a safer older version. The caller
    /// writes `chosen` and records the skip.
    HeldBack {
        chosen: String,
        skipped_version: String,
        skipped_published_at: DateTime<Utc>,
    },
    /// Every candidate was too new. The caller keeps the current version and
    /// records the skip.
    Skipped {
        skipped_version: String,
        /// When the registry published `skipped_version`, or `None` when it
        /// does not say. This anchor can be a version filtered out of the
        /// candidate list (a yanked or off-track release), which is the one
        /// case where the date is genuinely unknown.
        skipped_published_at: Option<DateTime<Utc>>,
    },
}

/// A diagnostic about a condition rather than about a package. Every package
/// resolved against the same registry meets the same condition, so `key`
/// says what counts as one occurrence and `message` is what the user reads.
/// Keeping them apart is what lets the message name a concrete cause without
/// one note per dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooldownNote {
    pub key: String,
    pub message: String,
}

/// Apply the active cooldown policy to a resolved `(current -> latest)` pair.
/// Returns the outcome plus an optional diagnostic note the caller should
/// stash on `UpdateOptions::note_cooldown_unavailable` for later reporting.
pub async fn apply_cooldown(
    registry: &dyn Registry,
    package: &str,
    current: &str,
    latest: &str,
    constraints: Option<&str>,
    current_is_prerelease: bool,
    options: &UpdateOptions,
) -> (CooldownOutcome, Option<CooldownNote>) {
    let ecosystem = registry.name();
    let Some(policy) = options.cooldown_policy.as_ref() else {
        return (CooldownOutcome::Unchanged(latest.to_string()), None);
    };
    let cooldown = policy.effective_for(ecosystem);
    if cooldown <= chrono::Duration::zero() {
        return (CooldownOutcome::Unchanged(latest.to_string()), None);
    }

    // Cooldown only applies to an actual forward update. Callers resolve every
    // dependency before checking whether its version changed, so an up-to-date
    // package can legitimately arrive here with `latest == current`. Passing
    // that pair to `select` leaves no newer candidates and produces a bogus
    // cooldown skip anchored to arbitrary registry metadata (often a prerelease).
    if crate::version::compare::compare_versions(latest, current) != std::cmp::Ordering::Greater {
        return (CooldownOutcome::Unchanged(latest.to_string()), None);
    }
    let now = options.cooldown_now.unwrap_or_else(Utc::now);

    // Three outcomes, two meanings. An empty list is the registry answering
    // that it holds no publish dates, which no retry changes. An error is the
    // question going unanswered, which a retry may well change, so it is
    // reported as the failure it is rather than as a registry limitation.
    let versions = match registry.list_versions(package).await {
        Ok(v) if !v.is_empty() => v,
        Ok(_) => {
            return (
                CooldownOutcome::Unchanged(latest.to_string()),
                Some(no_publish_dates_note(ecosystem)),
            );
        }
        Err(error) => {
            return (
                CooldownOutcome::Unchanged(latest.to_string()),
                Some(CooldownNote {
                    key: format!("{ecosystem}:lookup-failed"),
                    // The cause names the package it happened to, so the wording
                    // says this is one occurrence: every other package on the
                    // ecosystem meets the same condition and reports no note.
                    message: format!(
                        "cooldown not applied for {ecosystem}: a publish date lookup failed ({error})"
                    ),
                }),
            );
        }
    };

    use crate::cooldown::{CooldownDecision, select};
    match select(
        &versions,
        current,
        latest,
        constraints,
        current_is_prerelease,
        cooldown,
        now,
    ) {
        CooldownDecision::Use {
            version,
            held_back_from: None,
        } => (CooldownOutcome::Unchanged(version), None),
        CooldownDecision::Use {
            version,
            held_back_from: Some(info),
        } => (
            CooldownOutcome::HeldBack {
                chosen: version,
                skipped_version: info.version,
                skipped_published_at: info.published_at,
            },
            None,
        ),
        CooldownDecision::Skip { latest_too_new } => (
            CooldownOutcome::Skipped {
                skipped_version: latest_too_new.version,
                // Carried through as-is. Substituting the current time here
                // dates the release to the instant of the run, which reads as
                // "released 0s ago": false, and maximally fresh, so it agrees
                // with the cooldown that skipped it and never looks wrong.
                skipped_published_at: latest_too_new.published_at,
            },
            None,
        ),
        CooldownDecision::Unsupported => (
            CooldownOutcome::Unchanged(latest.to_string()),
            Some(no_publish_dates_note(ecosystem)),
        ),
    }
}

/// The registry answered and holds no publish dates, whether by returning no
/// versions at all or versions carrying no dates. Both are the same fact to a
/// user and no retry changes either, so they read and deduplicate as one.
fn no_publish_dates_note(ecosystem: &str) -> CooldownNote {
    CooldownNote {
        key: format!("{ecosystem}:no-publish-dates"),
        message: format!("cooldown unavailable for {ecosystem}"),
    }
}

/// Hidden entries the walker is allowed to descend into or yield.
///
/// Anything not in this set that starts with `.` is pruned during discovery,
/// which keeps `.git`, `.cache`, `.venv`, and similar noise out of the scan
/// while still letting us see the dotfiles `upd` actually updates.
const ALLOWED_HIDDEN_ENTRIES: &[&str] = &[
    ".github",
    ".pre-commit-config.yaml",
    ".mise.toml",
    ".tool-versions",
];

/// Knobs for [`discover_files_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DiscoverOptions<'a> {
    /// When true, ignore `.gitignore`, `.git/info/exclude`, and the global
    /// gitignore - walk every dependency file regardless. Mirrors
    /// `rg --no-ignore`.
    pub no_ignore: bool,
    /// When true, emit one `skipping <path>: gitignored` (or `excluded by
    /// config`) line on stderr for each dependency file discovery dropped, and
    /// report bounded text files with unreachable annotation markers, so users
    /// can see why `upd` is silent on a given file.
    pub verbose: bool,
    /// Path glob patterns (from config `include`) that add otherwise-unknown
    /// files to discovery as [`FileType::Annotated`]. A real detected file type
    /// always wins, and explicit file paths do not need an include pattern.
    pub include: &'a [String],
    /// Path glob patterns (from config `exclude`) dropped during discovery.
    ///
    /// Matched against each discovered dependency file path; a leading `**/`
    /// makes a pattern depth-independent. Explicit file-path arguments bypass
    /// this list, mirroring the gitignore bypass for explicit files.
    pub exclude: &'a [String],
}

/// Discover dependency files in the given paths, optionally filtered by language.
///
/// Directory walks honor `.gitignore`, `.git/info/exclude`, and the global
/// gitignore - even outside a git repository. Hidden directories and files are
/// skipped except for the small allowlist in [`ALLOWED_HIDDEN_ENTRIES`]. Explicit
/// file paths bypass the filter and are always processed.
///
/// Convenience wrapper around [`discover_files_with`] with default options.
pub fn discover_files(paths: &[PathBuf], langs: &[Lang]) -> Vec<(PathBuf, FileType)> {
    discover_files_with(paths, langs, DiscoverOptions::default())
}

/// Compile the config `exclude` globs into a matcher.
///
/// Returns `None` when there are no usable patterns. Individual invalid
/// patterns emit a warning on stderr and are skipped so one typo does not
/// silently disable the rest of the list.
fn build_glob_set(patterns: &[String], key: &str) -> Option<globset::GlobSet> {
    if patterns.is_empty() {
        return None;
    }

    let mut builder = globset::GlobSetBuilder::new();
    let mut added = 0;
    for pattern in patterns {
        match globset::Glob::new(pattern) {
            Ok(glob) => {
                builder.add(glob);
                added += 1;
            }
            Err(e) => eprintln!("warning: invalid {key} pattern '{pattern}': {e}"),
        }
    }

    if added == 0 {
        return None;
    }

    match builder.build() {
        Ok(set) => Some(set),
        Err(e) => {
            eprintln!("warning: failed to compile {key} patterns: {e}");
            None
        }
    }
}

/// Match both the path as returned by the walker and its path relative to each
/// scanned directory. The latter makes repository-root patterns such as
/// `docker-compose.yml` work whether the CLI path was `.`, relative, or
/// absolute; matching the former preserves existing depth-independent globs.
fn glob_matches(set: Option<&globset::GlobSet>, path: &Path, scan_paths: &[PathBuf]) -> bool {
    let Some(set) = set else {
        return false;
    };
    set.is_match(path)
        || scan_paths.iter().filter(|root| root.is_dir()).any(|root| {
            path.strip_prefix(root)
                .is_ok_and(|relative| set.is_match(relative))
        })
}

#[derive(Default)]
struct WalkResult {
    files: Vec<(PathBuf, FileType)>,
    skipped_markers: Vec<PathBuf>,
}

/// Verbose discovery only: recognize the same annotation grammar the updater
/// uses, without turning content sniffing into an implicit discovery rule.
/// Keep the diagnostic bounded because this runs over otherwise-unknown files.
fn contains_annotation_marker(path: &Path) -> bool {
    const MAX_MARKER_SNIFF_SIZE: u64 = 1024 * 1024;

    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if metadata.len() > MAX_MARKER_SNIFF_SIZE {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.lines().any(|line| {
        !matches!(
            crate::annotation::parse_line(line),
            crate::annotation::ParseOutcome::None
        )
    })
}

/// Discover dependency files with explicit [`DiscoverOptions`].
pub fn discover_files_with(
    paths: &[PathBuf],
    langs: &[Lang],
    options: DiscoverOptions<'_>,
) -> Vec<(PathBuf, FileType)> {
    let include_set = build_glob_set(options.include, "include");
    let after_gitignore = walk_dependency_files(
        paths,
        langs,
        options.no_ignore,
        include_set.as_ref(),
        options.verbose,
    );

    // Explicit file-path arguments bypass the exclude list, just as they bypass
    // gitignore (the directory walker is never consulted for them).
    let explicit_files: std::collections::HashSet<&Path> = paths
        .iter()
        .filter(|p| p.is_file())
        .map(|p| p.as_path())
        .collect();

    let exclude_set = build_glob_set(options.exclude, "exclude");

    let mut kept: Vec<(PathBuf, FileType)> = Vec::with_capacity(after_gitignore.files.len());
    let mut excluded: Vec<PathBuf> = Vec::new();
    for (path, file_type) in after_gitignore.files {
        let dropped = !explicit_files.contains(path.as_path())
            && glob_matches(exclude_set.as_ref(), &path, paths);
        if dropped {
            excluded.push(path);
        } else {
            kept.push((path, file_type));
        }
    }

    // In verbose mode, surface the dependency files discovery dropped so users
    // can answer "why didn't upd touch X?" without guessing.
    if options.verbose {
        // Gitignored files: present without ignore rules but absent after them.
        // Diff against the pre-exclude set so exclude drops are not mislabeled.
        if !options.no_ignore {
            let unrestricted =
                walk_dependency_files(paths, langs, true, include_set.as_ref(), options.verbose);
            let after_gitignore_set: std::collections::HashSet<&Path> = kept
                .iter()
                .map(|(p, _)| p.as_path())
                .chain(excluded.iter().map(|p| p.as_path()))
                .chain(after_gitignore.skipped_markers.iter().map(|p| p.as_path()))
                .collect();
            for path in unrestricted
                .files
                .iter()
                .map(|(path, _)| path)
                .chain(unrestricted.skipped_markers.iter())
            {
                if !after_gitignore_set.contains(path.as_path()) {
                    eprintln!("skipping {}: gitignored", path.display());
                }
            }
        }
        for path in &excluded {
            eprintln!("skipping {}: excluded by config", path.display());
        }
        for path in &after_gitignore.skipped_markers {
            eprintln!(
                "skipping {}: contains an `upd:` marker but is not a discovery candidate (add an `include` glob to .updrc.toml)",
                path.display()
            );
        }
    }

    kept
}

/// Apply `--lang` to a discovered file.
///
/// A file is opened when the selection names its own ecosystem, and also when
/// the selection can only be answered by looking inside it. Two shapes qualify
/// for the second: [`FileType::Annotated`] is nothing but annotations, so
/// turning it away here would leave every annotated line unread whatever it
/// names; and a type that [`FileType::scans_annotations`] carries annotations
/// beside the dependencies its own grammar describes.
///
/// This is only the outer half of the decision. Admitting a file says nothing
/// about which of its dependencies get written: `lang_selected` filters the
/// annotations line by line, and [`update_with_annotations`] skips the file's
/// own updater when the selection does not name it. A file never opened,
/// though, reports nothing at all - which is why the gate here is the loose one.
fn file_type_selected(file_type: FileType, langs: &[Lang]) -> bool {
    langs.is_empty()
        || langs.contains(&file_type.lang())
        || file_type == FileType::Annotated
        || (file_type.scans_annotations() && selection_reaches_annotations(langs))
}

fn walk_dependency_files(
    paths: &[PathBuf],
    langs: &[Lang],
    no_ignore: bool,
    include_set: Option<&globset::GlobSet>,
    sniff_skipped_markers: bool,
) -> WalkResult {
    let mut result = WalkResult::default();

    for path in paths {
        if path.is_file() {
            if let Some(file_type) = FileType::detect_with_annotated(path, true)
                && file_type_selected(file_type, langs)
            {
                result.files.push((path.clone(), file_type));
            }
            continue;
        }

        if !path.is_dir() {
            continue;
        }

        let walker = WalkBuilder::new(path)
            .hidden(false)
            .git_ignore(!no_ignore)
            .git_global(!no_ignore)
            .git_exclude(!no_ignore)
            .require_git(false)
            .filter_entry(|entry| {
                // Always traverse the user-supplied root, even when it is hidden
                // (e.g. `upd .github/workflows`).
                if entry.depth() == 0 {
                    return true;
                }

                let name = entry.file_name().to_string_lossy();

                // `.git` is internal - never descend into it.
                if name == ".git" {
                    return false;
                }

                if !name.starts_with('.') {
                    return true;
                }

                // Hidden files can be selected by `include` (for example
                // `.gitlab-ci.yml`) and inspected for the verbose marker
                // diagnostic. Continue pruning unknown hidden directories.
                entry.file_type().is_some_and(|kind| kind.is_file())
                    || ALLOWED_HIDDEN_ENTRIES.contains(&name.as_ref())
            })
            .build();

        for entry in walker.flatten() {
            let entry_path = entry.path();
            if !entry_path.is_file() {
                continue;
            }
            let detected = FileType::detect_with_annotated(entry_path, false);
            let file_type = detected.or_else(|| {
                glob_matches(include_set, entry_path, paths).then_some(FileType::Annotated)
            });
            if let Some(file_type) = file_type
                && file_type_selected(file_type, langs)
            {
                result.files.push((entry_path.to_path_buf(), file_type));
            } else if detected.is_none()
                && sniff_skipped_markers
                && contains_annotation_marker(entry_path)
            {
                result.skipped_markers.push(entry_path.to_path_buf());
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// The clause `specifier_floor` picks, as the text it points at, paired with
    /// whether an update may move it.
    fn floor_text(constraint: &str) -> Option<(&str, bool)> {
        specifier_floor(constraint, 0).map(|f| (&constraint[f.range], f.raisable))
    }

    /// The plain case, and the one every other rule is an exception to: a
    /// selection admits the files of the ecosystems it names, and no others.
    #[test]
    fn a_selection_admits_the_file_types_it_names() {
        assert!(file_type_selected(FileType::CargoToml, &[Lang::Rust]));
        assert!(!file_type_selected(FileType::CargoToml, &[Lang::Python]));
        // Empty means everything.
        assert!(file_type_selected(FileType::CargoToml, &[]));
        assert!(file_type_selected(FileType::GithubActions, &[]));
    }

    /// An annotated file declares its ecosystems line by line, so the file name
    /// answers nothing and only reading it can. It is admitted whatever the
    /// selection, and `lang_selected` then decides each line.
    #[test]
    fn an_annotated_file_is_admitted_by_every_selection() {
        for langs in [
            vec![Lang::Python],
            vec![Lang::Actions],
            vec![Lang::Terraform],
            vec![Lang::Annotated],
        ] {
            assert!(
                file_type_selected(FileType::Annotated, &langs),
                "turned away by {langs:?}"
            );
        }
    }

    /// The defect this predicate exists for: a workflow carries annotations
    /// beside its own dependencies, so a selection naming only annotations has
    /// to open it. Before this, `upd -l annotated` on a tree of workflows found
    /// no dependency files at all and said so with exit 0.
    #[test]
    fn a_workflow_is_admitted_by_a_selection_that_only_reaches_annotations() {
        assert!(file_type_selected(
            FileType::GithubActions,
            &[Lang::Annotated]
        ));
        // A source's own lang, which is what an annotation names in practice.
        assert!(file_type_selected(
            FileType::GithubActions,
            &[Lang::GithubReleases]
        ));
        assert!(file_type_selected(FileType::GithubActions, &[Lang::Python]));
        // The file's own lang still admits it, on its own account.
        assert!(file_type_selected(
            FileType::GithubActions,
            &[Lang::Actions]
        ));

        // Negative control: a lang no annotation can name and that is not this
        // file's own. Without one of these the assertions above would hold for
        // a predicate that simply returned true.
        assert!(!file_type_selected(
            FileType::GithubActions,
            &[Lang::Terraform]
        ));
        assert!(!file_type_selected(
            FileType::GithubActions,
            &[Lang::PreCommit]
        ));

        // And a type that composes no annotation pass is unaffected: reaching
        // annotations is not a reason to open a `Cargo.toml`.
        assert!(!file_type_selected(FileType::CargoToml, &[Lang::Annotated]));
    }

    #[test]
    fn a_specifier_floor_is_its_lower_bound_wherever_it_is_written() {
        assert_eq!(floor_text(">=1.0,<2.0"), Some(("1.0", true)));
        assert_eq!(floor_text("<2.0,>=1.0"), Some(("1.0", true)));
        assert_eq!(floor_text("!=1.2,>=1.0,<2.0"), Some(("1.0", true)));
        assert_eq!(floor_text("~=1.4"), Some(("1.4", true)));
        assert_eq!(floor_text("==1.0.0"), Some(("1.0.0", true)));
        assert_eq!(floor_text("===1.0.0"), Some(("1.0.0", true)));
        assert_eq!(floor_text(">= 1.0 , < 2.0"), Some(("1.0", true)));
        // Cargo's bare requirement, which means `^1.0`.
        assert_eq!(floor_text("1.0"), Some(("1.0", true)));
        assert_eq!(floor_text("^1.0"), Some(("1.0", true)));
        assert_eq!(floor_text("~1.0"), Some(("1.0", true)));
    }

    /// Several lower bounds are read together by every resolver, so the release
    /// in use is the highest of them and the lower ones name nothing anyone is
    /// on. Reading whichever came first made one requirement answer two ways
    /// depending on how it was typed, which then reported a different bump
    /// level and gated differently under `--max-bump`.
    #[test]
    fn the_highest_of_several_lower_bounds_is_the_floor() {
        // The pair that motivates this: one requirement, two spellings.
        assert_eq!(floor_text(">=1.0,>=2.30,<3.0"), Some(("2.30", true)));
        assert_eq!(floor_text(">=2.30,>=1.0,<3.0"), Some(("2.30", true)));
        // Ranking is by release segments, so a shorter bound is not the lower
        // one for being shorter, and a prerelease sits under the release it
        // qualifies rather than over it.
        assert_eq!(floor_text(">=2.0,>=2.0.1"), Some(("2.0.1", true)));
        assert_eq!(floor_text(">=2.0.1,>=2.0"), Some(("2.0.1", true)));
        assert_eq!(
            floor_text(">=2.0.0-rc.1,>=1.9.0"),
            Some(("2.0.0-rc.1", true))
        );
        assert_eq!(floor_text(">=2.0.0-rc.1,>=2.0.0"), Some(("2.0.0", true)));
        // A bound that names no floor takes no part in the ranking, whichever
        // side of the highest one it is written.
        assert_eq!(floor_text(">=1.0,>2.30,<3.0"), Some(("1.0", true)));
        assert_eq!(floor_text(">=1.0,~=2.4"), Some(("2.4", true)));

        // Equal bounds keep the first, and bounds that cannot be ranked at all
        // keep it too, so a specifier upd cannot fully read answers as before.
        let floor = |c: &str| specifier_floor(c, 0).unwrap().range;
        assert_eq!(floor(">=2.0,>=2.0"), 2..5);
        assert_eq!(floor(">=1!2.0,>=3.0"), 2..7);
    }

    /// A bound that names a version the specifier does not admit is not a floor,
    /// whatever direction it points. `>1.0` rules that version out rather than
    /// standing on it, so raising it writes a specifier that excludes the release
    /// it was raised to; a ceiling or an exclusion never had a floor to begin
    /// with. The position is still reported, because callers that only read a
    /// specifier need it and only callers that rewrite one need `raisable`.
    #[test]
    fn a_bound_the_specifier_does_not_admit_is_not_a_floor() {
        assert_eq!(floor_text(">1.0"), Some(("1.0", false)));
        assert_eq!(floor_text(">1.0,<2.0"), Some(("1.0", false)));
        assert_eq!(floor_text("<2.0,>1.0"), Some(("2.0", false)));
        assert_eq!(floor_text("<6"), Some(("6", false)));
        assert_eq!(floor_text("<=6"), Some(("6", false)));
        assert_eq!(floor_text("<6,!=5.0"), Some(("6", false)));
        assert_eq!(floor_text("!=1.5"), Some(("1.5", false)));
        // Nothing that reads as a version at all.
        assert_eq!(floor_text(""), None);
        assert_eq!(floor_text("*"), None);
    }

    /// The range is a byte offset into the caller's own string, not into the
    /// constraint, so a rewrite lands on the right clause of the whole line.
    #[test]
    fn a_specifier_floor_range_is_offset_by_its_base() {
        let line = "botocore<1.35.0,>=1.34.0";
        let base = "botocore".len();
        let floor = specifier_floor(&line[base..], base).unwrap();
        assert_eq!(&line[floor.range], "1.34.0");
        assert!(floor.raisable);
    }

    /// Each ecosystem reads a partial bound its own way, and the difference is
    /// not cosmetic: `>1.0` admits 1.0.229 under PEP 440 and admits nothing in
    /// the 1.0 series under Cargo. Answering with the other ecosystem's parser
    /// reports a requirement no release satisfies as satisfied.
    #[test]
    fn each_ecosystem_reads_a_partial_bound_with_its_own_parser() {
        assert_eq!(pep440_admits(">2.0", "2.7.0"), Some(true));
        assert_eq!(cargo_admits(">1.0", "1.0.229"), Some(false));
        assert_eq!(cargo_admits(">1.0", "1.1.0"), Some(true));

        assert_eq!(pep440_admits("<6", "5.2.0"), Some(true));
        assert_eq!(pep440_admits("<6", "6.0.1"), Some(false));
        assert_eq!(cargo_admits("<2.0", "1.0.229"), Some(true));
        assert_eq!(cargo_admits("*", "1.5.0"), Some(true));

        // Unreadable is its own answer, never "behind".
        assert_eq!(pep440_admits("not-a-spec", "1.0"), None);
        assert_eq!(cargo_admits(">=1.0.0 <2.0.0", "1.5.0"), None);
        assert_eq!(cargo_admits("<2.0", "not-a-version"), None);
    }

    #[test]
    fn classify_bump_reads_the_usual_semver_steps() {
        assert_eq!(classify_bump("1.0.0", "2.0.0"), BumpKind::Major);
        assert_eq!(classify_bump("1.5.3", "1.6.0"), BumpKind::Minor);
        assert_eq!(classify_bump("1.5.3", "1.5.4"), BumpKind::Patch);
        assert_eq!(classify_bump("0.9.0", "1.0.0"), BumpKind::Major);
    }

    #[test]
    fn classify_bump_tolerates_a_v_prefix_and_missing_segments() {
        assert_eq!(classify_bump("v1.2.3", "v2.0.0"), BumpKind::Major);
        assert_eq!(classify_bump("1", "2"), BumpKind::Major);
        assert_eq!(classify_bump("1.2", "1.3"), BumpKind::Minor);
    }

    /// Range specs reach the classifier verbatim: npm comparator ranges from
    /// the package.json updater, and caret/tilde specs anywhere a raw spec is
    /// recorded. The ceiling gates on the range's lower bound, so the level
    /// reported for one has to be the level the ceiling gated on. Reading the
    /// whole range string instead falls through to `patch`, which tells the
    /// reader a patch ceiling would let the change through when it would not.
    #[test]
    fn classify_bump_reads_the_lower_bound_of_a_range_spec() {
        assert_eq!(
            classify_bump(">=1.0.0 <2.0.0", ">=1.5.0 <2.0.0"),
            BumpKind::Minor
        );
        assert_eq!(classify_bump("^1.2.3", "^2.0.0"), BumpKind::Major);
        assert_eq!(classify_bump(">=1.0,<2.0", ">=1.0.1,<2.0"), BumpKind::Patch);
        assert_eq!(classify_bump("~=1.4", "~=1.5"), BumpKind::Minor);
    }

    #[test]
    fn classify_bump_falls_back_to_patch_for_unparseable_versions() {
        assert_eq!(classify_bump("abc", "1.0.0"), BumpKind::Patch);
        assert_eq!(classify_bump("1.0.0", "abc"), BumpKind::Patch);
        assert_eq!(classify_bump("", "1.0.0"), BumpKind::Patch);
    }

    /// `^0.12` resolves to `>=0.12, <0.13` under both Cargo and npm, so moving
    /// to 0.13 is a breaking change wearing a minor version number. Reading it
    /// as minor is what let `--max-bump minor` apply `reqwest 0.12 -> 0.13`
    /// unattended and break every caller of a renamed feature.
    #[test]
    fn a_zero_major_minor_step_is_breaking() {
        assert_eq!(classify_bump("0.12", "0.13"), BumpKind::Major);
        assert_eq!(classify_bump("0.12.1", "0.13.0"), BumpKind::Major);
        assert_eq!(classify_bump("0.1.0", "0.2.0"), BumpKind::Major);

        let minor_ceiling = BumpFilter {
            major: false,
            minor: true,
            patch: true,
        };
        assert!(!minor_ceiling.allows("0.12", "0.13"));
    }

    /// One digit further down the same rule applies: `^0.0.3` means
    /// `>=0.0.3, <0.0.4`, so there is no compatible newer release at all.
    #[test]
    fn a_zero_zero_patch_step_is_breaking() {
        assert_eq!(classify_bump("0.0.3", "0.0.4"), BumpKind::Major);
        assert_eq!(classify_bump("0.0.3", "0.1.0"), BumpKind::Major);
    }

    /// The narrowing stops at the patch segment of a non-zero minor, which is
    /// what `^0.12.1` genuinely permits. Without this the whole zero-major
    /// range would freeze under a minor ceiling.
    #[test]
    fn a_zero_major_patch_step_stays_compatible() {
        assert_eq!(classify_bump("0.12.1", "0.12.4"), BumpKind::Patch);

        let minor_ceiling = BumpFilter {
            major: false,
            minor: true,
            patch: true,
        };
        assert!(minor_ceiling.allows("0.12.1", "0.12.4"));
    }

    #[test]
    fn new_lang_wire_values_are_snake_case_and_not_the_clap_names() {
        use clap::ValueEnum;
        assert_eq!(Lang::GithubReleases.as_str(), "github_releases");
        assert_eq!(Lang::Annotated.as_str(), "annotated");
        assert_eq!(
            Lang::GithubReleases.to_possible_value().unwrap().get_name(),
            "github-releases"
        );
        assert_eq!(
            Lang::Annotated.to_possible_value().unwrap().get_name(),
            "annotated"
        );
    }

    /// The built-in set, matched against the file name alone.
    /// Every pattern is a bare name with no `/`, which is why file-name
    /// matching is right here and a scan-root-relative path is not needed.
    #[test]
    fn detect_with_annotated_claims_the_built_in_names() {
        for name in [
            "Makefile",
            "makefile",
            "GNUmakefile",
            "common.mk",
            "justfile",
            "Justfile",
            "release.sh",
            "release.bash",
        ] {
            let path = PathBuf::from("/repo/sub").join(name);
            assert_eq!(
                FileType::detect_with_annotated(&path, false),
                Some(FileType::Annotated),
                "{name} must be claimed by a walk"
            );
        }
    }

    /// The negative control for the set above. `Dockerfile` is reserved for
    /// Docker support and Markdown would rewrite this project's own README, so
    /// neither is in the set.
    #[test]
    fn detect_with_annotated_leaves_unlisted_names_alone() {
        for name in [
            "README.md",
            "Dockerfile",
            "Dockerfile.ci",
            "notes.txt",
            "build.zsh",
        ] {
            let path = PathBuf::from("/repo").join(name);
            assert_eq!(
                FileType::detect_with_annotated(&path, false),
                None,
                "{name} must not be claimed by a walk"
            );
        }
    }

    /// An earlier rule always wins, which holds by construction because
    /// `detect` runs first. `detect` is path-sensitive in
    /// two places and still receives the real candidate path, so both survive.
    #[test]
    fn detect_with_annotated_never_overrides_an_earlier_rule() {
        assert_eq!(
            FileType::detect_with_annotated(Path::new("/repo/Cargo.toml"), false),
            Some(FileType::CargoToml)
        );
        assert_eq!(
            FileType::detect_with_annotated(Path::new("/repo/.github/workflows/ci.yml"), false),
            Some(FileType::GithubActions)
        );
        let cached = Path::new("/repo/.terraform/modules/main.tf");
        assert_eq!(FileType::detect(cached), None);
        assert_eq!(
            FileType::detect_with_annotated(cached, false),
            None,
            "a .tf under .terraform/ is excluded by detect and is not in the annotated set either"
        );
    }

    /// An explicit file-path argument bypasses the pattern set, which is what
    /// makes `upd README.md` work with no configuration. The
    /// bypass still yields to an earlier rule.
    #[test]
    fn an_explicit_path_is_annotated_whatever_its_name() {
        let notes = Path::new("/repo/some-notes.txt");
        assert_eq!(FileType::detect_with_annotated(notes, false), None);
        assert_eq!(
            FileType::detect_with_annotated(notes, true),
            Some(FileType::Annotated)
        );
        assert_eq!(
            FileType::detect_with_annotated(Path::new("/repo/go.mod"), true),
            Some(FileType::GoMod)
        );
    }

    /// `--lang` filters at discovery, before a line is read, so it
    /// cannot know which ecosystems a file's annotations name. An annotated
    /// file is therefore admitted whatever the selection; without this,
    /// `--lang python` drops the Makefile and its PyPI pins are never seen.
    #[test]
    fn a_lang_filter_never_drops_an_annotated_file() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Makefile"),
            "BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\n",
        )
        .unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();

        let found = walk_dependency_files(
            &[dir.path().to_path_buf()],
            &[Lang::Python],
            true,
            None,
            false,
        )
        .files;

        assert!(
            found.iter().any(|(_, ft)| *ft == FileType::Annotated),
            "--lang python must still admit the Makefile: {found:?}"
        );
        assert!(
            !found.iter().any(|(_, ft)| *ft == FileType::CargoToml),
            "--lang python must still drop Cargo.toml: {found:?}"
        );

        let explicit = walk_dependency_files(
            &[dir.path().join("Makefile")],
            &[Lang::Rust],
            true,
            None,
            false,
        )
        .files;
        assert_eq!(
            explicit.len(),
            1,
            "an explicitly named annotated file survives --lang rust too: {explicit:?}"
        );
    }

    /// The built-in set is unconditional, not gated on an option
    /// that happens to default to empty. `discover_files` passes
    /// `DiscoverOptions::default()`, so this is the guard for that.
    #[test]
    fn discover_files_finds_a_makefile_by_default() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Makefile"),
            "BAO_VERSION ?= 2.6.1  # upd: pypi openbao-cli\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Dockerfile"),
            "ARG BAO_VERSION=2.6.1  # upd: pypi openbao-cli\n",
        )
        .unwrap();

        let found = discover_files(&[dir.path().to_path_buf()], &[]);

        assert!(
            found
                .iter()
                .any(|(p, ft)| p.file_name().unwrap() == "Makefile" && *ft == FileType::Annotated),
            "{found:?}"
        );
        assert!(
            !found
                .iter()
                .any(|(p, _)| p.file_name().unwrap() == "Dockerfile"),
            "Dockerfile is reserved for the Docker design: {found:?}"
        );
    }

    #[test]
    fn bump_filter_rejects_empty_current_version() {
        // An empty/missing current version means the updater failed to extract a
        // real version. Writing would corrupt the file (e.g. a non-version string
        // gaining an appended digit: "hello" -> "hello1"). The default filter must
        // refuse such a write even though it permits every bump level.
        let filter = BumpFilter::default();
        assert!(
            !filter.allows("", "1.0.0"),
            "an empty current version must never be writable"
        );
        assert!(
            !filter.allows("   ", "1.0.0"),
            "a blank current version must never be writable"
        );
        assert!(
            !filter.allows("1.0.0", ""),
            "an empty target version must never be writable"
        );
    }

    #[test]
    fn with_langs_carries_the_selection_and_defaults_to_empty() {
        let plain = UpdateOptions::new(true, false);
        assert!(
            plain.langs.is_empty(),
            "an empty selection means every lang, so the default cannot filter"
        );
        let filtered = UpdateOptions::new(true, false).with_langs(vec![Lang::Python]);
        assert_eq!(filtered.langs, vec![Lang::Python]);
    }

    #[test]
    fn write_file_atomic_preserves_crlf_and_bom() {
        // Original: UTF-8 BOM + CRLF line endings (a Windows/.NET style file).
        let dir = tempdir().unwrap();
        let path = dir.path().join("Cargo.toml");
        let mut original = vec![0xEF, 0xBB, 0xBF];
        original.extend_from_slice(b"a = 1\r\nb = 2\r\n");
        fs::write(&path, &original).unwrap();

        // The in-memory rewrite produces plain LF with no BOM.
        write_file_atomic(&path, "a = 1\nb = 3\n").unwrap();

        let mut expected = vec![0xEF, 0xBB, 0xBF];
        expected.extend_from_slice(b"a = 1\r\nb = 3\r\n");
        assert_eq!(
            fs::read(&path).unwrap(),
            expected,
            "atomic write must preserve the original BOM and CRLF line endings"
        );
    }

    #[test]
    fn write_file_atomic_lf_file_stays_lf_without_bom() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("x.txt");
        fs::write(&path, b"a\nb\n").unwrap();
        write_file_atomic(&path, "a\nc\n").unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            b"a\nc\n",
            "an LF file must stay LF with no BOM introduced"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_file_atomic_preserves_original_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("Cargo.toml");
        fs::write(&path, "old = 1\n").unwrap();
        // Mark the manifest read-only (0o444): a common "do not modify" signal.
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

        write_file_atomic(&path, "new = 2\n").unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "new = 2\n",
            "atomic write must still update the content"
        );
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o444,
            "atomic write must preserve original permissions, not reset them to the umask default"
        );
    }

    #[test]
    fn bump_filter_allows_normal_updates_by_default() {
        let filter = BumpFilter::default();
        assert!(filter.allows("1.0.0", "2.0.0"), "major allowed by default");
        assert!(filter.allows("1.0.0", "1.1.0"), "minor allowed by default");
        assert!(filter.allows("1.0.0", "1.0.1"), "patch allowed by default");
        assert!(
            filter.allows("v3", "v4"),
            "tag-style major allowed by default"
        );
    }

    #[test]
    fn test_update_result_merge() {
        let mut result1 = UpdateResult {
            updated: vec![(
                "pkg1".to_string(),
                "1.0".to_string(),
                "2.0".to_string(),
                Some(1),
            )],
            unchanged: 5,
            errors: vec!["error1".to_string()],
            warnings: vec!["warn1".to_string()],
            ignored: vec![("ignored1".to_string(), "1.0".to_string(), Some(3))],
            pinned: vec![(
                "pinned1".to_string(),
                "1.0".to_string(),
                "1.5".to_string(),
                Some(4),
            )],
            ..Default::default()
        };

        let result2 = UpdateResult {
            updated: vec![(
                "pkg2".to_string(),
                "2.0".to_string(),
                "3.0".to_string(),
                Some(2),
            )],
            unchanged: 3,
            errors: vec!["error2".to_string()],
            warnings: vec!["warn2".to_string()],
            ignored: vec![("ignored2".to_string(), "2.0".to_string(), Some(5))],
            pinned: vec![(
                "pinned2".to_string(),
                "2.0".to_string(),
                "2.5".to_string(),
                Some(6),
            )],
            ..Default::default()
        };

        result1.merge(result2);

        assert_eq!(result1.updated.len(), 2);
        assert_eq!(result1.unchanged, 8);
        assert_eq!(result1.errors.len(), 2);
        assert_eq!(result1.warnings.len(), 2);
        assert_eq!(result1.ignored.len(), 2);
        assert_eq!(result1.pinned.len(), 2);
        assert_eq!(result1.updated[0].0, "pkg1");
        assert_eq!(result1.updated[1].0, "pkg2");
        assert_eq!(result1.ignored[0].0, "ignored1");
        assert_eq!(result1.ignored[1].0, "ignored2");
        assert_eq!(result1.pinned[0].0, "pinned1");
        assert_eq!(result1.pinned[1].0, "pinned2");
    }

    #[test]
    fn test_update_result_default() {
        let result = UpdateResult::default();
        assert!(result.updated.is_empty());
        assert_eq!(result.unchanged, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_filetype_as_str_is_unique_and_stable() {
        let mut seen = std::collections::HashSet::new();
        for ft in ALL_FILE_TYPES {
            let name = ft.as_str();
            assert!(
                seen.insert(name),
                "duplicate FileType::as_str value: {name}"
            );
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "FileType::as_str must be snake_case ASCII: {name}"
            );
        }
        assert_eq!(FileType::PackageJson.as_str(), "package_json");
        assert_eq!(FileType::TerraformTf.as_str(), "terraform_tf");
    }

    #[test]
    fn test_lang_as_str_is_unique_and_stable() {
        let variants = [
            Lang::Python,
            Lang::Node,
            Lang::Rust,
            Lang::Go,
            Lang::Ruby,
            Lang::DotNet,
            Lang::Actions,
            Lang::PreCommit,
            Lang::Mise,
            Lang::Terraform,
        ];
        let mut seen = std::collections::HashSet::new();
        for lang in variants {
            let name = lang.as_str();
            assert!(seen.insert(name), "duplicate Lang::as_str value: {name}");
        }
        assert_eq!(Lang::DotNet.as_str(), "dotnet");
        assert_eq!(Lang::PreCommit.as_str(), "pre_commit");
    }

    #[test]
    fn test_discover_files_single_file() {
        let temp = tempdir().unwrap();
        let req_path = temp.path().join("requirements.txt");
        fs::write(&req_path, "flask>=2.0").unwrap();

        let files = discover_files(std::slice::from_ref(&req_path), &[]);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, req_path);
        assert_eq!(files[0].1, FileType::Requirements);
    }

    #[test]
    fn test_discover_files_directory() {
        let temp = tempdir().unwrap();

        // Create various dependency files
        fs::write(temp.path().join("requirements.txt"), "flask>=2.0").unwrap();
        fs::write(
            temp.path().join("pyproject.toml"),
            "[project]\nname = \"test\"",
        )
        .unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        // Create a non-matching file
        fs::write(temp.path().join("README.md"), "# Test").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[]);

        assert_eq!(files.len(), 3);

        // Check that all expected file types are present
        let types: Vec<_> = files.iter().map(|(_, ft)| *ft).collect();
        assert!(types.contains(&FileType::Requirements));
        assert!(types.contains(&FileType::PyProject));
        assert!(types.contains(&FileType::PackageJson));
    }

    #[test]
    fn test_discover_files_multiple_requirements() {
        let temp = tempdir().unwrap();

        fs::write(temp.path().join("requirements.txt"), "flask>=2.0").unwrap();
        fs::write(temp.path().join("requirements-dev.txt"), "pytest>=7.0").unwrap();
        fs::write(temp.path().join("requirements.in"), "django>=4.0").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[]);

        assert_eq!(files.len(), 3);
        assert!(files.iter().all(|(_, ft)| *ft == FileType::Requirements));
    }

    #[test]
    fn test_discover_files_empty_directory() {
        let temp = tempdir().unwrap();
        let files = discover_files(&[temp.path().to_path_buf()], &[]);
        assert!(files.is_empty());
    }

    #[test]
    fn test_discover_files_nonexistent_path() {
        let files = discover_files(&[PathBuf::from("/nonexistent/path")], &[]);
        assert!(files.is_empty());
    }

    #[test]
    fn test_discover_files_mixed_paths() {
        let temp = tempdir().unwrap();

        // Create a file directly in temp
        let direct_file = temp.path().join("requirements.txt");
        fs::write(&direct_file, "flask>=2.0").unwrap();

        // Create a subdirectory with a file
        let subdir = temp.path().join("subdir");
        fs::create_dir(&subdir).unwrap();
        fs::write(subdir.join("package.json"), "{}").unwrap();

        // Discover from both paths
        let files = discover_files(&[direct_file.clone(), subdir.clone()], &[]);

        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_file_type_detection() {
        // PyProject
        assert_eq!(
            FileType::detect(Path::new("pyproject.toml")),
            Some(FileType::PyProject)
        );
        assert_eq!(
            FileType::detect(Path::new("/some/path/pyproject.toml")),
            Some(FileType::PyProject)
        );

        // Package.json
        assert_eq!(
            FileType::detect(Path::new("package.json")),
            Some(FileType::PackageJson)
        );
        assert_eq!(
            FileType::detect(Path::new("/some/path/package.json")),
            Some(FileType::PackageJson)
        );

        // Requirements.txt patterns
        assert_eq!(
            FileType::detect(Path::new("requirements.txt")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("requirements.in")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("requirements-dev.txt")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("requirements_dev.txt")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("requirements-dev.in")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("dev-requirements.txt")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("dev_requirements.txt")),
            Some(FileType::Requirements)
        );
        assert_eq!(
            FileType::detect(Path::new("dev.requirements.txt")),
            Some(FileType::Requirements)
        );

        // Cargo.toml
        assert_eq!(
            FileType::detect(Path::new("Cargo.toml")),
            Some(FileType::CargoToml)
        );
        assert_eq!(
            FileType::detect(Path::new("/some/path/Cargo.toml")),
            Some(FileType::CargoToml)
        );

        // go.mod
        assert_eq!(FileType::detect(Path::new("go.mod")), Some(FileType::GoMod));
        assert_eq!(
            FileType::detect(Path::new("/some/path/go.mod")),
            Some(FileType::GoMod)
        );

        // Pre-commit config
        assert_eq!(
            FileType::detect(Path::new(".pre-commit-config.yaml")),
            Some(FileType::PreCommitConfig)
        );

        // Gemfile
        assert_eq!(
            FileType::detect(Path::new("Gemfile")),
            Some(FileType::Gemfile)
        );

        // Mise
        assert_eq!(
            FileType::detect(Path::new(".mise.toml")),
            Some(FileType::MiseToml)
        );

        // Tool versions
        assert_eq!(
            FileType::detect(Path::new(".tool-versions")),
            Some(FileType::ToolVersions)
        );

        // Non-matching patterns
        assert_eq!(FileType::detect(Path::new("requirements")), None);
        assert_eq!(FileType::detect(Path::new("requirements-dev")), None);
        assert_eq!(FileType::detect(Path::new("setup.py")), None);
        assert_eq!(FileType::detect(Path::new("cargo.toml")), None); // lowercase doesn't match
    }

    #[test]
    fn test_file_type_lang_mapping() {
        assert_eq!(FileType::Requirements.lang(), Lang::Python);
        assert_eq!(FileType::PyProject.lang(), Lang::Python);
        assert_eq!(FileType::PackageJson.lang(), Lang::Node);
        assert_eq!(FileType::CargoToml.lang(), Lang::Rust);
        assert_eq!(FileType::GoMod.lang(), Lang::Go);
        assert_eq!(FileType::Gemfile.lang(), Lang::Ruby);
        assert_eq!(FileType::GithubActions.lang(), Lang::Actions);
        assert_eq!(FileType::PreCommitConfig.lang(), Lang::PreCommit);
        assert_eq!(FileType::MiseToml.lang(), Lang::Mise);
        assert_eq!(FileType::ToolVersions.lang(), Lang::Mise);
    }

    #[test]
    fn test_discover_files_with_lang_filter() {
        let temp = tempdir().unwrap();

        // Create files for different ecosystems
        fs::write(temp.path().join("requirements.txt"), "flask>=2.0").unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();
        fs::write(temp.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        fs::write(temp.path().join("go.mod"), "module test").unwrap();

        // No filter - should get all 4
        let files = discover_files(&[temp.path().to_path_buf()], &[]);
        assert_eq!(files.len(), 4);

        // Filter for Python only
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Python]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::Requirements);

        // Filter for Node only
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Node]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::PackageJson);

        // Filter for Rust only
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Rust]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::CargoToml);

        // Filter for Go only
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Go]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::GoMod);

        // Filter for Python and Rust
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Python, Lang::Rust]);
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_discover_github_actions_files() {
        let temp = tempdir().unwrap();
        let workflows_dir = temp.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();
        fs::write(workflows_dir.join("ci.yml"), "name: CI\non: push").unwrap();
        fs::write(workflows_dir.join("release.yaml"), "name: Release").unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[]);
        assert_eq!(files.len(), 3);
        let types: Vec<_> = files.iter().map(|(_, ft)| *ft).collect();
        assert!(types.contains(&FileType::PackageJson));
        assert_eq!(
            types
                .iter()
                .filter(|ft| **ft == FileType::GithubActions)
                .count(),
            2
        );
    }

    #[test]
    fn test_discover_github_actions_respects_lang_filter() {
        let temp = tempdir().unwrap();
        let workflows_dir = temp.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();
        fs::write(workflows_dir.join("ci.yml"), "name: CI").unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Actions]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::GithubActions);

        // A lang reaching neither the workflow's own dependencies nor anything
        // an annotation could name leaves it out.
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::PreCommit]);
        assert!(files.is_empty(), "{files:?}");

        // `node` is a lang an annotation can name, so the workflow is opened
        // for the annotations it may carry, as `package.json` is for its own
        // dependencies. Which lines are then written is decided per line, and
        // the `uses:` refs are not among them.
        let mut files = discover_files(&[temp.path().to_path_buf()], &[Lang::Node]);
        files.sort_by_key(|(_, file_type)| file_type.as_str());
        assert_eq!(
            files
                .iter()
                .map(|(_, file_type)| *file_type)
                .collect::<Vec<_>>(),
            vec![FileType::GithubActions, FileType::PackageJson],
        );
    }

    #[test]
    fn test_discover_pre_commit_config() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join(".pre-commit-config.yaml"), "repos: []").unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[]);
        let types: Vec<_> = files.iter().map(|(_, ft)| *ft).collect();
        assert!(types.contains(&FileType::PreCommitConfig));
        assert!(types.contains(&FileType::PackageJson));
    }

    #[test]
    fn test_discover_mise_files() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join(".mise.toml"), "[tools]\nnode = \"20\"").unwrap();
        fs::write(temp.path().join(".tool-versions"), "node 20").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Mise]);
        assert_eq!(files.len(), 2);
        let types: Vec<_> = files.iter().map(|(_, ft)| *ft).collect();
        assert!(types.contains(&FileType::MiseToml));
        assert!(types.contains(&FileType::ToolVersions));
    }

    #[test]
    fn test_discover_mise_respects_lang_filter() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join(".mise.toml"), "[tools]").unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        // Node filter should not include mise files
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Node]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, FileType::PackageJson);
    }

    #[test]
    fn test_discover_nested_hidden_ecosystem_files() {
        let temp = tempdir().unwrap();
        let nested = temp.path().join("apps").join("api");

        fs::create_dir_all(nested.join(".github").join("workflows")).unwrap();
        fs::write(
            nested.join(".github").join("workflows").join("ci.yml"),
            "name: CI",
        )
        .unwrap();
        fs::write(nested.join(".pre-commit-config.yaml"), "repos: []").unwrap();
        fs::write(nested.join(".mise.toml"), "[tools]\nnode = \"20\"").unwrap();
        fs::write(nested.join(".tool-versions"), "node 20").unwrap();

        let files = discover_files(&[temp.path().to_path_buf()], &[]);
        let paths: Vec<_> = files.iter().map(|(path, _)| path.clone()).collect();

        assert!(paths.contains(&nested.join(".github").join("workflows").join("ci.yml")));
        assert!(paths.contains(&nested.join(".pre-commit-config.yaml")));
        assert!(paths.contains(&nested.join(".mise.toml")));
        assert!(paths.contains(&nested.join(".tool-versions")));
    }

    #[test]
    fn test_discover_no_github_dir() {
        let temp = tempdir().unwrap();
        let files = discover_files(&[temp.path().to_path_buf()], &[Lang::Actions]);
        assert!(files.is_empty());
    }

    /// Detection of a workflow file should depend on the .github/workflows
    /// path components, not just the extension. A bare YAML file elsewhere
    /// must not be classified as a GitHub Actions workflow.
    #[test]
    fn test_filetype_detect_github_actions_requires_workflows_dir() {
        assert_eq!(
            FileType::detect(Path::new("/repo/.github/workflows/ci.yml")),
            Some(FileType::GithubActions)
        );
        assert_eq!(
            FileType::detect(Path::new("/repo/.github/workflows/release.yaml")),
            Some(FileType::GithubActions)
        );
        // A workflow nested under a sub-project still counts.
        assert_eq!(
            FileType::detect(Path::new("/repo/apps/api/.github/workflows/ci.yml")),
            Some(FileType::GithubActions)
        );
        // Anything outside .github/workflows is not a workflow.
        assert_eq!(
            FileType::detect(Path::new("/repo/.github/dependabot.yml")),
            None
        );
        assert_eq!(FileType::detect(Path::new("/repo/random.yml")), None);
    }

    /// Files listed in .gitignore must not be discovered when walking a
    /// directory. This is the single most-requested default - users expect
    /// `upd` to ignore the same things git does.
    #[test]
    fn test_discover_files_respects_gitignore() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        fs::write(
            root.join(".gitignore"),
            "ignored.txt\nvendor/\n.github/workflows/internal.yml\n.pre-commit-config.yaml\n.mise.toml\n.tool-versions\n",
        )
        .unwrap();

        // Regular files: kept.
        fs::write(root.join("requirements.txt"), "flask>=2.0").unwrap();
        // Regular file: ignored by name.
        fs::write(root.join("ignored.txt"), "should-not-appear").unwrap();
        // Whole gitignored directory.
        fs::create_dir_all(root.join("vendor")).unwrap();
        fs::write(
            root.join("vendor").join("Cargo.toml"),
            "[package]\nname=\"x\"",
        )
        .unwrap();

        // GitHub Actions: one ignored, one kept.
        let workflows = root.join(".github").join("workflows");
        fs::create_dir_all(&workflows).unwrap();
        fs::write(workflows.join("ci.yml"), "name: CI").unwrap();
        fs::write(workflows.join("internal.yml"), "name: Internal").unwrap();

        // Hidden ecosystem files: gitignored.
        fs::write(root.join(".pre-commit-config.yaml"), "repos: []").unwrap();
        fs::write(root.join(".mise.toml"), "[tools]").unwrap();
        fs::write(root.join(".tool-versions"), "node 20").unwrap();

        let files = discover_files(&[root.to_path_buf()], &[]);
        let paths: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();

        assert!(paths.contains(&root.join("requirements.txt")));
        assert!(paths.contains(&workflows.join("ci.yml")));

        for forbidden in [
            root.join("ignored.txt"),
            root.join("vendor").join("Cargo.toml"),
            workflows.join("internal.yml"),
            root.join(".pre-commit-config.yaml"),
            root.join(".mise.toml"),
            root.join(".tool-versions"),
        ] {
            assert!(
                !paths.contains(&forbidden),
                "discover_files should skip gitignored {forbidden:?}; got {paths:#?}"
            );
        }
    }

    /// Negation patterns (`!foo`) must un-ignore matching paths so users
    /// can ship a top-level ignore but selectively keep individual files.
    #[test]
    fn test_discover_files_respects_gitignore_negation() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        fs::write(
            root.join(".gitignore"),
            ".github/workflows/*.yml\n!.github/workflows/keep.yml\n",
        )
        .unwrap();

        let workflows = root.join(".github").join("workflows");
        fs::create_dir_all(&workflows).unwrap();
        fs::write(workflows.join("drop.yml"), "name: Drop").unwrap();
        fs::write(workflows.join("keep.yml"), "name: Keep").unwrap();

        let files = discover_files(&[root.to_path_buf()], &[Lang::Actions]);
        let paths: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();

        assert!(paths.contains(&workflows.join("keep.yml")));
        assert!(!paths.contains(&workflows.join("drop.yml")));
    }

    /// Explicit file arguments bypass the gitignore filter - `upd
    /// path/to/file` should always process the file the user pointed at,
    /// matching `rg` / `fd` semantics for explicit paths.
    #[test]
    fn test_discover_files_explicit_path_bypasses_gitignore() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        fs::write(root.join(".gitignore"), ".mise.toml\n").unwrap();
        let mise = root.join(".mise.toml");
        fs::write(&mise, "[tools]").unwrap();

        // Directory walk: gitignored, so excluded.
        let walked = discover_files(&[root.to_path_buf()], &[]);
        assert!(walked.iter().all(|(p, _)| p != &mise));

        // Explicit path: included regardless of gitignore.
        let direct = discover_files(std::slice::from_ref(&mise), &[]);
        assert_eq!(direct, vec![(mise, FileType::MiseToml)]);
    }

    /// `--no-ignore` (via `DiscoverOptions::no_ignore`) must bypass
    /// gitignore entirely, including for hidden ecosystem files.
    #[test]
    fn test_discover_files_with_no_ignore_bypasses_gitignore() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        fs::write(
            root.join(".gitignore"),
            "ignored.txt\n.mise.toml\n.tool-versions\n",
        )
        .unwrap();
        fs::write(root.join("ignored.txt"), "x").unwrap();
        fs::write(root.join("requirements.txt"), "flask>=2.0").unwrap();
        fs::write(root.join(".mise.toml"), "[tools]").unwrap();
        fs::write(root.join(".tool-versions"), "node 20").unwrap();

        let unrestricted = discover_files_with(
            &[root.to_path_buf()],
            &[],
            DiscoverOptions {
                no_ignore: true,
                verbose: false,
                include: &[],
                exclude: &[],
            },
        );
        let paths: Vec<PathBuf> = unrestricted.iter().map(|(p, _)| p.clone()).collect();

        assert!(paths.contains(&root.join("requirements.txt")));
        assert!(paths.contains(&root.join(".mise.toml")));
        assert!(paths.contains(&root.join(".tool-versions")));
        // ignored.txt is not a dependency file, so it never appears regardless;
        // the meaningful assertion is that the hidden ecosystem files are picked
        // up despite being gitignored.
    }

    /// `--lang` filtering must compose with gitignore filtering: ignored
    /// files of the requested language stay out, kept files of other
    /// languages also stay out.
    #[test]
    fn test_discover_files_lang_filter_composes_with_gitignore() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        fs::write(root.join(".gitignore"), "requirements.txt\n").unwrap();
        fs::write(root.join("requirements.txt"), "flask").unwrap();
        fs::write(root.join("pyproject.toml"), "[project]\nname='x'").unwrap();
        fs::write(root.join("package.json"), "{}").unwrap();

        let files = discover_files(&[root.to_path_buf()], &[Lang::Python]);
        let paths: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();

        // gitignored Python file is dropped, non-Python file is dropped by --lang
        assert_eq!(paths, vec![root.join("pyproject.toml")]);
    }

    /// Nested `.gitignore` files (in a subdirectory) must be honored - the
    /// `ignore` crate handles this; this test pins the behavior so a future
    /// refactor can't regress it.
    #[test]
    fn test_discover_files_respects_nested_gitignore() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        // Top-level allows everything.
        fs::write(root.join(".gitignore"), "").unwrap();
        // Nested .gitignore drops a single file in its directory.
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join(".gitignore"), "secret.toml\n").unwrap();
        fs::write(sub.join("secret.toml"), "").unwrap();
        fs::write(sub.join("Cargo.toml"), "[package]\nname='x'").unwrap();
        fs::write(sub.join("package.json"), "{}").unwrap();

        let files = discover_files(&[root.to_path_buf()], &[]);
        let paths: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();

        assert!(paths.contains(&sub.join("Cargo.toml")));
        assert!(paths.contains(&sub.join("package.json")));
        assert!(!paths.contains(&sub.join("secret.toml")));
    }

    /// `exclude` path globs drop matching files from a directory walk while
    /// leaving non-matching files untouched.
    #[test]
    fn test_discover_files_exclude_drops_matching_paths() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        fs::write(root.join("requirements.txt"), "flask").unwrap();
        let archive = root.join("archive");
        fs::create_dir_all(&archive).unwrap();
        fs::write(archive.join("requirements.txt"), "flask").unwrap();

        let patterns = vec!["**/archive/**".to_string()];
        let files = discover_files_with(
            &[root.to_path_buf()],
            &[],
            DiscoverOptions {
                no_ignore: false,
                verbose: false,
                include: &[],
                exclude: &patterns,
            },
        );
        let paths: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();

        assert!(
            paths.contains(&root.join("requirements.txt")),
            "non-excluded file must remain; got: {paths:?}"
        );
        assert!(
            !paths.contains(&archive.join("requirements.txt")),
            "file under archive/ must be excluded; got: {paths:?}"
        );
    }

    #[test]
    fn test_discover_files_include_adds_unknown_file_as_annotated() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let vars = root.join("ansible/roles/shinyhub/vars");
        fs::create_dir_all(&vars).unwrap();
        let main_yml = vars.join("main.yml");
        fs::write(
            &main_yml,
            "shinyhub_version: \"0.11.16\"  # upd: pypi shinyhub\n",
        )
        .unwrap();

        let include = vec!["ansible/roles/*/vars/*.yml".to_string()];
        let files = discover_files_with(
            &[root.to_path_buf()],
            &[],
            DiscoverOptions {
                no_ignore: false,
                verbose: false,
                include: &include,
                exclude: &[],
            },
        );

        assert_eq!(files, vec![(main_yml, FileType::Annotated)]);
    }

    #[test]
    fn test_discover_files_include_matches_exact_and_hidden_file_names() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let compose = root.join("docker-compose.yml");
        let gitlab = root.join(".gitlab-ci.yml");
        fs::write(&compose, "version: 1.0.0  # upd: pypi example\n").unwrap();
        fs::write(&gitlab, "version: 1.0.0  # upd: pypi example\n").unwrap();

        let include = vec![
            "docker-compose.yml".to_string(),
            ".gitlab-ci.yml".to_string(),
        ];
        let files = discover_files_with(
            &[root.to_path_buf()],
            &[],
            DiscoverOptions {
                no_ignore: false,
                verbose: false,
                include: &include,
                exclude: &[],
            },
        );
        let discovered: std::collections::HashMap<_, _> = files.into_iter().collect();

        assert_eq!(discovered.get(&compose), Some(&FileType::Annotated));
        assert_eq!(discovered.get(&gitlab), Some(&FileType::Annotated));
    }

    #[test]
    fn test_discover_files_exclude_wins_over_include() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let vars = root.join("ansible/roles/shinyhub/vars");
        fs::create_dir_all(&vars).unwrap();
        let main_yml = vars.join("main.yml");
        fs::write(&main_yml, "version: 1.0.0  # upd: pypi shinyhub\n").unwrap();

        let include = vec!["ansible/roles/*/vars/*.yml".to_string()];
        let exclude = vec!["**/shinyhub/**".to_string()];
        let files = discover_files_with(
            &[root.to_path_buf()],
            &[],
            DiscoverOptions {
                no_ignore: false,
                verbose: false,
                include: &include,
                exclude: &exclude,
            },
        );

        assert!(files.is_empty(), "exclude must veto include: {files:?}");
    }

    #[test]
    fn test_discover_files_include_never_overrides_detected_type() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let terraform = root.join("main.tf");
        fs::write(&terraform, "variable \"version\" { default = \"1.0.0\" }\n").unwrap();

        let include = vec!["*.tf".to_string()];
        let files = discover_files_with(
            &[root.to_path_buf()],
            &[],
            DiscoverOptions {
                no_ignore: false,
                verbose: false,
                include: &include,
                exclude: &[],
            },
        );

        assert_eq!(files, vec![(terraform, FileType::TerraformTf)]);
    }

    /// An explicit file-path argument bypasses `exclude` even when the glob
    /// would match it in a directory walk.
    #[test]
    fn test_discover_files_explicit_path_bypasses_exclude() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        let archive = root.join("archive");
        fs::create_dir_all(&archive).unwrap();
        let archived = archive.join("requirements.txt");
        fs::write(&archived, "flask").unwrap();

        let patterns = vec!["**/archive/**".to_string()];
        let files = discover_files_with(
            std::slice::from_ref(&archived),
            &[],
            DiscoverOptions {
                no_ignore: false,
                verbose: false,
                include: &[],
                exclude: &patterns,
            },
        );
        let paths: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();

        assert_eq!(
            paths,
            vec![archived],
            "explicit file path must bypass exclude; got: {paths:?}"
        );
    }

    /// An invalid exclude pattern is skipped (with a warning) rather than
    /// disabling the whole list; valid patterns still apply.
    #[test]
    fn test_discover_files_invalid_exclude_pattern_is_skipped() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        fs::write(root.join("requirements.txt"), "flask").unwrap();
        let archive = root.join("archive");
        fs::create_dir_all(&archive).unwrap();
        fs::write(archive.join("requirements.txt"), "flask").unwrap();

        // First pattern is an invalid glob (unclosed class); second is valid.
        let patterns = vec!["**/[".to_string(), "**/archive/**".to_string()];
        let files = discover_files_with(
            &[root.to_path_buf()],
            &[],
            DiscoverOptions {
                no_ignore: false,
                verbose: false,
                include: &[],
                exclude: &patterns,
            },
        );
        let paths: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();

        assert!(paths.contains(&root.join("requirements.txt")));
        assert!(
            !paths.contains(&archive.join("requirements.txt")),
            "valid pattern must still apply when another pattern is invalid; got: {paths:?}"
        );
    }

    /// A whole-directory ignore (`.github/`) must prune the entire subtree,
    /// not just a single workflow file.
    #[test]
    fn test_discover_files_directory_ignore_prunes_subtree() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        fs::write(root.join(".gitignore"), ".github/\n").unwrap();
        let workflows = root.join(".github").join("workflows");
        fs::create_dir_all(&workflows).unwrap();
        fs::write(workflows.join("ci.yml"), "name: CI").unwrap();
        fs::write(workflows.join("release.yaml"), "name: Release").unwrap();
        fs::write(root.join("package.json"), "{}").unwrap();

        let files = discover_files(&[root.to_path_buf()], &[]);
        let paths: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();

        assert_eq!(paths, vec![root.join("package.json")]);
    }

    /// `.git` directories must never be walked into - they are not in
    /// `.gitignore` (git itself manages them) but still contain YAML/TOML
    /// content that would otherwise be picked up.
    #[test]
    fn test_discover_files_skips_dot_git_directory() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        let inner_git = root.join(".git").join("workflows-cache");
        fs::create_dir_all(&inner_git).unwrap();
        // A file matching one of our patterns, planted inside .git/.
        fs::write(inner_git.join(".mise.toml"), "[tools]").unwrap();
        fs::write(root.join(".git").join("config"), "[core]").unwrap();

        let files = discover_files(&[root.to_path_buf()], &[]);
        for (path, _) in &files {
            assert!(
                !path.to_string_lossy().contains("/.git/"),
                "discover_files must not descend into .git/, found {path:?}"
            );
        }
    }

    /// Every `FileType`. The wildcard-free match below is a compile error when a
    /// variant is added, which is the reminder to extend this list too.
    const ALL_FILE_TYPES: &[FileType] = &[
        FileType::Requirements,
        FileType::PyProject,
        FileType::PackageJson,
        FileType::CargoToml,
        FileType::GoMod,
        FileType::Gemfile,
        FileType::Csproj,
        FileType::GithubActions,
        FileType::PreCommitConfig,
        FileType::MiseToml,
        FileType::ToolVersions,
        FileType::TerraformTf,
        FileType::Annotated,
    ];

    #[test]
    fn all_file_types_lists_every_variant() {
        for file_type in ALL_FILE_TYPES {
            match file_type {
                FileType::Requirements
                | FileType::PyProject
                | FileType::PackageJson
                | FileType::CargoToml
                | FileType::GoMod
                | FileType::Gemfile
                | FileType::Csproj
                | FileType::GithubActions
                | FileType::PreCommitConfig
                | FileType::MiseToml
                | FileType::ToolVersions
                | FileType::TerraformTf
                | FileType::Annotated => {}
            }
        }
    }

    #[test]
    fn only_the_annotated_file_type_lacks_an_ecosystem_key() {
        for file_type in ALL_FILE_TYPES {
            assert_eq!(
                ecosystem_key(*file_type).is_some(),
                *file_type != FileType::Annotated,
                "{file_type:?}: every file type but Annotated names one registry"
            );
        }
    }

    #[test]
    fn merge_carries_entry_ecosystems() {
        use crate::annotation::AnnotationSource;
        let mut a = UpdateResult::default();
        a.entry_ecosystem
            .insert("ruff".to_string(), AnnotationSource::PyPi);
        let mut b = UpdateResult::default();
        b.entry_ecosystem
            .insert("serde".to_string(), AnnotationSource::Crates);

        a.merge(b);

        assert_eq!(a.entry_ecosystem.len(), 2);
        assert_eq!(a.entry_ecosystem["serde"], AnnotationSource::Crates);
    }
}

#[cfg(test)]
mod cooldown_integration_tests {
    use super::*;
    use crate::cooldown::CooldownPolicy;
    use crate::registry::MockRegistry;
    use crate::updater::{PackageJsonUpdater, PyProjectUpdater, RequirementsUpdater};
    use chrono::{Duration, TimeZone};
    use std::collections::HashMap;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_update_held_back_by_cooldown() {
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==2.28.0").unwrap();
        file.flush().unwrap();

        let registry = MockRegistry::new("pypi")
            .with_version("requests", "2.31.0")
            .with_version_meta(
                "requests",
                "2.31.0",
                Some(now - Duration::days(2)),
                false,
                false,
            )
            .with_version_meta(
                "requests",
                "2.30.0",
                Some(now - Duration::days(30)),
                false,
                false,
            );

        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: HashMap::new(),
            force_override: None,
        };

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(true, false).with_cooldown_policy(policy, now);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.held_back.len(), 1, "requests should be held back");
        let (name, old, new, skipped, _) = &result.held_back[0];
        assert_eq!(name, "requests");
        assert_eq!(old, "2.28.0");
        assert_eq!(new, "2.30.0");
        assert_eq!(skipped, "2.31.0");
    }

    #[tokio::test]
    async fn test_update_skipped_when_nothing_old_enough() {
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==2.28.0").unwrap();
        file.flush().unwrap();

        let registry = MockRegistry::new("pypi")
            .with_version("requests", "2.31.0")
            .with_version_meta(
                "requests",
                "2.31.0",
                Some(now - Duration::days(1)),
                false,
                false,
            );

        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: HashMap::new(),
            force_override: None,
        };

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(true, false).with_cooldown_policy(policy, now);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.skipped_by_cooldown.len(), 1);
        assert!(result.updated.is_empty());
        assert!(result.held_back.is_empty());
    }

    /// A skip anchored to a version the registry gave no publish date for must
    /// report the date as unknown. Substituting "now" dates it to the instant of
    /// the run, which renders as "released 0s ago" - both false and maximally
    /// fresh, so it is exactly consistent with being held by cooldown and
    /// nothing downstream can flag it.
    #[tokio::test]
    async fn test_skip_without_publish_date_reports_it_as_unknown() {
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==2.28.0").unwrap();
        file.flush().unwrap();

        // The one newer release is yanked, so it is filtered out of the
        // candidate list and the decision falls back to the raw anchor, which
        // is the path that can carry a missing publish date.
        let registry = MockRegistry::new("pypi")
            .with_version("requests", "2.31.0")
            .with_version_meta("requests", "2.31.0", None, true, false);

        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: HashMap::new(),
            force_override: None,
        };

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(true, false).with_cooldown_policy(policy, now);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(
            result.skipped_by_cooldown.len(),
            1,
            "the yanked-only case must still record a skip, got {:?}",
            result.skipped_by_cooldown
        );
        let (_, _, _, published_at) = &result.skipped_by_cooldown[0];
        assert!(
            published_at.is_none(),
            "a missing publish date must stay missing, not become the current time; got {published_at:?}"
        );
    }

    #[tokio::test]
    async fn test_pyproject_held_back_by_cooldown() {
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        writeln!(
            file,
            "[project]\nname = \"demo\"\ndependencies = [\"requests==2.28.0\"]"
        )
        .unwrap();
        file.flush().unwrap();

        let registry = MockRegistry::new("pypi")
            .with_version("requests", "2.31.0")
            .with_version_meta(
                "requests",
                "2.31.0",
                Some(now - Duration::days(2)),
                false,
                false,
            )
            .with_version_meta(
                "requests",
                "2.30.0",
                Some(now - Duration::days(30)),
                false,
                false,
            );

        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: HashMap::new(),
            force_override: None,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(true, false).with_cooldown_policy(policy, now);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.held_back.len(), 1, "requests should be held back");
        let (name, old, new, skipped, _) = &result.held_back[0];
        assert_eq!(name, "requests");
        assert_eq!(old, "2.28.0");
        assert_eq!(new, "2.30.0");
        assert_eq!(skipped, "2.31.0");
    }

    #[tokio::test]
    async fn test_poetry_held_back_respects_constraint() {
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        writeln!(
            file,
            "[tool.poetry]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[tool.poetry.dependencies]\npython = \"^3.10\"\nrequests = \"^1.0\"\n"
        )
        .unwrap();
        file.flush().unwrap();

        // Latest overall is 2.0.0 (outside the ^1.0 constraint). Cooldown must
        // skip it *and* ignore it when picking a held-back version, so the
        // chosen fallback is 1.5.0, which satisfies the Poetry specifier.
        let registry = MockRegistry::new("pypi")
            .with_version("requests", "2.0.0")
            .with_version_meta(
                "requests",
                "2.0.0",
                Some(now - Duration::days(30)),
                false,
                false,
            )
            .with_version_meta(
                "requests",
                "1.5.0",
                Some(now - Duration::days(30)),
                false,
                false,
            )
            .with_version_meta(
                "requests",
                "1.0.0",
                Some(now - Duration::days(365)),
                false,
                false,
            );

        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: HashMap::new(),
            force_override: None,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(true, false).with_cooldown_policy(policy, now);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(
            result.updated.len(),
            1,
            "requests should update within its ^1.0 constraint"
        );
        let (name, old, new, _) = &result.updated[0];
        assert_eq!(name, "requests");
        assert_eq!(old, "1.0");
        assert_eq!(
            new, "1.5",
            "constraint must prevent Poetry from selecting 2.0.0"
        );
    }

    #[tokio::test]
    async fn test_package_json_held_back_by_cooldown() {
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        writeln!(
            file,
            r#"{{"name":"demo","version":"0.0.0","dependencies":{{"lodash":"4.17.20"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("lodash", "4.17.22")
            .with_version_meta(
                "lodash",
                "4.17.22",
                Some(now - Duration::days(2)),
                false,
                false,
            )
            .with_version_meta(
                "lodash",
                "4.17.21",
                Some(now - Duration::days(30)),
                false,
                false,
            );

        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: HashMap::new(),
            force_override: None,
        };

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(true, false).with_cooldown_policy(policy, now);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.held_back.len(), 1, "lodash should be held back");
        let (name, old, new, skipped, _) = &result.held_back[0];
        assert_eq!(name, "lodash");
        assert_eq!(old, "4.17.20");
        assert_eq!(new, "4.17.21");
        assert_eq!(skipped, "4.17.22");
    }

    #[tokio::test]
    async fn test_package_json_current_latest_is_not_a_cooldown_skip() {
        let now = Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).unwrap();

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        writeln!(
            file,
            r#"{{"name":"demo","version":"0.0.0","dependencies":{{"astro":"7.2.2"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("astro", "7.2.2")
            .with_version_meta(
                "astro",
                "0.0.0-data-astro-transition-20240111220209",
                Some(now - Duration::days(900)),
                false,
                true,
            )
            .with_version_meta(
                "astro",
                "7.2.2",
                Some(now - Duration::days(30)),
                false,
                false,
            );

        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: HashMap::new(),
            force_override: None,
        };

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(true, false).with_cooldown_policy(policy, now);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(
            result.updated.is_empty(),
            "an up-to-date package must stay unchanged"
        );
        assert!(result.held_back.is_empty(), "no update exists to hold back");
        assert!(
            result.skipped_by_cooldown.is_empty(),
            "unrelated prerelease metadata must not become a cooldown skip"
        );
        assert_eq!(result.unchanged, 1);
    }

    /// Run one update under an active cooldown policy and return the notes it
    /// stashed, so a test can assert on what the run told the user.
    async fn cooldown_notes_for(registry: &MockRegistry, now: DateTime<Utc>) -> Vec<String> {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==2.28.0").unwrap();
        file.flush().unwrap();

        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: HashMap::new(),
            force_override: None,
        };
        let options = UpdateOptions::new(true, false).with_cooldown_policy(policy, now);
        let notes = std::sync::Arc::clone(&options.cooldown_unavailable_notes);

        RequirementsUpdater::new()
            .update(file.path(), registry, options)
            .await
            .unwrap();

        let notes = notes.lock().unwrap();
        notes.values().cloned().collect()
    }

    /// The note describes an ecosystem-wide condition, not a package. An
    /// outage that every package in the file runs into is one fact, and
    /// repeating it per dependency buries the rest of the output.
    #[tokio::test]
    async fn one_outage_is_reported_once_however_many_packages_hit_it() {
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==2.28.0").unwrap();
        writeln!(file, "urllib3==1.26.0").unwrap();
        file.flush().unwrap();

        let registry = MockRegistry::new("pypi")
            .with_version("requests", "2.31.0")
            .with_version("urllib3", "2.2.0")
            .with_unavailable_versions("requests")
            .with_unavailable_versions("urllib3");

        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: HashMap::new(),
            force_override: None,
        };
        let options = UpdateOptions::new(true, false).with_cooldown_policy(policy, now);
        let notes = std::sync::Arc::clone(&options.cooldown_unavailable_notes);

        RequirementsUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        let notes: Vec<String> = notes.lock().unwrap().values().cloned().collect();
        assert_eq!(
            notes.len(),
            1,
            "one outage on one ecosystem is one note, got: {notes:?}"
        );
    }

    /// A registry that answers the publish-date question with "I hold no dates"
    /// is permanently unable to support cooldown, and saying so is correct.
    #[tokio::test]
    async fn a_registry_without_publish_dates_reports_cooldown_as_unavailable() {
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();
        let registry = MockRegistry::new("nuget").with_version("requests", "2.31.0");

        let notes = cooldown_notes_for(&registry, now).await;

        assert_eq!(
            notes,
            vec!["cooldown unavailable for nuget".to_string()],
            "a registry holding no publish dates cannot support cooldown"
        );
    }

    /// A lookup that failed never answered the question, so reporting it with
    /// the wording above tells the user their registry does not support
    /// cooldown when in fact a retry would work. The two must read differently
    /// and the failure must name its cause.
    #[tokio::test]
    async fn a_failed_publish_date_lookup_is_not_reported_as_an_unsupported_registry() {
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();
        let registry = MockRegistry::new("pypi")
            .with_version("requests", "2.31.0")
            .with_unavailable_versions("requests");

        let notes = cooldown_notes_for(&registry, now).await;

        assert_eq!(notes.len(), 1, "one failure, one note, got: {notes:?}");
        let note = &notes[0];
        assert_ne!(
            note, "cooldown unavailable for pypi",
            "a failed lookup must not read as a registry that cannot support cooldown"
        );
        assert!(
            note.contains("pypi") && note.contains("Version listing failed"),
            "the note must name the ecosystem and why the lookup failed, got: {note}"
        );
    }

    /// Packages resolve concurrently, so which of them reaches a shared outage
    /// first is not fixed. Reporting whichever arrived first would make the same
    /// repository state print a different cause from run to run, so the
    /// representative is chosen by ordering instead of by arrival.
    #[test]
    fn the_reported_cause_does_not_depend_on_which_package_hit_it_first() {
        let options = UpdateOptions::new(true, false);

        options.note_cooldown_unavailable(&CooldownNote {
            key: "pypi:lookup-failed".to_string(),
            message: "zzz arrived first".to_string(),
        });
        options.note_cooldown_unavailable(&CooldownNote {
            key: "pypi:lookup-failed".to_string(),
            message: "aaa arrived second".to_string(),
        });

        let notes: Vec<String> = options
            .cooldown_unavailable_notes
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        assert_eq!(
            notes,
            vec!["aaa arrived second".to_string()],
            "the condition is still one note, and arrival order must not pick it"
        );
    }
}
