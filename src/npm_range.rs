//! npm range syntax: classification, translation and rewriting.
//!
//! npm's range grammar is not the `semver` crate's grammar. A space means AND
//! where the crate wants a comma, `||` means OR which the crate has no spelling
//! for at all, and `1.2.x`, `1.2` and `1.2.3 - 2.0.0` have no direct equivalent.
//! Handing an npm range straight to [`semver::VersionReq::parse`] therefore
//! fails on most real ranges, and a caller that treats that failure as "nothing
//! to do" reports a dependency as current without ever having looked it up.
//!
//! This module is the single place that understands the grammar. It serves two
//! callers with opposite needs: the registry asks *which published versions does
//! this range admit* ([`parse_npm_range`]), and the updater asks *where in this
//! string is the version I should raise* ([`classify`], [`lower_bound_anchor`]
//! and [`rewrite_lower_bound`]).
//!
//! The desugarings follow node-semver, including the ones that are easy to get
//! subtly wrong: `>1.2` admits 1.3.0 but not 1.2.9, `<=1.2` admits 1.2.9 but not
//! 1.3.0, and a partial bound on the right of a hyphen range widens rather than
//! truncates (`1.2.3 - 2.3` ends below 2.4.0, not at 2.3.0). They were checked
//! against node-semver rather than reasoned about: every range form here was
//! diffed against `semver.satisfies` from node-semver 7.8.5 across a grid of
//! versions, prereleases included.
//!
//! One form is read by where it is written rather than by what it says. A number
//! after a wildcard (`1.x.3`) is a range behind a caret, tilde or hyphen, where
//! it means `1.x`, and no range at all as a bare version or behind a comparator,
//! where npm installs nothing from it. That split is node-semver's: the order
//! check lives in the one expansion the comparator forms go through. It is
//! reproduced here rather than resolved because the question upd answers is what
//! the npm running alongside it will do, and `npm install` fails with ETARGET on
//! `"1.x.3"` while resolving `"^1.x.3"` to the newest 1.x.

/// What kind of npm spec a dependency string is, from the updater's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecShape {
    /// An exact version pin like `"1.2.3"`.
    ExactPin,
    /// A caret (`^`) or tilde (`~`) range written against a complete version,
    /// whose ceiling is implied by its floor.
    CaretOrTilde,
    /// A range carrying a ceiling the author chose independently of the floor:
    /// `">=1.0.0 <2.0.0"`, `"1.2.3 - 2.0.0"`, `">=1.0.0"`. Exactly one bound is
    /// an inclusive lower bound, so the replacement version has a single
    /// unambiguous home, and it has to be picked from inside the range so the
    /// ceiling survives.
    BoundedRange,
    /// A version written with components left open, whose ceiling follows from
    /// its floor: `"1.2.x"`, `"1.x"`, `"1.2"`, `"1"`, and the caret and tilde
    /// forms of the same (`"^1.2"`, `"~1"`). It floats like a caret, so it
    /// tracks the newest release and the whole shape moves with it: `"4.3.x"`
    /// becomes `"4.4.x"` and `"^1.2"` becomes `"^4.4"`, never `"4.4.3"`.
    ShapeRange,
    /// A range `upd` can evaluate but will not rewrite: alternation
    /// (`"^1 || ^2"`, where no branch is the obvious one to edit), upper-only
    /// bounds (`"<3"`, which has no floor to raise), and exclusive lower bounds
    /// (`">1.2.3"`, whose version is one the author refuses rather than one
    /// they are on - see [`rewrite_lower_bound`]).
    OpaqueRange,
    /// Not a registry version spec at all: `"*"`, `"latest"`, `"workspace:*"`,
    /// `"github:owner/repo"`, `"file:../local"`. There is no published version
    /// to compare against, so there is nothing to report either.
    NoVersion,
    /// Shaped like a version range, but outside the grammar this module knows.
    /// The dependency cannot be checked, which is not the same as it being
    /// current, and callers must not conflate the two.
    Unsupported,
}

/// Classify an npm version spec.
pub fn classify(spec: &str) -> SpecShape {
    let trimmed = spec.trim();
    if is_non_version(trimmed) {
        return SpecShape::NoVersion;
    }

    let branches: Vec<&str> = trimmed.split("||").collect();
    // Every branch has to translate, or the range as a whole cannot be
    // evaluated: a version admitted by an untranslatable branch would be
    // reported as outside the range.
    let analyzed: Option<Vec<Branch<'_>>> = branches.iter().map(|b| analyze(b)).collect();
    let Some(analyzed) = analyzed else {
        return SpecShape::Unsupported;
    };
    if analyzed.iter().any(|b| b.to_req().is_none()) {
        return SpecShape::Unsupported;
    }
    if analyzed.len() > 1 {
        return SpecShape::OpaqueRange;
    }

    match analyzed[0].shape {
        BranchShape::Exact => SpecShape::ExactPin,
        BranchShape::CaretOrTilde => SpecShape::CaretOrTilde,
        BranchShape::Shape => SpecShape::ShapeRange,
        BranchShape::Hyphen | BranchShape::Bounded { .. } => SpecShape::BoundedRange,
        BranchShape::Opaque => SpecShape::OpaqueRange,
    }
}

/// Translate an npm range into the set of [`semver::VersionReq`] alternatives it
/// stands for. A version satisfies the range when it satisfies **any** of them.
///
/// Returns `None` for a spec this module cannot translate, which callers must
/// report rather than treat as an empty match: "no published version satisfies
/// this range" and "upd could not evaluate this range" are different facts.
pub fn parse_npm_range(spec: &str) -> Option<Vec<semver::VersionReq>> {
    let trimmed = spec.trim();
    if is_non_version(trimmed) {
        return None;
    }
    trimmed
        .split("||")
        .map(|branch| analyze(branch).and_then(|b| b.to_req()))
        .collect()
}

/// Whether `version` satisfies `spec`, or `None` if the spec cannot be evaluated.
pub fn admits(spec: &str, version: &str) -> Option<bool> {
    let reqs = parse_npm_range(spec)?;
    let parsed = semver::Version::parse(version).ok()?;
    Some(reqs.iter().any(|req| req.matches(&parsed)))
}

/// The floor of `spec` as a complete `major.minor.patch` version, or `None` when
/// the spec has no single floor to speak of.
///
/// Callers use it as the "current version" a bump is measured from, so it is
/// completed (`">=1.2"` yields `"1.2.0"`) rather than returned as written: a
/// partial version is not a `semver::Version` and every consumer would have to
/// re-complete it.
pub fn lower_bound_anchor(spec: &str) -> Option<String> {
    let trimmed = spec.trim();
    if is_non_version(trimmed) || trimmed.contains("||") {
        return None;
    }
    let branch = analyze(trimmed)?;
    match branch.shape {
        BranchShape::Hyphen => Some(parse_partial(branch.tokens[0])?.floor()),
        BranchShape::Shape => Some(parse_comparator(branch.tokens[0])?.1.floor()),
        BranchShape::Bounded { lower } => Some(parse_comparator(branch.tokens[lower])?.1.floor()),
        BranchShape::Exact | BranchShape::CaretOrTilde | BranchShape::Opaque => None,
    }
}

