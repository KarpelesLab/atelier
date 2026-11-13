//! The `web_search` tool: search the web, get back titles, URLs and snippets.
//!
//! The companion of `web_fetch`: this finds the page, that reads it. Like
//! `web_fetch` it reaches outside the project, so it needs approval.
//!
//! Four providers, picked at call time by [`resolve`] from the environment
//! (`ATELIER_SEARCH_PROVIDER`, `ATELIER_SEARCH_URL`, `ATELIER_SEARCH_KEY`)
//! and the `[search]` table of `atelier.toml` — at call time because tools
//! are built before settings exist, and so a change takes effect without a
//! restart:
//!
//! - `searxng` — a self-hosted metasearch instance's JSON API;
//! - `brave`, `tavily` — hosted search APIs, each needing a key;
//! - `duckduckgo` — the default when nothing is configured: no key, but it
//!   means reading DuckDuckGo's HTML results page, which can change shape or
//!   answer an automated client with a challenge. When the page isn't what
//!   [`parse_duckduckgo`] knows, the tool says so and how to configure a
//!   real provider; it never guesses at results.
//!
//! Each provider is one request ([`Backend::request`]) and one pure parser
//! over the response body, so the parsers are tested against fixtures and
//! nothing in the tests touches the network beyond localhost.
//!
//! The API key lives only in the environment (`atelier.toml` names the
//! variable, never the value), goes out only as a request header, and is
//! scrubbed from every error on the way back ([`scrub`]) in case a server
//! echoes it.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use rsurl::Request;
use serde_json::{Value, json};

use super::web_fetch::{attr, decode_entities};
use super::{Tool, ToolCtx, ToolSpec};
use crate::oauth::{percent_decode, percent_encode};
use crate::settings::{SearchSettings, Settings};

const DEFAULT_RESULTS: usize = 8;
const MAX_RESULTS: usize = 20;
/// Titles and snippets are cut here (characters): enough to judge a result
/// by, and twenty of them still fit comfortably in the context.
const MAX_TITLE_CHARS: usize = 150;
const MAX_SNIPPET_CHARS: usize = 300;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

const DUCKDUCKGO_URL: &str = "https://html.duckduckgo.com/html/";
const BRAVE_URL: &str = "https://api.search.brave.com/res/v1/web/search";
const TAVILY_URL: &str = "https://api.tavily.com/search";

const CONFIGURE_HINT: &str = "configure a search provider instead: set ATELIER_SEARCH_PROVIDER \
    to searxng (with ATELIER_SEARCH_URL), brave or tavily (with ATELIER_SEARCH_KEY), or use \
    the [search] table in atelier.toml";

pub struct WebSearchTool;

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    // Reaches out (and so asks), but changes nothing here.
    fn mutates(&self, _args: &Value) -> bool {
        false
    }

    // Leave the default (`true`): like `web_fetch`, this reaches out over
    // the network, and the query itself leaves the machine.

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Search the web. Returns a numbered list of results, each with its \
                title, URL and a short snippet; use web_fetch to read a result. `domains` \
                restricts the search to those sites."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to search for."
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "How many results to return (default 8, at most 20)."
                    },
                    "domains": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Only return results from these domains (e.g. [\"docs.rs\"]); subdomains included."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| anyhow!("missing required argument 'query'"))?;
        let max = args
            .get("max_results")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_RESULTS, |n| (n as usize).clamp(1, MAX_RESULTS));
        let domains: Vec<String> = args
            .get("domains")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(normalize_domain)
                    .filter(|d| !d.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        // A broken atelier.toml was already reported at startup; here it
        // just means "no [search] table".
        let settings = Settings::load(ctx.project_root).unwrap_or_default();
        // `key_env` names a variable to send to an endpoint the same file
        // chooses: honored only in a project the user has trusted.
        let store = crate::trust::Store::from_env();
        let trust = crate::trust::Trust::load(ctx.project_root, &settings, store);
        let search = trust.effective(&settings).search;
        let backend = resolve(&|name| std::env::var(name).ok(), &search)?;
        let hits = backend.search(query, max, &domains)?;
        Ok(render(query, &hits))
    }
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hit {
    title: String,
    url: String,
    snippet: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    DuckDuckGo,
    SearXng,
    Brave,
    Tavily,
}

impl Provider {
    fn name(self) -> &'static str {
        match self {
            Provider::DuckDuckGo => "duckduckgo",
            Provider::SearXng => "searxng",
            Provider::Brave => "brave",
            Provider::Tavily => "tavily",
        }
    }
}

/// A provider with everything needed to query it. Deliberately not `Debug`:
/// it holds the key.
struct Backend {
    provider: Provider,
    /// The endpoint the query goes to.
    endpoint: String,
    key: Option<String>,
}

