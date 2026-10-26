//! HTTP: downloading feeds (conditionally), subscribing to whatever address
//! the user typed, and fetching article pages and images. Everything that
//! can be done offline with the downloaded bytes lives in `feed`.

use std::time::Duration;

use url::Url;

use crate::feed::{self, FeedJob, Fetched, Subscription};
use crate::pool;

const USER_AGENT: &str = concat!(
    "Aggrega/",
    env!("CARGO_PKG_VERSION"),
    " (desktop feed reader)"
);
const MAX_FEED_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Why a download, or subscribing to an address, failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Nothing was typed.
    #[error("no address given")]
    EmptyAddress,
    #[error("“{input}” isn't a valid address")]
    InvalidAddress {
        input: String,
        #[source]
        source: url::ParseError,
    },
    #[error("only http and https addresses are supported, not {scheme}")]
    UnsupportedScheme { scheme: String },
    /// The server couldn't be reached at all (no network, DNS failure, timeout…).
    /// Unlike every other variant this says nothing about the source itself, so
    /// it's never recorded as a broken feed or thumbnail.
    #[error("couldn't reach {url}")]
    Unreachable {
        url: String,
        #[source]
        source: ureq::Error,
    },
    /// The request failed for another reason (TLS, a bad redirect…).
    #[error("request to {url} failed")]
    Request {
        url: String,
        #[source]
        source: ureq::Error,
    },
    #[error("{url} answered HTTP {status}")]
    Status { url: String, status: u16 },
    /// The body couldn't be read, or was larger than allowed.
    #[error("couldn't read the response from {url}")]
    Body {
        url: String,
        #[source]
        source: ureq::Error,
    },
    #[error("{url} isn't a valid feed")]
    Feed {
        url: String,
        #[source]
        source: feed::Error,
    },
    /// Neither the address nor any feed it links to is a feed.
    #[error("couldn't find an RSS, Atom or JSON feed at {url}")]
    NoFeed { url: String },
}

/// `Ok(None)` means the server answered "304 Not Modified".
pub type FetchResult = Result<Option<Fetched>, Error>;

/// One feed's outcome in a refresh, as `Store::apply_refresh` takes it.
#[derive(Debug)]
pub struct FeedRefresh {
    pub id: i64,
    pub result: FetchResult,
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
) -> Result<Response, Error> {
    let mut req = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", accept);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req.call().map_err(|source| {
        let url = url.to_string();
        if matches!(
            source,
            ureq::Error::Io(_)
                | ureq::Error::Timeout(_)
                | ureq::Error::HostNotFound
                | ureq::Error::ConnectionFailed
                | ureq::Error::BodyStalled
        ) {
            Error::Unreachable { url, source }
        } else {
            Error::Request { url, source }
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
            .map_err(|source| Error::Body {
                url: url.to_string(),
                source,
            })?
    };
    Ok(Response {
        status,
        etag,
        last_modified,
        body,
    })
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
            let mut fetched = feed::parse(&job.url, &resp.body).map_err(|source| Error::Feed {
                url: job.url.clone(),
                source,
            })?;
            fetched.etag = resp.etag;
            fetched.last_modified = resp.last_modified;
            Ok(Some(fetched))
        }
        status => Err(Error::Status {
            url: job.url.clone(),
            status,
        }),
    }
}

/// Fetches all jobs on a bounded pool of threads.
pub fn fetch_all(agent: &ureq::Agent, jobs: &[FeedJob]) -> Vec<FeedRefresh> {
    pool::par_map(jobs, 8, |job| FeedRefresh {
        id: job.id,
        result: fetch_feed(agent, job),
    })
}

/// Turns whatever the user typed into a feed URL, discovering the feed
/// from an HTML page if needed. Returns the final feed URL and its contents.
pub fn subscribe(agent: &ureq::Agent, input: &str) -> Result<Subscription, Error> {
    let url = normalize_url(input)?;
    let resp = get(agent, &url, FEED_ACCEPT, &[], MAX_FEED_BYTES)?;
    if !(200..300).contains(&resp.status) {
        return Err(Error::Status {
            url,
            status: resp.status,
        });
    }
    if let Ok(mut f) = feed::parse(&url, &resp.body) {
        f.etag = resp.etag;
        f.last_modified = resp.last_modified;
        return Ok(Subscription { url, fetched: f });
    }

    // Not a feed: look for <link rel="alternate"> tags, then common paths.
    let html = String::from_utf8_lossy(&resp.body);
    let base = Url::parse(&url).map_err(|source| Error::InvalidAddress {
        input: url.clone(),
        source,
    })?;
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
            return Ok(Subscription { url: c, fetched: f });
        }
    }
    Err(Error::NoFeed { url })
}

/// A typed address as a full http(s) URL, the form feeds are stored under.
pub fn normalize_url(input: &str) -> Result<String, Error> {
    let s = input.trim();
    if s.is_empty() {
        return Err(Error::EmptyAddress);
    }
    let full = if s.contains("://") {
        s.to_string()
    } else {
        format!("https://{s}")
    };
    let u = Url::parse(&full).map_err(|source| Error::InvalidAddress {
        input: s.to_string(),
        source,
    })?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(Error::UnsupportedScheme {
            scheme: u.scheme().to_string(),
        });
    }
    Ok(u.to_string())
}

/// Downloads an article's web page for the reader view.
pub fn fetch_page(agent: &ureq::Agent, url: &str) -> Result<String, Error> {
    let r = get(
        agent,
        url,
        "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5",
        &[],
        MAX_PAGE_BYTES,
    )?;
    if !(200..300).contains(&r.status) {
        return Err(Error::Status {
            url: url.to_string(),
            status: r.status,
        });
    }
    Ok(String::from_utf8_lossy(&r.body).into_owned())
}

/// Downloads an image (thumbnail source).
pub fn download_image(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, Error> {
    let r = get(
        agent,
        url,
        "image/avif,image/webp,image/png,image/jpeg,image/*;q=0.8",
        &[],
        MAX_IMAGE_BYTES,
    )?;
    if !(200..300).contains(&r.status) {
        return Err(Error::Status {
            url: url.to_string(),
            status: r.status,
        });
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
        assert!(matches!(err, Error::Unreachable { .. }), "{err}");
    }

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_url("example.com/feed").unwrap(),
            "https://example.com/feed"
        );
        assert!(matches!(normalize_url("  "), Err(Error::EmptyAddress)));
        assert!(matches!(
            normalize_url("ftp://x.org"),
            Err(Error::UnsupportedScheme { scheme }) if scheme == "ftp"
        ));
        assert!(matches!(
            normalize_url("https://exa mple.com"),
            Err(Error::InvalidAddress { input, .. }) if input == "https://exa mple.com"
        ));
    }
}
