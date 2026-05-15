use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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

fn check_rate_limit(state: &FetchState, domain: &str) -> Result<()> {
    let now = Instant::now();
    let mut map = state
        .last_fetch_by_domain
        .lock()
        .expect("last_fetch_by_domain poisoned");
    if let Some(last) = map.get(domain) {
        let elapsed = now.saturating_duration_since(*last);
        let min = Duration::from_millis(PER_DOMAIN_MIN_INTERVAL_MS);
        if elapsed < min {
            let wait = (min - elapsed).as_millis();
            bail!(
                "Error: rate-limited on domain `{domain}` — wait {wait}ms before fetching again. \
                 Action: server-side polite-citizen guard caps each domain at 1 request per second. \
                 If you have several URLs from `{domain}`, space them with a 1s delay between calls."
            );
        }
    }
    map.insert(domain.to_string(), now);
    Ok(())
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

/// Fetch a docs URL with cache, rate-limit, and per-session cap.
pub async fn fetch_docs(
    paths: &Paths,
    state: &FetchState,
    url: &str,
    max_chars: usize,
) -> Result<FetchOutcome> {
    let max_chars = max_chars.clamp(500, MAX_MAX_CHARS);

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
    check_rate_limit(state, &domain)?;

    // 3. HTTP fetch.
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
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
    let body = response
        .text()
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

    #[test]
    fn rate_limit_blocks_within_window() {
        let state = FetchState::default();
        check_rate_limit(&state, "x.com").unwrap();
        let err = check_rate_limit(&state, "x.com").unwrap_err();
        assert!(format!("{err:#}").contains("rate-limited"));
    }

    #[test]
    fn rate_limit_isolated_per_domain() {
        let state = FetchState::default();
        check_rate_limit(&state, "a.com").unwrap();
        // Different domain — should not be blocked.
        check_rate_limit(&state, "b.com").unwrap();
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
        assert!(loaded.is_none(), "expired cache entry should not be returned");
    }
}