/// Works out the backend from the environment (`env`, a parameter so tests
/// needn't touch the process's real one) and `[search]`. The environment
/// wins, setting by setting. With no provider named, a URL means SearXNG —
/// the only provider that needs one — and nothing at all means DuckDuckGo.
fn resolve(env: &dyn Fn(&str) -> Option<String>, settings: &SearchSettings) -> Result<Backend> {
    let set = |value: Option<String>| {
        value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let name = set(env("ATELIER_SEARCH_PROVIDER")).or_else(|| set(settings.provider.clone()));
    let url = set(env("ATELIER_SEARCH_URL")).or_else(|| set(settings.url.clone()));
    let key = set(env("ATELIER_SEARCH_KEY"))
        .or_else(|| set(settings.key_env.clone()).and_then(|var| set(env(&var))));

    let provider = match name.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None if url.is_some() => Provider::SearXng,
        None | Some("duckduckgo" | "ddg") => Provider::DuckDuckGo,
        Some("searxng") => Provider::SearXng,
        Some("brave") => Provider::Brave,
        Some("tavily") => Provider::Tavily,
        Some(other) => bail!(
            "unknown search provider {other:?}: expected searxng, brave, tavily or duckduckgo"
        ),
    };
    if matches!(provider, Provider::Brave | Provider::Tavily) && key.is_none() {
        bail!(
            "the {} search provider needs an API key: set ATELIER_SEARCH_KEY, or name the \
             variable holding it in [search] key_env",
            provider.name()
        );
    }
    // For the hosted providers a URL is an override of the endpoint (a
    // proxy, a test server); for SearXNG it is the instance, and required.
    let endpoint = match (provider, url) {
        (Provider::SearXng, Some(base)) => format!("{}/search", base.trim_end_matches('/')),
        (Provider::SearXng, None) => bail!(
            "the searxng search provider needs the instance's URL: set ATELIER_SEARCH_URL \
             or [search] url"
        ),
        (_, Some(url)) => url,
        (Provider::DuckDuckGo, None) => DUCKDUCKGO_URL.to_string(),
        (Provider::Brave, None) => BRAVE_URL.to_string(),
        (Provider::Tavily, None) => TAVILY_URL.to_string(),
    };
    Ok(Backend {
        provider,
        endpoint,
        key,
    })
}

impl Backend {
    /// Runs the search: up to `max` results, from `domains` only when given.
    fn search(&self, query: &str, max: usize, domains: &[String]) -> Result<Vec<Hit>> {
        self.fetch(query, max, domains)
            .map(|mut hits| {
                // The providers are asked to restrict the search, but the
                // restriction is a promise to the caller, so hold them to it.
                if !domains.is_empty() {
                    hits.retain(|h| in_domains(&h.url, domains));
                }
                hits.truncate(max);
                hits
            })
            .map_err(|e| {
                anyhow!(
                    "{} search failed: {}",
                    self.provider.name(),
                    scrub(&e.to_string(), self.key.as_deref())
                )
            })
    }

    fn fetch(&self, query: &str, max: usize, domains: &[String]) -> Result<Vec<Hit>> {
        let host = host_of(&self.endpoint).unwrap_or("the search endpoint");
        let response = self
            .request(query, max, domains)?
            .header("user-agent", concat!("atelier/", env!("CARGO_PKG_VERSION")))
            .follow_redirects(true)
            .connect_timeout(CONNECT_TIMEOUT)
            .max_time(TOTAL_TIMEOUT)
            .send()
            .map_err(|e| anyhow!("could not reach {host} ({e})"))?;

        if !(200..300).contains(&response.status) {
            // The body is left out: it is the provider's, not a diagnosis.
            let hint = match (self.provider, response.status) {
                (Provider::SearXng, 403) => {
                    " — the instance must allow the json format (search.formats in its \
                     settings.yml)"
                }
                (Provider::Brave | Provider::Tavily, 401 | 403) => {
                    " — check the API key in ATELIER_SEARCH_KEY"
                }
                (_, 429) => " — rate limited; wait, or use another provider",
                _ => "",
            };
            bail!(
                "{host} answered HTTP {} {}{hint}",
                response.status,
                response.reason
            );
        }
        let body = String::from_utf8_lossy(&response.body);
        match self.provider {
            Provider::DuckDuckGo => parse_duckduckgo(&body),
            Provider::SearXng => parse_searxng(&body),
            Provider::Brave => parse_brave(&body),
            Provider::Tavily => parse_tavily(&body),
        }
    }

    /// The provider's request for `query`. Only Tavily takes the domain
    /// list as such; the others get it as `site:` operators in the query.
    fn request(&self, query: &str, max: usize, domains: &[String]) -> Result<Request> {
        let sited = with_sites(query, domains);
        let q = percent_encode(&sited);
        let endpoint = &self.endpoint;
        let key = self.key.as_deref().unwrap_or_default();
        let request = match self.provider {
            Provider::DuckDuckGo => Request::get(&format!("{endpoint}?q={q}")),
            Provider::SearXng => Request::get(&format!("{endpoint}?q={q}&format=json")),
            Provider::Brave => Request::get(&format!("{endpoint}?q={q}&count={max}")).map(|r| {
                r.header("accept", "application/json")
                    .header("x-subscription-token", key)
            }),
            Provider::Tavily => {
                let mut body = json!({ "query": query, "max_results": max });
                if !domains.is_empty() {
                    body["include_domains"] = json!(domains);
                }
                Request::new("POST", endpoint).map(|r| {
                    r.header("content-type", "application/json")
                        .header("authorization", &format!("Bearer {key}"))
                        .body(body.to_string())
                })
            }
        };
        request.map_err(|e| anyhow!("bad search URL ({e})"))
    }
}

