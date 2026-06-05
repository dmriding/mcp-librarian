use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Paths;

pub const CACHE_TTL_SECS: i64 = 7 * 24 * 60 * 60;
pub const PER_DOMAIN_MIN_INTERVAL_MS: u64 = 1000;
pub const DEFAULT_MAX_CHARS: usize = 20_000;
pub const MAX_MAX_CHARS: usize = 50_000;
pub const PER_SESSION_URL_CAP: usize = 50;
pub const HTTP_TIMEOUT_SECS: u64 = 15;

/// Hard cap on bytes read per response, regardless of what the agent passes
/// for `max_chars`. Protects against a misbehaving (or malicious) server
/// streaming gigabytes — currently `response.text()` would buffer the whole
/// body before truncation kicks in. 5 MB is generous for any documentation
/// page and small enough that a stuck or pathological response can't OOM
/// the process.
pub const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;

/// Polite UA so vendor admins seeing this in logs can identify the source
/// and trace it back to the project if there's a problem.
pub const USER_AGENT: &str = concat!(
    "mcp-librarian/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/dmriding/mcp-librarian)"
);

/// Shared mutable state for `librarian_fetch_docs`. Lives on the LibrarianServer.
#[derive(Clone, Default)]
pub struct FetchState {
    last_fetch_by_domain: Arc<Mutex<HashMap<String, Instant>>>,
    urls_fetched_this_session: Arc<Mutex<HashSet<String>>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedDoc {
    url: String,
    fetched_at: DateTime<Utc>,
    content: String,
}

#[derive(Debug)]
pub struct FetchOutcome {
    pub url: String,
    pub content: String,
    pub source_size: usize,
    pub returned_size: usize,
    pub truncated: bool,
    pub from_cache: bool,
    pub cache_age_secs: i64,
    pub cache_path: PathBuf,
}

/// True if the URL looks like raw markdown (so we skip HTML extraction).
pub fn is_markdown_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.ends_with(".md")
        || lower.ends_with(".mdx")
        || lower.ends_with(".markdown")
        || lower.contains("raw.githubusercontent.com")
}

fn cache_key_for(url: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    format!("{:016x}.json", h.finish())
}

pub fn cache_path_for(paths: &Paths, url: &str) -> PathBuf {
    paths.docs_cache_dir.join(cache_key_for(url))
}

fn read_cache(path: &Path) -> Result<Option<CachedDoc>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let doc: CachedDoc =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    let age = (Utc::now() - doc.fetched_at).num_seconds();
    if age > CACHE_TTL_SECS {
        return Ok(None);
    }
    Ok(Some(doc))
}

fn write_cache(path: &Path, doc: &CachedDoc) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(doc)?;
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Convert HTML to plain text. Wide width so we don't impose line wrapping —
/// the agent doesn't care about visual layout.
pub fn extract_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 10_000)
}

fn domain_of(url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("parsing URL `{url}`"))?;
    parsed
        .host_str()
        .ok_or_else(|| anyhow!("URL has no host: `{url}`"))
        .map(|s| s.to_lowercase())
}

/// Synchronous URL validation: scheme allowlist + reject literal IPs in
/// private/loopback ranges + reject obvious loopback hostnames. This is the
/// cheap defense; `resolve_and_check` follows up with a DNS lookup to catch
/// hostnames that resolve to private addresses.
///
/// Called both at the entry to `fetch_docs` (on the agent-supplied URL) and
/// inside the reqwest redirect callback so a 30x → localhost redirect is
/// caught before reqwest dials it.
pub fn check_url_sync(url: &reqwest::Url) -> Result<()> {
    match url.scheme() {
        "http" | "https" => {}
        other => bail!(
            "Error: scheme `{other}://` is not allowed in `librarian_fetch_docs` (`{url}`). \
             Action: only http and https URLs are supported. file://, ftp://, javascript:, \
             and others are rejected — this tool fetches public docs, not local files or \
             internal services."
        ),
    }
    let host = url.host_str().ok_or_else(|| {
        anyhow!(
            "Error: URL `{url}` has no host. Action: provide an absolute URL like \
             `https://docs.example.com/...`."
        )
    })?;
    // `host_str` returns bracketed form for IPv6 literals (e.g. `[::1]`).
    // Strip them before parsing so we recognize v6 IP literals.
    let host_for_parse = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    // If the host parses as an IP literal, check it directly. Otherwise it's
    // a domain name — string-screen the common loopback aliases here, and
    // the DNS-level check in `resolve_and_check` handles the general case.
    if let Ok(ip) = host_for_parse.parse::<IpAddr>() {
        if let Some(reason) = blocked_ip_reason(&ip) {
            bail!(
                "Error: refusing to fetch `{url}` — host literal `{ip}` is {reason}. \
                 Action: this tool only fetches public documentation URLs. SSRF guard \
                 blocks loopback, link-local, private, and metadata-service addresses."
            );
        }
    } else {
        let lower = host.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "localhost" | "localhost.localdomain" | "ip6-localhost" | "ip6-loopback"
        ) {
            bail!(
                "Error: refusing to fetch `{url}` — `{host}` is a loopback hostname. \
                 Action: only public documentation hosts are supported."
            );
        }
    }
    Ok(())
}