/// Rewrite `spec` so its floor becomes `new_version`, preserving the shape the
/// author wrote: a ceiling stays where it is, a hyphen stays a hyphen, and a
/// wildcard range keeps both its wildcard character and its component count
/// (`"4.3.x"` with `"4.4.3"` becomes `"4.4.x"`, not `"4.4.3"`).
///
/// Returns `None` when the spec has no floor to raise ([`SpecShape::OpaqueRange`],
/// [`SpecShape::NoVersion`]) or cannot be parsed.
pub fn rewrite_lower_bound(spec: &str, new_version: &str) -> Option<String> {
    let trimmed = spec.trim();
    let new_version = new_version.trim();
    if new_version.is_empty() || is_non_version(trimmed) || trimmed.contains("||") {
        return None;
    }
    let branch = analyze(trimmed)?;
    match branch.shape {
        BranchShape::Hyphen => Some(format!("{new_version} - {}", branch.tokens[2])),
        BranchShape::Bounded { lower } => {
            let prefix = version_prefix(branch.tokens[lower]);
            let rewritten: Vec<String> = branch
                .tokens
                .iter()
                .enumerate()
                .map(|(index, token)| {
                    if index == lower {
                        format!("{prefix}{new_version}")
                    } else {
                        (*token).to_string()
                    }
                })
                .collect();
            Some(rewritten.join(" "))
        }
        BranchShape::Shape => parse_comparator(branch.tokens[0])?
            .1
            .reshape(new_version, version_prefix(branch.tokens[0])),
        BranchShape::Exact | BranchShape::CaretOrTilde | BranchShape::Opaque => None,
    }
}

/// Specs that name no published version, so there is nothing to look up and
/// nothing to report. `"*"` and a dist-tag float by design; the rest are
/// resolution protocols (`workspace:`, `npm:`, `file:`, `link:`, `git+ssh:`,
/// `github:`) or the `owner/repo` shorthand, all of which resolve somewhere
/// other than the registry.
fn is_non_version(spec: &str) -> bool {
    if spec.is_empty() || spec.contains(':') || spec.contains('/') {
        return true;
    }
    let lower = spec.to_ascii_lowercase();
    lower == "*" || lower == "x" || is_dist_tag(spec)
}

/// Whether the spec names a dist-tag rather than versions of its own. `latest`
/// is the tag every package publishes, and npm resolves `next`, `beta` or
/// `canary` in the same position just as readily. The registry decides which
/// version a tag stands for, and it decides again tomorrow, so there is no
/// bound here to compare or to raise.
///
/// npm's own rule is "whatever is not a valid range", which reads a mistyped
/// version as a tag too. A tag has to look like a name here as well: it opens
/// with a letter and carries only what npm allows in one, and it is a tag only
/// where no version can be read out of it, since `"v1.2.3"` is a version
/// spelled with a prefix. A lone `"v"` is that prefix with its digits missing
/// rather than a name, and stays unreadable so a truncated version cannot pass
/// as a tag nothing looks at.
///
/// Read as one comparator, which is the position a whole spec occupies when it
/// is a single token: `"x.2.3"` is a range nowhere, so npm sends it to the
/// registry as a name and so does this.
fn is_dist_tag(spec: &str) -> bool {
    !spec.eq_ignore_ascii_case("v")
        && spec.starts_with(|c: char| c.is_ascii_alphabetic())
        && spec
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && parse_comparator(spec).is_none()
}

/// The comparator an npm token opens with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
    /// An explicit `=` or no operator at all; npm treats them identically.
    Eq,
    Caret,
    Tilde,
}

impl Op {
    /// The characters the operator was written with, for rebuilding a token.
    fn sigil(self) -> &'static str {
        match self {
            Op::Ge => ">=",
            Op::Gt => ">",
            Op::Le => "<=",
            Op::Lt => "<",
            Op::Eq => "",
            Op::Caret => "^",
            Op::Tilde => "~",
        }
    }

    /// Whether the operator puts a floor under the versions it admits. Every
    /// such token competes to be the one a rewrite should move, so a branch
    /// with more than one of them has no unambiguous floor.
    fn bounds_below(self) -> bool {
        matches!(self, Op::Ge | Op::Gt | Op::Eq | Op::Caret | Op::Tilde)
    }
}

/// The operator a token opens with and the version written behind it. npm
/// allows whitespace between the two (`">= 1.2.7"`), so the version is returned
/// without it - always as a suffix of `token`, which is what lets a rewrite
/// recover the operator exactly as the author spelled it.
///
/// `~>` is npm's second spelling of the tilde and means exactly what `~` does,
/// so it has to be read before `~` or its `>` would be left in front of the
/// version. The two spellings stay distinct on the page: a rewrite puts back
/// the characters the author wrote, not the operator's canonical form.
fn split_operator(token: &str) -> (Op, &str) {
    for (prefix, op) in [
        (">=", Op::Ge),
        ("<=", Op::Le),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("=", Op::Eq),
        ("^", Op::Caret),
        ("~>", Op::Tilde),
        ("~", Op::Tilde),
    ] {
        if let Some(rest) = token.strip_prefix(prefix) {
            return (op, rest.trim_start());
        }
    }
    (Op::Eq, token.trim_start())
}

/// Everything `token` writes before its version: the operator and whatever
/// separates it from the version. A rewrite puts this back verbatim rather than
/// rebuilding it from the operator, so `">= 1.0.0"` does not come back as
/// `">=1.0.0"`.
fn version_prefix(token: &str) -> &str {
    let (_, rest) = split_operator(token);
    &token[..token.len() - rest.len()]
}

/// The exclusive ceiling a partial version implies.
enum Ceiling {
    /// No components were written, so nothing is excluded.
    Any,
    /// Everything below this version, e.g. `"1.2"` excludes 1.3.0 and up.
    Below(String),
    /// All three components were written, so the version stands for itself.
    Exact,
}

/// A version as an npm range writes it: any component may be missing or a
/// wildcard, and only a fully written version may carry a prerelease suffix.
struct Partial {
    major: u64,
    minor: u64,
    patch: u64,
    /// How many leading components were written as numbers.
    numeric: usize,
    /// The wildcard character the author used, kept so a rewrite puts the same
    /// one back rather than normalising `X` to `x`.
    wildcard: Option<char>,
    /// Whether a number was written after a wildcard (`"1.x.3"`), which decides
    /// nothing here and everything at [`parse_comparator`].
    numeric_after_wildcard: bool,
    /// Everything from the first `-` or `+`, empty unless fully written.
    suffix: String,
}

impl Partial {
    /// The lowest version the partial admits, completed with zeroes.
    fn floor(&self) -> String {
        format!(
            "{}.{}.{}{}",
            self.major, self.minor, self.patch, self.suffix
        )
    }

    fn ceiling(&self) -> Ceiling {
        match self.numeric {
            0 => Ceiling::Any,
            1 => Ceiling::Below(format!("{}.0.0", self.major.saturating_add(1))),
            2 => Ceiling::Below(format!("{}.{}.0", self.major, self.minor.saturating_add(1))),
            _ => Ceiling::Exact,
        }
    }

    /// The numeric components as written, for handing a caret or tilde back to
    /// the `semver` crate: `"^1.x"` narrows to `"^1"`, which the crate reads
    /// exactly as npm reads the original.
    fn numeric_prefix(&self) -> String {
        match self.numeric {
            0 => String::new(),
            1 => format!("{}", self.major),
            2 => format!("{}.{}", self.major, self.minor),
            _ => format!(
                "{}.{}.{}{}",
                self.major, self.minor, self.patch, self.suffix
            ),
        }
    }

