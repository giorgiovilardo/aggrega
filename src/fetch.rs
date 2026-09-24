//! Networking: fetching and parsing feeds, discovering feeds from web pages,
//! and running many fetches in parallel on a small pool of OS threads.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use feed_rs::model::Entry;
use url::Url;

use crate::text;

const USER_AGENT: &str = concat!(
    "Aggrega/",
    env!("CARGO_PKG_VERSION"),
    " (desktop feed reader)"
);
const MAX_FEED_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const SNIPPET_CHARS: usize = 280;

/// An article parsed from a feed, ready to be stored.
#[derive(Debug, Clone)]
pub struct NewArticle {
    pub guid: String,
    pub title: String,
    pub link: String,
    pub snippet: String,
    pub image_url: Option<String>,
    pub published: i64,
}

/// A successfully downloaded and parsed feed.
#[derive(Debug)]
pub struct Fetched {
    pub title: String,
    pub site_url: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub articles: Vec<NewArticle>,
}

/// What we need to know to (conditionally) refresh one subscription.
#[derive(Debug, Clone)]
pub struct FeedJob {
    pub id: i64,
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// `Ok(None)` means the server answered "304 Not Modified".
pub type FetchResult = Result<Option<Fetched>>;

/// The server couldn't be reached at all (no network, DNS failure, timeout…).
/// Unlike an HTTP error this says nothing about the source itself, so it's
/// never recorded as a broken feed or a broken thumbnail.
#[derive(Debug)]
pub struct Unreachable(pub String);

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreachable {}

pub fn is_unreachable(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Unreachable>().is_some()
}

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .timeout_connect(Some(Duration::from_secs(8)))
        .http_status_as_error(false)
        .build()
        .into()
}

struct Response {
    status: u16,
    etag: Option<String>,
    last_modified: Option<String>,
    body: Vec<u8>,
}

fn get(
    agent: &ureq::Agent,
    url: &str,
    accept: &str,
    headers: &[(&str, &str)],
    limit: u64,
) -> Result<Response> {
    let mut req = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", accept);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req.call().map_err(|e| {
        let msg = friendly_error(&e);
        if matches!(
            e,
            ureq::Error::Io(_)
                | ureq::Error::Timeout(_)
                | ureq::Error::HostNotFound
                | ureq::Error::ConnectionFailed
                | ureq::Error::BodyStalled
        ) {
            anyhow::Error::new(Unreachable(msg))
        } else {
            anyhow!(msg)
        }
    })?;
    let status = resp.status().as_u16();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let etag = header("etag");
    let last_modified = header("last-modified");
    let body = if status == 304 {
        Vec::new()
    } else {
        resp.body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .context("failed to read response")?
    };
    Ok(Response {
        status,
        etag,
        last_modified,
        body,
    })
}

fn friendly_error(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "the server took too long to answer".into(),
        ureq::Error::HostNotFound => "host not found — check the address".into(),
        ureq::Error::Io(io) => format!("network error: {io}"),
        other => other.to_string(),
    }
}

const FEED_ACCEPT: &str = "application/rss+xml, application/atom+xml, application/feed+json, application/xml;q=0.9, text/xml;q=0.9, */*;q=0.5";

/// Refreshes one feed, using ETag / Last-Modified to skip unchanged feeds.
pub fn fetch_feed(agent: &ureq::Agent, job: &FeedJob) -> FetchResult {
    let mut headers = Vec::new();
    if let Some(e) = &job.etag {
        headers.push(("If-None-Match", e.as_str()));
    }
    if let Some(lm) = &job.last_modified {
        headers.push(("If-Modified-Since", lm.as_str()));
    }
    let resp = get(agent, &job.url, FEED_ACCEPT, &headers, MAX_FEED_BYTES)?;
    match resp.status {
        304 => Ok(None),
        200..=299 => {
            let mut fetched = parse(&job.url, &resp.body)?;
            fetched.etag = resp.etag;
            fetched.last_modified = resp.last_modified;
            Ok(Some(fetched))
        }
        s => bail!("server answered HTTP {s}"),
    }
}

