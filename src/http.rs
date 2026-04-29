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
use std::sync::OnceLock;

#[derive(Debug, Default)]
pub struct HttpOptions {
    pub insecure: bool,
    /// PEM-parsed certificates loaded once at startup from env-var-resolved paths.
    pub extra_certs: Vec<Certificate>,
}

static HTTP_OPTIONS: OnceLock<HttpOptions> = OnceLock::new();
static DEFAULT_OPTIONS: OnceLock<HttpOptions> = OnceLock::new();

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
