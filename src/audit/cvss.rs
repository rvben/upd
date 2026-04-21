//! Minimal CVSS v3.0/3.1 base-score parser and severity-label mapper.
//!
//! Only the base score is computed — temporal and environmental metrics are
//! ignored, which matches the OSV API's score field (base-score only).
//!
//! Reference: FIRST CVSS v3.1 specification
//! <https://www.first.org/cvss/v3.1/specification-document>

/// Human-readable severity label derived from a CVSS base score or a
/// `database_specific.severity` string.
///
/// Variants are ordered from highest to lowest severity so that `Ord`-based
/// sorting gives descending severity naturally when reversed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SeverityLabel {
    Critical,
    High,
    Medium,
    Low,
    None,
    Unknown,
}

impl SeverityLabel {
    /// Title-case string used in text and JSON output.
    pub fn as_str(&self) -> &'static str {
        match self {
            SeverityLabel::Critical => "Critical",
            SeverityLabel::High => "High",
            SeverityLabel::Medium => "Medium",
            SeverityLabel::Low => "Low",
            SeverityLabel::None => "None",
            SeverityLabel::Unknown => "Unknown",
        }
    }

    /// Map a CVSS v3.x base score to the NIST severity band.
    ///
    /// Bands: 0.0 None / 0.1–3.9 Low / 4.0–6.9 Medium / 7.0–8.9 High / 9.0–10.0 Critical
    pub fn from_score(score: f64) -> Self {
        if score == 0.0 {
            SeverityLabel::None
        } else if score < 4.0 {
            SeverityLabel::Low
        } else if score < 7.0 {
            SeverityLabel::Medium
        } else if score < 9.0 {
            SeverityLabel::High
        } else {
            SeverityLabel::Critical
        }
    }

    /// Normalise a raw severity string (e.g. from `database_specific.severity`
    /// or from a human-readable label) to a [`SeverityLabel`].
    ///
    /// Recognised inputs (case-insensitive):
    /// - "CRITICAL" / "Critical" → [`SeverityLabel::Critical`]
    /// - "HIGH" / "High" → [`SeverityLabel::High`]
    /// - "MODERATE" / "MEDIUM" / "Medium" → [`SeverityLabel::Medium`]
    /// - "LOW" / "Low" → [`SeverityLabel::Low`]
    /// - "NONE" / "None" → [`SeverityLabel::None`]
    ///
    /// Any other value returns [`SeverityLabel::Unknown`].
    pub fn from_str_label(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "CRITICAL" => SeverityLabel::Critical,
            "HIGH" => SeverityLabel::High,
            "MODERATE" | "MEDIUM" => SeverityLabel::Medium,
            "LOW" => SeverityLabel::Low,
            "NONE" => SeverityLabel::None,
            _ => SeverityLabel::Unknown,
        }
    }
}

// ── CVSS v3.x metric weights ──────────────────────────────────────────────────

/// Attack Vector weights
struct Av;
impl Av {
    const N: f64 = 0.85; // Network
    const A: f64 = 0.62; // Adjacent
    const L: f64 = 0.55; // Local
    const P: f64 = 0.20; // Physical
}

/// Attack Complexity weights
struct Ac;
impl Ac {
    const L: f64 = 0.77; // Low
    const H: f64 = 0.44; // High
}

/// Privileges Required weights — scope-dependent
struct Pr;
impl Pr {
    // Unchanged scope
    const N_U: f64 = 0.85;
    const L_U: f64 = 0.62;
    const H_U: f64 = 0.27;
    // Changed scope
    const N_C: f64 = 0.85;
    const L_C: f64 = 0.68;
    const H_C: f64 = 0.50;
}

/// User Interaction weights
struct Ui;
impl Ui {
    const N: f64 = 0.85; // None
    const R: f64 = 0.62; // Required
}

/// Confidentiality / Integrity / Availability impact weights
struct Cia;
impl Cia {
    const N: f64 = 0.00; // None
    const L: f64 = 0.22; // Low
    const H: f64 = 0.56; // High
}

