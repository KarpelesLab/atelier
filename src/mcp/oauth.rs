//! OAuth sign-in for remote (`[[mcp_http]]`) MCP servers.
//!
//! A server that needs it answers `401` with a `WWW-Authenticate` header.
//! From there the MCP authorization spec strings four RFCs together, and
//! [`login`] follows them in order:
//!
//! 1. **Protected-resource metadata** (RFC 9728). The header's
//!    `resource_metadata` parameter — or, failing that, the well-known path
//!    on the server's origin — says which authorization server guards the
//!    resource.
//! 2. **Authorization-server metadata** (RFC 8414, or OpenID Connect
//!    discovery): its authorization, token and registration endpoints.
//! 3. **Dynamic client registration** (RFC 7591), unless the server's table
//!    in `atelier.toml` gives a `client_id`. atelier registers as a public
//!    client (no secret) for exactly the redirect it is about to use.
//! 4. **Authorization code + PKCE** with a loopback redirect, the same
//!    mechanics as Sign in with ChatGPT ([`crate::oauth`]), with the server's
//!    URL as the `resource` (RFC 8707) so the token is good for it alone.
//!
//! The resulting [`Grant`] is kept per server URL in a [`TokenStore`]
//! (`mcp-auth.json` in the user config dir, 0600). The HTTP transport asks
//! [`bearer`] for the token before each request, which refreshes an expired
//! one, and [`refresh`] when a token is rejected early.
//!
//! # What this is careful about
//!
//! - **Nothing opens a browser unasked.** Connecting never starts a sign-in:
//!   a server that wants one fails with [`LoginRequired`], which the session
//!   reports with the command to run. Only `/mcp login` calls [`login`].
//! - **Where tokens and codes travel.** Every endpoint the discovery
//!   documents name must be `https`, or `http` on loopback ([`is_secure`]):
//!   they come from the network, and a token endpoint over plain HTTP would
//!   leak the code and the tokens. The bearer token itself is only ever sent
//!   to the server URL it was issued for.
//! - **PKCE S256 or nothing.** A server that lists its challenge methods
//!   without S256 is refused. `state` is checked on the way back.
//!
//! [`login`] takes the "open this URL" step as a closure. The session passes
//! one that opens the browser and shows the URL; the tests pass one that
//! fetches it, which is how the whole flow runs against a fake server on
//! loopback with no browser.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::oauth::{self, Loopback, TokenResponse, form_encode, is_expired, now_unix};

/// How long `/mcp login` waits for the browser to come back.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a discovery or registration request may take.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Serializes read-modify-write of token files within this process: servers
/// connect on parallel threads, and two refreshing at once must not lose
/// each other's entry.
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// The server answered 401 and there is no token that it accepts: the user
/// has to sign in (`/mcp login <name>`). An error type of its own so the
/// session can tell this from a server that is merely broken.
#[derive(Debug)]
pub struct LoginRequired;

impl std::fmt::Display for LoginRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the server requires sign-in")
    }
}

impl std::error::Error for LoginRequired {}

/// What signing in to one server left us with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    /// The client the tokens were issued to (configured or registered).
    pub client_id: String,
    /// Where to refresh them.
    pub token_endpoint: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Access-token expiry, epoch seconds; `0` when the server gave none,
    /// in which case the token is used until the server rejects it.
    #[serde(default)]
    pub expires_at: u64,
    /// Space-separated granted scopes, when the server said.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
}

