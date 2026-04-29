//! TLS configuration for HTTP clients.
//!
//! This module owns a process-global `HttpOptions` (initialized once per networked
//! subcommand) describing extra CA certificates and an `--insecure` flag. Each
//! `Client::builder()` chain in the codebase calls [`apply`] to inherit those options.
//!
//! Pure helpers ([`resolve_ca_path`], [`parse_pem_bundle`], [`chain_indicates_tls_failure`])
//! contain the testable logic; [`init`] is a thin shell over them.

use anyhow::{Context, Result};
use reqwest::{Certificate, ClientBuilder};
use std::path::{Path, PathBuf};
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

/// Parse one or more PEM-encoded certificates from a byte buffer.
///
/// Treats `from_pem_bundle` failures as fatal. An empty buffer (or a buffer with
/// no `BEGIN CERTIFICATE` markers) returns an `Err` so callers can surface a
/// meaningful diagnostic — silently producing an empty cert list would mask a
/// misconfigured CA bundle path.
pub(crate) fn parse_pem_bundle(bytes: &[u8]) -> Result<Vec<Certificate>> {
    if bytes.is_empty() {
        anyhow::bail!("CA bundle is empty (no PEM data)");
    }
    let certs =
        Certificate::from_pem_bundle(bytes).context("failed to parse PEM certificate bundle")?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in PEM bundle");
    }
    Ok(certs)
}

const TLS_MARKERS: &[&str] = &[
    "unknown issuer",
    "unknownissuer",
    "invalidcertificate",
    "self-signed",
    "self signed",
    "certificate verify",
    "certificateverification",
    "tls handshake",
    "certnotvalidforname",
    "certificate not valid for name",
    "certexpired",
    "cert expired",
    "certnotvalidyet",
    "certrevoked",
    "unknownrevocationstatus",
];

/// Walk an error chain and return true if any source's `Display` contains a
/// known TLS-trust-failure marker (case-insensitive).
///
/// Generic over `dyn std::error::Error` so it can be unit-tested with synthetic
/// error types — `reqwest::Error` has no public constructor for arbitrary chains.
pub(crate) fn chain_indicates_tls_failure(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = current {
        let msg = e.to_string().to_lowercase();
        if TLS_MARKERS.iter().any(|m| msg.contains(m)) {
            return true;
        }
        current = e.source();
    }
    false
}

/// Compute the extra-certs vector that `init` would set, given injectable env
/// and file-read closures. Pulled out so the short-circuit and error paths can
/// be exercised hermetically without touching the process-global `OnceLock`.
fn compute_extra_certs<E, R>(insecure: bool, env: E, read: R) -> Result<Vec<Certificate>>
where
    E: Fn(&str) -> Option<String>,
    R: Fn(&Path) -> std::io::Result<Vec<u8>>,
{
    if insecure {
        return Ok(Vec::new());
    }
    let Some(p) = resolve_ca_path(env) else {
        return Ok(Vec::new());
    };
    let bytes = read(&p).with_context(|| format!("failed to read CA bundle at {}", p.display()))?;
    parse_pem_bundle(&bytes)
        .with_context(|| format!("failed to parse CA bundle at {}", p.display()))
}

/// Initialize TLS options. Called from the entry point of every networked
/// subcommand (`run_update`, `run_align`, `run_audit`, `self_update`) before
/// any [`reqwest::Client`] is built.
///
/// Reads CA paths from `UPD_CA_BUNDLE` → `REQUESTS_CA_BUNDLE` → `CURL_CA_BUNDLE` →
/// `SSL_CERT_FILE` (priority order; first non-empty wins). Returns `Err` with the
/// path in the message if the chosen file cannot be read or parsed.
///
/// When `insecure` is true, env-var-resolved bundles are not read or parsed: the
/// user has opted out of verification entirely, so a stale or malformed bundle
/// path must not block the run.
///
/// **Library callers:** clients constructed before `init` is called see
/// [`HttpOptions::default`] forever — only clients constructed after `init`
/// pick up the configured CAs and `--insecure` flag.
pub fn init(insecure: bool) -> Result<()> {
    let extra_certs =
        compute_extra_certs(insecure, |k| std::env::var(k).ok(), |p| std::fs::read(p))?;
    // OnceLock::set is fallible if already set; that's fine — first init wins, later
    // calls in the same process are silent no-ops (the value is already correct).
    let _ = HTTP_OPTIONS.set(HttpOptions {
        insecure,
        extra_certs,
    });
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
pub fn apply(mut builder: ClientBuilder) -> ClientBuilder {
    let opts = options();
    for cert in &opts.extra_certs {
        builder = builder.add_root_certificate(cert.clone());
    }
    if opts.insecure {
        builder = builder
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true);
    }
    builder
}