// ── Parser ────────────────────────────────────────────────────────────────────

/// Parse a CVSS v3.0 or v3.1 vector string and return the base score.
///
/// Returns `None` if the vector is malformed or uses an unsupported version.
/// v4.0 vectors are recognised by the "CVSS:4.0/" prefix and handled with a
/// rough approximation (see below).
pub fn parse_cvss_score(vector: &str) -> Option<f64> {
    if let Some(rest) = vector.strip_prefix("CVSS:4.0/") {
        return parse_cvss4_approx(rest);
    }

    let rest = vector
        .strip_prefix("CVSS:3.1/")
        .or_else(|| vector.strip_prefix("CVSS:3.0/"))?;

    parse_cvss3_base(rest)
}

/// Parse CVSS v3.x base metrics and compute the base score.
fn parse_cvss3_base(metrics: &str) -> Option<f64> {
    let mut av: Option<f64> = None;
    let mut ac: Option<f64> = None;
    let mut pr_raw: Option<&str> = None; // keep raw for scope-dependent lookup
    let mut ui: Option<f64> = None;
    let mut scope_changed = false;
    let mut c: Option<f64> = None;
    let mut i: Option<f64> = None;
    let mut a: Option<f64> = None;

    for part in metrics.split('/') {
        let (key, val) = part.split_once(':')?;
        match key {
            "AV" => {
                av = Some(match val {
                    "N" => Av::N,
                    "A" => Av::A,
                    "L" => Av::L,
                    "P" => Av::P,
                    _ => return None,
                })
            }
            "AC" => {
                ac = Some(match val {
                    "L" => Ac::L,
                    "H" => Ac::H,
                    _ => return None,
                })
            }
            "PR" => pr_raw = Some(val),
            "UI" => {
                ui = Some(match val {
                    "N" => Ui::N,
                    "R" => Ui::R,
                    _ => return None,
                })
            }
            "S" => {
                scope_changed = match val {
                    "U" => false,
                    "C" => true,
                    _ => return None,
                }
            }
            "C" => {
                c = Some(match val {
                    "N" => Cia::N,
                    "L" => Cia::L,
                    "H" => Cia::H,
                    _ => return None,
                })
            }
            "I" => {
                i = Some(match val {
                    "N" => Cia::N,
                    "L" => Cia::L,
                    "H" => Cia::H,
                    _ => return None,
                })
            }
            "A" => {
                a = Some(match val {
                    "N" => Cia::N,
                    "L" => Cia::L,
                    "H" => Cia::H,
                    _ => return None,
                })
            }
            // Temporal / environmental metrics are ignored for base score
            _ => {}
        }
    }

    let av = av?;
    let ac = ac?;
    let pr = match pr_raw? {
        "N" => {
            if scope_changed {
                Pr::N_C
            } else {
                Pr::N_U
            }
        }
        "L" => {
            if scope_changed {
                Pr::L_C
            } else {
                Pr::L_U
            }
        }
        "H" => {
            if scope_changed {
                Pr::H_C
            } else {
                Pr::H_U
            }
        }
        _ => return None,
    };
    let ui = ui?;
    let c = c?;
    let i = i?;
    let a = a?;

    // Impact Sub-Score (ISS)
    let iss = 1.0 - (1.0 - c) * (1.0 - i) * (1.0 - a);

    if iss <= 0.0 {
        return Some(0.0);
    }

    let impact = if scope_changed {
        // Scope Changed formula
        7.52 * (iss - 0.029) - 3.25 * (iss - 0.02_f64).powi(15)
    } else {
        // Scope Unchanged formula
        6.42 * iss
    };

    let exploitability = 8.22 * av * ac * pr * ui;

    let base_score = if scope_changed {
        roundup(f64::min(1.08 * (impact + exploitability), 10.0))
    } else {
        roundup(f64::min(impact + exploitability, 10.0))
    };

    Some(base_score)
}

