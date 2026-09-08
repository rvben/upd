//! Gradle's documented version-part ordering (not SemVer ordering).
use std::cmp::Ordering;

fn parts(version: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    for part in version
        .split(['.', '-', '_', '+'])
        .filter(|p| !p.is_empty())
    {
        let mut start = 0;
        let mut numeric = part.as_bytes()[0].is_ascii_digit();
        for (i, c) in part.char_indices().skip(1) {
            if c.is_ascii_digit() != numeric {
                parts.push(&part[start..i]);
                start = i;
                numeric = c.is_ascii_digit();
            }
        }
        parts.push(&part[start..]);
    }
    parts
}

pub fn is_literal(version: &str) -> bool {
    version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
        && !version.ends_with(['.', '-', '_'])
        && !version.to_ascii_lowercase().contains("snapshot")
}

pub fn is_prerelease(version: &str) -> bool {
    parts(version).iter().any(|p| {
        matches!(
            p.to_ascii_lowercase().as_str(),
            "dev"
                | "alpha"
                | "a"
                | "beta"
                | "b"
                | "milestone"
                | "m"
                | "rc"
                | "cr"
                | "snapshot"
                | "eap"
                | "preview"
        )
    })
}

pub fn compare(a: &str, b: &str) -> Ordering {
    let a = parts(a);
    let b = parts(b);
    fn special(s: &str) -> i8 {
        match s.to_ascii_lowercase().as_str() {
            "dev" => -1,
            "rc" => 1,
            "snapshot" => 2,
            "final" => 3,
            "ga" => 4,
            "release" => 5,
            "sp" => 6,
            _ => 0,
        }
    }
    for i in 0..a.len().max(b.len()) {
        let order = match (a.get(i), b.get(i)) {
            (Some(a), Some(b)) => {
                let an = a.as_bytes()[0].is_ascii_digit();
                let bn = b.as_bytes()[0].is_ascii_digit();
                match (an, bn) {
                    (true, true) => {
                        let a = a.trim_start_matches('0');
                        let b = b.trim_start_matches('0');
                        a.len().cmp(&b.len()).then_with(|| a.cmp(b))
                    }
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => special(a).cmp(&special(b)).then_with(|| {
                        if special(a) != 0 {
                            Ordering::Equal
                        } else {
                            a.cmp(b)
                        }
                    }),
                }
            }
            (Some(a), None) => {
                if a.as_bytes()[0].is_ascii_digit() {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (None, Some(b)) => {
                if b.as_bytes()[0].is_ascii_digit() {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (None, None) => Ordering::Equal,
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gradle_ordering() {
        for (a, b) in [
            ("1.9", "1.10"),
            ("1.1", "1.1.0"),
            ("1.1.a", "1.1"),
            ("1.0-dev", "1.0-ALPHA"),
            ("1.0-zeta", "1.0-rc"),
            ("1.0-rc2", "1.0-rc10"),
            ("1.0-final", "1.0-ga"),
            ("1.0-release", "1.0-sp"),
        ] {
            assert_eq!(compare(a, b), Ordering::Less, "{a} < {b}");
        }
        assert_eq!(compare("1.0-RC-1", "1.0.rc.1"), Ordering::Equal);
        assert!(!is_prerelease("4.3.1.RELEASE"));
        assert!(is_prerelease("2.3.0-Beta2"));
        for v in [
            "1.+",
            "latest.release",
            "[1,2)",
            "1.0-SNAPSHOT",
            "${version}",
        ] {
            assert!(!is_literal(v));
        }
    }
}