/// The file of [`Grant`]s, keyed by server URL.
#[derive(Debug, Clone)]
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    /// The user's store: `$XDG_CONFIG_HOME/atelier/mcp-auth.json`, else
    /// `~/.config/atelier/mcp-auth.json`.
    pub fn user() -> Self {
        let dir = crate::settings::Settings::user_dir()
            .unwrap_or_else(|| PathBuf::from(".config").join("atelier"));
        Self::at(dir.join("mcp-auth.json"))
    }

    /// A store in the file at `path`.
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The grant for the server at `url`, if the user signed in to it.
    pub fn get(&self, url: &str) -> Option<Grant> {
        let mut grants = self.read();
        grants.remove(url)
    }

    pub(crate) fn put(&self, url: &str, grant: Grant) -> Result<()> {
        self.update(|grants| grants.insert(url.to_string(), grant).is_none())
            .map(|_| ())
    }

    /// Forget the grant for `url` (sign out). Returns whether there was one.
    pub fn remove(&self, url: &str) -> Result<bool> {
        self.update(|grants| grants.remove(url).is_some())
    }

    /// A missing or unreadable file is an empty store: the worst case is
    /// being asked to sign in again.
    fn read(&self) -> BTreeMap<String, Grant> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Apply `change` to the stored grants and write them back (0600),
    /// returning what it returned.
    pub(crate) fn update<T>(
        &self,
        change: impl FnOnce(&mut BTreeMap<String, Grant>) -> T,
    ) -> Result<T> {
        let _lock = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut grants = self.read();
        let result = change(&mut grants);
        let json = serde_json::to_string_pretty(&grants).context("serializing MCP tokens")?;
        oauth::write_private(&self.path, &json)?;
        Ok(result)
    }
}

/// The access token to send to the server at `url`: `None` if the user has
/// not signed in to it, else the stored token — refreshed first, and the
/// store updated, if it has expired.
pub fn bearer(store: &TokenStore, url: &str) -> Result<Option<String>> {
    let Some(grant) = store.get(url) else {
        return Ok(None);
    };
    if grant.expires_at != 0 && is_expired(grant.expires_at, now_unix()) {
        return renew(store, url, grant).map(Some);
    }
    Ok(Some(grant.access_token))
}

/// A new access token for `url` after the server rejected the stored one
/// before its stated expiry (revoked, or the server gave no expiry). `None`
/// if the user has not signed in; [`LoginRequired`] if the grant cannot be
/// refreshed.
pub fn refresh(store: &TokenStore, url: &str) -> Result<Option<String>> {
    match store.get(url) {
        Some(grant) => renew(store, url, grant).map(Some),
        None => Ok(None),
    }
}

/// Trade the grant's refresh token for a new access token and store it.
fn renew(store: &TokenStore, url: &str, mut grant: Grant) -> Result<String> {
    let Some(refresh_token) = grant.refresh_token.clone() else {
        return Err(
            anyhow!(LoginRequired).context("the access token expired and cannot be renewed")
        );
    };
    let form = form_encode(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &refresh_token),
        ("client_id", &grant.client_id),
        ("resource", url),
    ]);
    // Whatever the reason the server won't refresh (expired, revoked), the
    // way forward is the same: sign in again.
    let resp = oauth::post_form(&grant.token_endpoint, form)
        .map_err(|e| anyhow!(LoginRequired).context(format!("renewing the access token: {e:#}")))?;
    apply(&mut grant, resp);
    let token = grant.access_token.clone();
    store.put(url, grant)?;
    Ok(token)
}

/// Take a token response into `grant`.
fn apply(grant: &mut Grant, resp: TokenResponse) {
    grant.access_token = resp.access_token;
    // Refresh tokens may rotate; keep the old one if none came back.
    if resp.refresh_token.is_some() {
        grant.refresh_token = resp.refresh_token;
    }
    grant.expires_at = match resp.expires_in {
        0 => 0,
        secs => now_unix().saturating_add(secs),
    };
    if !resp.scope.is_empty() {
        grant.scope = resp.scope;
    }
}

/// What [`login`] needs to know about the server.
pub struct LoginRequest<'a> {
    /// The server's MCP endpoint.
    pub url: &'a str,
    /// The headers configured for it, sent with the probe.
    pub headers: &'a [(String, String)],
    /// A pre-registered client id (`client_id` in the server's table);
    /// without one, atelier registers itself dynamically.
    pub client_id: Option<&'a str>,
    /// How long to wait for the browser to come back.
    pub timeout: Duration,
}