    /// Put `new_version` into the shape this partial was written in, behind
    /// `prefix`: the operator as the author spelled it, separator included.
    fn reshape(&self, new_version: &str, prefix: &str) -> Option<String> {
        let new = parse_partial(new_version)?;
        if new.numeric != 3 {
            return None;
        }
        let numbers = match self.numeric {
            0 => return None,
            1 => format!("{}", new.major),
            2 => format!("{}.{}", new.major, new.minor),
            _ => format!("{}.{}.{}{}", new.major, new.minor, new.patch, new.suffix),
        };
        Some(match self.wildcard {
            Some(wildcard) => format!("{prefix}{numbers}.{wildcard}"),
            None => format!("{prefix}{numbers}"),
        })
    }

    /// The comparators this partial stands for under `op`.
    fn expand(&self, op: Op) -> Vec<String> {
        let floor = self.floor();
        match op {
            Op::Caret | Op::Tilde => match self.numeric {
                0 => vec![">=0.0.0".to_string()],
                _ => vec![format!("{}{}", op.sigil(), self.numeric_prefix())],
            },
            Op::Eq => match self.ceiling() {
                Ceiling::Any => vec![">=0.0.0".to_string()],
                Ceiling::Below(hi) => vec![format!(">={floor}"), format!("<{hi}")],
                Ceiling::Exact => vec![format!("={floor}")],
            },
            // ">=1.2" admits 1.2.0, so the missing components complete to zero.
            Op::Ge => vec![format!(">={floor}")],
            // ">1.2" excludes every 1.2.z, so it starts at the ceiling instead.
            Op::Gt => match self.ceiling() {
                Ceiling::Any => vec![NOTHING.to_string()],
                Ceiling::Below(hi) => vec![format!(">={hi}")],
                Ceiling::Exact => vec![format!(">{floor}")],
            },
            // "<1.2" excludes every 1.2.z, so it stops at the floor.
            Op::Lt => match self.ceiling() {
                Ceiling::Any => vec![NOTHING.to_string()],
                _ => vec![format!("<{floor}")],
            },
            // "<=1.2" admits every 1.2.z, so it stops at the ceiling.
            Op::Le => match self.ceiling() {
                Ceiling::Any => vec![">=0.0.0".to_string()],
                Ceiling::Below(hi) => vec![format!("<{hi}")],
                Ceiling::Exact => vec![format!("<={floor}")],
            },
        }
    }
}

/// A comparator no released version can satisfy, for the ranges npm defines as
/// empty (`">*"`, `"<*"`).
const NOTHING: &str = "<0.0.0";

fn parse_partial(token: &str) -> Option<Partial> {
    let token = token.strip_prefix(['v', 'V']).unwrap_or(token);

    // Build metadata says nothing about which versions a range admits, and npm
    // allows it after any number of components. Dropping it first also keeps
    // the `-` inside it ("1.2.3+build-1") from reading as a prerelease.
    let token = match token.find('+') {
        Some(idx) => &token[..idx],
        None => token,
    };
    let (core, suffix) = match token.find('-') {
        Some(idx) => (&token[..idx], token[idx..].to_string()),
        None => (token, String::new()),
    };

    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    // npm's grammar puts the prerelease after the third component, so there has
    // to be a third component for it to follow: "1.2.x-beta" is a range,
    // "1.x-beta" is not.
    if !suffix.is_empty() && parts.len() != 3 {
        return None;
    }

    let mut numbers = [0u64; 3];
    let mut numeric = 0usize;
    let mut wildcard = None;
    let mut numeric_after_wildcard = false;
    for (index, part) in parts.iter().enumerate() {
        match *part {
            // The first wildcard written is the one a rewrite puts back, so a
            // later one ("1.x.x") does not replace it.
            "x" | "X" | "*" => {
                if wildcard.is_none() {
                    wildcard = part.chars().next();
                }
            }
            // A component with nothing in it. This is also where a token that
            // names no version at all lands, since a bare operator (">=") and a
            // bare "v" both leave an empty string that splits to one empty
            // component: npm rejects all three and so does this.
            "" => return None,
            digits => {
                let number = digits.parse::<u64>().ok()?;
                // A number after a wildcard says something about a position the
                // wildcard already opened, so it names no bound of its own and
                // the components stop here. Whether writing one at all is
                // allowed is the caller's question, not this one's.
                if wildcard.is_some() {
                    numeric_after_wildcard = true;
                    continue;
                }
                numbers[index] = number;
                numeric += 1;
            }
        }
    }

    // A prerelease qualifies one exact version, so it is only meaningful once
    // every component is written.
    let suffix = if numeric == 3 { suffix } else { String::new() };

    Some(Partial {
        major: numbers[0],
        minor: numbers[1],
        patch: numbers[2],
        numeric,
        wildcard,
        numeric_after_wildcard,
        suffix,
    })
}

/// Read one comparator token: the operator it opens with and the version behind
/// it, or `None` where npm reads no range at all.
///
/// This is where npm's rule against a number written after a wildcard applies,
/// and it applies only here. node-semver checks the order while expanding an
/// x-range, which is the path every comparator takes and the one a bare version
/// takes too, so `"1.x.3"` and `">=1.x.3"` name no range. A caret or tilde is
/// expanded by a different rule that never reaches the check, and so is either
/// bound of a hyphen range, so `"^1.x.3"`, `"~1.x.3"` and `"1.x.3 - 2.0.0"` are
/// all ranges npm installs from, each reading the number as the wildcard already
/// covering its position. Checked against node-semver 7.8.5 and against `npm
/// install` itself, which fails with ETARGET on the first pair and resolves the
/// second.
fn parse_comparator(token: &str) -> Option<(Op, Partial)> {
    let (op, rest) = split_operator(token);
    let partial = parse_partial(rest)?;
    if partial.numeric_after_wildcard && !matches!(op, Op::Caret | Op::Tilde) {
        return None;
    }
    Some((op, partial))
}

/// How a single alternative of a range is put together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchShape {
    /// A bare fully written version: `"1.2.3"`.
    Exact,
    /// A lone caret or tilde: `"^1.2.3"`.
    CaretOrTilde,
    /// A bare partial version: `"1.2.x"`, `"1.2"`, `"1"`.
    Shape,
    /// A hyphen range `"A - B"`.
    Hyphen,
    /// Comparators with exactly one lower bound, at `tokens[lower]`.
    Bounded { lower: usize },
    /// Translatable, but with no single floor a rewrite could move.
    Opaque,
}

struct Branch<'a> {
    tokens: Vec<&'a str>,
    shape: BranchShape,
}

impl Branch<'_> {
    fn to_req(&self) -> Option<semver::VersionReq> {
        let mut comparators: Vec<String> = Vec::new();
        match self.shape {
            BranchShape::Hyphen => {
                let low = parse_partial(self.tokens[0])?;
                let high = parse_partial(self.tokens[2])?;
                comparators.push(format!(">={}", low.floor()));
                // A partial on the right widens to the end of what it names:
                // "1.2.3 - 2.3" reaches every 2.3.z, so it stops below 2.4.0.
                comparators.push(match high.ceiling() {
                    Ceiling::Any => ">=0.0.0".to_string(),
                    Ceiling::Below(hi) => format!("<{hi}"),
                    Ceiling::Exact => format!("<={}", high.floor()),
                });
            }
            _ => {
                for token in &self.tokens {
                    let (op, partial) = parse_comparator(token)?;
                    comparators.extend(partial.expand(op));
                }
                // An alternative with nothing in it constrains nothing. Spelled
                // out rather than left to an empty parse so the prerelease rule
                // is the same one every other comparator here gets.
                if comparators.is_empty() {
                    comparators.push(">=0.0.0".to_string());
                }
            }
        }
        semver::VersionReq::parse(&comparators.join(", ")).ok()
    }
}