/// Async: DNS-resolve the host and reject if any returned IP is in a blocked
/// range. Catches the case where a hostname like `localtest.me` resolves to
/// `127.0.0.1`, or an attacker-controlled domain points at a private IP.
///
/// Limitation: this is one-shot. The actual reqwest fetch resolves again,
/// so a DNS-rebinding attacker could in theory return a public IP here and
/// a private IP for the real fetch. Mitigated for the most common cases by
/// the redirect-policy re-validation and is documented in the README.
pub async fn resolve_and_check(url: &reqwest::Url) -> Result<()> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("URL has no host: `{url}`"))?;
    // If it's an IP literal, `check_url_sync` already handled it.
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let port = url
        .port()
        .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
    let host_port = format!("{host}:{port}");
    let addrs: Vec<_> = tokio::net::lookup_host(&host_port)
        .await
        .with_context(|| format!("resolving `{host}`"))?
        .collect();
    if addrs.is_empty() {
        bail!("Error: host `{host}` resolved to no addresses.");
    }
    for addr in &addrs {
        let ip = addr.ip();
        if let Some(reason) = blocked_ip_reason(&ip) {
            bail!(
                "Error: refusing to fetch `{url}` — host `{host}` resolves to {ip} which is {reason}. \
                 Action: only public documentation URLs are supported. If you intended an internal \
                 service, this tool can't reach it by design (SSRF guard)."
            );
        }
    }
    Ok(())
}

/// Classify an IP into a block reason, or None if it's a safe public address.
/// Covers loopback, link-local, private (RFC1918), CGNAT (100.64.0.0/10),
/// this-network (0.0.0.0/8), multicast, broadcast, and unspecified — plus
/// the IPv6 equivalents (loopback ::1, link-local fe80::/10, unique-local
/// fc00::/7) and v4-mapped v6 addresses.
fn blocked_ip_reason(ip: &IpAddr) -> Option<&'static str> {
    if ip.is_unspecified() {
        return Some("the unspecified address (0.0.0.0 / ::)");
    }
    if ip.is_loopback() {
        return Some("a loopback address");
    }
    if ip.is_multicast() {
        return Some("a multicast address");
    }
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_private() {
                return Some("an RFC1918 private address");
            }
            if v4.is_link_local() {
                // 169.254.0.0/16 — includes the AWS / GCP / Azure metadata IPs.
                return Some("a link-local address (includes cloud metadata services)");
            }
            if v4.is_broadcast() {
                return Some("the broadcast address");
            }
            let octets = v4.octets();
            // 0.0.0.0/8 — "this network" (RFC 6890)
            if octets[0] == 0 {
                return Some("in the 0.0.0.0/8 \"this network\" range");
            }
            // 100.64.0.0/10 — CGNAT (RFC 6598)
            if octets[0] == 100 && (octets[1] & 0xC0) == 0x40 {
                return Some("in the carrier-grade NAT range (100.64/10)");
            }
            None
        }
        IpAddr::V6(v6) => {
            let segs = v6.segments();
            // Unique local fc00::/7
            if (segs[0] & 0xfe00) == 0xfc00 {
                return Some("an IPv6 unique-local address (fc00::/7)");
            }
            // Link local fe80::/10
            if (segs[0] & 0xffc0) == 0xfe80 {
                return Some("an IPv6 link-local address (fe80::/10)");
            }
            // IPv4-mapped/translated — recursively check the embedded v4.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return blocked_ip_reason(&IpAddr::V4(v4));
            }
            None
        }
    }
}

