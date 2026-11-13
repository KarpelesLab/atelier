//! OAuth building blocks shared by atelier's two sign-in flows.
//!
//! Both Sign in with ChatGPT ([`crate::auth`]) and sign-in to a remote MCP
//! server ([`crate::mcp::oauth`]) are the authorization-code grant with PKCE
//! (S256) for a public client: send the user's browser to an authorization
//! endpoint, catch the redirect on a loopback listener, exchange the code at
//! a token endpoint, keep the tokens in a private file and refresh them when
//! they expire. What differs between the two — the endpoints, how the
//! client id is obtained, what is stored — stays in those modules; what is
//! the same lives here:
//!
//! - PKCE and anti-forgery material ([`random_b64url`], [`pkce_challenge`]),
//! - the loopback redirect ([`Loopback`], [`respond`]) and [`open_browser`],
//! - the token endpoint ([`post_form`], [`TokenResponse`]),
//! - expiry with a safety margin ([`now_unix`], [`is_expired`]),
//! - URL and form encoding, and a file only its owner can read
//!   ([`write_private`]).
//!
//! Nothing here knows whose tokens it handles, and nothing here logs or
//! prints one.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

/// Refresh this many seconds before the token's stated expiry.
const EXPIRY_SKEW_SECS: u64 = 60;

// ─── Token endpoint ─────────────────────────────────────────────────────────

/// JSON returned by a token endpoint for both grant types.
#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Seconds the access token is good for; `0` when the server didn't say.
    #[serde(default)]
    pub expires_in: u64,
    /// Space-separated granted scopes; empty when the server didn't say.
    #[serde(default)]
    pub scope: String,
}

/// POST a form-encoded body to the token endpoint at `url` and parse the
/// JSON reply.
pub fn post_form(url: &str, form: String) -> Result<TokenResponse> {
    let resp = rsurl::Request::new("POST", url)
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

// ─── Loopback redirect ──────────────────────────────────────────────────────

/// A listener on an ephemeral loopback port for the authorization server to
/// redirect the browser back to.
pub struct Loopback {
    listener: TcpListener,
    /// `http://127.0.0.1:<port>/callback`: what to give as `redirect_uri`.
    pub redirect_uri: String,
}

impl Loopback {
    pub fn bind() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").context("binding loopback listener")?;
        let port = listener
            .local_addr()
            .context("reading listener address")?
            .port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
        })
    }

    /// Wait up to `timeout` for the browser to arrive and return the query
    /// parameters it came with, plus the connection to answer it on (see
    /// [`respond`]). `what` names the sign-in in the timeout error.
    pub fn wait(
        &self,
        timeout: Duration,
        what: &str,
    ) -> Result<(HashMap<String, String>, TcpStream)> {
        let mut stream = accept_callback(&self.listener, timeout, what)?;
        let request = read_request(&mut stream)?;
        let target = request_target(&request)
            .ok_or_else(|| anyhow!("malformed callback request from browser"))?;
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        Ok((parse_query(query), stream))
    }
}

/// Accept one connection, polling with a bounded deadline so we never hang.
fn accept_callback(listener: &TcpListener, timeout: Duration, what: &str) -> Result<TcpStream> {
    listener
        .set_nonblocking(true)
        .context("configuring listener")?;
    let deadline = Instant::now() + timeout;
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
                    bail!("timed out waiting for {what} callback");
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
pub fn respond(stream: &mut TcpStream, body: &str) -> Result<()> {
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
fn parse_query(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
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
/// because callers also show the URL as a fallback.
pub fn open_browser(url: &str) {
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

// ─── Storage ────────────────────────────────────────────────────────────────

/// Write `contents` to `path` (creating its directory) as a file only the
/// owner can read (0600): credentials. A file that doesn't exist yet is
/// created empty and restricted before anything is written to it, so the
/// contents are never on disk under the default mode.
pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let restrict = || -> Result<()> {
        #[cfg(unixlike)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restricting permissions on {}", path.display()))?;
        }
        Ok(())
    };
    if !path.exists() {
        std::fs::write(path, "").with_context(|| format!("creating {}", path.display()))?;
    }
    restrict()?;
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

// ─── Crypto / encoding helpers ──────────────────────────────────────────────

/// Current time in epoch seconds (0 if the clock is before the epoch).
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether an access token at `expires_at` should be treated as expired at
/// `now`, refreshing a little early to absorb clock skew and request latency.
pub fn is_expired(expires_at: u64, now: u64) -> bool {
    now >= expires_at.saturating_sub(EXPIRY_SKEW_SECS)
}

/// `base64url(n random bytes)` from the OS CSPRNG.
pub fn random_b64url(n: usize) -> String {
    use purecrypto::rng::{OsRng, RngCore};
    let mut buf = vec![0u8; n];
    let mut rng = OsRng;
    rng.fill_bytes(&mut buf);
    base64url_encode(&buf)
}

/// PKCE S256 challenge: `base64url(SHA-256(verifier))`.
pub fn pkce_challenge(verifier: &str) -> String {
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
pub fn percent_encode(s: &str) -> String {
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
pub fn percent_decode(s: &str) -> String {
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

/// Build an `application/x-www-form-urlencoded` body (or a URL query).
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
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
        let body = form_encode(&[
            ("grant_type", "authorization_code"),
            ("resource", "https://api.openai.com/v1"),
        ]);
        assert_eq!(
            body,
            "grant_type=authorization_code&resource=https%3A%2F%2Fapi.openai.com%2Fv1"
        );
    }

    #[test]
    fn the_loopback_listener_hands_over_the_callback_query() {
        let loopback = Loopback::bind().unwrap();
        let url = format!("{}?code=c%201&state=s", loopback.redirect_uri);
        let browser = std::thread::spawn(move || rsurl::get(&url).map(|r| r.status));
        let (fields, mut stream) = loopback.wait(Duration::from_secs(10), "the test").unwrap();
        assert_eq!(fields.get("code").map(String::as_str), Some("c 1"));
        assert_eq!(fields.get("state").map(String::as_str), Some("s"));
        respond(&mut stream, "<html>ok</html>").unwrap();
        drop(stream);
        assert_eq!(browser.join().unwrap().unwrap(), 200);

        // Nobody comes: an error naming the sign-in, not a hang.
        let err = loopback.wait(Duration::from_millis(50), "the test");
        assert!(
            err.unwrap_err()
                .to_string()
                .contains("timed out waiting for the test callback")
        );
    }

    #[cfg(unixlike)]
    #[test]
    fn private_files_are_owner_only_and_rewritten_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("atl-oauth-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub/secret.json");
        write_private(&path, "one").unwrap();
        write_private(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