/// Split one alternative into comparator tokens, keeping a comparator written
/// apart from its version (`">= 1.2.7"`) in one piece. npm allows that spacing
/// and means the same range by it, while every reader below wants the operator
/// and the version it applies to together. The two are contiguous in the
/// source, so the joined token is a slice of it and carries the author's
/// spacing with it.
///
/// A comparator with nothing after it stays on its own and is left to fail as
/// the version-less token it is, which is what npm makes of it too.
fn tokenize(branch: &str) -> Vec<&str> {
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (index, ch) in branch.char_indices() {
        if ch.is_whitespace() {
            if let Some(from) = start.take() {
                words.push((from, index));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(from) = start {
        words.push((from, branch.len()));
    }

    let mut tokens = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        let (from, to) = words[index];
        let joins_the_next = split_operator(&branch[from..to]).1.is_empty();
        if joins_the_next && index + 1 < words.len() {
            tokens.push(&branch[from..words[index + 1].1]);
            index += 2;
        } else {
            tokens.push(&branch[from..to]);
            index += 1;
        }
    }
    tokens
}

fn analyze(branch: &str) -> Option<Branch<'_>> {
    let tokens: Vec<&str> = tokenize(branch);
    // An empty alternative admits everything, which is what npm makes of a
    // trailing `"||"`. It has no floor, so nothing can be raised in it, and it
    // drags the whole range up with it: `"^1 || "` accepts every published
    // version, exactly as npm would install it.
    if tokens.is_empty() {
        return Some(Branch {
            tokens,
            shape: BranchShape::Opaque,
        });
    }

    // A hyphen range is the one form where a token is not a comparator, so it
    // has to be recognised before anything tries to read "-" as one.
    if let Some(position) = tokens.iter().position(|token| *token == "-") {
        if tokens.len() != 3 || position != 1 {
            return None;
        }
        parse_partial(tokens[0])?;
        parse_partial(tokens[2])?;
        return Some(Branch {
            tokens,
            shape: BranchShape::Hyphen,
        });
    }

    let mut lower_bounds: Vec<usize> = Vec::new();
    let mut other_floors = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        let (op, _) = parse_comparator(token)?;
        match op {
            Op::Ge | Op::Gt => lower_bounds.push(index),
            other if other.bounds_below() => other_floors += 1,
            _ => {}
        }
    }

    // A `>` names a version the author refuses, not one they are on, so there
    // is nothing under it to raise: writing the newest release into it makes
    // the range exclude that very release, and a range that ends `">4.18.1"`
    // when 4.18.1 is the newest published version admits nothing at all. Both
    // bounds still count as competing floors above, so `">=1 >2"` stays opaque
    // rather than becoming a range with one home for the new version.
    let raisable = |index: usize| split_operator(tokens[index]).0 == Op::Ge;

    let shape = if tokens.len() == 1 {
        let (op, partial) = parse_comparator(tokens[0])?;
        match op {
            // A caret or tilde over a complete version already has a home for
            // the new version; over a partial one ("^1.2") it does not, and the
            // rewrite has to put the same number of components back or it
            // silently widens the range it was asked to move.
            Op::Caret | Op::Tilde if partial.numeric == 3 => BranchShape::CaretOrTilde,
            Op::Caret | Op::Tilde if partial.numeric > 0 => BranchShape::Shape,
            Op::Eq if partial.numeric == 3 => BranchShape::Exact,
            Op::Eq if partial.numeric > 0 => BranchShape::Shape,
            Op::Ge => BranchShape::Bounded { lower: 0 },
            _ => BranchShape::Opaque,
        }
    } else if lower_bounds.len() == 1 && other_floors == 0 && raisable(lower_bounds[0]) {
        BranchShape::Bounded {
            lower: lower_bounds[0],
        }
    } else {
        BranchShape::Opaque
    };

    Some(Branch { tokens, shape })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admitted(spec: &str, version: &str) -> bool {
        admits(spec, version).unwrap_or_else(|| panic!("{spec} could not be translated"))
    }

    #[test]
    fn classifies_the_shapes_the_updater_routes_on() {
        assert_eq!(classify("1.2.3"), SpecShape::ExactPin);
        assert_eq!(classify("^1.2.3"), SpecShape::CaretOrTilde);
        assert_eq!(classify("~1.2.3"), SpecShape::CaretOrTilde);
        assert_eq!(classify(">=1.0.0 <2.0.0"), SpecShape::BoundedRange);
        assert_eq!(classify("<2.0.0 >=1.0.0"), SpecShape::BoundedRange);
        assert_eq!(classify(">=1.0.0"), SpecShape::BoundedRange);
        assert_eq!(classify("1.2.3 - 2.0.0"), SpecShape::BoundedRange);
        assert_eq!(classify("4.3.x"), SpecShape::ShapeRange);
        assert_eq!(classify("1.x"), SpecShape::ShapeRange);
        assert_eq!(classify("1.2"), SpecShape::ShapeRange);
        assert_eq!(classify("1"), SpecShape::ShapeRange);
        // A caret or tilde over a partial anchor floats the same way, and the
        // completed forms must not be dragged along with them.
        assert_eq!(classify("^1.2"), SpecShape::ShapeRange);
        assert_eq!(classify("^1.x"), SpecShape::ShapeRange);
        assert_eq!(classify("~1"), SpecShape::ShapeRange);
        assert_eq!(classify("^1.2.3-beta.1"), SpecShape::CaretOrTilde);
        assert_eq!(classify("^1.0.0 || ^2.0.0"), SpecShape::OpaqueRange);
        assert_eq!(classify("<3"), SpecShape::OpaqueRange);
        assert_eq!(classify(">1.2.3"), SpecShape::OpaqueRange);
        assert_eq!(classify(">1.2.3 <2.0.0"), SpecShape::OpaqueRange);
        // Two floors, no home for the replacement, whichever kind they are.
        assert_eq!(classify(">=1.0.0 >2.0.0"), SpecShape::OpaqueRange);
    }

    #[test]
    fn a_spec_naming_no_published_version_is_not_a_range() {
        for spec in [
            "*",
            "x",
            "latest",
            "",
            "workspace:*",
            "npm:lodash@^4",
            "file:../local",
            "link:../local",
            "github:chalk/chalk#v5.3.0",
            "git+ssh://git@example.com/o/r.git",
            "https://example.com/x.tgz",
            "owner/repo",
        ] {
            assert_eq!(classify(spec), SpecShape::NoVersion, "spec {spec:?}");
            assert!(parse_npm_range(spec).is_none(), "spec {spec:?}");
        }
    }

    #[test]
    fn a_range_outside_the_grammar_is_unsupported_not_current() {
        // The distinction this asserts is the whole point of the variant: a
        // caller that cannot tell "no match" from "could not evaluate" reports
        // an unchecked dependency as up to date.
        for spec in [
            "1.2.3 - ",
            ">=nope",
            "1.2.3.4",
            ">=1.0.0 - 2.0.0",
            "1..2",
            // A number after a wildcard, in the two positions where npm reads no
            // range from it. The positions where it does are in
            // `a_number_after_a_wildcard_is_a_range_only_where_npm_reads_one`.
            "1.x.3",
            ">=1.x.3",
            // An operator or a "v" with no version behind it.
            ">=",
            "^",
            "v",
            // A prerelease has to follow a third component, and these have two.
            "1.x-beta",
            "1.2-beta",
        ] {
            assert_eq!(classify(spec), SpecShape::Unsupported, "spec {spec:?}");
            assert!(parse_npm_range(spec).is_none(), "spec {spec:?}");
        }
    }

    /// npm resolves a dependency written as a dist-tag at the registry, and
    /// `latest` is only the most common one. Reading `"next"` as a broken range
    /// makes an error out of a manifest npm installs from; reading a mistyped
    /// version as a tag would hide a real one, so a tag has to look like a name
    /// and hold no version.
    #[test]
    fn a_dist_tag_names_no_version_to_compare() {
        for spec in [
            "latest",
            "next",
            "beta",
            "canary",
            "rc",
            "dev",
            "nightly",
            "experimental",
            "next-1",
        ] {
            assert_eq!(classify(spec), SpecShape::NoVersion, "spec {spec:?}");
            assert!(parse_npm_range(spec).is_none(), "spec {spec:?}");
        }

        // The negative control: none of these is a name, so they stay
        // unreadable rather than passing as a tag nothing looks at.
        for spec in ["1.2.3.4", "4,17,21", "^^1.2", ">=nope", "1..2", "v"] {
            assert_eq!(classify(spec), SpecShape::Unsupported, "spec {spec:?}");
        }

        // A version npm lets you spell with a prefix is still a version.
        assert_eq!(classify("v1.2.3"), SpecShape::ExactPin);
        assert!(admitted("v1.2.3", "1.2.3"));
    }

    /// npm reads a trailing `||` as an empty alternative, and an empty
    /// alternative constrains nothing, so the whole range admits everything.
    /// Reporting it as unreadable would be an error on a range npm installs
    /// happily; reporting it as a floor to raise would rewrite a spec that has
    /// no floor.
    #[test]
    fn a_trailing_alternation_bar_admits_everything() {
        assert_eq!(classify("^1 || "), SpecShape::OpaqueRange);
        assert!(admitted("^1 || ", "9.9.9"));
        assert!(admitted("^1 || ", "0.0.1"));
        assert_eq!(lower_bound_anchor("^1 || "), None);
        assert!(rewrite_lower_bound("^1 || ", "9.9.9").is_none());
    }

    /// node-semver lets a comparator stand apart from the version it applies to
    /// (`">= 1.2.7 < 1.3.0"`), which is the same range as the unspaced spelling.
    /// Reading the operator as a token of its own leaves it a version with
    /// nothing in it, and a range npm installs from becomes one upd calls
    /// unreadable - an error, on a manifest that is fine.
    #[test]
    fn a_comparator_may_be_written_apart_from_its_version() {
        assert_eq!(classify(">= 1.0.0 < 2.0.0"), SpecShape::BoundedRange);
        assert!(admitted(">= 1.0.0 < 2.0.0", "1.9.0"));
        assert!(!admitted(">= 1.0.0 < 2.0.0", "2.0.0"));
        assert_eq!(
            lower_bound_anchor(">= 1.0.0 < 2.0.0").as_deref(),
            Some("1.0.0")
        );
        // The spacing is the author's, so the rewrite puts it back.
        assert_eq!(
            rewrite_lower_bound(">= 1.0.0 < 2.0.0", "1.9.0").as_deref(),
            Some(">= 1.9.0 < 2.0.0")
        );
        assert_eq!(classify("^ 1.2"), SpecShape::ShapeRange);
        assert_eq!(
            rewrite_lower_bound("^ 1.2", "4.4.3").as_deref(),
            Some("^ 4.4")
        );
        assert_eq!(classify("= 1.2.3"), SpecShape::ExactPin);
        assert!(admitted("= 1.2.3", "1.2.3"));
        // An operator with nothing after it still names no version.
        assert_eq!(classify(">= 1.0.0 <"), SpecShape::Unsupported);
        assert_eq!(classify(">= >= 1.0.0"), SpecShape::Unsupported);
    }

    /// npm reads `~>` as the tilde, so the range it names caps at the next
    /// minor just as `~` does. Reading the `>` as an operator of its own turns
    /// a capped range into an open floor, and a rewrite that follows that
    /// reading walks the dependency straight through the ceiling its author
    /// wrote.
    #[test]
    fn the_tilde_may_be_written_with_a_trailing_arrow() {
        for spec in ["~>4.17.0", "~> 4.17.0"] {
            assert_eq!(classify(spec), SpecShape::CaretOrTilde, "spec {spec:?}");
            assert!(admitted(spec, "4.17.21"), "spec {spec:?}");
            assert!(!admitted(spec, "4.18.1"), "spec {spec:?}");
            // The arrow is a spelling, not a second operator: the range it
            // stands for is the plain tilde's, to the letter.
            assert_eq!(parse_npm_range(spec), parse_npm_range("~4.17.0"));
        }

        // A partial version behind the arrow floats the same way `~4.17` does,
        // and the rewrite puts the author's spelling back around it.
        assert_eq!(classify("~>4.17"), SpecShape::ShapeRange);
        assert_eq!(
            rewrite_lower_bound("~>4.17", "5.2.0").as_deref(),
            Some("~>5.2")
        );
        assert_eq!(
            rewrite_lower_bound("~> 4.17", "5.2.0").as_deref(),
            Some("~> 5.2")
        );

        // The arrow reaches the tilde's expansion, not the x-range one, so a
        // number written after a wildcard is a range behind it exactly as it is
        // behind a plain tilde.
        assert_eq!(classify("~>1.x.3"), SpecShape::ShapeRange);
        assert_eq!(parse_npm_range("~>1.x.3"), parse_npm_range("~1.x"));

        // The negative control: the plain tilde and the plain `>` keep their
        // own readings, so neither spelling has swallowed the other.
        assert_eq!(classify("~4.17.0"), SpecShape::CaretOrTilde);
        assert!(!admitted("~4.17.0", "4.18.1"));
        assert_eq!(classify(">4.17.0"), SpecShape::OpaqueRange);
        assert!(admitted(">4.17.0", "4.18.1"));
        assert_eq!(rewrite_lower_bound(">4.17.0", "4.18.1"), None);
    }

    #[test]
    fn a_space_separated_range_means_and() {
        assert!(admitted(">=4.17.0 <5.0.0", "4.17.21"));
        assert!(!admitted(">=4.17.0 <5.0.0", "5.0.0"));
        assert!(!admitted(">=4.17.0 <5.0.0", "4.16.0"));
        // Order is not significance: npm intersects, it does not sequence.
        assert!(admitted("<6.0.0 >=4.0.0", "5.3.0"));
        assert!(!admitted("<6.0.0 >=4.0.0", "6.0.0"));
        assert!(!admitted("<6.0.0 >=4.0.0", "3.0.0"));
    }

    #[test]
    fn alternation_admits_any_branch() {
        assert!(admitted("^0.27.0 || ^1.0.0", "0.27.2"));
        assert!(admitted("^0.27.0 || ^1.0.0", "1.13.0"));
        assert!(!admitted("^0.27.0 || ^1.0.0", "2.0.0"));
        assert!(!admitted("^0.27.0 || ^1.0.0", "0.26.0"));
    }

    #[test]
    fn a_hyphen_range_is_inclusive_at_both_ends() {
        assert!(admitted("4.17.0 - 4.18.0", "4.17.0"));
        assert!(admitted("4.17.0 - 4.18.0", "4.18.0"));
        assert!(!admitted("4.17.0 - 4.18.0", "4.18.1"));
        assert!(!admitted("4.17.0 - 4.18.0", "4.16.9"));
    }

    #[test]
    fn a_partial_hyphen_bound_widens_rather_than_truncating() {
        // node-semver: "1.2.3 - 2.3" reaches every 2.3.z, and "1.2 - 2.3.4"
        // starts at 1.2.0.
        assert!(admitted("1.2.3 - 2.3", "2.3.9"));
        assert!(!admitted("1.2.3 - 2.3", "2.4.0"));
        assert!(admitted("1.2.3 - 2", "2.9.9"));
        assert!(!admitted("1.2.3 - 2", "3.0.0"));
        assert!(admitted("1.2 - 2.3.4", "1.2.0"));
        assert!(!admitted("1.2 - 2.3.4", "1.1.9"));
    }

    #[test]
    fn a_wildcard_range_covers_exactly_the_components_left_open() {
        assert!(admitted("4.3.x", "4.3.7"));
        assert!(!admitted("4.3.x", "4.4.0"));
        assert!(!admitted("4.3.x", "4.2.9"));
        assert!(admitted("1.x", "1.9.9"));
        assert!(!admitted("1.x", "2.0.0"));
        // An uppercase X and a star are the same wildcard.
        assert!(admitted("4.3.X", "4.3.7"));
        assert!(admitted("4.3.*", "4.3.7"));
        // A partial version is a wildcard range with the wildcard left out.
        assert!(admitted("1.2", "1.2.9"));
        assert!(!admitted("1.2", "1.3.0"));
        assert!(admitted("1", "1.9.9"));
        assert!(!admitted("1", "2.0.0"));
    }

    /// A number written after a wildcard is a range in some positions and not in
    /// others, and the split is npm's own: node-semver checks the order of the
    /// components while expanding an x-range, and a caret, a tilde and either
    /// bound of a hyphen range are expanded by rules that never reach the check.
    /// Every case here was read off node-semver 7.8.5, and the two that decide
    /// the split were run through `npm install` as well: `"1.x.3"` fails with
    /// ETARGET, `"^1.x.3"` installs the newest 1.x.
    #[test]
    fn a_number_after_a_wildcard_is_a_range_only_where_npm_reads_one() {
        // Behind a caret or tilde it is a range, and it means what the wildcard
        // alone would mean: the number over an already-open position adds
        // nothing, so "^1.x.3" is "^1.x".
        for spec in ["^1.x.3", "~1.x.3", "^v1.x.3", "^1.x.x", "^1.x"] {
            assert_eq!(classify(spec), SpecShape::ShapeRange, "spec {spec:?}");
            assert!(admitted(spec, "1.9.9"), "spec {spec:?}");
            assert!(!admitted(spec, "2.0.0"), "spec {spec:?}");
            assert!(!admitted(spec, "0.9.9"), "spec {spec:?}");
        }
        // A prerelease or build tag only qualifies a version written in full, so
        // a truncated one drops it rather than carrying it into the bound.
        assert!(admitted("^1.x.3-beta", "1.9.9"));
        assert!(admitted("^1.x.3+build", "1.9.9"));
        // Either bound of a hyphen range reads it the same way.
        assert_eq!(classify("1.x.3 - 2.0.0"), SpecShape::BoundedRange);
        assert!(admitted("1.x.3 - 2.0.0", "1.0.0"));
        assert!(!admitted("1.x.3 - 2.0.0", "2.0.1"));
        assert!(admitted("1.0.0 - 1.x.3", "1.9.9"));
        assert!(!admitted("1.0.0 - 1.x.3", "2.0.0"));
        // The check is per comparator, not per range, so a caret carrying one
        // stands beside an ordinary bound.
        assert!(admitted("^1.x.3 <2.0.0", "1.9.9"));
        assert!(!admitted("^1.x.3 <2.0.0", "2.0.0"));

        // Everywhere else npm reads no range at all, and a dependency written
        // that way installs nothing. Reporting it is the only useful answer:
        // treating it as current hides a manifest npm cannot resolve.
        for spec in [
            "1.x.3",
            ">=1.x.3",
            "<=1.x.3",
            ">1.x.3",
            "<1.x.3",
            "=1.x.3",
            "= 1.x.3",
            ">= 1.x.3",
            "1.X.3",
            "1.*.3",
            "1.x.0",
            "1.x.3 || 2.0.0",
        ] {
            assert_eq!(classify(spec), SpecShape::Unsupported, "spec {spec:?}");
            assert!(parse_npm_range(spec).is_none(), "spec {spec:?}");
        }
        // Where one opens with a letter, npm's other rule reaches it first:
        // whatever is not a range is a dist-tag to resolve at the registry, and
        // the registry decides what it stands for. `"v1.x.3"` is not a version
        // spelled with a prefix, because there is no version there to spell.
        for spec in ["v1.x.3", "x.x.3", "x.2.3"] {
            assert_eq!(classify(spec), SpecShape::NoVersion, "spec {spec:?}");
            assert!(parse_npm_range(spec).is_none(), "spec {spec:?}");
        }
        // One unreadable alternative makes the whole spec unreadable, even
        // beside one that reads.
        assert_eq!(classify("^1.x.3 || 1.x.3"), SpecShape::Unsupported);
        assert_eq!(classify("^1.x.3 || ^2.0.0"), SpecShape::OpaqueRange);

        // A wildcard in the leading position opens every position after it, so
        // "^x.2.3" is "*" and bounds nothing a rewrite could raise.
        assert_eq!(classify("^x.2.3"), SpecShape::OpaqueRange);
        assert!(admitted("^x.2.3", "9.9.9"));

        // Rewriting drops the component npm ignores rather than carrying it
        // forward, so the range upd writes back is one every npm reads.
        assert_eq!(lower_bound_anchor("^1.x.3").as_deref(), Some("1.0.0"));
        assert_eq!(
            rewrite_lower_bound("^1.x.3", "2.4.1").as_deref(),
            Some("^2.x")
        );
        assert_eq!(
            rewrite_lower_bound("1.x.3 - 2.0.0", "1.4.0").as_deref(),
            Some("1.4.0 - 2.0.0")
        );
    }

    #[test]
    fn a_partial_bound_completes_the_way_node_semver_says() {
        // The four desugarings that differ from "just append .0".
        assert!(!admitted(">1.2", "1.2.9"), ">1.2 must exclude every 1.2.z");
        assert!(admitted(">1.2", "1.3.0"));
        assert!(admitted(">=1.2", "1.2.0"), ">=1.2 must admit 1.2.0");
        assert!(!admitted("<1.2", "1.2.0"), "<1.2 must exclude every 1.2.z");
        assert!(admitted("<1.2", "1.1.9"));
        assert!(admitted("<=1.2", "1.2.9"), "<=1.2 must admit every 1.2.z");
        assert!(!admitted("<=1.2", "1.3.0"));
        // And the same one component up.
        assert!(!admitted(">1", "1.9.9"));
        assert!(admitted(">1", "2.0.0"));
        assert!(admitted("<=1", "1.9.9"));
        assert!(!admitted("<=1", "2.0.0"));
    }

    #[test]
    fn caret_and_tilde_keep_their_npm_meaning_through_translation() {
        assert!(admitted("^1.2.3", "1.9.9"));
        assert!(!admitted("^1.2.3", "2.0.0"));
        // A caret on a leading zero is narrower: the zero is the API surface.
        assert!(admitted("^0.2.3", "0.2.9"));
        assert!(!admitted("^0.2.3", "0.3.0"));
        assert!(admitted("^0.0.3", "0.0.3"));
        assert!(!admitted("^0.0.3", "0.0.4"));
        assert!(admitted("~1.2.3", "1.2.9"));
        assert!(!admitted("~1.2.3", "1.3.0"));
        // Partial and wildcard anchors behave as node-semver documents.
        assert!(admitted("^1.2", "1.9.9"));
        assert!(!admitted("^1.2", "2.0.0"));
        assert!(admitted("~1.2", "1.2.9"));
        assert!(!admitted("~1.2", "1.3.0"));
        assert!(admitted("^1.x", "1.9.9"));
        assert!(!admitted("^1.x", "2.0.0"));
    }

    #[test]
    fn a_v_prefix_and_an_explicit_equals_are_read_as_the_version_alone() {
        assert!(admitted("v1.2.3", "1.2.3"));
        assert!(!admitted("v1.2.3", "1.2.4"));
        assert!(admitted("=1.2.3", "1.2.3"));
        assert!(admitted(">=v1.2.0 <v2.0.0", "1.5.0"));
    }

    /// Build metadata never narrows a range, and npm lets it follow any number
    /// of components, so it is dropped rather than refused. A prerelease is the
    /// opposite: it names one specific version, so it needs a full one to
    /// qualify (`"1.2.x-beta"` is the wildcard range with the tail ignored).
    #[test]
    fn build_metadata_is_dropped_and_a_partial_prerelease_is_not_a_range() {
        assert!(admitted("1.2.3+build", "1.2.3"));
        assert!(!admitted("1.2.3+build", "1.2.4"));
        assert!(admitted("1.2+build", "1.2.9"));
        assert!(!admitted("1.2+build", "1.3.0"));
        assert!(admitted("1.2.x-beta", "1.2.9"));
        assert!(!admitted("1.2.x-beta", "1.3.0"));
        assert_eq!(classify("1.x-beta"), SpecShape::Unsupported);
    }

    #[test]
    fn an_empty_range_admits_nothing() {
        assert!(!admitted(">*", "1.0.0"));
        assert!(!admitted("<*", "1.0.0"));
    }

    #[test]
    fn the_anchor_is_the_floor_completed_to_three_components() {
        assert_eq!(
            lower_bound_anchor(">=1.0.0 <2.0.0").as_deref(),
            Some("1.0.0")
        );
        assert_eq!(
            lower_bound_anchor("<2.0.0 >=1.0.0").as_deref(),
            Some("1.0.0")
        );
        assert_eq!(lower_bound_anchor(">=1.2").as_deref(), Some("1.2.0"));
        assert_eq!(
            lower_bound_anchor("4.17.0 - 4.18.0").as_deref(),
            Some("4.17.0")
        );
        assert_eq!(lower_bound_anchor("4.3.x").as_deref(), Some("4.3.0"));
        assert_eq!(lower_bound_anchor("1").as_deref(), Some("1.0.0"));
        assert_eq!(
            lower_bound_anchor(">=1.0.0-beta <2.0.0").as_deref(),
            Some("1.0.0-beta")
        );
    }

    #[test]
    fn a_spec_with_no_single_floor_has_no_anchor() {
        assert_eq!(lower_bound_anchor("<3"), None);
        assert_eq!(lower_bound_anchor("<=2.0.0"), None);
        assert_eq!(lower_bound_anchor("^1.0.0 || ^2.0.0"), None);
        assert_eq!(lower_bound_anchor("*"), None);
        assert_eq!(lower_bound_anchor("workspace:*"), None);
    }

    /// `">1.2.3"` is the one bound that reads like a floor and is not one. The
    /// version after it is the one version the author has ruled out, so there
    /// is nothing "current" under it to move forward, and moving it anyway
    /// produces a range that excludes the release it was moved to.
    #[test]
    fn an_exclusive_lower_bound_is_not_a_floor_a_rewrite_can_raise() {
        // What the rewrite would have produced, spelled out: a range that
        // admits nothing at all once its floor is the newest release. This is
        // what reached a manifest and made the next run exit with an error.
        assert!(!admitted(">4.18.1 <5.0.0", "4.18.1"));

        for spec in [">1.2.3", ">1.2.3 <2.0.0", "<2.0.0 >1.2.3", ">1.2"] {
            assert_eq!(
                classify(spec),
                SpecShape::OpaqueRange,
                "{spec} has no floor to raise"
            );
            assert_eq!(lower_bound_anchor(spec), None, "{spec}");
            assert_eq!(rewrite_lower_bound(spec, "1.9.0"), None, "{spec}");
        }

        // Still evaluated, though: refusing to rewrite a range is not refusing
        // to read it, and the caller decides whether it is current from this.
        assert!(admitted(">1.2.3 <2.0.0", "1.9.0"));
        assert!(!admitted(">1.2.3 <2.0.0", "2.0.0"));

        // The inclusive form is unaffected - it names a version the author
        // accepts, which is exactly what makes it movable.
        assert_eq!(classify(">=1.2.3 <2.0.0"), SpecShape::BoundedRange);
        assert_eq!(
            rewrite_lower_bound(">=1.2.3 <2.0.0", "1.9.0").as_deref(),
            Some(">=1.9.0 <2.0.0")
        );
    }

    #[test]
    fn rewriting_moves_the_floor_and_leaves_the_ceiling_alone() {
        assert_eq!(
            rewrite_lower_bound(">=1.0.0 <2.0.0", "1.5.0").as_deref(),
            Some(">=1.5.0 <2.0.0")
        );
        assert_eq!(
            rewrite_lower_bound("<2.0.0 >=1.0.0", "1.5.0").as_deref(),
            Some("<2.0.0 >=1.5.0")
        );
        assert_eq!(
            rewrite_lower_bound(">=1.0.0   <2.0.0", "1.5.0").as_deref(),
            Some(">=1.5.0 <2.0.0")
        );
        assert_eq!(
            rewrite_lower_bound(">=1.0.0", "1.5.0").as_deref(),
            Some(">=1.5.0")
        );
    }

    #[test]
    fn rewriting_a_hyphen_range_keeps_it_a_hyphen_range() {
        assert_eq!(
            rewrite_lower_bound("4.17.0 - 4.18.0", "4.17.21").as_deref(),
            Some("4.17.21 - 4.18.0")
        );
        assert_eq!(
            rewrite_lower_bound("1.2 - 2.3", "1.9.0").as_deref(),
            Some("1.9.0 - 2.3")
        );

        // The newest release a range admits can be the ceiling itself, and the
        // rewrite then names one version at both ends. Writing a bare `7.6.0`
        // instead would turn a range into an exact pin, a shape the author did
        // not choose; the two-ended form says the same thing and leaves the
        // spec recognisable as the range it still is.
        assert_eq!(
            rewrite_lower_bound("7.5.0 - 7.6.0", "7.6.0").as_deref(),
            Some("7.6.0 - 7.6.0")
        );
    }

    #[test]
    fn rewriting_a_wildcard_range_keeps_its_wildcard_and_its_width() {
        assert_eq!(
            rewrite_lower_bound("4.3.x", "4.4.3").as_deref(),
            Some("4.4.x")
        );
        assert_eq!(
            rewrite_lower_bound("4.3.X", "4.4.3").as_deref(),
            Some("4.4.X")
        );
        assert_eq!(
            rewrite_lower_bound("4.3.*", "4.4.3").as_deref(),
            Some("4.4.*")
        );
        assert_eq!(rewrite_lower_bound("1.x", "2.5.0").as_deref(), Some("2.x"));
        assert_eq!(rewrite_lower_bound("1.2", "1.3.4").as_deref(), Some("1.3"));
        assert_eq!(rewrite_lower_bound("1", "2.0.0").as_deref(), Some("2"));
        // Where the author wrote two wildcards, the width comes from the first
        // number left out, so that is the one whose character the rewrite puts
        // back. Taking the last would restyle a spec upd was only asked to move.
        assert_eq!(
            rewrite_lower_bound("1.X.x", "2.5.0").as_deref(),
            Some("2.X")
        );
    }

    #[test]
    fn rewriting_a_partial_caret_or_tilde_keeps_the_operator_and_the_width() {
        // Writing "^4.4.3" over "^1.2" would narrow nothing and widen nothing,
        // but it silently changes what the author wrote into a different shape.
        assert_eq!(
            rewrite_lower_bound("^1.2", "4.4.3").as_deref(),
            Some("^4.4")
        );
        assert_eq!(rewrite_lower_bound("~1", "4.4.3").as_deref(), Some("~4"));
        assert_eq!(
            rewrite_lower_bound("^1.x", "4.4.3").as_deref(),
            Some("^4.x")
        );
        assert_eq!(lower_bound_anchor("^1.2").as_deref(), Some("1.2.0"));
        assert_eq!(lower_bound_anchor("~1").as_deref(), Some("1.0.0"));
    }

    #[test]
    fn a_rewrite_needs_a_complete_replacement_version() {
        // The replacement comes from a registry, so a partial one means the
        // lookup returned something unusable; putting it into the shape anyway
        // would write "^4.4" from "4.4" and call the dependency updated.
        assert!(rewrite_lower_bound("4.3.x", "4.4").is_none());
        assert!(rewrite_lower_bound("^1.2", "nope").is_none());
    }

    /// The updater picks a route from [`classify`] alone and only then asks for
    /// a rewrite, so a shape it routes as rewritable has to be one
    /// [`rewrite_lower_bound`] will actually rewrite. Where the two disagree the
    /// dependency lands in the "read as a range with a lower bound to raise, but
    /// it has none" error, having been looked up for nothing.
    #[test]
    fn the_shapes_routed_for_rewriting_are_the_shapes_that_rewrite() {
        for spec in [
            ">=1.0.0 <2.0.0",
            "<2.0.0 >=1.0.0",
            ">=1.0.0",
            "4.17.0 - 4.18.0",
            "4.3.x",
            "1.2",
            "^1.2",
            "~1",
        ] {
            assert!(
                matches!(
                    classify(spec),
                    SpecShape::BoundedRange | SpecShape::ShapeRange
                ),
                "{spec} is routed for rewriting"
            );
            assert!(
                rewrite_lower_bound(spec, "9.9.9").is_some(),
                "{spec} is routed for rewriting but does not rewrite"
            );
        }

        for spec in [">1.0.0", ">1.0.0 <2.0.0", "<2.0.0", "<=2.0.0", "^1 || ^2"] {
            assert!(
                !matches!(
                    classify(spec),
                    SpecShape::BoundedRange | SpecShape::ShapeRange
                ),
                "{spec} does not rewrite, so it must not be routed for rewriting"
            );
            assert!(rewrite_lower_bound(spec, "9.9.9").is_none(), "{spec}");
        }
    }

    #[test]
    fn a_rewritten_range_still_admits_the_version_written_into_it() {
        // The property that makes a rewrite safe: whatever shape it puts back,
        // the result has to accept the version it was given. A rewrite that
        // fails it is worse than none at all: the manifest is left naming a
        // range with nothing in it, and the next run cannot resolve the
        // dependency it just wrote.
        for (spec, new_version) in [
            (">=1.0.0 <2.0.0", "1.5.0"),
            ("<2.0.0 >=1.0.0", "1.5.0"),
            (">=1.0.0", "9.9.9"),
            ("4.17.0 - 4.18.0", "4.17.21"),
            ("1.2 - 2.3", "1.9.0"),
            ("4.3.x", "4.4.3"),
            ("1.x", "2.5.0"),
            ("1.2", "1.3.4"),
            ("1", "2.0.0"),
            ("^1.2", "4.4.3"),
            ("~1", "4.4.3"),
            ("^1.x", "4.4.3"),
        ] {
            let rewritten = rewrite_lower_bound(spec, new_version)
                .unwrap_or_else(|| panic!("{spec} was not rewritten"));
            assert!(
                admitted(&rewritten, new_version),
                "{spec} became {rewritten}, which excludes {new_version}"
            );
        }
    }

    #[test]
    fn nothing_is_rewritten_without_a_floor_or_a_version() {
        assert!(rewrite_lower_bound("<3", "2.5.0").is_none());
        assert!(rewrite_lower_bound("<=2.0.0", "1.9.0").is_none());
        assert!(rewrite_lower_bound(">1.0.0", "1.9.0").is_none());
        assert!(rewrite_lower_bound("^1.0.0 || ^2.0.0", "2.5.0").is_none());
        assert!(rewrite_lower_bound("workspace:*", "2.5.0").is_none());
        assert!(rewrite_lower_bound(">=1.0.0 <2.0.0", "").is_none());
        assert!(rewrite_lower_bound(">=1.0.0", "   ").is_none());
    }

    /// The forms whose meaning is easiest to get wrong by reasoning about it.
    /// Every expectation here is node-semver's own answer, taken from a
    /// differential run of `satisfies` over 61 range forms and 32 versions
    /// (node-semver 7.8.5): all 1952 pairs agreed, and these are the ones no
    /// other test in this module pins.
    #[test]
    fn the_forms_that_are_easiest_to_reason_about_wrongly() {
        // A caret over a zero prefix narrows to whatever the zeroes leave.
        assert!(admitted("^0.0", "0.0.4"));
        assert!(!admitted("^0.0", "0.1.0"));
        assert!(admitted("^0", "0.3.0"));
        assert!(!admitted("^0", "1.0.0"));
        assert!(admitted("~0", "0.3.0"));
        assert!(!admitted("~0", "1.0.0"));
        assert!(admitted("0.x", "0.3.0"));
        assert!(!admitted("0.x", "1.0.0"));
        assert!(admitted("0.0.x", "0.0.4"));
        assert!(!admitted("0.0.x", "0.1.0"));

        // A prerelease anchor admits later prereleases of the SAME version and
        // every release above it, but no prerelease of any other version.
        assert!(admitted("^1.2.3-beta.1", "1.2.3-beta.2"));
        assert!(admitted("^1.2.3-beta.1", "1.9.9"));
        assert!(!admitted("^1.2.3-beta.1", "1.2.3-alpha"));
        assert!(!admitted("^1.2.3-beta.1", "2.0.0"));
        // A version with no prerelease anchor covering it stays out, however
        // far inside the numeric range it looks.
        assert!(!admitted(">=1.0.0 <2.0.0", "1.2.3-beta.1"));

        // Alternation of three different shapes, each still meaning what it
        // means on its own.
        let three = "1.x || >=2.5.0 || 5.0.0 - 7.2.3";
        assert!(admitted(three, "1.9.9"));
        assert!(admitted(three, "7.2.3"));
        assert!(admitted(three, "9.9.9"));
        assert!(!admitted(three, "2.0.0"));
        assert!(!admitted(three, "2.4.0"));
    }

    #[test]
    fn a_prerelease_anchor_survives_classification_and_rewriting() {
        assert_eq!(classify(">=1.0.0-beta <2.0.0"), SpecShape::BoundedRange);
        assert_eq!(
            rewrite_lower_bound(">=1.0.0-beta <2.0.0", "1.0.0-rc.1").as_deref(),
            Some(">=1.0.0-rc.1 <2.0.0")
        );
    }
}
