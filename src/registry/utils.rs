//! Shared utilities for registry authentication

use std::io::BufRead;
use std::path::PathBuf;

/// Maximum size for credential files (10 MB) to prevent DoS
const MAX_CREDENTIAL_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Get the user's home directory
pub fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("USERPROFILE").ok().map(PathBuf::from))
}

/// Simple base64 encoding without external dependency
pub fn base64_encode(input: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut result = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let mut n: u32 = 0;
        for (i, &byte) in chunk.iter().enumerate() {
            n |= (byte as u32) << (16 - 8 * i);
        }

        let len = chunk.len();
        result.push(ALPHABET[(n >> 18) as usize & 0x3F] as char);
        result.push(ALPHABET[(n >> 12) as usize & 0x3F] as char);
        if len > 1 {
            result.push(ALPHABET[(n >> 6) as usize & 0x3F] as char);
        } else {
            result.push('=');
        }
        if len > 2 {
            result.push(ALPHABET[n as usize & 0x3F] as char);
        } else {
            result.push('=');
        }
    }

    result
}

/// Credentials parsed from a netrc file
#[derive(Debug, Clone)]
pub struct NetrcCredentials {
    pub login: String,
    pub password: String,
}

/// Get the path to the user's netrc file
pub fn get_netrc_path() -> Option<PathBuf> {
    // Check NETRC environment variable first (allows overriding default locations)
    if let Ok(path) = std::env::var("NETRC") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    // Try ~/.netrc (Unix-style)
    if let Some(home) = home_dir() {
        let netrc = home.join(".netrc");
        if netrc.exists() {
            return Some(netrc);
        }

        // Try _netrc for Windows compatibility
        let netrc_win = home.join("_netrc");
        if netrc_win.exists() {
            return Some(netrc_win);
        }
    }

    None
}

/// Read credentials from ~/.netrc for a given host
pub fn read_netrc_credentials(host: &str) -> Option<NetrcCredentials> {
    let netrc_path = get_netrc_path()?;
    read_netrc_credentials_from_path(&netrc_path, host)
}

