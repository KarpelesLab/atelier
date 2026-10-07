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

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

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
/// Refresh this many seconds before the token's stated expiry.
const EXPIRY_SKEW_SECS: u64 = 60;

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
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(tokens).context("serializing credentials")?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", path.display()))?;
    }
    Ok(())
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
    let listener = TcpListener::bind("127.0.0.1:0").context("binding loopback listener")?;
    let port = listener
        .local_addr()
        .context("reading listener address")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

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

    open_browser(&auth_url);
    eprintln!("If your browser didn't open, visit:\n{auth_url}");

    // 4. Accept the single callback connection, parse it, verify state.
    let mut stream = accept_callback(&listener)?;
    let request = read_request(&mut stream)?;
    let target = request_target(&request)
        .ok_or_else(|| anyhow!("malformed callback request from browser"))?;
    let query_str = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let fields = parse_query(query_str);

    if let Some(err) = fields.get("error") {
        let _ = write_response(
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

    let _ = write_response(
        &mut stream,
        "<html><body><h1>Signed in to ChatGPT</h1>\
         <p>You can close this tab and return to atelier.</p></body></html>",
    );
    drop(stream);
    drop(listener);

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

// ─── Token endpoint ─────────────────────────────────────────────────────────

/// JSON returned by the token endpoint for both grant types.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: u64,
    #[serde(default)]
    scope: String,
    // Present but unused here; the Responses backend does not need the id_token.
    #[serde(default)]
    #[allow(dead_code)]
    id_token: Option<String>,
}

/// POST a form-encoded body to the token endpoint and parse the JSON reply.
fn post_token(form: String) -> Result<TokenResponse> {
    let resp = rsurl::Request::new("POST", TOKEN_URL)
        .map_err(|e| anyhow!("building token request: {e}"))?
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form)
        .max_time(Duration::from_secs(30))
        .send()
        .map_err(|e| anyhow!("token request failed: {e}"))?;

    if !(200..300).contains(&resp.status) {
        // The error body carries an OAuth error code/description, never tokens.
        let body = resp.text().unwrap_or_default();
        bail!(
            "token endpoint returned HTTP {}: {}",
            resp.status,
            body.trim()
        );
    }
    serde_json::from_slice(&resp.body).context("parsing token response JSON")
}

// ─── Loopback callback helpers ──────────────────────────────────────────────

/// Accept one connection, polling with a bounded deadline so we never hang.
fn accept_callback(listener: &TcpListener) -> Result<TcpStream> {
    listener
        .set_nonblocking(true)
        .context("configuring listener")?;
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                stream
                    .set_nonblocking(false)
                    .context("configuring callback stream")?;
                return Ok(stream);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for the OpenAI sign-in callback");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(anyhow!("accepting callback connection: {e}")),
        }
    }
}

/// Read the callback HTTP request (enough to recover the request line).
fn read_request(stream: &mut TcpStream) -> Result<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .context("setting read timeout")?;
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                // The request line is all we need; stop at the first newline.
                if data.contains(&b'\n') || data.len() > 65_536 {
                    break;
                }
            }
            Err(ref e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(e) => return Err(anyhow!("reading callback request: {e}")),
        }
    }
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// Send a tiny HTML page so the browser tab shows something and can be closed.
fn write_response(stream: &mut TcpStream, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Extract the request target (path+query) from an HTTP request's first line.
fn request_target(request: &str) -> Option<&str> {
    request.lines().next()?.split_whitespace().nth(1)
}

/// Parse a `a=b&c=d` query string into a map, percent-decoding keys and values.
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), percent_decode(v));
    }
    map
}

// ─── Browser ────────────────────────────────────────────────────────────────

/// Best-effort attempt to open the system browser; failures are non-fatal
/// because the URL is also printed to stderr as a fallback.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let _ = url;
}

// ─── Crypto / encoding helpers ──────────────────────────────────────────────

/// Current time in epoch seconds (0 if the clock is before the epoch).
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether an access token at `expires_at` should be treated as expired at
/// `now`, refreshing a little early to absorb clock skew and request latency.
fn is_expired(expires_at: u64, now: u64) -> bool {
    now >= expires_at.saturating_sub(EXPIRY_SKEW_SECS)
}