/// Sign in to the server (see the module docs for the steps) and store the
/// grant. `open` is handed the authorization URL to take the user to.
pub fn login(store: &TokenStore, req: &LoginRequest, open: &mut dyn FnMut(&str)) -> Result<()> {
    // 1. Which authorization server, and which scopes.
    let challenge = probe(req)?;
    let resource = protected_resource(req.url, challenge.resource_metadata.as_deref());
    let issuer = resource
        .as_ref()
        .and_then(|r| r["authorization_servers"][0].as_str())
        .map(str::to_string)
        // Servers predating RFC 9728 are their own authorization server.
        .unwrap_or_else(|| origin(req.url).to_string());
    let scope = challenge.scope.or_else(|| {
        let listed = resource.as_ref()?["scopes_supported"].as_array()?;
        let scopes: Vec<&str> = listed.iter().filter_map(Value::as_str).collect();
        (!scopes.is_empty()).then(|| scopes.join(" "))
    });

    // 2. Its endpoints.
    let metadata = server_metadata(&issuer)?;
    let endpoint = |key: &str| -> Result<String> {
        let url = metadata[key]
            .as_str()
            .ok_or_else(|| anyhow!("authorization server {issuer} has no `{key}`"))?;
        if !is_secure(url) {
            bail!("refusing `{key}` {url}: not https (or loopback)");
        }
        Ok(url.to_string())
    };
    let authorization_endpoint = endpoint("authorization_endpoint")?;
    let token_endpoint = endpoint("token_endpoint")?;
    if let Some(methods) = metadata["code_challenge_methods_supported"].as_array()
        && !methods.iter().any(|m| m == "S256")
    {
        bail!("authorization server {issuer} does not support PKCE with S256");
    }

    // 3. The redirect, and a client allowed to use it.
    let loopback = Loopback::bind()?;
    let client_id = match req.client_id {
        Some(id) => id.to_string(),
        None => register(
            &endpoint("registration_endpoint").context(
                "the server offers no dynamic client registration: register a client with it \
             yourself and set `client_id` in the server's table",
            )?,
            &loopback.redirect_uri,
        )?,
    };

    // 4. Authorization code + PKCE.
    let verifier = oauth::random_b64url(32);
    let challenge = oauth::pkce_challenge(&verifier);
    let state = oauth::random_b64url(32);
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", loopback.redirect_uri.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
        ("resource", req.url),
    ];
    if let Some(scope) = &scope {
        params.push(("scope", scope));
    }
    let separator = if authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    open(&format!(
        "{authorization_endpoint}{separator}{}",
        form_encode(&params)
    ));

    let (fields, mut stream) = loopback.wait(req.timeout, "the MCP server sign-in")?;
    let code = match callback_code(&fields, &state) {
        Ok(code) => {
            let _ = oauth::respond(
                &mut stream,
                &page("Signed in", "You can close this tab and return to atelier."),
            );
            code
        }
        Err(e) => {
            let _ = oauth::respond(
                &mut stream,
                &page("Sign-in failed", "You can close this tab."),
            );
            return Err(e);
        }
    };
    drop(stream);

    let form = form_encode(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("code_verifier", &verifier),
        ("redirect_uri", &loopback.redirect_uri),
        ("client_id", &client_id),
        ("resource", req.url),
    ]);
    let resp = oauth::post_form(&token_endpoint, form).context("exchanging authorization code")?;
    let mut grant = Grant {
        client_id,
        token_endpoint,
        access_token: String::new(),
        refresh_token: None,
        expires_at: 0,
        scope: scope.unwrap_or_default(),
    };
    apply(&mut grant, resp);
    store.put(req.url, grant)
}

fn page(title: &str, text: &str) -> String {
    format!("<html><body><h1>{title}</h1><p>{text}</p></body></html>")
}

/// The authorization code out of the redirect's query, once the server has
/// not reported an error and `state` is the one we sent.
fn callback_code(
    fields: &std::collections::HashMap<String, String>,
    state: &str,
) -> Result<String> {
    if let Some(err) = fields.get("error") {
        let description = fields.get("error_description").map(String::as_str);
        bail!(
            "authorization failed: {err} ({})",
            description.unwrap_or("no description")
        );
    }
    if fields.get("state").map(String::as_str) != Some(state) {
        bail!("state mismatch on callback — possible CSRF; aborting");
    }
    fields
        .get("code")
        .cloned()
        .ok_or_else(|| anyhow!("callback missing authorization `code`"))
}