/// Read credentials from a specific netrc file path for a given host
pub fn read_netrc_credentials_from_path(
    netrc_path: &PathBuf,
    host: &str,
) -> Option<NetrcCredentials> {
    // Check file size to prevent DoS
    if let Ok(metadata) = std::fs::metadata(netrc_path)
        && metadata.len() > MAX_CREDENTIAL_FILE_SIZE
    {
        return None;
    }

    let file = std::fs::File::open(netrc_path).ok()?;
    let reader = std::io::BufReader::new(file);

    let mut current_machine: Option<String> = None;
    let mut login: Option<String> = None;
    let mut password: Option<String> = None;

    for line in reader.lines().map_while(Result::ok) {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let mut i = 0;

        while i < tokens.len() {
            match tokens[i] {
                "machine" => {
                    // If we had a previous machine entry that matches, return it
                    if current_machine.as_ref().is_some_and(|m| m == host)
                        && let Some(l) = login.take()
                        && let Some(p) = password.take()
                    {
                        return Some(NetrcCredentials {
                            login: l,
                            password: p,
                        });
                    }

                    // Start a new machine entry
                    if i + 1 < tokens.len() {
                        current_machine = Some(tokens[i + 1].to_string());
                        login = None;
                        password = None;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "default" => {
                    // Default entry matches any host
                    if login.is_none() && password.is_none() {
                        current_machine = Some("*".to_string());
                    }
                    i += 1;
                }
                "login" => {
                    if i + 1 < tokens.len() {
                        login = Some(tokens[i + 1].to_string());
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                "password" => {
                    if i + 1 < tokens.len() {
                        password = Some(tokens[i + 1].to_string());
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                _ => {
                    i += 1;
                }
            }
        }
    }

    // Check the last machine entry
    if let Some(machine) = current_machine
        && (machine == host || machine == "*")
        && let Some(l) = login
        && let Some(p) = password
    {
        return Some(NetrcCredentials {
            login: l,
            password: p,
        });
    }

    None
}

/// Configuration values parsed from pip.conf/pip.ini
#[derive(Debug, Clone, Default)]
pub struct PipConfig {
    pub index_url: Option<String>,
    pub extra_index_urls: Vec<String>,
}

/// Get pip config file paths in order of precedence (highest first)
pub fn get_pip_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // 1. PIP_CONFIG_FILE environment variable (highest precedence)
    if let Ok(path) = std::env::var("PIP_CONFIG_FILE") {
        paths.push(PathBuf::from(path));
    }

    // 2. Virtual environment pip.conf
    if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
        paths.push(PathBuf::from(&venv).join("pip.conf"));
    }

    // 3. User config locations
    if let Some(home) = home_dir() {
        // XDG config (Linux/macOS)
        if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
            paths.push(PathBuf::from(xdg_config).join("pip").join("pip.conf"));
        } else {
            paths.push(home.join(".config").join("pip").join("pip.conf"));
        }

        // Legacy user config
        paths.push(home.join(".pip").join("pip.conf"));

        // Windows user config (APPDATA)
        if let Ok(appdata) = std::env::var("APPDATA") {
            paths.push(PathBuf::from(appdata).join("pip").join("pip.ini"));
        }
    }

    // 4. System-wide config
    #[cfg(unix)]
    paths.push(PathBuf::from("/etc/pip.conf"));

    #[cfg(windows)]
    if let Ok(program_data) = std::env::var("ProgramData") {
        paths.push(PathBuf::from(program_data).join("pip").join("pip.ini"));
    }

    paths
}

/// Read pip configuration from pip.conf/pip.ini files
/// Searches multiple locations and merges results (first found wins for index-url)
pub fn read_pip_config() -> PipConfig {
    let mut config = PipConfig::default();

    for path in get_pip_config_paths() {
        if let Some(parsed) = read_pip_config_from_path(&path) {
            // First index-url wins
            if config.index_url.is_none() && parsed.index_url.is_some() {
                config.index_url = parsed.index_url;
            }
            // Collect all extra-index-urls
            config.extra_index_urls.extend(parsed.extra_index_urls);
        }
    }

    config
}

/// Read pip configuration from a specific file path
pub fn read_pip_config_from_path(path: &PathBuf) -> Option<PipConfig> {
    // Check file size to prevent DoS
    if let Ok(metadata) = std::fs::metadata(path)
        && metadata.len() > MAX_CREDENTIAL_FILE_SIZE
    {
        return None;
    }

    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);

    let mut config = PipConfig::default();
    let mut in_global_section = false;

    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();

        // Skip comments and empty lines
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        // Check for section headers
        if line.starts_with('[') && line.ends_with(']') {
            let section = &line[1..line.len() - 1].to_lowercase();
            in_global_section = section == "global";
            continue;
        }

        // Parse key = value pairs in [global] section
        if in_global_section && let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_lowercase();
            let value = value.trim().to_string();

            match key.as_str() {
                "index-url" => {
                    if !value.is_empty() {
                        config.index_url = Some(value);
                    }
                }
                "extra-index-url" => {
                    // Can be space or newline separated in the config
                    for url in value.split_whitespace() {
                        if !url.is_empty() {
                            config.extra_index_urls.push(url.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if config.index_url.is_some() || !config.extra_index_urls.is_empty() {
        Some(config)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(""), "");
        assert_eq!(base64_encode("f"), "Zg==");
        assert_eq!(base64_encode("fo"), "Zm8=");
        assert_eq!(base64_encode("foo"), "Zm9v");
        assert_eq!(base64_encode("foob"), "Zm9vYg==");
        assert_eq!(base64_encode("fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode("foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode("user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64_encode("hello"), "aGVsbG8=");
    }

    #[test]
    fn test_read_netrc_credentials() {
        let mut netrc_file = NamedTempFile::new().unwrap();
        writeln!(
            netrc_file,
            "machine example.com login testuser password testpass"
        )
        .unwrap();

        let netrc_path = netrc_file.path().to_path_buf();

        let creds = read_netrc_credentials_from_path(&netrc_path, "example.com");
        assert!(creds.is_some());
        let creds = creds.unwrap();
        assert_eq!(creds.login, "testuser");
        assert_eq!(creds.password, "testpass");

        // Non-existent host
        let creds = read_netrc_credentials_from_path(&netrc_path, "nonexistent.com");
        assert!(creds.is_none());
    }

    #[test]
    fn test_read_netrc_multiline() {
        let mut netrc_file = NamedTempFile::new().unwrap();
        writeln!(netrc_file, "machine example.com").unwrap();
        writeln!(netrc_file, "  login testuser").unwrap();
        writeln!(netrc_file, "  password testpass").unwrap();

        let netrc_path = netrc_file.path().to_path_buf();

        let creds = read_netrc_credentials_from_path(&netrc_path, "example.com");
        assert!(creds.is_some());
        let creds = creds.unwrap();
        assert_eq!(creds.login, "testuser");
        assert_eq!(creds.password, "testpass");
    }

    #[test]
    fn test_read_netrc_default_entry() {
        let mut netrc_file = NamedTempFile::new().unwrap();
        writeln!(netrc_file, "default login defaultuser password defaultpass").unwrap();

        let netrc_path = netrc_file.path().to_path_buf();

        let creds = read_netrc_credentials_from_path(&netrc_path, "anyhost.com");
        assert!(creds.is_some());
        let creds = creds.unwrap();
        assert_eq!(creds.login, "defaultuser");
        assert_eq!(creds.password, "defaultpass");
    }

    #[test]
    fn test_read_pip_config_index_url() {
        let mut pip_conf = NamedTempFile::new().unwrap();
        writeln!(pip_conf, "[global]").unwrap();
        writeln!(pip_conf, "index-url = https://private.pypi.com/simple").unwrap();

        let path = pip_conf.path().to_path_buf();
        let config = read_pip_config_from_path(&path).unwrap();

        assert_eq!(
            config.index_url,
            Some("https://private.pypi.com/simple".to_string())
        );
        assert!(config.extra_index_urls.is_empty());
    }

    #[test]
    fn test_read_pip_config_extra_index_urls() {
        let mut pip_conf = NamedTempFile::new().unwrap();
        writeln!(pip_conf, "[global]").unwrap();
        writeln!(pip_conf, "index-url = https://private.pypi.com/simple").unwrap();
        writeln!(
            pip_conf,
            "extra-index-url = https://pypi.org/simple https://extra.pypi.com/simple"
        )
        .unwrap();

        let path = pip_conf.path().to_path_buf();
        let config = read_pip_config_from_path(&path).unwrap();

        assert_eq!(
            config.index_url,
            Some("https://private.pypi.com/simple".to_string())
        );
        assert_eq!(config.extra_index_urls.len(), 2);
        assert_eq!(config.extra_index_urls[0], "https://pypi.org/simple");
        assert_eq!(config.extra_index_urls[1], "https://extra.pypi.com/simple");
    }

    #[test]
    fn test_read_pip_config_with_comments() {
        let mut pip_conf = NamedTempFile::new().unwrap();
        writeln!(pip_conf, "# This is a comment").unwrap();
        writeln!(pip_conf, "[global]").unwrap();
        writeln!(pip_conf, "; Another comment").unwrap();
        writeln!(pip_conf, "index-url = https://private.pypi.com/simple").unwrap();

        let path = pip_conf.path().to_path_buf();
        let config = read_pip_config_from_path(&path).unwrap();

        assert_eq!(
            config.index_url,
            Some("https://private.pypi.com/simple".to_string())
        );
    }

    #[test]
    fn test_read_pip_config_ignores_other_sections() {
        let mut pip_conf = NamedTempFile::new().unwrap();
        writeln!(pip_conf, "[install]").unwrap();
        writeln!(pip_conf, "trusted-host = private.pypi.com").unwrap();
        writeln!(pip_conf, "[global]").unwrap();
        writeln!(pip_conf, "index-url = https://private.pypi.com/simple").unwrap();

        let path = pip_conf.path().to_path_buf();
        let config = read_pip_config_from_path(&path).unwrap();

        assert_eq!(
            config.index_url,
            Some("https://private.pypi.com/simple".to_string())
        );
    }

    #[test]
    fn test_read_pip_config_empty_file() {
        let pip_conf = NamedTempFile::new().unwrap();
        let path = pip_conf.path().to_path_buf();
        let config = read_pip_config_from_path(&path);

        assert!(config.is_none());
    }

    #[test]
    fn test_read_pip_config_no_global_section() {
        let mut pip_conf = NamedTempFile::new().unwrap();
        writeln!(pip_conf, "[install]").unwrap();
        writeln!(pip_conf, "trusted-host = private.pypi.com").unwrap();

        let path = pip_conf.path().to_path_buf();
        let config = read_pip_config_from_path(&path);

        assert!(config.is_none());
    }
}