/// `text` with every occurrence of the key blanked. Nothing here puts the
/// key in a message, but a server or a proxy quoting the request back might.
fn scrub(text: &str, key: Option<&str>) -> String {
    match key {
        Some(key) if !key.is_empty() => text.replace(key, "[key]"),
        _ => text.to_string(),
    }
}

/// `query` restricted to `domains` with `site:` operators.
fn with_sites(query: &str, domains: &[String]) -> String {
    match domains {
        [] => query.to_string(),
        [one] => format!("{query} site:{one}"),
        many => {
            let sites: Vec<String> = many.iter().map(|d| format!("site:{d}")).collect();
            format!("{query} ({})", sites.join(" OR "))
        }
    }
}

/// A domain as the model may write it (`https://Docs.rs/`, `*.docs.rs`)
/// reduced to the bare host.
fn normalize_domain(domain: &str) -> String {
    let d = domain.trim().to_ascii_lowercase();
    let d = d.split_once("://").map_or(d.as_str(), |(_, rest)| rest);
    let d = d.split(['/', '?', '#']).next().unwrap_or_default();
    d.trim_start_matches("*.").trim_matches('.').to_string()
}

/// The host of an absolute URL, without userinfo or port.
fn host_of(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

/// Whether `url` is on one of `domains` or a subdomain of one.
fn in_domains(url: &str, domains: &[String]) -> bool {
    let Some(host) = host_of(url).map(str::to_ascii_lowercase) else {
        return false;
    };
    domains.iter().any(|d| {
        host == *d
            || host
                .strip_suffix(d.as_str())
                .is_some_and(|sub| sub.ends_with('.'))
    })
}

/// The numbered list the model reads.
fn render(query: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!("no results for {query:?}");
    }
    let mut out = String::new();
    for (i, hit) in hits.iter().enumerate() {
        let title = if hit.title.is_empty() {
            "(untitled)"
        } else {
            &hit.title
        };
        out.push_str(&format!("{}. {title}\n   {}\n", i + 1, hit.url));
        if !hit.snippet.is_empty() {
            out.push_str(&format!("   {}\n", hit.snippet));
        }
    }
    out.trim_end().to_string()
}

/// Text out of a provider's markup: tags dropped (Brave and DuckDuckGo bold
/// the matched words), entities decoded, whitespace collapsed, cut at `max`
/// characters.
fn plain(fragment: &str, max: usize) -> String {
    let mut text = String::new();
    let mut rest = fragment;
    while let Some(lt) = rest.find('<') {
        text.push_str(&rest[..lt]);
        rest = rest[lt..].find('>').map_or("", |gt| &rest[lt + gt + 1..]);
    }
    text.push_str(rest);
    let text = decode_entities(&text);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max {
        return text;
    }
    let mut cut: String = text.chars().take(max).collect();
    cut.push('…');
    cut
}

/// SearXNG: `results[].{title,url,content}`.
fn parse_searxng(body: &str) -> Result<Vec<Hit>> {
    json_hits(body, &["results"], "content")
}

/// Brave: `web.results[].{title,url,description}`. A query with no web
/// results has no `web` object at all.
fn parse_brave(body: &str) -> Result<Vec<Hit>> {
    json_hits(body, &["web", "results"], "description")
}

/// Tavily: `results[].{title,url,content}`.
fn parse_tavily(body: &str) -> Result<Vec<Hit>> {
    json_hits(body, &["results"], "content")
}

/// The hits of a JSON response whose result array is at `path`, each an
/// object with `title`, `url` and a snippet under `snippet_key`. A missing
/// array is no results; a body that isn't a JSON object is an error, since
/// it means we aren't talking to the API we think we are.
fn json_hits(body: &str, path: &[&str], snippet_key: &str) -> Result<Vec<Hit>> {
    let root: Value = serde_json::from_str(body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| anyhow!("the response is not the JSON this provider's API returns"))?;
    let results = path.iter().try_fold(&root, |v, key| v.get(key));
    let Some(results) = results.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let text = |v: &Value, key: &str, max: usize| {
        plain(v.get(key).and_then(Value::as_str).unwrap_or_default(), max)
    };
    Ok(results
        .iter()
        .filter_map(|r| {
            let url = r.get("url").and_then(Value::as_str)?.trim();
            (!url.is_empty()).then(|| Hit {
                title: text(r, "title", MAX_TITLE_CHARS),
                url: url.to_string(),
                snippet: text(r, snippet_key, MAX_SNIPPET_CHARS),
            })
        })
        .collect())
}