fn check_url_cap(state: &FetchState, url: &str) -> Result<()> {
    let mut set = state
        .urls_fetched_this_session
        .lock()
        .expect("urls_fetched_this_session poisoned");
    if set.contains(url) {
        return Ok(());
    }
    if set.len() >= PER_SESSION_URL_CAP {
        bail!(
            "Error: this MCP session has already fetched {PER_SESSION_URL_CAP} distinct URLs (the \
             per-session cap). Action: URLs already fetched this session can be re-fetched freely \
             from cache; only NEW URLs count against the cap. For more new fetches, restart the \
             session OR fall back to `librarian_seed_playbook` using the tool list you can see in \
             your deferred-tools reminder."
        );
    }
    set.insert(url.to_string());
    Ok(())
}

/// Hard upper bound on how long a single `librarian_fetch_docs` call will
/// wait for the per-domain rate limit before giving up. Each in-call URL
/// claims the next 1-second slot, so 16 same-domain URLs (the `extra_urls`
/// cap) queue up at most ~16 seconds; 30 seconds is the safety margin.
const MAX_RATE_LIMIT_WAIT_SECS: u64 = 30;

/// Wait until this caller's reserved slot in the per-domain rate-limit queue
/// arrives, then return Ok. The slot is reserved synchronously under the lock
/// BEFORE the sleep, so concurrent callers see the updated `last` and queue
/// behind us — no thundering-herd at the moment the slot opens.
///
/// This replaces the older bail-on-recent-fetch behavior so `extra_urls`
/// batches against a single vendor domain (the common case the README
/// recommends) work without surfacing "rate-limited, retry" errors. The
/// hard cap above prevents runaway waits if the queue gets pathological.
async fn wait_for_rate_limit(state: &FetchState, domain: &str) -> Result<()> {
    let wait = {
        let mut map = state
            .last_fetch_by_domain
            .lock()
            .expect("last_fetch_by_domain poisoned");
        let now = Instant::now();
        let min = Duration::from_millis(PER_DOMAIN_MIN_INTERVAL_MS);
        let target = match map.get(domain) {
            Some(last) if *last + min > now => *last + min,
            _ => now,
        };
        map.insert(domain.to_string(), target);
        target.saturating_duration_since(now)
    };
    if wait > Duration::from_secs(MAX_RATE_LIMIT_WAIT_SECS) {
        bail!(
            "Error: rate-limit queue for domain `{domain}` would force a wait of {}s, \
             over the {MAX_RATE_LIMIT_WAIT_SECS}s cap. \
             Action: fetch fewer URLs from this domain in a single call, or split into \
             multiple calls. The polite-citizen cap is 1 request per second per domain.",
            wait.as_secs(),
        );
    }
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    Ok(())
}

/// Read a reqwest Response body with a hard byte ceiling. Streams chunks,
/// accumulates into a Vec, and bails as soon as the running total exceeds
/// `max_bytes`. This is the defense against a server that streams unbounded
/// data when `Content-Length` was missing or lied about.
///
/// On the happy path (response under cap) this is equivalent to `.bytes()`
/// followed by `String::from_utf8_lossy` — a small extra cost we accept for
/// the safety guarantee.
async fn read_body_bounded(response: reqwest::Response, max_bytes: usize) -> Result<String> {
    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(8192.min(max_bytes));
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("streaming response body")?;
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            bail!(
                "Error: response body exceeded {max_bytes} bytes (per-response cap). \
                 Action: this tool is for documentation pages, not bulk downloads. \
                 Try a more specific URL that returns just the relevant section."
            );
        }
        buf.extend_from_slice(&chunk);
    }
    // Decode as UTF-8 with replacement — docs pages occasionally have stray
    // non-UTF8 bytes (windows-1252 escapes, etc.) and a hard failure here is
    // less useful than a best-effort decode.
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Truncate a string at a UTF-8 char boundary, never mid-codepoint.
fn truncate_to(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        (s, false)
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        (&s[..end], true)
    }
}

