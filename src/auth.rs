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
//! The flow's mechanics — PKCE, the loopback listener, the token POST,
//! expiry, the private file — are [`crate::oauth`]'s, shared with sign-in to
//! remote MCP servers; this module is what is particular to OpenAI: the
//! endpoints above, the bootstrap client id, the required scope. Tokens
//! persist in [`config_path`].

use crate::oauth::{
    self, Loopback, TokenResponse, form_encode, is_expired, now_unix, percent_encode,
    pkce_challenge, random_b64url,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::time::Duration;

// ─── Protocol constants (see module docs; confirmed against the official docs) ──
const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
/// Bootstrap client id used until the server issues a permanent `oaiapp_…` one.
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const RESOURCE: &str = "https://api.openai.com/v1";
/// The ChatGPT-plan scope that must be granted for direct token use.
const REQUIRED_SCOPE: &str = "chatgpt.tokens.use.direct";
/// How long we wait for the browser to hit the loopback callback.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

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
pub fn save(tokens: &Tokens) -> Result<()> {
    let json = serde_json::to_string_pretty(tokens).context("serializing credentials")?;
    oauth::write_private(&config_path(), &json)
}

/// Delete stored credentials.
pub fn logout() -> Result<()> {
    let _ = std::fs::remove_file(config_path());
    Ok(())
}

/// Run the full interactive login (open browser, loopback callback, exchange)
/// and return+persist the tokens.
pub fn login() -> Result<Tokens> {
    // 1. PKCE + anti-forgery material, all from the OS CSPRNG.
    let code_verifier = random_b64url(32);
    let code_challenge = pkce_challenge(&code_verifier);
    let state = random_b64url(32);
    let nonce = random_b64url(32);

    // 2. Loopback listener on an ephemeral port.
    let loopback = Loopback::bind()?;
    let redirect_uri = loopback.redirect_uri.clone();

    // 3. Build the authorize URL and send the user to it.
    let params: [(&str, &str); 9] = [
        ("response_type", "code"),
        ("client_id", DYNAMIC_CLIENT_ID),
        ("redirect_uri", &redirect_uri),
        ("scope", SCOPES),
        ("code_challenge", &code_challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("nonce", &nonce),
        ("resource", RESOURCE),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let auth_url = format!("{AUTHORIZE_URL}?{query}");

    oauth::open_browser(&auth_url);
    eprintln!("If your browser didn't open, visit:\n{auth_url}");

    // 4. Accept the single callback connection, parse it, verify state.
    let (fields, mut stream) = loopback.wait(CALLBACK_TIMEOUT, "the OpenAI sign-in")?;

    if let Some(err) = fields.get("error") {
        let _ = oauth::respond(
            &mut stream,
            "<html><body><h1>Sign-in failed</h1><p>You can close this tab.</p></body></html>",
        );
        let desc = fields
            .get("error_description")
            .map(String::as_str)
            .unwrap_or("no description");
        bail!("authorization failed: {err} ({desc})");
    }

    let returned_state = fields
        .get("state")
        .ok_or_else(|| anyhow!("callback missing `state`"))?;
    if returned_state != &state {
        bail!("state mismatch on callback — possible CSRF; aborting");
    }
    let code = fields
        .get("code")
        .cloned()
        .ok_or_else(|| anyhow!("callback missing authorization `code`"))?;

    // The server may hand back a permanent client id to use from now on.
    let issued_client_id = fields.get("client_id").filter(|id| !id.is_empty()).cloned();
    let exchange_client_id = issued_client_id
        .clone()
        .unwrap_or_else(|| DYNAMIC_CLIENT_ID.to_string());

    let _ = oauth::respond(
        &mut stream,
        "<html><body><h1>Signed in to ChatGPT</h1>\
         <p>You can close this tab and return to atelier.</p></body></html>",
    );
    drop(stream);
    drop(loopback);

    // 5. Exchange the code for tokens.
    let form = form_encode(&[
        ("grant_type", "authorization_code"),
        ("client_id", &exchange_client_id),
        ("code", &code),
        ("code_verifier", &code_verifier),
        ("redirect_uri", &redirect_uri),
        ("resource", RESOURCE),
    ]);
    let resp = post_token(form).context("exchanging authorization code")?;

    if !resp.scope.split_whitespace().any(|s| s == REQUIRED_SCOPE) {
        bail!(
            "the ChatGPT-plan scope `{REQUIRED_SCOPE}` was not granted \
             (got `{}`); this account may not be eligible for direct token use",
            resp.scope
        );
    }

    // 6. Persist and return.
    let tokens = Tokens {
        client_id: exchange_client_id,
        access_token: resp.access_token,
        refresh_token: resp.refresh_token,
        expires_at: now_unix().saturating_add(resp.expires_in),
        scopes: resp.scope,
    };
    save(&tokens)?;
    Ok(tokens)
}

/// Return a valid bearer access token, refreshing (and persisting) if expired.
pub fn access_token(tokens: &mut Tokens) -> Result<String> {
    let now = now_unix();
    if is_expired(tokens.expires_at, now) {
        let refresh_token = tokens
            .refresh_token
            .clone()
            .ok_or_else(|| anyhow!("access token expired and no refresh token; run login again"))?;
        let form = form_encode(&[
            ("grant_type", "refresh_token"),
            ("client_id", &tokens.client_id),
            ("refresh_token", &refresh_token),
            ("resource", RESOURCE),
        ]);
        let resp = post_token(form).context("refreshing access token")?;

        tokens.access_token = resp.access_token;
        // Refresh tokens rotate; keep the old one only if none was returned.
        if resp.refresh_token.is_some() {
            tokens.refresh_token = resp.refresh_token;
        }
        tokens.expires_at = now.saturating_add(resp.expires_in);
        if !resp.scope.is_empty() {
            tokens.scopes = resp.scope;
        }
        save(tokens)?;
    }
    Ok(tokens.access_token.clone())
}

/// POST a form-encoded body to OpenAI's token endpoint.
fn post_token(form: String) -> Result<TokenResponse> {
    oauth::post_form(TOKEN_URL, form)
}
