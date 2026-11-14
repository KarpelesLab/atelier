//! Where a request goes: a named [`Provider`] (an OpenAI-compatible endpoint
//! with its key source, headers and timeout) and a [`Target`] on it (the
//! model id plus the request options of the variant it was addressed with).
//!
//! # Keys
//!
//! A provider never holds a key from `atelier.toml` — that file is committed.
//! It holds *where the key comes from*: nowhere, the `ATELIER_API_KEY` the
//! process was started with (the implicit `env` provider), an environment
//! variable read at each request, or a command whose standard output is the
//! key. A command is run once and its answer kept for the process; when the
//! endpoint answers 401 the answer is dropped and the command run again
//! ([`Provider::refresh_key`]), which is what makes it usable for
//! credentials that rotate.
//!
//! The key is sent as `Authorization: Bearer` and nowhere else: `Debug`
//! prints the source, not the value, and the request code blanks it out of
//! an error body should a server echo it back.

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::settings::ProviderConfig;

/// The name of the provider the environment describes
/// (`ATELIER_BASE_URL`, `ATELIER_API_KEY`).
pub const ENV_PROVIDER: &str = "env";

/// The name of the provider that is the local model server.
pub const LOCAL_PROVIDER: &str = "local";
/// How long the local server may stay silent on a request.
const LOCAL_TIMEOUT_MS: u64 = 3_600_000;

/// A named OpenAI-compatible endpoint.
#[derive(Debug)]
pub struct Provider {
    pub name: String,
    /// Base URL including the `/v1` suffix; endpoints are appended to it.
    pub base_url: String,
    key: KeySource,
    /// Extra headers sent with every request.
    pub headers: Vec<(String, String)>,
    /// Connect timeout in milliseconds, when the provider sets one.
    pub timeout_ms: Option<u64>,
}

/// Where a provider's bearer token comes from.
enum KeySource {
    /// No authentication (many local servers).
    None,
    /// The value `ATELIER_API_KEY` had at startup.
    Fixed(String),
    /// The environment variable of this name, read at each request.
    Env(String),
    /// `sh -c command`'s standard output, kept once obtained.
    Command {
        command: String,
        cached: Mutex<Option<String>>,
    },
}

/// The source, never the value.
impl std::fmt::Debug for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

impl KeySource {
    fn describe(&self) -> String {
        match self {
            KeySource::None => "none".into(),
            KeySource::Fixed(_) => "ATELIER_API_KEY".into(),
            KeySource::Env(var) => {
                let state = match std::env::var(var) {
                    Ok(v) if !v.is_empty() => "set",
                    _ => "not set",
                };
                format!("${var} ({state})")
            }
            KeySource::Command { .. } => "from a command".into(),
        }
    }
}

impl Provider {
    /// The implicit provider: the endpoint and key the environment gives.
    pub fn from_env(base_url: String, api_key: Option<String>) -> Self {
        Self {
            name: ENV_PROVIDER.to_string(),
            base_url,
            key: api_key.map_or(KeySource::None, KeySource::Fixed),
            headers: Vec::new(),
            timeout_ms: None,
        }
    }

    /// The model server atelier runs on this machine (see [`crate::local`]).
    pub fn local(base_url: String, key: String) -> Self {
        Self {
            name: LOCAL_PROVIDER.to_string(),
            base_url,
            key: KeySource::Fixed(key),
            headers: Vec::new(),
            // The server answers once the prompt is read, and on a CPU a
            // long prompt takes minutes. Giving up sooner would send the
            // request again on top of the one still being worked on.
            timeout_ms: Some(LOCAL_TIMEOUT_MS),
        }
    }

    /// A `[[provider]]` table. An error names what is wrong with it.
    pub fn from_config(cfg: &ProviderConfig) -> Result<Self> {
        let name = cfg.name.trim();
        // The name is the part of `provider/model#variant` before the slash.
        if name.is_empty() || name.contains(['/', '#']) || name.contains(char::is_whitespace) {
            bail!(
                "provider name {:?} must be one word without '/' or '#'",
                cfg.name
            );
        }
        if cfg.base_url.trim().is_empty() {
            bail!("provider '{name}' has no base_url");
        }
        let key = match (&cfg.api_key_env, &cfg.api_key_command) {
            (Some(_), Some(_)) => {
                bail!("provider '{name}' sets both api_key_env and api_key_command; keep one")
            }
            (Some(var), None) => KeySource::Env(var.clone()),
            (None, Some(command)) => KeySource::Command {
                command: command.clone(),
                cached: Mutex::new(None),
            },
            (None, None) => KeySource::None,
        };
        Ok(Self {
            name: name.to_string(),
            base_url: cfg.base_url.trim().to_string(),
            key,
            headers: cfg
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            timeout_ms: cfg.timeout_secs.map(|s| s.saturating_mul(1000)),
        })
    }

