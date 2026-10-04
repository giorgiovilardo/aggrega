//! HTTP: downloading feeds (conditionally), subscribing to whatever address
//! the user typed, and fetching article pages and images. Everything that
//! can be done offline with the downloaded bytes lives in `feed`.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use url::Url;

use crate::feed::{self, FeedJob, FetchResult, Fetched};
use crate::pool;

const USER_AGENT: &str = concat!(
    "Aggrega/",
    env!("CARGO_PKG_VERSION"),
    " (desktop feed reader)"
);
const MAX_FEED_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;

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
            let mut fetched = feed::parse(&job.url, &resp.body)?;
            fetched.etag = resp.etag;
            fetched.last_modified = resp.last_modified;
            Ok(Some(fetched))
        }
        s => bail!("server answered HTTP {s}"),
    }
}

/// Fetches all jobs on a bounded pool of threads.
pub fn fetch_all(agent: &ureq::Agent, jobs: &[FeedJob]) -> Vec<(i64, FetchResult)> {
    pool::par_map(jobs, 8, |job| (job.id, fetch_feed(agent, job)))
}

/// Turns whatever the user typed into a feed URL, discovering the feed
/// from an HTML page if needed. Returns the final feed URL and its contents.
pub fn subscribe(agent: &ureq::Agent, input: &str) -> Result<(String, Fetched)> {
    let url = normalize_url(input)?;
    let resp = get(agent, &url, FEED_ACCEPT, &[], MAX_FEED_BYTES)?;
    if !(200..300).contains(&resp.status) {
        bail!("server answered HTTP {}", resp.status);
    }
    if let Ok(mut f) = feed::parse(&url, &resp.body) {
        f.etag = resp.etag;
        f.last_modified = resp.last_modified;
        return Ok((url, f));
    }

    // Not a feed: look for <link rel="alternate"> tags, then common paths.
    let html = String::from_utf8_lossy(&resp.body);
    let base = Url::parse(&url)?;
    let mut candidates = feed::discover_links(&html, &base);
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
        if let Ok(mut f) = feed::parse(&c, &r.body) {
            f.etag = r.etag;
            f.last_modified = r.last_modified;
            return Ok((c, f));
        }
    }
    bail!("couldn't find an RSS, Atom or JSON feed at that address")
}

/// A typed address as a full http(s) URL, the form feeds are stored under.
pub fn normalize_url(input: &str) -> Result<String> {
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

/// Downloads an article's web page for the reader view.
pub fn fetch_page(agent: &ureq::Agent, url: &str) -> Result<String> {
    let r = get(
        agent,
        url,
        "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5",
        &[],
        MAX_PAGE_BYTES,
    )?;
    if !(200..300).contains(&r.status) {
        bail!("the page answered HTTP {}", r.status);
    }
    Ok(String::from_utf8_lossy(&r.body).into_owned())
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
}