/// DuckDuckGo's HTML results page. Each result is an `<a class="result__a">`
/// (the title, linking through a `uddg=` redirect) followed, before the next
/// one, by an element of class `result__snippet`.
///
/// A page with no such anchor is either the "no results" page, which says
/// so in a `no-results` element, or something else entirely — a challenge,
/// a redesign — and that is an error, not an empty list.
fn parse_duckduckgo(html: &str) -> Result<Vec<Hit>> {
    let mut anchors = Vec::new();
    let mut from = 0;
    while let Some(found) = classed(html, "result__a", from) {
        from = found.body;
        anchors.push(found);
    }
    if anchors.is_empty() {
        if html.contains("no-results") || html.contains("No results") {
            return Ok(Vec::new());
        }
        bail!(
            "the page DuckDuckGo returned is not a results page (it may be a challenge for \
             automated clients, or a new layout); {CONFIGURE_HINT}"
        );
    }

    let mut hits = Vec::new();
    for (i, anchor) in anchors.iter().enumerate() {
        // Up to the next result: where this one's snippet is.
        let end = anchors.get(i + 1).map_or(html.len(), |next| next.tag);
        let region = &html[anchor.body..end];
        let Some(url) = attr(anchor.text(html), "href").and_then(|h| duckduckgo_target(&h)) else {
            continue;
        };
        let snippet = classed(region, "result__snippet", 0)
            .map(|s| plain(inner(&region[s.body..], &s.name(region)), MAX_SNIPPET_CHARS))
            .unwrap_or_default();
        hits.push(Hit {
            title: plain(inner(region, "a"), MAX_TITLE_CHARS),
            url,
            snippet,
        });
    }
    Ok(hits)
}

/// Where a DuckDuckGo result link really goes: the `uddg=` parameter of its
/// `/l/?uddg=<encoded url>&rut=…` redirect, or the link itself when it is
/// direct. `None` for what isn't a web result — ads go through `/y.js`.
fn duckduckgo_target(href: &str) -> Option<String> {
    let url = match href.split_once("uddg=") {
        Some((_, rest)) => percent_decode(rest.split('&').next().unwrap_or_default()),
        None if href.starts_with("//") => format!("https:{href}"),
        None => href.to_string(),
    };
    let web = url.starts_with("http://") || url.starts_with("https://");
    (web && !url.contains("duckduckgo.com/y.js")).then_some(url)
}

/// An opening tag found by [`classed`]: byte offsets into the searched text
/// of its `<` and of what follows its `>`.
struct Tag {
    tag: usize,
    body: usize,
}

impl Tag {
    /// The tag's text between `<` and `>`.
    fn text<'a>(&self, html: &'a str) -> &'a str {
        &html[self.tag + 1..self.body - 1]
    }

    /// The element's name, lowercased.
    fn name(&self, html: &str) -> String {
        self.text(html)
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    }
}

/// The first opening tag at or after `from` whose `class` attribute lists
/// `class`.
fn classed(html: &str, class: &str, mut from: usize) -> Option<Tag> {
    while let Some(lt) = html[from..].find('<').map(|i| from + i) {
        let gt = lt + html[lt..].find('>')?;
        from = gt + 1;
        let tag = &html[lt + 1..gt];
        // The substring test spares parsing the attributes of every tag.
        if !tag.starts_with('/')
            && tag.contains(class)
            && attr(tag, "class").is_some_and(|c| c.split_whitespace().any(|t| t == class))
        {
            return Some(Tag {
                tag: lt,
                body: from,
            });
        }
    }
    None
}

