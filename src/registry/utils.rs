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
}