/// Fetches all jobs using a bounded pool of threads. Order of results is unspecified.
pub fn fetch_all(agent: &ureq::Agent, jobs: &[FeedJob]) -> Vec<(i64, FetchResult)> {
    let results = Mutex::new(Vec::with_capacity(jobs.len()));
    par_for_each(jobs, 8, |job| {
        let r = fetch_feed(agent, job);
        results.lock().unwrap().push((job.id, r));
    });
    results.into_inner().unwrap()
}

/// Runs `f` over `items` on up to `workers` scoped threads.
pub fn par_for_each<T: Sync>(items: &[T], workers: usize, f: impl Fn(&T) + Sync) {
    let next = AtomicUsize::new(0);
    let n = workers.min(items.len());
    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| {
                while let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) {
                    f(item);
                }
            });
        }
    });
}

/// Turns whatever the user typed into a feed URL, discovering the feed
/// from an HTML page if needed. Returns the final feed URL and its contents.
pub fn subscribe(agent: &ureq::Agent, input: &str) -> Result<(String, Fetched)> {
    let url = normalize_url(input)?;
    let resp = get(agent, &url, FEED_ACCEPT, &[], MAX_FEED_BYTES)?;
    if !(200..300).contains(&resp.status) {
        bail!("server answered HTTP {}", resp.status);
    }
    if let Ok(mut f) = parse(&url, &resp.body) {
        f.etag = resp.etag;
        f.last_modified = resp.last_modified;
        return Ok((url, f));
    }

    // Not a feed: look for <link rel="alternate"> tags, then common paths.
    let html = String::from_utf8_lossy(&resp.body);
    let base = Url::parse(&url)?;
    let mut candidates = discover_links(&html, &base);
    for p in [
        "/feed",
        "/rss",
        "/feed.xml",
        "/rss.xml",
        "/atom.xml",
        "/index.xml",
        "/feed/",
    ] {
        if let Ok(u) = base.join(p) {
            candidates.push(u.to_string());
        }
    }
    let mut seen = std::collections::HashSet::new();
    for c in candidates
        .into_iter()
        .filter(|c| *c != url && seen.insert(c.clone()))
    {
        let Ok(r) = get(agent, &c, FEED_ACCEPT, &[], MAX_FEED_BYTES) else {
            continue;
        };
        if !(200..300).contains(&r.status) {
            continue;
        }
        if let Ok(mut f) = parse(&c, &r.body) {
            f.etag = r.etag;
            f.last_modified = r.last_modified;
            return Ok((c, f));
        }
    }
    bail!("couldn't find an RSS, Atom or JSON feed at that address")
}

fn normalize_url(input: &str) -> Result<String> {
    let s = input.trim();
    if s.is_empty() {
        bail!("please enter an address");
    }
    let s = if s.contains("://") {
        s.to_string()
    } else {
        format!("https://{s}")
    };
    let u = Url::parse(&s).map_err(|_| anyhow!("that doesn't look like a valid address"))?;
    if !matches!(u.scheme(), "http" | "https") {
        bail!("only http and https addresses are supported");
    }
    Ok(u.to_string())
}

/// Finds feed URLs advertised by an HTML page.
fn discover_links(html: &str, base: &Url) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("<link") {
        let start = pos + start;
        let end = lower[start..]
            .find('>')
            .map(|e| start + e)
            .unwrap_or(lower.len());
        let tag = &html[start..end];
        pos = end;
        let rel = attr(tag, "rel").unwrap_or_default().to_ascii_lowercase();
        let ty = attr(tag, "type").unwrap_or_default().to_ascii_lowercase();
        let is_feed = ty.contains("rss") || ty.contains("atom") || ty.contains("feed+json");
        if rel.contains("alternate")
            && is_feed
            && let Some(href) = attr(tag, "href")
            && let Ok(u) = base.join(&html_escape::decode_html_entities(&href))
        {
            out.push(u.to_string());
        }
    }
    out
}