    /// Full URL for an endpoint path like `"chat/completions"`.
    pub fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// The bearer token to send, if the provider has one. An error when it
    /// is configured but can't be had — sending the request without it
    /// would only trade a clear message for the server's 401.
    pub fn key(&self) -> Result<Option<String>> {
        match &self.key {
            KeySource::None => Ok(None),
            KeySource::Fixed(key) => Ok(Some(key.clone())),
            KeySource::Env(var) => match std::env::var(var) {
                Ok(key) if !key.trim().is_empty() => Ok(Some(key.trim().to_string())),
                _ => bail!(
                    "provider '{}': environment variable {var} (api_key_env) is not set",
                    self.name
                ),
            },
            KeySource::Command { command, cached } => {
                let mut cached = cached.lock().unwrap_or_else(|e| e.into_inner());
                if cached.is_none() {
                    *cached = Some(run_key_command(&self.name, command)?);
                }
                Ok(cached.clone())
            }
        }
    }

    /// Forget a key obtained from a command, after the endpoint refused it.
    /// Returns whether there was one to forget — that is, whether asking
    /// [`key`](Self::key) again can give a different answer.
    pub fn refresh_key(&self) -> bool {
        match &self.key {
            KeySource::Command { cached, .. } => cached
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .is_some(),
            _ => false,
        }
    }

    /// Where the key comes from, for `/providers` — never the key.
    pub fn key_description(&self) -> String {
        self.key.describe()
    }
}

/// Run a provider's key helper and take its trimmed standard output. The
/// error carries neither the output nor what the command wrote to stderr
/// (discarded): a helper that fails halfway may have printed a credential.
fn run_key_command(provider: &str, command: &str) -> Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("provider '{provider}': running api_key_command"))?;
    if !output.status.success() {
        bail!(
            "provider '{provider}': api_key_command failed ({})",
            output.status
        );
    }
    let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if key.is_empty() {
        bail!("provider '{provider}': api_key_command printed nothing");
    }
    Ok(key)
}

/// One model on one provider, with the request options of its variant: what
/// a request is sent to.
#[derive(Debug, Clone)]
pub struct Target {
    pub provider: Arc<Provider>,
    /// The model id as the endpoint knows it.
    pub model: String,
    /// Body fields added to every request (see [`crate::config`] on
    /// variants). Empty for a model addressed without a variant.
    pub options: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.into(),
            base_url: "http://127.0.0.1:1/v1/".into(),
            api_key_env: None,
            api_key_command: None,
            headers: Default::default(),
            timeout_secs: None,
        }
    }

    #[test]
    fn a_provider_table_is_checked() {
        let p = Provider::from_config(&config("local")).unwrap();
        assert_eq!(p.endpoint("/models"), "http://127.0.0.1:1/v1/models");
        assert_eq!(p.key().unwrap(), None);
        assert_eq!(p.key_description(), "none");

        assert!(Provider::from_config(&config("a/b")).is_err());
        assert!(Provider::from_config(&config("")).is_err());
        let mut both = config("x");
        both.api_key_env = Some("A".into());
        both.api_key_command = Some("echo a".into());
        let err = Provider::from_config(&both).unwrap_err().to_string();
        assert!(err.contains("both"), "{err}");
    }

    #[test]
    fn a_key_variable_that_is_missing_is_an_error_naming_it() {
        let mut cfg = config("x");
        cfg.api_key_env = Some("ATELIER_TEST_NO_SUCH_KEY_VAR".into());
        let p = Provider::from_config(&cfg).unwrap();
        let err = p.key().unwrap_err().to_string();
        assert!(err.contains("ATELIER_TEST_NO_SUCH_KEY_VAR"), "{err}");
        assert!(p.key_description().contains("not set"));
    }

    /// The command runs once, its answer is kept, and a refresh (after a
    /// 401) runs it again.
    #[test]
    fn a_key_command_is_cached_until_refreshed() {
        let counter = std::env::temp_dir().join(format!(
            "atelier-key-command-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&counter);
        let mut cfg = config("x");
        cfg.api_key_command = Some(format!(
            "echo run >> '{0}'; echo \" key-$(wc -l < '{0}' | tr -d ' ') \"",
            counter.display()
        ));
        let p = Provider::from_config(&cfg).unwrap();
        assert!(!p.refresh_key(), "nothing obtained yet");
        assert_eq!(p.key().unwrap().as_deref(), Some("key-1"));
        assert_eq!(p.key().unwrap().as_deref(), Some("key-1"));
        assert!(p.refresh_key());
        assert_eq!(p.key().unwrap().as_deref(), Some("key-2"));
        let _ = std::fs::remove_file(&counter);
    }

    #[test]
    fn a_failing_key_command_says_so_without_its_output() {
        let mut cfg = config("x");
        cfg.api_key_command = Some("echo half-a-secret; echo oops >&2; exit 3".into());
        let p = Provider::from_config(&cfg).unwrap();
        let err = format!("{:#}", p.key().unwrap_err());
        assert!(err.contains("api_key_command failed"), "{err}");
        assert!(
            !err.contains("half-a-secret") && !err.contains("oops"),
            "{err}"
        );
    }

    #[test]
    fn debug_output_names_the_source_not_the_key() {
        let p = Provider::from_env("http://x/v1".into(), Some("sk-very-secret".into()));
        let shown = format!("{p:?}");
        assert!(!shown.contains("sk-very-secret"), "{shown}");
        assert!(shown.contains("ATELIER_API_KEY"), "{shown}");
    }
}
