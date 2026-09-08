//! Docker CLI credential discovery. Secrets are deliberately neither Debug nor persisted.
use anyhow::{Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Mutex,
};

const HUB: &str = "https://index.docker.io/v1/";
const MAX_HELPER_OUTPUT: u64 = 1024 * 1024;

#[derive(Clone)]
pub(super) enum Credential {
    Basic(String, String),
    IdentityToken(String),
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    #[serde(default)]
    auths: BTreeMap<String, Auth>,
    #[serde(default)]
    creds_store: String,
    #[serde(default)]
    cred_helpers: BTreeMap<String, String>,
}

#[derive(Default, Deserialize)]
struct Auth {
    #[serde(default)]
    auth: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    identitytoken: String,
}

pub(super) struct Credentials {
    path: Option<PathBuf>,
    // Serialize discovery so concurrent images do not repeatedly prompt the keychain.
    cache: Mutex<BTreeMap<String, Result<Option<Credential>, String>>>,
}

fn config_path(docker_config: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    docker_config
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| home.map(|p| p.join(".docker")))
        .map(|p| p.join("config.json"))
}

fn server_key(server: &str) -> String {
    let host = server
        .strip_prefix("https://")
        .or_else(|| server.strip_prefix("http://"))
        .unwrap_or(server)
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match host.as_str() {
        "docker.io" | "index.docker.io" | "registry-1.docker.io" => HUB.to_string(),
        _ => host,
    }
}

fn entry<'a, T>(entries: &'a BTreeMap<String, T>, server: &str) -> Option<&'a T> {
    entries.get(server).or_else(|| {
        entries
            .iter()
            .find_map(|(key, value)| (server_key(key) == server).then_some(value))
    })
}

impl Credentials {
    pub(super) fn new() -> Self {
        Self::at(config_path(
            std::env::var_os("DOCKER_CONFIG").map(PathBuf::from),
            super::utils::home_dir(),
        ))
    }

    pub(super) fn at(path: Option<PathBuf>) -> Self {
        Self {
            path,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    pub(super) async fn get(&self, registry: &str) -> Result<Option<Credential>> {
        let server = server_key(registry);
        let mut cache = self.cache.lock().await;
        if !cache.contains_key(&server) {
            let result = self.read(&server).await.map_err(|e| e.to_string());
            cache.insert(server.clone(), result);
        }
        cache[&server].clone().map_err(|message| anyhow!(message))
    }

    async fn read(&self, server: &str) -> Result<Option<Credential>> {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                bail!("Cannot read Docker config.json; check DOCKER_CONFIG and file permissions")
            }
        };
        // Serde errors may quote input, including a secret of the wrong type.
        let config: Config = serde_json::from_slice(&bytes).map_err(|_| {
            anyhow!("Invalid Docker config.json; check its JSON and credential fields")
        })?;
        let helper = entry(&config.cred_helpers, server)
            .filter(|s| !s.is_empty())
            .unwrap_or(&config.creds_store);
        if !helper.is_empty() {
            // A selected helper owns this registry. Never fall back to stale inline auth.
            return helper_credentials(helper, server).await;
        }
        entry(&config.auths, server)
            .map(Auth::credential)
            .transpose()
            .map(Option::flatten)
    }
}

impl Auth {
    fn credential(&self) -> Result<Option<Credential>> {
        if !self.identitytoken.is_empty() {
            return Ok(Some(Credential::IdentityToken(self.identitytoken.clone())));
        }
        if !self.auth.is_empty() {
            let decoded = STANDARD
                .decode(&self.auth)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .ok_or_else(|| {
                    anyhow!("Invalid Docker auth entry: expected base64 username:password")
                })?;
            let (username, password) = decoded
                .split_once(':')
                .ok_or_else(|| anyhow!("Invalid Docker auth entry: expected username:password"))?;
            if username.is_empty() {
                bail!("Invalid Docker auth entry: empty username")
            }
            return Ok(Some(Credential::Basic(
                username.into(),
                password.trim_end_matches('\0').into(),
            )));
        }
        if !self.username.is_empty() {
            return Ok(Some(Credential::Basic(
                self.username.clone(),
                self.password.clone(),
            )));
        }
        Ok(None)
    }
}

async fn helper_credentials(helper: &str, server: &str) -> Result<Option<Credential>> {
    // A suffix, never a shell command or path supplied by a config file.
    if !helper
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("Invalid Docker credential helper name in config.json");
    }
    run_helper(
        Command::new(format!("docker-credential-{helper}")),
        server,
        Duration::from_secs(30),
    )
    .await
}