/// The content of an element whose opening tag was just consumed: `html` up
/// to the first `</name`. Results' titles and snippets hold only inline
/// markup, so there is no nesting of the same element to track.
fn inner<'a>(html: &'a str, name: &str) -> &'a str {
    let close = format!("</{name}");
    let end = html.to_ascii_lowercase().find(&close);
    &html[..end.unwrap_or(html.len())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn hit(title: &str, url: &str, snippet: &str) -> Hit {
        Hit {
            title: title.to_string(),
            url: url.to_string(),
            snippet: snippet.to_string(),
        }
    }

    fn backend(provider: Provider, endpoint: String, key: Option<&str>) -> Backend {
        Backend {
            provider,
            endpoint,
            key: key.map(str::to_string),
        }
    }

    /// Serves one canned response on localhost; hands back the base URL and
    /// a handle yielding the request it received (head and body).
    fn serve_once(
        status: &'static str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            // Read the head, then as much body as it announces, so the
            // client is never cut off mid-send.
            loop {
                let n = stream.read(&mut buf).unwrap();
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let length = text[..head_end]
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                    if request.len() >= head_end + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (base, server)
    }

    const SEARXNG_BODY: &str = r#"{
        "query": "rust borrow checker",
        "number_of_results": 0,
        "results": [
            {
                "url": "https://doc.rust-lang.org/book/ch04-02-references-and-borrowing.html",
                "title": "References and Borrowing - The Rust Programming Language",
                "content": "The rules of references: at any given time, you can have either one mutable reference or any number of immutable references.",
                "engine": "duckduckgo", "parsed_url": ["https", "doc.rust-lang.org", "/book/", "", "", ""],
                "engines": ["duckduckgo", "brave"], "positions": [1, 2], "score": 4.0, "category": "general"
            },
            {
                "url": "https://rustc-dev-guide.rust-lang.org/borrow_check.html",
                "title": "MIR borrow check - Rust Compiler Development Guide",
                "content": "",
                "engine": "brave", "engines": ["brave"], "positions": [3], "score": 0.33, "category": "general"
            },
            { "title": "an infobox-ish entry without a url", "content": "skipped" }
        ],
        "answers": [], "corrections": [], "infoboxes": [], "suggestions": ["rust lifetimes"],
        "unresponsive_engines": [["google", "CAPTCHA"]]
    }"#;

    #[test]
    fn parses_searxng() {
        assert_eq!(
            parse_searxng(SEARXNG_BODY).unwrap(),
            vec![
                hit(
                    "References and Borrowing - The Rust Programming Language",
                    "https://doc.rust-lang.org/book/ch04-02-references-and-borrowing.html",
                    "The rules of references: at any given time, you can have either one \
                     mutable reference or any number of immutable references."
                ),
                hit(
                    "MIR borrow check - Rust Compiler Development Guide",
                    "https://rustc-dev-guide.rust-lang.org/borrow_check.html",
                    ""
                ),
            ]
        );
        assert!(
            parse_searxng(r#"{"query": "zxqj", "results": []}"#)
                .unwrap()
                .is_empty()
        );
        // An HTML page (the instance has no JSON format, a login wall).
        assert!(parse_searxng("<!DOCTYPE html><html>…</html>").is_err());
    }

    #[test]
    fn parses_brave() {
        let body = r#"{
            "type": "search",
            "query": { "original": "tokio select", "more_results_available": true },
            "mixed": { "type": "mixed", "main": [{ "type": "web", "index": 0, "all": false }] },
            "web": {
                "type": "search",
                "results": [
                    {
                        "title": "select in tokio - Rust",
                        "url": "https://docs.rs/tokio/latest/tokio/macro.select.html",
                        "is_source_local": false,
                        "description": "Waits on multiple concurrent branches, returning when the <strong>first</strong> branch completes, cancelling the remaining branches.",
                        "profile": { "name": "Docs.rs", "url": "https://docs.rs" },
                        "language": "en", "family_friendly": true, "type": "search_result",
                        "meta_url": { "scheme": "https", "netloc": "docs.rs", "hostname": "docs.rs" }
                    },
                    {
                        "title": "Select | Tokio - An asynchronous Rust runtime",
                        "url": "https://tokio.rs/tokio/tutorial/select",
                        "description": "So far, when we wanted to add concurrency to the system, we spawned a new task. We will now cover some additional ways to concurrently execute asynchronous code with Tokio &amp; friends.",
                        "type": "search_result"
                    }
                ],
                "family_friendly": true
            }
        }"#;
        let hits = parse_brave(body).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].url,
            "https://docs.rs/tokio/latest/tokio/macro.select.html"
        );
        assert_eq!(
            hits[0].snippet,
            "Waits on multiple concurrent branches, returning when the first branch \
             completes, cancelling the remaining branches."
        );
        assert!(hits[1].snippet.ends_with("with Tokio & friends."));
        // No web results: Brave leaves `web` out.
        let none = r#"{"type": "search", "query": {"original": "zxqj"}}"#;
        assert!(parse_brave(none).unwrap().is_empty());
        assert!(parse_brave("Unauthorized").is_err());
    }

    #[test]
    fn parses_tavily() {
        let body = r#"{
            "query": "sqlite wal mode",
            "follow_up_questions": null,
            "answer": null,
            "images": [],
            "results": [
                {
                    "url": "https://www.sqlite.org/wal.html",
                    "title": "Write-Ahead Logging",
                    "content": "The default method by which SQLite implements atomic commit and rollback is a rollback journal. Beginning with version 3.7.0, a new \"Write-Ahead Log\" option is available.",
                    "score": 0.98, "raw_content": null
                },
                {
                    "url": "https://sqlite.org/pragma.html#pragma_journal_mode",
                    "title": "Pragma statements supported by SQLite",
                    "content": "PRAGMA journal_mode = WAL",
                    "score": 0.71, "raw_content": null
                }
            ],
            "response_time": 1.12,
            "request_id": "6d0f9c1e-0000-4000-8000-000000000000"
        }"#;
        let hits = parse_tavily(body).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Write-Ahead Logging");
        assert!(hits[0].snippet.contains("\"Write-Ahead Log\" option"));
        assert_eq!(
            hits[1].url,
            "https://sqlite.org/pragma.html#pragma_journal_mode"
        );
    }

    /// Trimmed from a real html.duckduckgo.com page: one ad, two results.
    const DUCKDUCKGO_PAGE: &str = r##"<!DOCTYPE html>
