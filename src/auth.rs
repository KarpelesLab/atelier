//! Sign in with ChatGPT (SIWC) — OAuth for running on the user's ChatGPT plan.
//!
//! OpenAI's sanctioned open-source flow: authorization-code + PKCE (S256), no
//! client secret. The user signs into their own OpenAI account in a browser;
//! we catch the callback on a loopback listener, exchange the code for tokens,
//! and store them in the user config dir. The access token is then used as a
//! bearer on Responses API requests (`provider::responses`). We never transmit
//! it anywhere but OpenAI.
//!
//! Endpoints (from developers.openai.com/siwc):
//! - authorize: `https://auth.openai.com/api/accounts/authorize`
//! - token:     `https://auth.openai.com/api/accounts/oauth/token`
//! - dynamic client bootstrap `client_id=dynamic_agent_client`; the server
//!   returns a permanent `oaiapp_…` client id in the callback, reused after.
//! - redirect: `http://127.0.0.1:<port>/callback` (loopback)
//! - scopes: `openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`
//! - token exchange posts `resource=https://api.openai.com/v1`.
//!
//! # Contract (stable — the implementer owns `src/auth.rs`)
//!
//! Fill in the flow below. Use `purecrypto::hash` for SHA-256 (PKCE challenge)
//! and a CSPRNG for the verifier/state/nonce; `rsurl` for the token POSTs
//! (form-encoded); hand-roll base64url. Tokens persist in [`config_path`].

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Stored OAuth credentials for the ChatGPT backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    /// The permanent client id issued on first auth (`oaiapp_…`).
    pub client_id: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Access-token expiry, epoch seconds.
    #[serde(default)]
    pub expires_at: u64,
    /// Space-separated granted scopes.
    #[serde(default)]
    pub scopes: String,
}

/// Path to the stored credentials in the user config dir
/// (`$XDG_CONFIG_HOME/atelier/chatgpt-auth.json`, else `$HOME/.config/...`).
pub fn config_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| std::path::PathBuf::from(".config"));
    base.join("atelier").join("chatgpt-auth.json")
}

/// Load stored credentials, if a prior `login` saved any.
pub fn load() -> Option<Tokens> {
    let text = std::fs::read_to_string(config_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Persist credentials (0600) to the user config dir.
#[allow(dead_code)] // used by login/refresh once implemented
pub fn save(_tokens: &Tokens) -> Result<()> {
    bail!("sign in with ChatGPT is not yet implemented")
}

/// Delete stored credentials.
pub fn logout() -> Result<()> {
    let _ = std::fs::remove_file(config_path());
    Ok(())
}

/// Run the full interactive login (open browser, loopback callback, exchange)
/// and return+persist the tokens.
#[allow(dead_code)] // body is a stub until implemented
pub fn login() -> Result<Tokens> {
    bail!("sign in with ChatGPT is not yet implemented")
}

/// Return a valid bearer access token, refreshing (and persisting) if expired.
#[allow(dead_code)] // body is a stub until implemented
pub fn access_token(_tokens: &mut Tokens) -> Result<String> {
    bail!("sign in with ChatGPT is not yet implemented")
}
