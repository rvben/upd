//! uv upload cutoffs, evaluated once per manifest operation.
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use jiff::{Span, Timestamp, ToSpan, tz::TimeZone};
use std::collections::HashMap;
use toml_edit::{DocumentMut, Item};

#[derive(Debug, Clone, Copy)]
enum Cutoff {
    Disabled,
    Before(Timestamp),
}

impl Cutoff {
    fn parse(item: &Item, now: Timestamp) -> Result<Self> {
        if item.as_bool() == Some(false) {
            return Ok(Self::Disabled);
        }
        let value = item
            .as_str()
            .context("exclude-newer must be a date, timestamp or duration string, or false")?;
        if let Ok(timestamp) = value.parse::<Timestamp>() {
            return Ok(Self::Before(timestamp));
        }
        if let Ok(date) = value.parse::<jiff::civil::Date>() {
            // uv's date shorthand includes the entire local calendar day.
            return Ok(Self::Before(
                date.checked_add(1.day())?
                    .to_zoned(TimeZone::system())?
                    .timestamp(),
            ));
        }
        let span = value
            .parse::<Span>()
            .context("invalid exclude-newer date, timestamp or duration")?;
        if span.get_years() != 0 || span.get_months() != 0 {
            bail!("exclude-newer durations cannot contain calendar months or years; use days");
        }
        Ok(Self::Before(
            now.to_zoned(TimeZone::UTC)
                .checked_sub(span.abs())?
                .timestamp(),
        ))
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct UvPolicy {
    global: Option<Cutoff>,
    packages: HashMap<String, Cutoff>,
    indexes: HashMap<String, Cutoff>,
}

/// Registry identity contains no authentication material and uses the same
/// optional `/simple` suffix normalization as the PyPI client.
pub(crate) fn index_identity(value: &str) -> String {
    let mut value = url::Url::parse(value).ok();
    if let Some(url) = &mut value {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
    }
    let value = value.map(|v| v.to_string()).unwrap_or_default();
    value
        .trim_end_matches('/')
        .trim_end_matches("/simple")
        .to_string()
}

impl UvPolicy {
    pub fn from_document(doc: &DocumentMut, now: DateTime<Utc>) -> Result<Self> {
        let Some(uv) = doc
            .get("tool")
            .and_then(|t| t.get("uv"))
            .and_then(Item::as_table_like)
        else {
            return Ok(Self::default());
        };
        let now: Timestamp = now.to_rfc3339().parse()?;
        let mut policy = Self::default();
        if let Some(value) = uv.get("exclude-newer") {
            policy.global = Some(Cutoff::parse(value, now).context("[tool.uv].exclude-newer")?);
        }
        if let Some(values) = uv.get("exclude-newer-package") {
            for (package, value) in values
                .as_table_like()
                .context("exclude-newer-package must be a table")?
                .iter()
            {
                let name = crate::normalize::pep503_normalize(package);
                if policy
                    .packages
                    .insert(
                        name,
                        Cutoff::parse(value, now)
                            .with_context(|| format!("exclude-newer-package.{package}"))?,
                    )
                    .is_some()
                {
                    bail!("duplicate normalized package in exclude-newer-package: {package}");
                }
            }
        }
        if let Some(indexes) = uv.get("index") {
            let tables: Vec<&dyn toml_edit::TableLike> =
                if let Some(array) = indexes.as_array_of_tables() {
                    array
                        .iter()
                        .map(|t| t as &dyn toml_edit::TableLike)
                        .collect()
                } else if let Some(array) = indexes.as_array() {
                    array
                        .iter()
                        .filter_map(|v| v.as_inline_table().map(|t| t as &dyn toml_edit::TableLike))
                        .collect()
                } else {
                    bail!("tool.uv.index must be an array of tables");
                };
            for table in tables {
                if let Some(value) = table.get("exclude-newer") {
                    let url = table
                        .get("url")
                        .and_then(Item::as_str)
                        .context("an index cutoff requires an index URL")?;
                    let identity = index_identity(url);
                    if identity.is_empty() {
                        bail!("invalid URL for index exclude-newer policy");
                    }
                    let cutoff =
                        Cutoff::parse(value, now).context("tool.uv.index.exclude-newer")?;
                    if policy.indexes.insert(identity, cutoff).is_some() {
                        bail!("multiple exclude-newer policies for the same index URL");
                    }
                }
            }
        }
        Ok(policy)
    }

    pub fn active(&self) -> bool {
        self.global.is_some() || !self.packages.is_empty() || !self.indexes.is_empty()
    }

    pub fn admits(&self, package: &str, index: Option<&str>, uploaded: Option<&str>) -> bool {
        let index_policy = index.and_then(|url| self.indexes.get(&index_identity(url)));
        let cutoff = self
            .packages
            .get(&crate::normalize::pep503_normalize(package))
            .or(index_policy)
            .or(self.global.as_ref());
        match cutoff {
            None | Some(Cutoff::Disabled) => true,
            Some(Cutoff::Before(cutoff)) => match uploaded {
                // uv allows absent timestamps for indexes with their own policy.
                None => index_policy.is_some(),
                Some(value) => value.parse::<Timestamp>().is_ok_and(|at| at < *cutoff),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy(setting: &str) -> UvPolicy {
        UvPolicy::from_document(
            &format!("[tool.uv]\n{setting}").parse().unwrap(),
            "2026-09-07T12:00:00Z".parse().unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn durations_are_equivalent_and_boundary_is_exclusive() {
        for duration in ["1 week", "7d", "P7D", "PT168H", "6 days 24 hours"] {
            let policy = policy(&format!("exclude-newer = '{duration}'"));
            assert!(
                policy.admits("demo", None, Some("2026-08-31T11:59:59Z")),
                "{duration}"
            );
            assert!(
                !policy.admits("demo", None, Some("2026-08-31T12:00:00Z")),
                "{duration}"
            );
            assert!(!policy.admits("demo", None, None));
            assert!(!policy.admits("demo", None, Some("invalid")));
        }
    }
    #[test]
    fn package_overrides_index_and_global_for_known_uploads() {
        let policy = policy(
            "exclude-newer = '2025-01-01T00:00:00Z'\nexclude-newer-package = { Demo_Pkg = '2024-01-01T00:00:00Z' }\n[[tool.uv.index]]\nurl = 'https://example.com/simple/'\nexclude-newer = false\n",
        );
        assert!(!policy.admits(
            "demo-pkg",
            Some("https://example.com"),
            Some("2024-06-01T00:00:00Z")
        ));
        assert!(policy.admits(
            "other",
            Some("https://example.com"),
            Some("2026-01-01T00:00:00Z")
        ));
        assert!(!policy.admits(
            "other",
            Some("https://elsewhere.com"),
            Some("2026-01-01T00:00:00Z")
        ));
    }
    #[test]
    fn local_date_includes_the_whole_day() {
        let p = policy("exclude-newer = '2025-01-01'");
        let Some(Cutoff::Before(actual)) = p.global else {
            panic!()
        };
        let expected = "2025-01-02"
            .parse::<jiff::civil::Date>()
            .unwrap()
            .to_zoned(TimeZone::system())
            .unwrap()
            .timestamp();
        assert_eq!(actual, expected);
    }
    #[test]
    fn invalid_policy_is_an_error_not_a_disabled_cutoff() {
        for setting in [
            "exclude-newer = true",
            "exclude-newer = 7",
            "exclude-newer = '2025-99-99'",
            "exclude-newer = '1 month'",
            "exclude-newer = 'P1Y'",
            "exclude-newer-package = {demo = true}",
            "exclude-newer-package = { Demo_Pkg = false, demo-pkg = false }",
        ] {
            assert!(
                UvPolicy::from_document(
                    &format!("[tool.uv]\n{setting}").parse().unwrap(),
                    Utc::now()
                )
                .is_err(),
                "{setting}"
            );
        }
    }
    #[test]
    fn registry_identity_drops_authentication() {
        assert_eq!(
            index_identity("https://user:secret@example.com/simple/?token=secret"),
            "https://example.com"
        );
    }
}