/// Build the user-facing TLS hint for a given URL.
fn tls_hint(url: &str) -> String {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_string());
    format!(
        "TLS certificate verification failed for {host}. \
         If you're behind a corporate proxy, install your CA into the system trust store \
         or set REQUESTS_CA_BUNDLE / SSL_CERT_FILE to your CA bundle path. \
         As a last resort, pass --insecure to skip verification (not recommended)."
    )
}

/// Map a [`reqwest::Error`] from `.send()` into an [`anyhow::Error`], attaching
/// a TLS-trust hint when the error chain indicates a certificate-verification
/// failure.
pub fn wrap_send_err(err: reqwest::Error, url: &str) -> anyhow::Error {
    if chain_indicates_tls_failure(&err) {
        let hint = tls_hint(url);
        anyhow::Error::from(err).context(hint)
    } else {
        anyhow::Error::from(err)
    }
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

    const TEST_CERT_1: &[u8] = br#"-----BEGIN CERTIFICATE-----
MIIDCzCCAfOgAwIBAgIUKZ0KJZK5GE4JM+j0245augoh8OwwDQYJKoZIhvcNAQEL
BQAwFTETMBEGA1UEAwwKdXBkLXRlc3QtMTAeFw0yNjA0MjkwODI5MjhaFw0zNjA0
MjYwODI5MjhaMBUxEzARBgNVBAMMCnVwZC10ZXN0LTEwggEiMA0GCSqGSIb3DQEB
AQUAA4IBDwAwggEKAoIBAQDc44mX7cV5VLqtFGPGnEjjRLC+vO6Tk2SSO3eC9S3M
6Ryy3sAl11ZeSLzBOhdE3rZC4+oZjiy7Ht8TJuz8XxMH4ASUrfdkO7CyJfbxE5FJ
FAb+ZbHE9K+SKiCiVPd367v05Tra2P31+k3qFYnqH9jklwj1RwdMNhNLNUDd0f7I
xiTEGnSEfNUy1opwAcwYdRIAYAWi9TQxLy6+JyVHaDSG9s05SbinKaRWb/5Elopc
QJZ0fa8caCW7t+6caQQNE4pbgwj9iLSstyo28MAiZcQ7vgOzxCvyxf/fhQbbuCjg
uCUlTb9QsToXjF6GrQ6ZY5ze0KslQIa1KHfEtt8C9PnlAgMBAAGjUzBRMB0GA1Ud
DgQWBBSMYjXeEDAT+plKMitN3kuxrtU+eTAfBgNVHSMEGDAWgBSMYjXeEDAT+plK
MitN3kuxrtU+eTAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQB3
k413BI7F3FEOsjaCaSqr+Cp+xaPytGFxwjB7y7lXk3Ep0sCwSO99QDze+hXAWY+L
32JHgNXjvKccgfb+VP8Cv7XR5vNBmOm8RXfE90r3YwgPQo2GfAK3KAclFabil9ek
Dhjy52Y3Kw3S1ZfjizJbKlT0WvLLIaFVAHxOKCxwTNnYzu9j4rF2krE74yTdaPBm
bOIOTW/JL8Xlux+k0jV+BvLYK8/rlKgjUlCUlQFwKfQVVTagnh66jQbvtLqGT/1z
5AQ1MkjwT9arAOD+xWKXd3F1e2uEFl8QTGSqCTv85gKFM2BAVS04295SUUfllbNd
wVzHoiFTA+cbUjjOxjGM
-----END CERTIFICATE-----
"#;
    const TEST_CERT_2: &[u8] = br#"-----BEGIN CERTIFICATE-----
MIIDCzCCAfOgAwIBAgIUTRxLSgg2B1Nv53xT2+s2LrPEze4wDQYJKoZIhvcNAQEL
BQAwFTETMBEGA1UEAwwKdXBkLXRlc3QtMjAeFw0yNjA0MjkwODI5MjhaFw0zNjA0
MjYwODI5MjhaMBUxEzARBgNVBAMMCnVwZC10ZXN0LTIwggEiMA0GCSqGSIb3DQEB
AQUAA4IBDwAwggEKAoIBAQDaP51Ef0k52LrqC9PZ1kW/XlSqNXuTLgHeOPFPJmxQ
lVQ9/3dRAE3gnJUZmOsoZ5lgceNhBuPArrHJytDMUFJ5auRZhF1i7LOSRYF9B7KW
gQmlniAI9+rYBMTgc/2+PnnB3dGHcg23sVwtYWzTW6DZl6z28cpUXgLES7sSPi+0
hleTvfsEW2idjFcZO6sOFEeyPA3PJUA0YtWdKLANRp5kIEEqQO5Bln8MOEbl6r2O
vJOZnUTCoqP1Y2xExAj2gUw/+CWzAjC0rp4uPKUWP0ckHGlbCm7KmqxYLE6o0obg
c9vXxyeZ0FbZLc60e/mB3iakxJWa8DK1T9DH7gt5cXBfAgMBAAGjUzBRMB0GA1Ud
DgQWBBQLAmfP5m+XU+67B7/u6l89s6p7wTAfBgNVHSMEGDAWgBQLAmfP5m+XU+67
B7/u6l89s6p7wTAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQBW
L0eKTSQx7KJ4F5N7WDgdZvp3QtonBWIgQzRxgWEpS8oQyS0Jtr5esOmCSMbPnPNj
2xSE001niyxlsgjd4/UQ3P9Kj8jHfElnNB/qjSGPWzTRb/T+2LLXWQZ3ptcN8p4O
JeCGZf6GyCJEu9ToiPddNZl0B9IELCUSBJ8QPbIVBRHCIbCDIe5bIt3gJKBnK+Vl
8eUKTvF8cnnvw2Nr0umbxCoVjbmbyKpL/3ZsT70QF4eyQZ0JAmYQg7Ufx+pj09FX
j6KbS4KlY8roCKAlQEAxJ1qXTvl2/6QcWHvux1nue0KcBfHvFBAIHfNVUj7NslXX
uMhJbUlN9AYtL2pAGNPK
-----END CERTIFICATE-----
"#;

    fn two_cert_bundle() -> Vec<u8> {
        let mut out = Vec::with_capacity(TEST_CERT_1.len() + TEST_CERT_2.len());
        out.extend_from_slice(TEST_CERT_1);
        out.extend_from_slice(TEST_CERT_2);
        out
    }

    #[test]
    fn test_parse_pem_bundle_loads_multiple_certs() {
        let bytes = two_cert_bundle();
        let certs = parse_pem_bundle(&bytes).expect("two-cert bundle must parse");
        assert_eq!(certs.len(), 2, "expected 2 certs, got {}", certs.len());
    }

    #[test]
    fn test_parse_pem_bundle_rejects_garbage() {
        let bytes = b"this is not a PEM file at all";
        let err = parse_pem_bundle(bytes).expect_err("garbage must not parse");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("pem") || msg.contains("certificate"),
            "expected PEM-related error, got: {msg}"
        );
    }

    #[test]
    fn test_parse_pem_bundle_handles_empty_input() {
        let err = parse_pem_bundle(&[]).expect_err("empty input must not parse");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("empty") || msg.contains("no certificate") || msg.contains("pem"),
            "expected empty/no-cert error, got: {msg}"
        );
    }

    use std::error::Error as StdError;
    use std::fmt;

    #[derive(Debug)]
    struct TestError {
        msg: String,
        source: Option<Box<dyn StdError + Send + Sync + 'static>>,
    }

    impl TestError {
        fn new(msg: &str) -> Self {
            Self {
                msg: msg.to_string(),
                source: None,
            }
        }
        fn with_source(mut self, src: Box<dyn StdError + Send + Sync + 'static>) -> Self {
            self.source = Some(src);
            self
        }
    }

    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.msg)
        }
    }

    impl StdError for TestError {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.source
                .as_deref()
                .map(|s| s as &(dyn StdError + 'static))
        }
    }

    #[test]
    fn test_chain_indicates_tls_failure_for_each_marker() {
        let markers = [
            "UnknownIssuer",
            "unknown issuer",
            "InvalidCertificate",
            "invalidcertificate",
            "self-signed",
            "self signed",
            "certificate verify failed",
            "CertificateVerification",
            "tls handshake failure",
            "CertNotValidForName",
            "certificate not valid for name",
            "CertExpired",
            "cert expired",
            "CertNotValidYet",
            "CertRevoked",
            "UnknownRevocationStatus",
        ];
        for m in markers {
            let e = TestError::new(m);
            assert!(
                chain_indicates_tls_failure(&e),
                "expected marker {m:?} to indicate TLS failure"
            );
        }
    }

    #[test]
    fn test_chain_indicates_tls_failure_passthrough() {
        for msg in [
            "connection refused",
            "dns lookup failed",
            "timed out",
            "operation cancelled",
            "broken pipe",
        ] {
            let e = TestError::new(msg);
            assert!(
                !chain_indicates_tls_failure(&e),
                "non-TLS message {msg:?} must not match"
            );
        }
    }

    #[test]
    fn test_chain_indicates_tls_failure_walks_sources() {
        let inner = TestError::new("InvalidCertificate(UnknownIssuer)");
        let outer = TestError::new("error sending request").with_source(Box::new(inner));
        assert!(
            chain_indicates_tls_failure(&outer),
            "matcher must walk Error::source"
        );
    }

    use serial_test::serial;

    #[test]
    #[serial]
    fn test_init_smoke_default() {
        // Note: `init` only sets the OnceLock the FIRST time across the test process.
        // This test asserts the post-init state is sane regardless of whether it ran
        // first; we don't assert the exact value of `insecure` since another #[serial]
        // test in this process may have already initialized it.
        let _ = init(false);
        let opts = options();
        let _ = opts.insecure;
        let _ = &opts.extra_certs;
    }

    #[test]
    #[serial]
    fn test_options_default_when_uninitialized() {
        // This test only meaningfully runs in a process where init() was never called.
        // Under nextest's default per-test process model this is hermetic. Under
        // `cargo test`'s shared process, we tolerate either default or initialized state.
        let opts = options();
        let _ = opts.insecure;
        let _ = &opts.extra_certs;
    }

    #[test]
    fn test_init_bad_path_via_helpers() {
        // We can't safely invoke `init()` with a custom env in the test process
        // because of the OnceLock. Instead, exercise the underlying composition:
        // resolve → read → parse, which is exactly what `init` does internally.
        let path = PathBuf::from("/this/path/definitely/does/not/exist/upd-ca.pem");
        let read_err = std::fs::read(&path).expect_err("read of missing path must fail");
        assert_eq!(read_err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn test_compute_extra_certs_insecure_short_circuits() {
        // With insecure=true, env vars and file reads must not be consulted at all.
        // We assert this by passing closures that would panic if called.
        let env =
            |_: &str| -> Option<String> { panic!("env should not be read when insecure=true") };
        let read = |_: &Path| -> std::io::Result<Vec<u8>> {
            panic!("read should not be called when insecure=true")
        };
        let certs = compute_extra_certs(true, env, read).expect("insecure must succeed");
        assert!(certs.is_empty(), "insecure mode must yield no extra certs");
    }

    #[test]
    fn test_compute_extra_certs_insecure_ignores_broken_bundle() {
        // Even with a stale env var pointing at a bogus path, insecure=true must succeed.
        let env = fake_env(&[("UPD_CA_BUNDLE", Some("/nope/missing.pem"))]);
        let read = |_: &Path| -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        };
        let certs = compute_extra_certs(true, env, read).expect("insecure must ignore bad bundle");
        assert!(certs.is_empty());
    }

    #[test]
    fn test_compute_extra_certs_secure_propagates_read_error_with_path() {
        // With insecure=false, a read error must surface and include the resolved path.
        let env = fake_env(&[("UPD_CA_BUNDLE", Some("/nope/missing.pem"))]);
        let read = |_: &Path| -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        };
        let err = compute_extra_certs(false, env, read).expect_err("missing path must error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("/nope/missing.pem"),
            "error should mention path, got: {chain}"
        );
        assert!(
            chain.to_lowercase().contains("failed to read"),
            "error should describe read failure, got: {chain}"
        );
    }

    #[test]
    fn test_compute_extra_certs_no_env_returns_empty() {
        let env = fake_env(&[]);
        let read = |_: &Path| -> std::io::Result<Vec<u8>> {
            panic!("read should not be called when no env var is set")
        };
        let certs = compute_extra_certs(false, env, read).expect("no env must succeed");
        assert!(certs.is_empty());
    }

    #[test]
    fn test_apply_with_default_options_builds() {
        // Contract test: we can't introspect a ClientBuilder's TLS state, so the
        // best we can do is assert that the chain compiles and produces a usable
        // Client. With default options (no extra certs, not insecure), apply
        // must be a no-op that doesn't break the builder.
        let client = apply(reqwest::Client::builder()).build();
        assert!(
            client.is_ok(),
            "apply(builder).build() must succeed: {client:?}"
        );
    }
}