/// Reads an attribute value from a single HTML tag (quoted or unquoted).
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find(name) {
        let p = from + p;
        from = p + name.len();
        let before_ok = p > 0 && lower.as_bytes()[p - 1].is_ascii_whitespace();
        let rest = lower[from..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let offset = tag.len() - rest.len() + 1;
        let value = tag[offset..].trim_start();
        return Some(match value.chars().next()? {
            q @ ('"' | '\'') => value[1..].split(q).next()?.to_string(),
            _ => value
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()?
                .to_string(),
        });
    }
    None
}

fn parse(url: &str, body: &[u8]) -> Result<Fetched> {
    let feed = feed_rs::parser::Builder::new()
        .base_uri(Some(url))
        .build()
        .parse(body)
        .context("not a valid RSS/Atom feed")?;

    let now = chrono::Utc::now().timestamp();
    let title = feed
        .title
        .map(|t| text::html_to_text(&t.content))
        .filter(|t| !t.is_empty())
        .or_else(|| {
            Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
        })
        .unwrap_or_else(|| url.to_string());
    let site_url = feed
        .links
        .iter()
        .find(|l| l.rel.as_deref().is_none_or(|r| r == "alternate"))
        .map(|l| l.href.clone());

    let articles = feed
        .entries
        .iter()
        .filter_map(|e| convert_entry(e, now))
        .collect();
    Ok(Fetched {
        title,
        site_url,
        etag: None,
        last_modified: None,
        articles,
    })
}

fn convert_entry(e: &Entry, now: i64) -> Option<NewArticle> {
    let link = e
        .links
        .iter()
        .find(|l| l.rel.as_deref().is_none_or(|r| r == "alternate"))
        .or_else(|| e.links.first())
        .map(|l| l.href.clone())
        .or_else(|| e.id.starts_with("http").then(|| e.id.clone()))?;

    let html = e
        .summary
        .as_ref()
        .map(|s| s.content.as_str())
        .or_else(|| e.content.as_ref().and_then(|c| c.body.as_deref()))
        .unwrap_or("");
    let content_html = e
        .content
        .as_ref()
        .and_then(|c| c.body.as_deref())
        .unwrap_or("");

    let mut title = e
        .title
        .as_ref()
        .map(|t| text::html_to_text(&t.content))
        .unwrap_or_default();
    let snippet = text::truncate(&text::html_to_text(html), SNIPPET_CHARS);
    if title.is_empty() {
        title = if snippet.is_empty() {
            "(untitled)".into()
        } else {
            text::truncate(&snippet, 90)
        };
    }

    let published = e
        .published
        .or(e.updated)
        .map(|d| d.timestamp())
        .unwrap_or(now)
        .min(now); // clamp bogus future dates

    let image_url = find_image(e, html, content_html).and_then(|src| {
        Url::parse(&link)
            .ok()
            .and_then(|base| base.join(&src).ok())
            .map(|u| u.to_string())
            .or(Some(src))
    });

    let guid = if e.id.is_empty() {
        link.clone()
    } else {
        e.id.clone()
    };
    Some(NewArticle {
        guid,
        title,
        link,
        snippet,
        image_url,
        published,
    })
}