/// `base64url(32 random bytes)` from the OS CSPRNG.
fn random_b64url(n: usize) -> String {
    use purecrypto::rng::{OsRng, RngCore};
    let mut buf = vec![0u8; n];
    let mut rng = OsRng;
    rng.fill_bytes(&mut buf);
    base64url_encode(&buf)
}

/// PKCE S256 challenge: `base64url(SHA-256(verifier))`.
fn pkce_challenge(verifier: &str) -> String {
    let digest = purecrypto::hash::sha256(verifier.as_bytes());
    base64url_encode(&digest)
}

/// base64url encoding with the `-`/`_` alphabet and no padding (RFC 4648 §5).
fn base64url_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

/// Percent-encode everything but the RFC 3986 unreserved set.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Percent-decode a query component (`%XX` → byte); leaves other bytes as-is.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Build an `application/x-www-form-urlencoded` body.
fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_known_vectors() {
        // RFC 4648 §10 test vectors (padding stripped).
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"fo"), "Zm8");
        assert_eq!(base64url_encode(b"foo"), "Zm9v");
        assert_eq!(base64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(base64url_encode(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_encode(b"foobar"), "Zm9vYmFy");
        // Exercises the URL-safe `-` and `_` substitutions.
        assert_eq!(base64url_encode(&[0xff, 0xef]), "_-8");
        assert_eq!(base64url_encode(&[0xff, 0xff, 0xff]), "____");
    }

    #[test]
    fn pkce_challenge_rfc7636_vector() {
        // RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(pkce_challenge(verifier), expected);

        // Cross-check against purecrypto's SHA-256 independently.
        let digest = purecrypto::hash::sha256(verifier.as_bytes());
        assert_eq!(base64url_encode(&digest), expected);
    }

    #[test]
    fn verifier_length_is_43_chars() {
        // 32 bytes → 43 base64url chars (no padding), within RFC 7636's 43..=128.
        assert_eq!(random_b64url(32).len(), 43);
    }

    #[test]
    fn is_expired_honours_skew() {
        // now well before expiry → valid.
        assert!(!is_expired(1_000, 900));
        // Exactly at the skew boundary (expires_at - 60) → expired.
        assert!(is_expired(1_000, 1_000 - EXPIRY_SKEW_SECS));
        // One second inside the skew window → still valid.
        assert!(!is_expired(1_000, 1_000 - EXPIRY_SKEW_SECS - 1));
        // Past expiry → expired.
        assert!(is_expired(1_000, 1_001));
        // Degenerate expiry (saturating_sub floors at 0).
        assert!(is_expired(0, 0));
        assert!(is_expired(30, 0));
    }

    #[test]
    fn percent_roundtrip_and_encoding() {
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("abc-._~"), "abc-._~");
        assert_eq!(
            percent_encode("https://api.openai.com/v1"),
            "https%3A%2F%2Fapi.openai.com%2Fv1"
        );
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("x%2Fy"), "x/y");
        // A stray percent with too few digits is left untouched.
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn parse_callback_query() {
        let target =
            request_target("GET /callback?code=abc123&state=x%20y&client_id=oaiapp_7 HTTP/1.1")
                .expect("request line parses");
        assert_eq!(
            target,
            "/callback?code=abc123&state=x%20y&client_id=oaiapp_7"
        );
        let q = target.split_once('?').unwrap().1;
        let fields = parse_query(q);
        assert_eq!(fields.get("code").map(String::as_str), Some("abc123"));
        assert_eq!(fields.get("state").map(String::as_str), Some("x y"));
        assert_eq!(
            fields.get("client_id").map(String::as_str),
            Some("oaiapp_7")
        );
    }

    #[test]
    fn form_encode_builds_body() {
        let body = form_encode(&[("grant_type", "authorization_code"), ("resource", RESOURCE)]);
        assert_eq!(
            body,
            "grant_type=authorization_code&resource=https%3A%2F%2Fapi.openai.com%2Fv1"
        );
    }
}