/// CVSS v4.0 approximation: pick the highest CIA impact value and map it to a
/// rough score threshold. This is intentionally coarse — v4.0 scoring is
/// significantly more complex, and the approximation is preferable to
/// returning `None` for every v4.0 vector.
fn parse_cvss4_approx(metrics: &str) -> Option<f64> {
    let mut max_cia: f64 = 0.0;

    for part in metrics.split('/') {
        let (key, val) = part.split_once(':')?;
        if matches!(key, "VC" | "VI" | "VA" | "SC" | "SI" | "SA") {
            let weight = match val {
                "H" => Cia::H,
                "L" => Cia::L,
                "N" => Cia::N,
                _ => return None,
            };
            if weight > max_cia {
                max_cia = weight;
            }
        }
    }

    // Map the highest CIA component to a representative score in the right band
    let score = if max_cia >= Cia::H {
        8.0 // High band default for v4.0
    } else if max_cia >= Cia::L {
        4.0 // Medium band default for v4.0
    } else {
        0.0
    };

    Some(score)
}

/// Round up to the nearest 0.1 as defined by the CVSS v3 specification.
///
/// `roundup(x) = ceil(x * 10) / 10`
fn roundup(x: f64) -> f64 {
    (x * 10.0).ceil() / 10.0
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Determine the severity label for a raw string from the OSV API.
///
/// Resolution order:
/// 1. If `db_severity` is `Some`, normalise it via [`SeverityLabel::from_str_label`].
/// 2. If a `cvss_vector` is provided, parse it and map the score to a label.
/// 3. If the vector cannot be parsed, return the raw vector as a fallback
///    (preserves the current behaviour for unrecognised inputs).
/// 4. If neither is present, return `Unknown`.
pub fn resolve_severity(db_severity: Option<&str>, cvss_vector: Option<&str>) -> ResolvedSeverity {
    if let Some(label) = db_severity {
        let normalized = SeverityLabel::from_str_label(label);
        return ResolvedSeverity::Label(normalized);
    }

    if let Some(vector) = cvss_vector {
        if let Some(score) = parse_cvss_score(vector) {
            return ResolvedSeverity::Label(SeverityLabel::from_score(score));
        }
        // Unparseable vector: fall back to raw string
        return ResolvedSeverity::Raw(vector.to_string());
    }

    ResolvedSeverity::Label(SeverityLabel::Unknown)
}

/// The resolved severity, which is either a normalised label or a raw fallback.
pub enum ResolvedSeverity {
    Label(SeverityLabel),
    Raw(String),
}

impl ResolvedSeverity {
    /// The string to store in the `Vulnerability::severity` field.
    pub fn as_severity_string(&self) -> String {
        match self {
            ResolvedSeverity::Label(l) => l.as_str().to_string(),
            ResolvedSeverity::Raw(s) => s.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── CVSS v3.1 base-score tests ────────────────────────────────────────────

    /// CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H → 9.8 → Critical
    #[test]
    fn cvss_31_critical_network_all_high() {
        let score =
            parse_cvss_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H").expect("should parse");
        assert!((score - 9.8).abs() < 0.05, "expected ~9.8, got {score}");
        assert_eq!(SeverityLabel::from_score(score), SeverityLabel::Critical);
    }

    /// CVSS:3.1/AV:L/AC:H/PR:H/UI:R/S:U/C:L/I:L/A:L → low score → Low
    #[test]
    fn cvss_31_low_end_local_high_complexity() {
        let score =
            parse_cvss_score("CVSS:3.1/AV:L/AC:H/PR:H/UI:R/S:U/C:L/I:L/A:L").expect("should parse");
        assert!(score < 4.0, "expected score < 4.0 (Low), got {score}");
        assert_eq!(SeverityLabel::from_score(score), SeverityLabel::Low);
    }

    /// CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H → scope-changed → Critical (≥9.0)
    #[test]
    fn cvss_31_scope_changed_critical() {
        let score =
            parse_cvss_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H").expect("should parse");
        // The CVSS calculator gives 10.0 for this vector
        assert!(score >= 9.0, "expected Critical (≥9.0), got {score}");
        assert_eq!(SeverityLabel::from_score(score), SeverityLabel::Critical);
    }

    /// CVSS:3.0/AV:N/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H → ~6.5 → Medium
    #[test]
    fn cvss_30_medium_availability_only() {
        let score =
            parse_cvss_score("CVSS:3.0/AV:N/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H").expect("should parse");
        // Expected: 6.5 (confirmed with FIRST calculator)
        assert!((score - 6.5).abs() < 0.15, "expected ~6.5, got {score}");
        assert_eq!(SeverityLabel::from_score(score), SeverityLabel::Medium);
    }

    /// Malformed vector → returns None
    #[test]
    fn cvss_malformed_returns_none() {
        assert!(parse_cvss_score("not-a-cvss-vector").is_none());
        assert!(parse_cvss_score("CVSS:3.1/AV:X/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H").is_none());
        assert!(parse_cvss_score("CVSS:3.1/AV:N").is_none()); // missing required metrics
    }

    // ── SeverityLabel ordering ────────────────────────────────────────────────

    #[test]
    fn severity_label_ordering_is_descending() {
        assert!(SeverityLabel::Critical < SeverityLabel::High);
        assert!(SeverityLabel::High < SeverityLabel::Medium);
        assert!(SeverityLabel::Medium < SeverityLabel::Low);
        assert!(SeverityLabel::Low < SeverityLabel::None);
        assert!(SeverityLabel::None < SeverityLabel::Unknown);
    }

    // ── from_str_label normalization ──────────────────────────────────────────

    #[test]
    fn from_str_label_normalizes_common_values() {
        assert_eq!(
            SeverityLabel::from_str_label("CRITICAL"),
            SeverityLabel::Critical
        );
        assert_eq!(SeverityLabel::from_str_label("HIGH"), SeverityLabel::High);
        assert_eq!(
            SeverityLabel::from_str_label("MODERATE"),
            SeverityLabel::Medium
        );
        assert_eq!(
            SeverityLabel::from_str_label("MEDIUM"),
            SeverityLabel::Medium
        );
        assert_eq!(SeverityLabel::from_str_label("LOW"), SeverityLabel::Low);
        assert_eq!(SeverityLabel::from_str_label("NONE"), SeverityLabel::None);
        assert_eq!(
            SeverityLabel::from_str_label("bogus"),
            SeverityLabel::Unknown
        );
    }

    #[test]
    fn from_str_label_case_insensitive() {
        assert_eq!(
            SeverityLabel::from_str_label("critical"),
            SeverityLabel::Critical
        );
        assert_eq!(SeverityLabel::from_str_label("High"), SeverityLabel::High);
        assert_eq!(
            SeverityLabel::from_str_label("moderate"),
            SeverityLabel::Medium
        );
        assert_eq!(SeverityLabel::from_str_label("low"), SeverityLabel::Low);
    }

    // ── resolve_severity ──────────────────────────────────────────────────────

    #[test]
    fn resolve_severity_prefers_db_specific() {
        let r = resolve_severity(
            Some("CRITICAL"),
            Some("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"),
        );
        assert_eq!(r.as_severity_string(), "Critical");
    }

    #[test]
    fn resolve_severity_falls_back_to_cvss_vector() {
        let r = resolve_severity(None, Some("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"));
        assert_eq!(r.as_severity_string(), "Critical");
    }

    #[test]
    fn resolve_severity_raw_fallback_for_unparseable_vector() {
        let r = resolve_severity(None, Some("UNPARSEABLE"));
        assert_eq!(r.as_severity_string(), "UNPARSEABLE");
    }

    #[test]
    fn resolve_severity_unknown_when_no_info() {
        let r = resolve_severity(None, None);
        assert_eq!(r.as_severity_string(), "Unknown");
    }
}
