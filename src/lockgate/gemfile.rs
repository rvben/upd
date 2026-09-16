//! The rubygems.org gems a `Gemfile.lock` holds.

use super::LockEntry;

/// Every `(name, version)` a `GEM` section locks, in lockfile order without
/// repeats, split into those from rubygems.org and those from any other gem
/// server, each of the latter with its section's remotes. A platform gem
/// (`nokogiri (1.18.0-x86_64-linux)`) is the release its version names, so
/// the platform suffix is dropped. `GIT` and `PATH` sections are neither. A
/// section listing several remotes does not say which one served each gem,
/// so its gems count as from rubygems.org only when every remote is.
pub(crate) fn gem_entries(content: &str) -> (Vec<LockEntry>, Vec<(LockEntry, Vec<String>)>) {
    let mut rubygems: Vec<LockEntry> = Vec::new();
    let mut other: Vec<(LockEntry, Vec<String>)> = Vec::new();
    let mut in_gem = false;
    let mut remotes: Vec<String> = Vec::new();
    for line in content.lines() {
        if !line.starts_with(' ') {
            in_gem = line.trim_end() == "GEM";
            remotes.clear();
            continue;
        }
        if !in_gem {
            continue;
        }
        if let Some(remote) = line.strip_prefix("  remote: ") {
            remotes.push(remote.trim().to_string());
            continue;
        }
        let from_rubygems = !remotes.is_empty() && remotes.iter().all(|r| is_rubygems(r));
        // Specs sit at four spaces; their dependencies at six.
        let Some(spec) = line.strip_prefix("    ") else {
            continue;
        };
        if spec.starts_with(' ') {
            continue;
        }
        let Some((name, rest)) = spec.split_once(" (") else {
            continue;
        };
        let Some(version) = rest.strip_suffix(')') else {
            continue;
        };
        let version = version.split('-').next().unwrap_or(version);
        let entry = (name.to_string(), version.to_string());
        if from_rubygems {
            if !rubygems.contains(&entry) {
                rubygems.push(entry);
            }
        } else {
            let entry = (entry, remotes.clone());
            if !other.contains(&entry) {
                other.push(entry);
            }
        }
    }
    (rubygems, other)
}

fn is_rubygems(remote: &str) -> bool {
    let host = remote
        .split_once("://")
        .map_or(remote, |(_, rest)| rest)
        .split(['/', ':'])
        .next()
        .unwrap_or("");
    host.eq_ignore_ascii_case("rubygems.org")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = "GIT
  remote: https://github.com/example/gitgem.git
  revision: abc
  specs:
    gitgem (0.1.0)

GEM
  remote: https://rubygems.org/
  specs:
    nokogiri (1.18.0-arm64-darwin)
      racc (~> 1.4)
    nokogiri (1.18.0-x86_64-linux)
      racc (~> 1.4)
    racc (1.8.1)

GEM
  remote: https://gems.example.com/
  specs:
    private (2.0.0)

PLATFORMS
  arm64-darwin

DEPENDENCIES
  nokogiri (~> 1.18)

BUNDLED WITH
   2.6.2
";

    #[test]
    fn gem_specs_are_split_by_server_with_platforms_dropped() {
        let (rubygems, other) = gem_entries(LOCK);
        assert_eq!(
            rubygems,
            [
                ("nokogiri".to_string(), "1.18.0".to_string()),
                ("racc".to_string(), "1.8.1".to_string()),
            ]
        );
        assert_eq!(
            other,
            [(
                ("private".to_string(), "2.0.0".to_string()),
                vec!["https://gems.example.com/".to_string()]
            )],
            "the git gem is neither"
        );
    }

    #[test]
    fn a_section_with_several_remotes_is_from_rubygems_only_when_all_are() {
        let lock = "GEM
  remote: https://rubygems.org/
  remote: https://gems.example.com/
  specs:
    mixed (1.0.0)

GEM
  remote: https://gems.example.com/
  remote: https://rubygems.org/
  specs:
    reversed (1.0.0)

GEM
  remote: https://rubygems.org/
  remote: https://RubyGems.org
  specs:
    public (1.0.0)
";
        let (rubygems, other) = gem_entries(lock);
        assert_eq!(rubygems, [("public".to_string(), "1.0.0".to_string())]);
        let remotes = |remotes: &[&str]| remotes.iter().map(|r| r.to_string()).collect();
        assert_eq!(
            other,
            [
                (
                    ("mixed".to_string(), "1.0.0".to_string()),
                    remotes(&["https://rubygems.org/", "https://gems.example.com/"])
                ),
                (
                    ("reversed".to_string(), "1.0.0".to_string()),
                    remotes(&["https://gems.example.com/", "https://rubygems.org/"])
                ),
            ]
        );
    }

    #[test]
    fn the_remote_host_must_be_rubygems_itself() {
        assert!(is_rubygems("https://rubygems.org/"));
        assert!(is_rubygems("https://RubyGems.org"));
        assert!(!is_rubygems("https://rubygems.org.example.com/"));
        assert!(!is_rubygems("https://example.com/rubygems.org/"));
    }
}