async fn run_helper(
    mut command: Command,
    server: &str,
    timeout: Duration,
) -> Result<Option<Credential>> {
    let mut child = command
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            anyhow!(
                "Cannot start configured Docker credential helper; ensure it is installed on PATH"
            )
        })?;
    let result = tokio::time::timeout(timeout, async {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(server.as_bytes()).await?;
        drop(stdin);
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .expect("piped stdout")
            .take(MAX_HELPER_OUTPUT + 1)
            .read_to_end(&mut output)
            .await?;
        if output.len() as u64 > MAX_HELPER_OUTPUT {
            return Err(std::io::Error::other("helper output too large"));
        }
        Ok::<_, std::io::Error>((child.wait().await?, output))
    })
    .await;
    let (status, output) = match result {
        Ok(Ok(result)) => result,
        other => {
            let _ = child.kill().await;
            if other.is_err() {
                bail!("Docker credential helper timed out")
            }
            bail!("Failed to read Docker credential helper response");
        }
    };
    if !status.success() {
        if std::str::from_utf8(&output).is_ok_and(|s| {
            s.trim_end_matches(['\r', '\n']) == "credentials not found in native keychain"
        }) {
            return Ok(None);
        }
        bail!(
            "Docker credential helper failed; check the helper and run docker login for this registry"
        );
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Response {
        username: String,
        secret: String,
    }
    let response: Response = serde_json::from_slice(&output)
        .map_err(|_| anyhow!("Docker credential helper returned invalid credentials"))?;
    if response.username.is_empty() || response.secret.is_empty() {
        bail!("Docker credential helper returned empty credentials");
    }
    Ok(Some(if response.username == "<token>" {
        Credential::IdentityToken(response.secret)
    } else {
        Credential::Basic(response.username, response.secret)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(value: serde_json::Value) -> (tempfile::TempDir, Credentials) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, value.to_string()).unwrap();
        (dir, Credentials::at(Some(path)))
    }

    fn basic(credential: Option<Credential>) -> (String, String) {
        match credential.unwrap() {
            Credential::Basic(u, p) => (u, p),
            _ => panic!("expected basic credentials"),
        }
    }

    #[test]
    fn config_directory_override_and_home_default() {
        assert_eq!(
            config_path(Some("custom".into()), Some("home".into())),
            Some(PathBuf::from("custom/config.json"))
        );
        assert_eq!(
            config_path(Some("".into()), Some("home".into())),
            Some(PathBuf::from("home/.docker/config.json"))
        );
        assert_eq!(config_path(None, None), None);
    }

    #[tokio::test]
    async fn inline_auth_matches_exact_registry_and_hub_aliases() {
        let (_dir, credentials) = config(json!({"auths": {
            "https://index.docker.io/v1/": {"auth": STANDARD.encode("user:pass:with:colons")},
            "https://registry.example:5000/v1/": {"username": "other", "password": "secret"}
        }}));
        for hub in ["docker.io", "index.docker.io", "registry-1.docker.io"] {
            assert_eq!(
                basic(credentials.get(hub).await.unwrap()),
                ("user".into(), "pass:with:colons".into())
            );
        }
        assert_eq!(
            basic(credentials.get("registry.example:5000").await.unwrap()),
            ("other".into(), "secret".into())
        );
        for host in [
            "registry.example",
            "registry.example:5001",
            "evilregistry.example:5000",
            "docker.io.attacker.example",
        ] {
            assert!(credentials.get(host).await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn identity_token_wins_over_inline_password() {
        let (_dir, credentials) = config(
            json!({"auths": {"example.com": {"identitytoken": "refresh", "auth": "invalid"}}}),
        );
        assert!(
            matches!(credentials.get("example.com").await.unwrap(), Some(Credential::IdentityToken(t)) if t == "refresh")
        );
    }

    #[tokio::test]
    async fn missing_credentials_are_anonymous_but_bad_config_is_an_error() {
        let (dir, credentials) = config(json!({"auths": {"example.com": {}}}));
        assert!(credentials.get("example.com").await.unwrap().is_none());
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"auths":{"example.com":{"auth":{"secret":"DO-NOT-PRINT"}}}}"#,
        )
        .unwrap();
        let error = match credentials.get("other.example").await {
            Err(e) => e,
            _ => panic!("expected error"),
        };
        assert!(!format!("{error:#}").contains("DO-NOT-PRINT"));
        let missing = Credentials::at(Some(dir.path().join("missing.json")));
        assert!(missing.get("example.com").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn selected_helper_never_falls_back_to_inline_credentials() {
        let (_dir, credentials) = config(json!({
            "credHelpers": {"example.com": "invalid/name"},
            "credsStore": "upd-test-nonexistent-helper",
            "auths": {"example.com": {"auth": STANDARD.encode("user:secret")}}
        }));
        let error = match credentials.get("example.com").await {
            Err(e) => e,
            _ => panic!("expected error"),
        };
        assert!(
            error
                .to_string()
                .contains("Invalid Docker credential helper name")
        );
        let error = match credentials.get("other.example").await {
            Err(e) => e,
            _ => panic!("expected error"),
        };
        assert!(
            error
                .to_string()
                .contains("Cannot start configured Docker credential helper")
        );
    }

    #[cfg(unix)]
    async fn script(body: &str, timeout: Duration) -> Result<Option<Credential>> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("helper.sh");
        std::fs::write(&path, body).unwrap();
        let mut command = Command::new("/bin/sh");
        command.arg(path);
        run_helper(command, HUB, timeout).await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_protocol_supports_passwords_and_identity_tokens() {
        let result = script(
            r#"
[ "$1" = get ] || exit 2
server=$(cat)
[ "$server" = https://index.docker.io/v1/ ] || exit 3
printf '%s' '{"Username":"user","Secret":"secret"}'
"#,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(basic(result), ("user".into(), "secret".into()));
        let result = script(
            r#"printf '%s' '{"Username":"<token>","Secret":"refresh"}'"#,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert!(matches!(result, Some(Credential::IdentityToken(t)) if t == "refresh"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_missing_failure_malformed_and_timeout_are_distinct_and_redacted() {
        let result = script(
            "echo 'credentials not found in native keychain'; exit 1",
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert!(result.is_none());
        for body in [
            "echo 'DO-NOT-PRINT'; echo 'DO-NOT-PRINT' >&2; exit 1",
            r#"echo '{"Username":{"secret":"DO-NOT-PRINT"},"Secret":"secret"}'"#,
        ] {
            let error = match script(body, Duration::from_secs(2)).await {
                Err(e) => e,
                _ => panic!("expected error"),
            };
            assert!(!format!("{error:#}").contains("DO-NOT-PRINT"));
        }
        let error = match script("exec sleep 10", Duration::from_millis(20)).await {
            Err(e) => e,
            _ => panic!("expected timeout"),
        };
        assert!(error.to_string().contains("timed out"));
    }
}