fn find_image(e: &Entry, summary_html: &str, content_html: &str) -> Option<String> {
    for m in &e.media {
        if let Some(t) = m.thumbnails.first() {
            return Some(t.image.uri.clone());
        }
        for c in &m.content {
            let is_image = c
                .content_type
                .as_ref()
                .is_some_and(|t| t.to_string().starts_with("image/"))
                || c.url.as_ref().is_some_and(|u| looks_like_image(u.as_str()));
            if is_image && let Some(u) = &c.url {
                return Some(u.to_string());
            }
        }
    }
    for l in &e.links {
        if l.rel.as_deref() == Some("enclosure")
            && (l
                .media_type
                .as_deref()
                .is_some_and(|t| t.starts_with("image/"))
                || looks_like_image(&l.href))
        {
            return Some(l.href.clone());
        }
    }
    first_img_src(summary_html).or_else(|| first_img_src(content_html))
}

fn looks_like_image(u: &str) -> bool {
    let path = u
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".webp", ".gif"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

fn first_img_src(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut pos = 0;
    while let Some(p) = lower[pos..].find("<img") {
        let start = pos + p;
        let end = lower[start..]
            .find('>')
            .map(|e| start + e)
            .unwrap_or(lower.len());
        pos = end;
        let tag = &html[start..end];
        // Skip tracking pixels.
        if attr(tag, "width").is_some_and(|w| w.trim() == "1") {
            continue;
        }
        if let Some(src) = attr(tag, "src").filter(|s| !s.starts_with("data:")) {
            return Some(html_escape::decode_html_entities(&src).into_owned());
        }
    }
    None
}

/// Downloads an image (thumbnail source).
pub fn download_image(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let r = get(
        agent,
        url,
        "image/avif,image/webp,image/png,image/jpeg,image/*;q=0.8",
        &[],
        MAX_IMAGE_BYTES,
    )?;
    if !(200..300).contains(&r.status) {
        bail!("HTTP {}", r.status);
    }
    Ok(r.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_attributes() {
        let t = r#"<link rel="alternate" type='application/rss+xml' href=/feed.xml>"#;
        assert_eq!(attr(t, "rel").as_deref(), Some("alternate"));
        assert_eq!(attr(t, "type").as_deref(), Some("application/rss+xml"));
        assert_eq!(attr(t, "href").as_deref(), Some("/feed.xml"));
    }

    #[test]
    fn discovers_feed_links() {
        let html = r#"<html><head><link rel="stylesheet" href="a.css">
            <link rel="alternate" type="application/atom+xml" href="/atom.xml"></head></html>"#;
        let base = Url::parse("https://example.com/blog/").unwrap();
        assert_eq!(
            discover_links(html, &base),
            vec!["https://example.com/atom.xml".to_string()]
        );
    }

    #[test]
    fn refused_connection_counts_as_unreachable() {
        // Port 9 on localhost is closed: the request fails before any HTTP exchange.
        let job = FeedJob {
            id: 1,
            url: "http://127.0.0.1:9/feed".into(),
            etag: None,
            last_modified: None,
        };
        let err = fetch_feed(&agent(), &job).unwrap_err();
        assert!(is_unreachable(&err), "{err:#}");
    }

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_url("example.com/feed").unwrap(),
            "https://example.com/feed"
        );
        assert!(normalize_url("ftp://x.org").is_err());
    }

    #[test]
    fn parses_rss() {
        let xml = br#"<?xml version="1.0"?><rss version="2.0"><channel><title>Demo</title>
            <link>https://demo.org</link>
            <item><title>Hello &amp; welcome</title><link>https://demo.org/1</link><guid>1</guid>
            <description>&lt;p&gt;Body &lt;img src="/pic.jpg"&gt;&lt;/p&gt;</description>
            <pubDate>Tue, 01 Sep 2026 10:00:00 GMT</pubDate></item>
            </channel></rss>"#;
        let f = parse("https://demo.org/rss", xml).unwrap();
        assert_eq!(f.title, "Demo");
        assert_eq!(f.articles.len(), 1);
        let a = &f.articles[0];
        assert_eq!(a.title, "Hello & welcome");
        assert_eq!(a.snippet, "Body");
        assert_eq!(a.image_url.as_deref(), Some("https://demo.org/pic.jpg"));
    }
}