/// Fetch a docs URL with cache, rate-limit, per-session cap, and SSRF guard.
pub async fn fetch_docs(
    paths: &Paths,
    state: &FetchState,
    url: &str,
    max_chars: usize,
) -> Result<FetchOutcome> {
    let max_chars = max_chars.clamp(500, MAX_MAX_CHARS);

    // 0. SSRF guard — sync URL screen (scheme + literal IP). Done before
    //    cache lookup so a poisoned cache file with a private URL can't
    //    accidentally serve content. The cache_key is hash(url), so if an
    //    agent passes the same private URL twice, both fail here.
    let parsed = reqwest::Url::parse(url).with_context(|| format!("parsing URL `{url}`"))?;
    check_url_sync(&parsed)?;

    // 1. Cache hit short-circuits everything — free, no cap accounting.
    let cache_path = cache_path_for(paths, url);
    if let Some(cached) = read_cache(&cache_path)? {
        let age_secs = (Utc::now() - cached.fetched_at).num_seconds().max(0);
        let (returned, truncated) = truncate_to(&cached.content, max_chars);
        return Ok(FetchOutcome {
            url: cached.url,
            content: returned.to_string(),
            source_size: cached.content.len(),
            returned_size: returned.len(),
            truncated,
            from_cache: true,
            cache_age_secs: age_secs,
            cache_path,
        });
    }

    // 2. New URL — count it against the session cap, then check domain rate.
    check_url_cap(state, url)?;
    let domain = domain_of(url)?;
    wait_for_rate_limit(state, &domain).await?;

    // 3. DNS-level SSRF check now that we're about to actually fetch. Done
    //    AFTER the cache check so private URLs that came in earlier and are
    //    now in cache (theoretically impossible since check_url_sync would
    //    have blocked the prior write) don't pay the resolve cost.
    resolve_and_check(&parsed).await?;

    // 4. HTTP fetch. Redirect policy re-validates each hop's URL so a 30x
    //    bouncing to localhost/private is blocked before reqwest follows it.
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many redirects (max 5)");
            }
            match check_url_sync(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(format!("{e:#}")),
            }
        }))
        .build()
        .context("building HTTP client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching `{url}`"))?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "Error: HTTP {} fetching `{url}`. \
             Action: check the URL spelling. If the page is behind auth, this tool can't reach it \
             (public docs only). If the vendor docs are JS-only SPA, this tool won't see the \
             rendered content — fall back to `librarian_seed_playbook` from the tool list you can \
             see in your deferred-tools reminder.",
            status.as_u16()
        );
    }

    // Fast-fail on declared content length: if the server tells us up front
    // that the body exceeds our ceiling, refuse before streaming. Optional —
    // many servers omit Content-Length, in which case we fall through to the
    // streaming check below.
    if let Some(len) = response.content_length()
        && len as usize > MAX_RESPONSE_BYTES
    {
        bail!(
            "Error: response from `{url}` declares Content-Length {len}, which exceeds the \
             librarian's per-response cap of {MAX_RESPONSE_BYTES} bytes. \
             Action: this tool is for documentation pages, not bulk downloads."
        );
    }

    // Stream the body with an enforced byte ceiling. We can't trust the
    // server to honor Content-Length, so we count bytes as they arrive and
    // bail the moment we exceed the cap.
    let body = read_body_bounded(response, MAX_RESPONSE_BYTES)
        .await
        .with_context(|| format!("reading body from `{url}`"))?;

    // 4. Extract: raw-markdown shortcut for *.md / GitHub raw, else HTML→text.
    let content = if is_markdown_url(url) {
        body
    } else {
        extract_text(&body)
    };
    let content = content.trim().to_string();

    // 5. Cache the full (untruncated) content — re-fetches with larger max_chars
    //    can serve more without going back to the network.
    let cached = CachedDoc {
        url: url.to_string(),
        fetched_at: Utc::now(),
        content: content.clone(),
    };
    write_cache(&cache_path, &cached)?;

    // 6. Truncate for return.
    let (returned, truncated) = truncate_to(&content, max_chars);
    Ok(FetchOutcome {
        url: url.to_string(),
        content: returned.to_string(),
        source_size: content.len(),
        returned_size: returned.len(),
        truncated,
        from_cache: false,
        cache_age_secs: 0,
        cache_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    #[test]
    fn check_url_sync_rejects_non_http_schemes() {
        assert!(check_url_sync(&url("file:///etc/passwd")).is_err());
        // ftp is parseable by reqwest::Url even though it can't fetch it
        assert!(check_url_sync(&url("ftp://example.com/x")).is_err());
        // javascript: parses as a non-host URL; we reject via the scheme check
        if let Ok(u) = reqwest::Url::parse("javascript:alert(1)") {
            assert!(check_url_sync(&u).is_err());
        }
    }

    #[test]
    fn check_url_sync_accepts_public_http_and_https() {
        check_url_sync(&url("https://docs.example.com/foo")).unwrap();
        check_url_sync(&url("http://docs.example.com/foo")).unwrap();
    }

    #[test]
    fn check_url_sync_rejects_loopback_literals() {
        assert!(check_url_sync(&url("http://127.0.0.1/admin")).is_err());
        assert!(check_url_sync(&url("https://127.0.0.1:8443/")).is_err());
        assert!(check_url_sync(&url("http://[::1]/")).is_err());
    }

    #[test]
    fn check_url_sync_rejects_private_ranges() {
        assert!(check_url_sync(&url("http://10.0.0.1/")).is_err());
        assert!(check_url_sync(&url("http://10.255.255.255/")).is_err());
        assert!(check_url_sync(&url("http://172.16.0.1/")).is_err());
        assert!(check_url_sync(&url("http://172.31.0.1/")).is_err());
        assert!(check_url_sync(&url("http://192.168.1.1/")).is_err());
        // 100.64/10 — CGNAT
        assert!(check_url_sync(&url("http://100.64.0.1/")).is_err());
        // 0.0.0.0/8 — this network
        assert!(check_url_sync(&url("http://0.0.0.0/")).is_err());
        assert!(check_url_sync(&url("http://0.1.2.3/")).is_err());
    }

    #[test]
    fn check_url_sync_rejects_link_local_and_metadata() {
        // Link-local 169.254/16 — includes 169.254.169.254 (AWS / Azure / GCP metadata)
        assert!(check_url_sync(&url("http://169.254.169.254/latest/meta-data/")).is_err());
        assert!(check_url_sync(&url("http://169.254.1.1/")).is_err());
        // IPv6 link-local
        assert!(check_url_sync(&url("http://[fe80::1]/")).is_err());
        // IPv6 unique-local
        assert!(check_url_sync(&url("http://[fc00::1]/")).is_err());
        assert!(check_url_sync(&url("http://[fd00::1]/")).is_err());
    }

    #[test]
    fn check_url_sync_rejects_loopback_hostnames() {
        assert!(check_url_sync(&url("http://localhost/admin")).is_err());
        assert!(check_url_sync(&url("http://LOCALHOST:8080/")).is_err());
        assert!(check_url_sync(&url("http://localhost.localdomain/")).is_err());
        assert!(check_url_sync(&url("http://ip6-localhost/")).is_err());
    }

    #[test]
    fn check_url_sync_rejects_ipv4_mapped_v6_private() {
        // ::ffff:127.0.0.1 — IPv4-mapped IPv6 of loopback
        assert!(check_url_sync(&url("http://[::ffff:127.0.0.1]/")).is_err());
        // ::ffff:10.0.0.1 — IPv4-mapped IPv6 of private range
        assert!(check_url_sync(&url("http://[::ffff:10.0.0.1]/")).is_err());
    }

    #[test]
    fn blocked_ip_reason_identifies_each_class() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        assert!(blocked_ip_reason(&IpAddr::from(Ipv4Addr::new(127, 0, 0, 1))).is_some());
        assert!(blocked_ip_reason(&IpAddr::from(Ipv4Addr::new(192, 168, 1, 1))).is_some());
        assert!(blocked_ip_reason(&IpAddr::from(Ipv4Addr::new(169, 254, 169, 254))).is_some());
        assert!(blocked_ip_reason(&IpAddr::from(Ipv6Addr::from_str("::1").unwrap())).is_some());
        assert!(blocked_ip_reason(&IpAddr::from(Ipv6Addr::from_str("fe80::1").unwrap())).is_some());
        // Pass-through cases — non-blocked public addresses
        assert!(blocked_ip_reason(&IpAddr::from(Ipv4Addr::new(8, 8, 8, 8))).is_none());
        assert!(blocked_ip_reason(&IpAddr::from(Ipv4Addr::new(1, 1, 1, 1))).is_none());
        assert!(
            blocked_ip_reason(&IpAddr::from(Ipv6Addr::from_str("2606:4700::1").unwrap())).is_none()
        );
    }

    #[test]
    fn cache_key_is_stable_and_distinct() {
        let a1 = cache_key_for("https://a.com/foo");
        let a2 = cache_key_for("https://a.com/foo");
        let b = cache_key_for("https://b.com/foo");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }

    #[test]
    fn markdown_detection() {
        assert!(is_markdown_url("https://example.com/README.md"));
        assert!(is_markdown_url("https://example.com/x.MDX"));
        assert!(is_markdown_url(
            "https://raw.githubusercontent.com/o/r/main/README"
        ));
        assert!(!is_markdown_url("https://example.com/index.html"));
        assert!(!is_markdown_url("https://example.com/docs/intro"));
    }

    #[test]
    fn extract_strips_html_tags() {
        let html = "<html><body><h1>Title</h1><p>Hello <b>world</b>!</p></body></html>";
        let text = extract_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(!text.contains("<h1>"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        // 'é' is 2 bytes in UTF-8
        let s = "héllo wörld";
        let (out, trunc) = truncate_to(s, 3);
        assert!(trunc);
        // The cut must land on a char boundary
        assert!(s.starts_with(out));
        assert!(s.is_char_boundary(out.len()));
    }

    #[test]
    fn truncate_no_op_when_under_limit() {
        let (out, trunc) = truncate_to("hi", 100);
        assert_eq!(out, "hi");
        assert!(!trunc);
    }

    #[test]
    fn url_cap_blocks_after_limit() {
        let state = FetchState::default();
        for i in 0..PER_SESSION_URL_CAP {
            check_url_cap(&state, &format!("https://x.com/{i}")).unwrap();
        }
        // The next NEW URL should fail
        let err = check_url_cap(&state, "https://x.com/over").unwrap_err();
        assert!(format!("{err:#}").contains("per-session cap"));
        // But a URL we already counted should still pass (free re-fetch from cache flow)
        check_url_cap(&state, "https://x.com/0").unwrap();
    }

    #[tokio::test]
    async fn rate_limit_queues_same_domain_within_window() {
        // The new behavior: two same-domain calls succeed; the second waits
        // for its reserved slot rather than erroring. We bound the assertion
        // by checking that the second call took at least ~half the min
        // interval (giving wide tolerance for CI jitter) and at most the
        // hard wait cap. This is the contract `extra_urls` relies on.
        let state = FetchState::default();
        wait_for_rate_limit(&state, "x.com").await.unwrap();
        let start = Instant::now();
        wait_for_rate_limit(&state, "x.com").await.unwrap();
        let elapsed = start.elapsed();
        let min_expected = Duration::from_millis(PER_DOMAIN_MIN_INTERVAL_MS / 2);
        assert!(
            elapsed >= min_expected,
            "second call should have waited at least {min_expected:?}, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(MAX_RATE_LIMIT_WAIT_SECS),
            "second call exceeded the hard wait cap: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn rate_limit_isolated_per_domain() {
        let state = FetchState::default();
        wait_for_rate_limit(&state, "a.com").await.unwrap();
        // Different domain — should not have to wait.
        let start = Instant::now();
        wait_for_rate_limit(&state, "b.com").await.unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "different domains must not block each other; took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn domain_of_strips_scheme_and_lowercases() {
        assert_eq!(domain_of("https://Example.COM/x").unwrap(), "example.com");
    }

    #[test]
    fn cache_round_trip() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.json");
        let doc = CachedDoc {
            url: "https://x".into(),
            fetched_at: Utc::now(),
            content: "hello".into(),
        };
        write_cache(&path, &doc).unwrap();
        let loaded = read_cache(&path).unwrap().unwrap();
        assert_eq!(loaded.content, "hello");
    }

    #[test]
    fn cache_skips_expired_entries() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.json");
        let doc = CachedDoc {
            url: "https://x".into(),
            fetched_at: Utc::now() - chrono::Duration::seconds(CACHE_TTL_SECS + 1),
            content: "hello".into(),
        };
        write_cache(&path, &doc).unwrap();
        let loaded = read_cache(&path).unwrap();
        assert!(
            loaded.is_none(),
            "expired cache entry should not be returned"
        );
    }
}