/// What the server's 401 said about signing in.
struct Challenge {
    resource_metadata: Option<String>,
    scope: Option<String>,
}

/// Ask the server, without credentials, how to sign in: send `initialize`
/// and read the `WWW-Authenticate` of its 401.
fn probe(req: &LoginRequest) -> Result<Challenge> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "atelier", "version": "0.0.0" },
        },
    });
    let mut request = rsurl::Request::new("POST", req.url)
        .context("building HTTP request")?
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .max_time(DISCOVERY_TIMEOUT)
        .body(body.to_string());
    for (name, value) in req.headers {
        request = request.header(name, value);
    }
    let resp = request
        .send()
        .context("asking the MCP server how to sign in")?;
    if resp.status != 401 {
        bail!(
            "the server did not ask for sign-in (it answered HTTP {} without credentials)",
            resp.status
        );
    }
    let header = resp.header("www-authenticate").unwrap_or("");
    Ok(Challenge {
        resource_metadata: auth_param(header, "resource_metadata"),
        scope: auth_param(header, "scope"),
    })
}

/// The value of parameter `name` in a `WWW-Authenticate` header
/// (`Bearer realm="x", scope="a b"`): quoted or a bare token.
fn auth_param(header: &str, name: &str) -> Option<String> {
    header.match_indices(name).find_map(|(at, _)| {
        // A whole parameter name, not the tail of another (`error_scope`).
        let before = &header[..at];
        if !(before.is_empty() || before.ends_with([' ', ',', '\t'])) {
            return None;
        }
        let value = header[at + name.len()..].trim_start().strip_prefix('=')?;
        let value = value.trim_start();
        Some(match value.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next().unwrap_or("").to_string(),
            None => value.split([',', ' ']).next().unwrap_or("").to_string(),
        })
    })
}

/// The server's protected-resource metadata (RFC 9728): from the URL its
/// 401 named, else from the well-known locations for `url`. `None` if it
/// publishes none.
fn protected_resource(url: &str, named: Option<&str>) -> Option<Value> {
    let (origin, path) = (origin(url), path(url));
    let well_known = "/.well-known/oauth-protected-resource";
    let candidates = named.map(str::to_string).into_iter().chain([
        format!("{origin}{well_known}{path}"),
        format!("{origin}{well_known}"),
    ]);
    candidates.filter_map(|u| get_json(&u).ok()).next()
}

/// The authorization server's metadata: RFC 8414's location for `issuer`,
/// then the two OpenID Connect discovery ones.
fn server_metadata(issuer: &str) -> Result<Value> {
    let (origin, path) = (origin(issuer), path(issuer));
    let candidates = [
        format!("{origin}/.well-known/oauth-authorization-server{path}"),
        format!("{origin}/.well-known/openid-configuration{path}"),
        format!("{origin}{path}/.well-known/openid-configuration"),
    ];
    let mut last = anyhow!("no metadata location to try");
    for url in &candidates {
        match get_json(url) {
            Ok(metadata) => return Ok(metadata),
            Err(e) => last = e,
        }
    }
    Err(last.context(format!(
        "finding the metadata of authorization server {issuer}"
    )))
}