<html lang="en-US"><head><title>rust error handling at DuckDuckGo</title></head>
<body class="body--html">
  <form class="header__form" action="/html/" method="post">
    <input class="search__input" type="text" name="q" value="rust error handling">
  </form>
  <div class="serp__results">
  <div id="links" class="results">

    <div class="result results_links results_links_deep result--ad ">
      <div class="links_main links_deep result__body">
        <h2 class="result__title">
          <a rel="nofollow" class="result__a" href="https://duckduckgo.com/y.js?ad_domain=example.com&amp;ad_provider=bingv7aa&amp;u3=https%3A%2F%2Fexample.com%2Flanding">Sponsored: Learn Rust Fast</a>
        </h2>
        <a class="result__snippet" href="https://duckduckgo.com/y.js?ad_domain=example.com">Buy our course.</a>
      </div>
    </div>

    <div class="result results_links results_links_deep web-result ">
      <div class="links_main links_deep result__body">
        <h2 class="result__title">
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2Fbook%2Fch09%2D00%2Derror%2Dhandling.html&amp;rut=0f5c1e2d3b4a59687766554433221100ffeeddccbbaa99887766554433221100">Error Handling - The <b>Rust</b> Programming Language</a>
        </h2>
        <div class="result__extras">
          <div class="result__extras__url">
            <span class="result__icon">
              <a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2Fbook%2Fch09%2D00%2Derror%2Dhandling.html&amp;rut=0f5c">
                <img class="result__icon__img" width="16" height="16" alt="" src="//external-content.duckduckgo.com/ip3/doc.rust-lang.org.ico" name="i15" />
              </a>
            </span>
            <a class="result__url" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2Fbook%2Fch09%2D00%2Derror%2Dhandling.html&amp;rut=0f5c">
              doc.rust-lang.org/book/ch09-00-error-handling.html
            </a>
          </div>
        </div>
        <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2Fbook%2Fch09%2D00%2Derror%2Dhandling.html&amp;rut=0f5c"><b>Rust</b> groups errors into two major categories: recoverable and unrecoverable errors. For a recoverable <b>error</b>, such as a file not found error, we most likely just want to&nbsp;report the problem.</a>
        <div class="clear"></div>
      </div>
    </div>

    <div class="result results_links results_links_deep web-result ">
      <div class="links_main links_deep result__body">
        <h2 class="result__title">
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.example.org%2Fblog%2Frust%2Derrors%3Fref%3Dddg%26lang%3Den&amp;rut=aa11">A Guide to &quot;?&quot; &amp; Result</a>
        </h2>
        <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.example.org%2Fblog%2Frust%2Derrors%3Fref%3Dddg%26lang%3Den&amp;rut=aa11">How the question-mark
          operator propagates <b>errors</b>.</a>
        <div class="clear"></div>
      </div>
    </div>

    <div class="nav-link">
      <form action="/html/" method="post"><input type="submit" class="btn btn--alt" value="Next" /></form>
    </div>
  </div>
  </div>
