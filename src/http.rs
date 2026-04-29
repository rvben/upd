//! TLS configuration for HTTP clients.
//!
//! This module owns a process-global `HttpOptions` (initialized once per networked
//! subcommand) describing extra CA certificates and an `--insecure` flag. Each
//! `Client::builder()` chain in the codebase calls [`apply`] to inherit those options.
//!
//! Pure helpers ([`resolve_ca_path`], [`parse_pem_bundle`], [`chain_indicates_tls_failure`])
//! contain the testable logic; [`init`] is a thin shell over them.

use anyhow::Result;
use reqwest::{Certificate, ClientBuilder};
use std::path::PathBuf;
use std::sync::OnceLock;

const CA_BUNDLE_ENV_VARS: &[&str] = &[
    "UPD_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "SSL_CERT_FILE",
];

#[derive(Debug, Default)]
pub struct HttpOptions {
    pub insecure: bool,
    /// PEM-parsed certificates loaded once at startup from env-var-resolved paths.
    pub extra_certs: Vec<Certificate>,
}

static HTTP_OPTIONS: OnceLock<HttpOptions> = OnceLock::new();
static DEFAULT_OPTIONS: OnceLock<HttpOptions> = OnceLock::new();

/// Resolve which CA-bundle env var holds the path we should load.
///
/// Returns the first non-empty value among `UPD_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`,
/// `CURL_CA_BUNDLE`, `SSL_CERT_FILE` (in that order), or `None` when none are set.
/// Empty strings are treated as unset.
pub(crate) fn resolve_ca_path<E>(env: E) -> Option<PathBuf>
where
    E: Fn(&str) -> Option<String>,
{
    CA_BUNDLE_ENV_VARS
        .iter()
        .filter_map(|name| env(name))
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Initialize TLS options. Called from the entry point of every networked
/// subcommand (`run_update`, `run_align`, `run_audit`, `self_update`) before
/// any [`reqwest::Client`] is built.
///
/// Reads CA paths from `UPD_CA_BUNDLE` → `REQUESTS_CA_BUNDLE` → `CURL_CA_BUNDLE` →
/// `SSL_CERT_FILE` (priority order; first non-empty wins). Returns `Err` with the
/// path in the message if the chosen file cannot be read or parsed.
///
/// **Library callers:** clients constructed before `init` is called see
/// [`HttpOptions::default`] forever — only clients constructed after `init`
/// pick up the configured CAs and `--insecure` flag.
pub fn init(insecure: bool) -> Result<()> {
    // Implemented in Task 5.
    let _ = insecure;
    let _ = resolve_ca_path(|k| std::env::var(k).ok());
    Ok(())
}

/// Returns the initialized options, or [`HttpOptions::default`] if `init` was
/// never called (the contract used by library tests).
pub fn options() -> &'static HttpOptions {
    HTTP_OPTIONS
        .get()
        .unwrap_or_else(|| DEFAULT_OPTIONS.get_or_init(HttpOptions::default))
}

/// Apply the configured TLS options to a [`ClientBuilder`].
pub fn apply(builder: ClientBuilder) -> ClientBuilder {
    // Implemented in Task 6.
    builder
}

/// Map a [`reqwest::Error`] from `.send()` into an [`anyhow::Error`], attaching
/// a TLS-trust hint when the error chain indicates a certificate-verification
/// failure.
pub fn wrap_send_err(err: reqwest::Error, url: &str) -> anyhow::Error {
    // Implemented in Task 7.
    let _ = url;
    anyhow::Error::from(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_env<'a>(map: &'a [(&'a str, Option<&'a str>)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| {
            map.iter()
                .find(|(name, _)| *name == k)
                .and_then(|(_, v)| v.map(str::to_owned))
        }
    }

    #[test]
    fn test_resolve_ca_path_priority_order() {
        // UPD_CA_BUNDLE wins
        let env = fake_env(&[
            ("UPD_CA_BUNDLE", Some("/upd")),
            ("REQUESTS_CA_BUNDLE", Some("/requests")),
            ("CURL_CA_BUNDLE", Some("/curl")),
            ("SSL_CERT_FILE", Some("/ssl")),
        ]);
        assert_eq!(resolve_ca_path(&env), Some(PathBuf::from("/upd")));

        // Without UPD_CA_BUNDLE, REQUESTS_CA_BUNDLE wins
        let env = fake_env(&[
            ("UPD_CA_BUNDLE", None),
            ("REQUESTS_CA_BUNDLE", Some("/requests")),
            ("CURL_CA_BUNDLE", Some("/curl")),
            ("SSL_CERT_FILE", Some("/ssl")),
        ]);
        assert_eq!(resolve_ca_path(&env), Some(PathBuf::from("/requests")));

        // Down to SSL_CERT_FILE
        let env = fake_env(&[("SSL_CERT_FILE", Some("/ssl"))]);
        assert_eq!(resolve_ca_path(&env), Some(PathBuf::from("/ssl")));
    }

    #[test]
    fn test_resolve_ca_path_skips_empty_strings() {
        let env = fake_env(&[
            ("UPD_CA_BUNDLE", Some("")),
            ("REQUESTS_CA_BUNDLE", Some("/requests")),
        ]);
        assert_eq!(resolve_ca_path(&env), Some(PathBuf::from("/requests")));
    }

    #[test]
    fn test_resolve_ca_path_returns_none_when_all_unset() {
        let env = fake_env(&[]);
        assert_eq!(resolve_ca_path(&env), None);
    }
}