/// Register atelier as a public client for `redirect_uri` (RFC 7591) and
/// return the client id it was given.
fn register(endpoint: &str, redirect_uri: &str) -> Result<String> {
    let body = json!({
        "client_name": "atelier",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let resp = rsurl::Request::new("POST", endpoint)
        .context("building registration request")?
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .max_time(DISCOVERY_TIMEOUT)
        .body(body.to_string())
        .send()
        .context("registering with the authorization server")?;
    if !(200..300).contains(&resp.status) {
        bail!(
            "client registration returned HTTP {}: {}",
            resp.status,
            String::from_utf8_lossy(&resp.body).trim()
        );
    }
    let registered: Value =
        serde_json::from_slice(&resp.body).context("parsing the registration response")?;
    registered["client_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the registration response has no `client_id`"))
}

/// GET a JSON document from a URL that must be safe to talk to.
fn get_json(url: &str) -> Result<Value> {
    if !is_secure(url) {
        bail!("refusing {url}: not https (or loopback)");
    }
    let resp = rsurl::Request::new("GET", url)
        .with_context(|| format!("building request for {url}"))?
        .header("accept", "application/json")
        .max_time(DISCOVERY_TIMEOUT)
        .send()
        .with_context(|| format!("fetching {url}"))?;
    if !(200..300).contains(&resp.status) {
        bail!("{url} returned HTTP {}", resp.status);
    }
    serde_json::from_slice(&resp.body).with_context(|| format!("parsing {url}"))
}

/// Whether `url` may carry codes and tokens: `https`, or `http` to this
/// machine.
fn is_secure(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // No userinfo tricks (`http://127.0.0.1@evil.example/`).
    if authority.contains('@') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// `scheme://authority` of a URL.
fn origin(url: &str) -> &str {
    let after_scheme = url.find("://").map_or(0, |i| i + 3);
    let end = url[after_scheme..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |i| after_scheme + i);
    &url[..end]
}

/// The path of a URL without a trailing slash; empty for the root.
fn path(url: &str) -> &str {
    let rest = &url[origin(url).len()..];
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    rest[..end].trim_end_matches('/')
}

#[cfg(test)]
pub(crate) mod testutil {
    //! A fake authorization server and protected MCP resource in one, on a
    //! loopback port, and the "browser" that signs in to it.

    use super::*;
    use crate::mcp::testutil::{FakeHttp, HttpReply, HttpRequest, mcp_reply, tools_only};
    use std::sync::Arc;

    /// What the fake remembers between requests, for tests to look at.
    #[derive(Default)]
    pub(crate) struct Seen {
        /// `redirect_uris` of each registration.
        pub(crate) registered: Vec<String>,
        /// The query of each authorization request.
        pub(crate) authorizations: Vec<std::collections::HashMap<String, String>>,
        /// The form of each token request.
        pub(crate) token_requests: Vec<std::collections::HashMap<String, String>>,
        /// The `Authorization` header of each MCP request (`""` if none).
        pub(crate) bearers: Vec<String>,
        /// Access tokens issued so far; the last one is the valid one.
        pub(crate) issued: u32,
        /// The PKCE challenge of the pending authorization.
        challenge: String,
    }

    pub(crate) struct FakeAuth {
        pub(crate) http: FakeHttp,
        pub(crate) seen: Arc<Mutex<Seen>>,
    }

    impl FakeAuth {
        /// The MCP endpoint.
        pub(crate) fn url(&self) -> String {
            self.http.url("/mcp")
        }

        /// The access token the server currently accepts.
        pub(crate) fn valid_token(&self) -> String {
            format!("at-{}", self.seen.lock().unwrap().issued)
        }
    }

    fn form(text: &str) -> std::collections::HashMap<String, String> {
        text.split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (oauth::percent_decode(k), oauth::percent_decode(v)))
            .collect()
    }

    /// Start the fake. Its MCP side offers one tool, `whoami`, and accepts
    /// only the most recently issued access token.
    pub(crate) fn fake_auth() -> FakeAuth {
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let state = seen.clone();
        let http = FakeHttp::serve(move |request: &HttpRequest| {
            let base = format!("http://{}", request.headers["host"]);
            let mut seen = state.lock().unwrap();
            let path = request.target.split('?').next().unwrap_or("");
            let query = request.target.split_once('?').map_or("", |(_, q)| q);
            match (request.method.as_str(), path) {
                ("POST", "/mcp") => {
                    let sent = request.headers.get("authorization").cloned();
                    seen.bearers.push(sent.clone().unwrap_or_default());
                    let valid = format!("Bearer at-{}", seen.issued);
                    if seen.issued == 0 || sent.as_deref() != Some(valid.as_str()) {
                        let challenge = format!(
                            "Bearer realm=\"mcp\", resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\", scope=\"tools\""
                        );
                        return HttpReply {
                            status: 401,
                            headers: vec![("WWW-Authenticate".into(), challenge)],
                            body: String::new(),
                        };
                    }
                    mcp_reply(request, &tools_only(json!([{ "name": "whoami" }])))
                }
                ("GET", "/.well-known/oauth-protected-resource/mcp") => HttpReply::json(
                    200,
                    &json!({ "resource": format!("{base}/mcp"), "authorization_servers": [base] }),
                ),
                ("GET", "/.well-known/oauth-authorization-server") => HttpReply::json(
                    200,
                    &json!({
                        "issuer": base,
                        "authorization_endpoint": format!("{base}/authorize"),
                        "token_endpoint": format!("{base}/token"),
                        "registration_endpoint": format!("{base}/register"),
                        "code_challenge_methods_supported": ["S256"],
                    }),
                ),
                ("POST", "/register") => {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    let uri = body["redirect_uris"][0].as_str().unwrap().to_string();
                    seen.registered.push(uri);
                    let id = format!("dyn-{}", seen.registered.len());
                    HttpReply::json(201, &json!({ "client_id": id }))
                }
                ("GET", "/authorize") => {
                    let query = form(query);
                    seen.challenge = query["code_challenge"].clone();
                    let back = format!(
                        "{}?code=the-code&state={}",
                        query["redirect_uri"],
                        oauth::percent_encode(&query["state"])
                    );
                    seen.authorizations.push(query);
                    HttpReply {
                        status: 302,
                        headers: vec![("Location".into(), back)],
                        body: String::new(),
                    }
                }
                ("POST", "/token") => {
                    let sent = form(&String::from_utf8_lossy(&request.body));
                    let ok = match sent["grant_type"].as_str() {
                        // The code, and proof of the verifier behind the
                        // challenge the authorization request carried.
                        "authorization_code" => {
                            sent["code"] == "the-code"
                                && oauth::pkce_challenge(&sent["code_verifier"]) == seen.challenge
                        }
                        "refresh_token" => sent["refresh_token"] == format!("rt-{}", seen.issued),
                        _ => false,
                    };
                    seen.token_requests.push(sent);
                    if !ok {
                        return HttpReply::json(400, &json!({ "error": "invalid_grant" }));
                    }
                    seen.issued += 1;
                    HttpReply::json(
                        200,
                        &json!({
                            "access_token": format!("at-{}", seen.issued),
                            "refresh_token": format!("rt-{}", seen.issued),
                            "expires_in": 3600,
                            "scope": "tools",
                        }),
                    )
                }
                _ => HttpReply::json(404, &json!({ "error": "not found" })),
            }
        });
        FakeAuth { http, seen }
    }

    /// What a user does with the authorization URL, minus the user: fetch
    /// it, and follow the redirect it answers with back to atelier. On a
    /// thread, as a browser is: atelier is not listening for the redirect
    /// until the "open" step returns.
    pub(crate) fn browser(url: &str) {
        let url = url.to_string();
        std::thread::spawn(move || {
            let resp = rsurl::get(&url).expect("fetch the authorization URL");
            assert_eq!(resp.status, 302, "the authorization server redirects");
            let back = resp.header("location").expect("a redirect target");
            let _ = rsurl::get(back);
        });
    }

    /// An empty token store in a fresh temp directory.
    pub(crate) fn temp_store(name: &str) -> TokenStore {
        let dir = std::env::temp_dir().join(format!("atl-mcp-oauth-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        TokenStore::at(dir.join("atelier/mcp-auth.json"))
    }

    pub(crate) fn sign_in(
        store: &TokenStore,
        fake: &FakeAuth,
        client_id: Option<&str>,
    ) -> Result<()> {
        let url = fake.url();
        let req = LoginRequest {
            url: &url,
            headers: &[],
            client_id,
            timeout: Duration::from_secs(20),
        };
        login(store, &req, &mut |authorize| browser(authorize))
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn login_discovers_registers_and_stores_a_grant() {
        let fake = fake_auth();
        let store = temp_store("login");
        sign_in(&store, &fake, None).expect("the sign-in completes");

        let url = fake.url();
        let grant = store
            .get(&url)
            .expect("a grant is stored for the server's URL");
        assert_eq!(grant.client_id, "dyn-1");
        assert_eq!(grant.access_token, "at-1");
        assert_eq!(grant.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(grant.token_endpoint, fake.http.url("/token"));
        assert_eq!(grant.scope, "tools");
        assert!(grant.expires_at > now_unix() + 3000);

        let seen = fake.seen.lock().unwrap();
        // Registered for the loopback redirect the request then used.
        let authorize = &seen.authorizations[0];
        assert!(seen.registered[0].starts_with("http://127.0.0.1:"));
        assert_eq!(authorize["redirect_uri"], seen.registered[0]);
        assert_eq!(authorize["client_id"], "dyn-1");
        assert_eq!(authorize["response_type"], "code");
        assert_eq!(authorize["code_challenge_method"], "S256");
        // Bound to this server (RFC 8707), with the scope its 401 asked for.
        assert_eq!(authorize["resource"], url);
        assert_eq!(authorize["scope"], "tools");
        // The exchange proved the verifier (the fake checked it) and named
        // the same client, redirect and resource.
        let exchange = &seen.token_requests[0];
        assert_eq!(exchange["grant_type"], "authorization_code");
        assert_eq!(exchange["client_id"], "dyn-1");
        assert_eq!(exchange["redirect_uri"], seen.registered[0]);
        assert_eq!(exchange["resource"], url);
        // The probe carried no credentials.
        assert_eq!(seen.bearers, [""]);
    }

    #[cfg(unixlike)]
    #[test]
    fn the_token_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let fake = fake_auth();
        let store = temp_store("mode");
        sign_in(&store, &fake, None).unwrap();
        let mode = std::fs::metadata(&store.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_configured_client_id_skips_registration() {
        let fake = fake_auth();
        let store = temp_store("client-id");
        sign_in(&store, &fake, Some("my-app")).unwrap();
        assert_eq!(store.get(&fake.url()).unwrap().client_id, "my-app");
        let seen = fake.seen.lock().unwrap();
        assert!(seen.registered.is_empty());
        assert_eq!(seen.authorizations[0]["client_id"], "my-app");
    }

    #[test]
    fn an_expired_token_is_refreshed_and_the_new_one_stored() {
        let fake = fake_auth();
        let store = temp_store("refresh");
        let url = fake.url();
        sign_in(&store, &fake, None).unwrap();
        // Fresh: used as is, no request to the token endpoint.
        assert_eq!(bearer(&store, &url).unwrap().as_deref(), Some("at-1"));
        assert_eq!(fake.seen.lock().unwrap().token_requests.len(), 1);

        let mut grant = store.get(&url).unwrap();
        grant.expires_at = now_unix() - 10;
        store.put(&url, grant).unwrap();
        assert_eq!(bearer(&store, &url).unwrap().as_deref(), Some("at-2"));
        let renewed = store.get(&url).unwrap();
        assert_eq!(renewed.access_token, "at-2");
        assert_eq!(renewed.refresh_token.as_deref(), Some("rt-2"));
        assert!(renewed.expires_at > now_unix());
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen.token_requests[1]["grant_type"], "refresh_token");
        assert_eq!(seen.token_requests[1]["client_id"], "dyn-1");
    }

    #[test]
    fn a_grant_that_cannot_be_renewed_asks_for_sign_in() {
        let fake = fake_auth();
        let store = temp_store("dead");
        let url = fake.url();
        // Never signed in: nothing to send, and nothing to refresh.
        assert_eq!(bearer(&store, &url).unwrap(), None);
        assert_eq!(refresh(&store, &url).unwrap(), None);

        sign_in(&store, &fake, None).unwrap();
        let mut grant = store.get(&url).unwrap();
        grant.refresh_token = Some("revoked".into());
        store.put(&url, grant.clone()).unwrap();
        let err = refresh(&store, &url).unwrap_err();
        assert!(err.downcast_ref::<LoginRequired>().is_some(), "{err:#}");

        grant.refresh_token = None;
        grant.expires_at = 1;
        store.put(&url, grant).unwrap();
        let err = bearer(&store, &url).unwrap_err();
        assert!(err.downcast_ref::<LoginRequired>().is_some(), "{err:#}");

        // Signing out forgets the grant, once.
        assert!(store.remove(&url).unwrap());
        assert!(!store.remove(&url).unwrap());
        assert_eq!(store.get(&url), None);
    }

    #[test]
    fn a_server_that_does_not_want_sign_in_is_not_signed_in_to() {
        let open = crate::mcp::testutil::FakeHttp::mcp(crate::mcp::testutil::tools_only(json!([])));
        let url = open.url("/mcp");
        let req = LoginRequest {
            url: &url,
            headers: &[],
            client_id: None,
            timeout: Duration::from_secs(5),
        };
        let mut opened = false;
        let err = login(&temp_store("open"), &req, &mut |_| opened = true).unwrap_err();
        assert!(
            err.to_string().contains("did not ask for sign-in"),
            "{err:#}"
        );
        assert!(!opened);
    }

    #[test]
    fn a_callback_with_the_wrong_state_or_an_error_is_refused() {
        let fields = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<std::collections::HashMap<_, _>>()
        };
        let ok = fields(&[("code", "c"), ("state", "s")]);
        assert_eq!(callback_code(&ok, "s").unwrap(), "c");
        let forged = callback_code(&ok, "other").unwrap_err();
        assert!(forged.to_string().contains("state mismatch"));
        let no_state = callback_code(&fields(&[("code", "c")]), "s").unwrap_err();
        assert!(no_state.to_string().contains("state mismatch"));
        let denied = fields(&[("error", "access_denied"), ("state", "s")]);
        let denied = callback_code(&denied, "s").unwrap_err();
        assert!(denied.to_string().contains("access_denied"));
        let no_code = callback_code(&fields(&[("state", "s")]), "s").unwrap_err();
        assert!(no_code.to_string().contains("missing authorization `code`"));
    }

    #[test]
    fn www_authenticate_parameters() {
        let header = r#"Bearer realm="mcp", error_scope="no", resource_metadata="https://x/.well-known/oauth-protected-resource", scope="a b""#;
        assert_eq!(
            auth_param(header, "resource_metadata").as_deref(),
            Some("https://x/.well-known/oauth-protected-resource")
        );
        // Not the tail of `error_scope`.
        assert_eq!(auth_param(header, "scope").as_deref(), Some("a b"));
        assert_eq!(
            auth_param("Bearer scope=tools, realm=x", "scope").as_deref(),
            Some("tools")
        );
        assert_eq!(auth_param("Bearer", "scope"), None);
        assert_eq!(auth_param("", "resource_metadata"), None);
    }

    #[test]
    fn only_https_or_loopback_may_carry_tokens() {
        for ok in [
            "https://auth.example.com/token",
            "http://127.0.0.1:8080/token",
            "http://localhost/token",
            "http://[::1]:9/token",
        ] {
            assert!(is_secure(ok), "{ok}");
        }
        for bad in [
            "http://auth.example.com/token",
            "http://127.0.0.1.evil.example/token",
            "http://127.0.0.1@evil.example/token",
            "http://localhost.evil.example/",
            "ftp://127.0.0.1/",
            "//127.0.0.1/",
        ] {
            assert!(!is_secure(bad), "{bad}");
        }
    }

    #[test]
    fn url_parts() {
        assert_eq!(
            origin("https://h.example:8443/a/b?q#f"),
            "https://h.example:8443"
        );
        assert_eq!(path("https://h.example:8443/a/b/?q#f"), "/a/b");
        assert_eq!(origin("https://h.example"), "https://h.example");
        assert_eq!(path("https://h.example"), "");
        assert_eq!(path("https://h.example/"), "");
    }
}