</body></html>"##;

    #[test]
    fn parses_duckduckgo_results_and_skips_ads() {
        assert_eq!(
            parse_duckduckgo(DUCKDUCKGO_PAGE).unwrap(),
            vec![
                hit(
                    "Error Handling - The Rust Programming Language",
                    "https://doc.rust-lang.org/book/ch09-00-error-handling.html",
                    "Rust groups errors into two major categories: recoverable and \
                     unrecoverable errors. For a recoverable error, such as a file not found \
                     error, we most likely just want to report the problem."
                ),
                hit(
                    "A Guide to \"?\" & Result",
                    "https://www.example.org/blog/rust-errors?ref=ddg&lang=en",
                    "How the question-mark operator propagates errors."
                ),
            ]
        );
    }

    #[test]
    fn duckduckgo_page_without_results() {
        // The real "nothing found" page is an empty list…
        let none = r#"<html><body><div id="links" class="results">
            <div class="result results_links results_links_deep result--no-result">
              <div class="links_main links_deep"><div class="no-results">No results.</div></div>
            </div></div></body></html>"#;
        assert!(parse_duckduckgo(none).unwrap().is_empty());

        // …but a challenge page is an error that says what to do, never an
        // empty (or invented) result list.
        let challenge = r#"<html><body><form id="challenge-form" action="//duckduckgo.com/anomaly.js">
            <div class="anomaly-modal__title">Unfortunately, bots use DuckDuckGo too.</div>
            <div class="anomaly-modal__description">Please complete the following challenge.</div>
            </form></body></html>"#;
        let err = parse_duckduckgo(challenge).unwrap_err().to_string();
        assert!(err.contains("not a results page"), "{err}");
        assert!(err.contains("ATELIER_SEARCH_PROVIDER"), "{err}");
    }

    #[test]
    fn duckduckgo_links_are_unwrapped() {
        assert_eq!(
            duckduckgo_target(
                "//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.example%2Fx%3Fy%3D1&rut=ff"
            )
            .as_deref(),
            Some("https://a.example/x?y=1")
        );
        assert_eq!(
            duckduckgo_target("https://direct.example/page").as_deref(),
            Some("https://direct.example/page")
        );
        assert_eq!(
            duckduckgo_target("//lite.example/p").as_deref(),
            Some("https://lite.example/p")
        );
        assert!(duckduckgo_target("https://duckduckgo.com/y.js?ad_domain=x").is_none());
        assert!(duckduckgo_target("javascript:void(0)").is_none());
    }

    #[test]
    fn renders_a_compact_numbered_list() {
        let hits = [
            hit("First", "https://a.example/1", "About the first."),
            hit("", "https://b.example/2", ""),
        ];
        assert_eq!(
            render("q", &hits),
            "1. First\n   https://a.example/1\n   About the first.\n\
             2. (untitled)\n   https://b.example/2"
        );
        assert_eq!(render("zxqj", &[]), "no results for \"zxqj\"");
        // Long snippets are cut, on a character boundary.
        let long = plain(&"é".repeat(400), MAX_SNIPPET_CHARS);
        assert_eq!(long.chars().count(), MAX_SNIPPET_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn domains_are_normalized_and_enforced() {
        assert_eq!(normalize_domain(" https://Docs.RS/tokio "), "docs.rs");
        assert_eq!(normalize_domain("*.rust-lang.org"), "rust-lang.org");
        let domains = vec!["docs.rs".to_string(), "rust-lang.org".to_string()];
        assert!(in_domains("https://docs.rs/tokio", &domains));
        assert!(in_domains("https://doc.rust-lang.org:443/book/", &domains));
        assert!(!in_domains("https://notdocs.rs/", &domains));
        assert!(!in_domains("https://docs.rs.evil.example/", &domains));
        assert_eq!(with_sites("q", &[]), "q");
        assert_eq!(with_sites("q", &domains[..1]), "q site:docs.rs");
        assert_eq!(
            with_sites("q", &domains),
            "q (site:docs.rs OR site:rust-lang.org)"
        );
    }

    fn env_of(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn resolves_the_provider_from_env_and_settings() {
        let none = SearchSettings::default();

        // Nothing configured: DuckDuckGo, no key.
        let b = resolve(&env_of(&[]), &none).unwrap();
        assert_eq!(b.provider, Provider::DuckDuckGo);
        assert_eq!(b.endpoint, DUCKDUCKGO_URL);
        assert!(b.key.is_none());

        // A URL alone means SearXNG.
        let b = resolve(
            &env_of(&[("ATELIER_SEARCH_URL", "https://searx.example/")]),
            &none,
        )
        .unwrap();
        assert_eq!(b.provider, Provider::SearXng);
        assert_eq!(b.endpoint, "https://searx.example/search");

        // [search] names the variable holding the key, never the key.
        let settings = SearchSettings {
            provider: Some("tavily".to_string()),
            url: None,
            key_env: Some("MY_TAVILY_KEY".to_string()),
        };
        let b = resolve(&env_of(&[("MY_TAVILY_KEY", "tvly-abc")]), &settings).unwrap();
        assert_eq!(b.provider, Provider::Tavily);
        assert_eq!(b.endpoint, TAVILY_URL);
        assert_eq!(b.key.as_deref(), Some("tvly-abc"));

        // The environment overrides the file.
        let b = resolve(
            &env_of(&[
                ("ATELIER_SEARCH_PROVIDER", "Brave"),
                ("ATELIER_SEARCH_KEY", "BSA-xyz"),
                ("MY_TAVILY_KEY", "tvly-abc"),
            ]),
            &settings,
        )
        .unwrap();
        assert_eq!(b.provider, Provider::Brave);
        assert_eq!(b.key.as_deref(), Some("BSA-xyz"));

        // What can't work says why, up front.
        let err = |env: &dyn Fn(&str) -> Option<String>, s: &SearchSettings| {
            resolve(env, s).err().unwrap().to_string()
        };
        assert!(err(&env_of(&[]), &settings).contains("needs an API key"));
        assert!(
            err(&env_of(&[("ATELIER_SEARCH_PROVIDER", "searxng")]), &none)
                .contains("ATELIER_SEARCH_URL")
        );
        assert!(
            err(&env_of(&[("ATELIER_SEARCH_PROVIDER", "altavista")]), &none)
                .contains("unknown search provider")
        );
    }

    /// The key in the user's environment goes to whatever endpoint is
    /// resolved, so an endpoint named by a project's file counts only once
    /// the project is trusted — as `call` resolves it.
    #[test]
    fn an_untrusted_projects_endpoint_is_not_sent_the_users_key() {
        use crate::trust::{Store, Trust, testing};
        let root = testing::dir("search-endpoint");
        let settings = Settings {
            search: SearchSettings {
                provider: Some("brave".to_string()),
                url: Some("https://evil.example/collect".to_string()),
                key_env: None,
            },
            ..Settings::default()
        };
        let env = env_of(&[("ATELIER_SEARCH_KEY", "BSA-xyz")]);

        let untrusted = Trust::load(&root, &settings, Store::default());
        let b = resolve(&env, &untrusted.effective(&settings).search).unwrap();
        assert_eq!(b.endpoint, BRAVE_URL);
        assert_eq!(b.key.as_deref(), Some("BSA-xyz"));

        let accepted = Store {
            accept_all: true,
            ..Store::default()
        };
        let trusted = Trust::load(&root, &settings, accepted);
        let b = resolve(&env, &trusted.effective(&settings).search).unwrap();
        assert_eq!(b.endpoint, "https://evil.example/collect");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn searxng_round_trip_against_a_local_server() {
        let (base, server) = serve_once("200 OK", SEARXNG_BODY);
        let b = backend(Provider::SearXng, format!("{base}/search"), None);
        let hits = b
            .search("borrow checker", 5, &["rust-lang.org".to_string()])
            .unwrap();
        let request = server.join().unwrap();
        assert!(
            request
                .starts_with("GET /search?q=borrow%20checker%20site%3Arust-lang.org&format=json "),
            "{request}"
        );
        assert_eq!(hits.len(), 2);
        assert!(render("borrow checker", &hits).starts_with(
            "1. References and Borrowing - The Rust Programming Language\n   \
             https://doc.rust-lang.org/book/ch04-02-references-and-borrowing.html\n   The rules"
        ));
    }

    #[test]
    fn results_are_capped_and_filtered_to_the_domains() {
        let (base, server) = serve_once("200 OK", SEARXNG_BODY);
        let b = backend(Provider::SearXng, format!("{base}/search"), None);
        let hits = b
            .search("q", 1, &["rustc-dev-guide.rust-lang.org".to_string()])
            .unwrap();
        server.join().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].url,
            "https://rustc-dev-guide.rust-lang.org/borrow_check.html"
        );
    }

    #[test]
    fn brave_sends_the_key_as_a_header_only() {
        let (base, server) = serve_once(
            "200 OK",
            r#"{"web": {"results": [{"title": "T", "url": "https://t.example/", "description": "d"}]}}"#,
        );
        let b = backend(
            Provider::Brave,
            format!("{base}/web"),
            Some("BSA-secret-123"),
        );
        let hits = b.search("a b", 3, &[]).unwrap();
        let request = server.join().unwrap();
        assert_eq!(hits, vec![hit("T", "https://t.example/", "d")]);
        let (line, headers) = request.split_once("\r\n").unwrap();
        assert!(line.starts_with("GET /web?q=a%20b&count=3 "), "{line}");
        assert!(!line.contains("BSA-secret-123"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("x-subscription-token: bsa-secret-123"),
            "{headers}"
        );
    }

    #[test]
    fn tavily_posts_json_and_errors_never_show_the_key() {
        // A server that quotes the credential back in its refusal.
        let (base, server) = serve_once(
            "401 Unauthorized",
            r#"{"detail": {"error": "Unauthorized: invalid API key tvly-secret-456"}}"#,
        );
        let b = backend(
            Provider::Tavily,
            format!("{base}/search"),
            Some("tvly-secret-456"),
        );
        let err = b
            .search("wal mode", 4, &["sqlite.org".to_string()])
            .unwrap_err()
            .to_string();
        let request = server.join().unwrap();

        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST /search "), "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: bearer tvly-secret-456"),
            "{head}"
        );
        let sent: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            sent,
            json!({"query": "wal mode", "max_results": 4, "include_domains": ["sqlite.org"]})
        );

        assert!(!err.contains("tvly-secret-456"), "{err}");
        assert!(
            err.starts_with("tavily search failed: 127.0.0.1 answered HTTP 401"),
            "{err}"
        );
        assert!(err.contains("ATELIER_SEARCH_KEY"), "{err}");
        assert_eq!(scrub("token=abc&x", Some("abc")), "token=[key]&x");
    }

    #[test]
    fn an_unreachable_provider_fails_short_and_clear() {
        // Bind then drop, so the port is (almost certainly) refused.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let b = backend(Provider::SearXng, format!("{base}/search"), None);
        let err = b.search("q", 3, &[]).unwrap_err().to_string();
        assert!(
            err.starts_with("searxng search failed: could not reach 127.0.0.1"),
            "{err}"
        );
        assert_eq!(err.lines().count(), 1, "{err}");
        assert!(err.len() < 200, "{err}");
    }

    #[test]
    fn missing_query_errors() {
        let root = std::env::temp_dir();
        let mut fstate = crate::tools::FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: true,
        };
        assert!(WebSearchTool.call(&mut ctx, json!({})).is_err());
        assert!(
            WebSearchTool
                .call(&mut ctx, json!({"query": "  "}))
                .is_err()
        );
    }
}
