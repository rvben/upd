//! Cooldown (minimum release age) policy and selection logic.
//!
//! See docs/superpowers/specs/2026-04-22-cooldown-release-age-design.md.

use anyhow::{Result, anyhow};
use chrono::Duration;

/// Parse a cooldown duration string.
///
/// Accepted forms: `<integer><unit>` where unit is `s`, `m`, `h`, `d`, `w`.
/// A bare `"0"` means "disabled" and parses as zero duration.
pub fn parse_duration(input: &str) -> Result<Duration> {
    if input.is_empty() {
        return Err(anyhow!("empty cooldown duration"));
    }
    // The bare "0" shortcut means "disabled".
    if input == "0" {
        return Ok(Duration::zero());
    }
    // Reject leading/trailing whitespace so config files stay unambiguous.
    if input.trim() != input {
        return Err(anyhow!(
            "invalid cooldown duration '{input}': whitespace not allowed"
        ));
    }

    let last = input.chars().last().unwrap();
    if !last.is_ascii_alphabetic() {
        return Err(anyhow!(
            "invalid cooldown duration '{input}': missing unit (expected s/m/h/d/w)"
        ));
    }
    let (num_part, unit) = input.split_at(input.len() - last.len_utf8());
    let value: i64 = num_part.parse().map_err(|_| {
        anyhow!("invalid cooldown duration '{input}': '{num_part}' is not a non-negative integer")
    })?;
    if value < 0 {
        return Err(anyhow!(
            "invalid cooldown duration '{input}': value must be non-negative"
        ));
    }
    match unit {
        "s" => Ok(Duration::seconds(value)),
        "m" => Ok(Duration::minutes(value)),
        "h" => Ok(Duration::hours(value)),
        "d" => Ok(Duration::days(value)),
        "w" => Ok(Duration::weeks(value)),
        _ => Err(anyhow!(
            "invalid cooldown duration '{input}': unknown unit '{unit}' (expected s/m/h/d/w)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::seconds(30));
    }

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(parse_duration("15m").unwrap(), Duration::minutes(15));
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(parse_duration("72h").unwrap(), Duration::hours(72));
    }

    #[test]
    fn test_parse_duration_days() {
        assert_eq!(parse_duration("7d").unwrap(), Duration::days(7));
    }

    #[test]
    fn test_parse_duration_weeks() {
        assert_eq!(parse_duration("2w").unwrap(), Duration::weeks(2));
    }

    #[test]
    fn test_parse_duration_zero_bare() {
        assert_eq!(parse_duration("0").unwrap(), Duration::zero());
    }

    #[test]
    fn test_parse_duration_zero_with_unit() {
        assert_eq!(parse_duration("0d").unwrap(), Duration::zero());
    }

    #[test]
    fn test_parse_duration_rejects_empty() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn test_parse_duration_rejects_missing_unit() {
        let err = parse_duration("7").unwrap_err().to_string();
        assert!(err.contains("unit"), "error should mention unit: {err}");
    }

    #[test]
    fn test_parse_duration_rejects_unknown_unit() {
        let err = parse_duration("7y").unwrap_err().to_string();
        assert!(
            err.contains("unit") || err.contains("y"),
            "error should mention unit/y: {err}"
        );
    }

    #[test]
    fn test_parse_duration_rejects_negative() {
        assert!(parse_duration("-1d").is_err());
    }

    #[test]
    fn test_parse_duration_rejects_non_numeric() {
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn test_parse_duration_rejects_whitespace() {
        assert!(
            parse_duration(" 7d ").is_err(),
            "leading/trailing whitespace should be rejected"
        );
    }

    #[test]
    fn test_parse_duration_rejects_float() {
        assert!(parse_duration("1.5d").is_err());
    }
}
