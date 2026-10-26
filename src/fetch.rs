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
        why: Offline,
        #[source]
        source: ureq::Error,
    },
    /// The request, or reading its answer, failed for another reason (TLS, a
    /// bad redirect, a body that doesn't decompress…).
    #[error("request to {url} failed")]
    Request {
        url: String,
        #[source]
        source: ureq::Error,
    },
    #[error("{url} answered HTTP {status}")]
    Status { url: String, status: u16 },
    /// The body reached the download limit, which is the server's doing.
    #[error("the response from {url} reached the {limit}-byte limit")]
    TooLarge { url: String, limit: u64 },
    #[error("{url} isn't a valid feed")]
    Feed {
        url: String,
        #[source]
        source: feed::Error,
    },
    /// Neither the address nor any feed it links to is a feed. `parse` is why
    /// the address itself isn't a feed.
    #[error("couldn't find an RSS, Atom or JSON feed at {url}")]
    NoFeed {
        url: String,
        #[source]
        parse: feed::Error,
    },
}

/// Why a server couldn't be reached, as far as a frontend cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offline {
    /// The server didn't answer in time.
    TimedOut,
    /// The host name didn't resolve.
    HostNotFound,
    /// Anything else: no network, refused connection, a dropped transfer…
    NoConnection,
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

impl Response {
    /// The response if the server answered 2xx, else a `Status` error for `url`.
    fn error_for_status(self, url: &str) -> Result<Self, Error> {
        if (200..300).contains(&self.status) {
            Ok(self)
        } else {
            Err(Error::Status {
                url: url.to_string(),
                status: self.status,
            })
        }
    }

    /// The body as a feed, with the validators for the next conditional fetch.
    fn parse_feed(&self, url: &str) -> Result<Fetched, feed::Error> {
        let mut fetched = feed::parse(url, &self.body)?;
        fetched.etag.clone_from(&self.etag);
        fetched.last_modified.clone_from(&self.last_modified);
        Ok(fetched)
    }
}

impl Error {
    /// Names what went wrong with a request to `url` or reading its body.
    fn from_ureq(url: &str, source: ureq::Error) -> Self {
        let url = url.to_owned();
        let why = match source {
            ureq::Error::Timeout(_) => Offline::TimedOut,
            ureq::Error::HostNotFound => Offline::HostNotFound,
            ureq::Error::Io(_) | ureq::Error::ConnectionFailed | ureq::Error::BodyStalled => {
                Offline::NoConnection
            }
            ureq::Error::BodyExceedsLimit(limit) => return Self::TooLarge { url, limit },
            source => return Self::Request { url, source },
        };
        Self::Unreachable { url, why, source }
    }
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
    let mut resp = req.call().map_err(|e| Error::from_ureq(url, e))?;
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
            .map_err(|e| Error::from_ureq(url, e))?
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
            let fetched = resp.parse_feed(&job.url).map_err(|source| Error::Feed {
                url: job.url.clone(),
                source,
            })?;
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
    let base = normalize_url(input)?;
    let url = base.as_str();
    let resp = get(agent, url, FEED_ACCEPT, &[], MAX_FEED_BYTES)?.error_for_status(url)?;
    let parse = match resp.parse_feed(url) {
        Ok(fetched) => {
            return Ok(Subscription {
                url: base.into(),
                fetched,
            });
        }
        Err(e) => e,
    };

    // Not a feed: look for <link rel="alternate"> tags, then common paths.
    let html = String::from_utf8_lossy(&resp.body);
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
        .filter(|c| c != url && seen.insert(c.clone()))
    {
        let Ok(r) =
            get(agent, &c, FEED_ACCEPT, &[], MAX_FEED_BYTES).and_then(|r| r.error_for_status(&c))
        else {
            continue;
        };
        if let Ok(fetched) = r.parse_feed(&c) {
            return Ok(Subscription { url: c, fetched });
        }
    }
    Err(Error::NoFeed {
        url: base.into(),
        parse,
    })
}

/// A typed address as a full http(s) URL. Feeds are stored under its string form.
pub fn normalize_url(input: &str) -> Result<Url, Error> {
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
    Ok(u)
}

/// Downloads an article's web page for the reader view. Also returns the
/// parsed address, which the page's relative links resolve against.
pub fn fetch_page(agent: &ureq::Agent, link: &str) -> Result<(Url, String), Error> {
    let base = Url::parse(link).map_err(|source| Error::InvalidAddress {
        input: link.to_string(),
        source,
    })?;
    let r = get(
        agent,
        link,
        "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5",
        &[],
        MAX_PAGE_BYTES,
    )?
    .error_for_status(link)?;
    Ok((base, String::from_utf8_lossy(&r.body).into_owned()))
}

/// Downloads an image (thumbnail source).
pub fn download_image(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, Error> {
    let r = get(
        agent,
        url,
        "image/avif,image/webp,image/png,image/jpeg,image/*;q=0.8",
        &[],
        MAX_IMAGE_BYTES,
    )?
    .error_for_status(url)?;
    Ok(r.body)
}

/// Answers the next `connections` HTTP requests on a local port with `body`
/// as `content_type`, and returns the server's address.
#[cfg(test)]
pub(crate) fn serve(content_type: &'static str, body: Vec<u8>, connections: usize) -> String {
    use std::io::{BufRead, BufReader, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().take(connections).flatten() {
            let _request: Vec<_> = BufReader::new(&stream)
                .lines()
                .map_while(Result::ok)
                .take_while(|l| !l.is_empty())
                .collect();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    format!("http://{addr}")
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
    fn ureq_errors_say_what_went_wrong() {
        let url = "http://x.org/feed";
        let timed_out = ureq::Error::Timeout(ureq::Timeout::Global);
        assert!(matches!(
            Error::from_ureq(url, timed_out),
            Error::Unreachable {
                why: Offline::TimedOut,
                ..
            }
        ));
        assert!(matches!(
            Error::from_ureq(url, ureq::Error::HostNotFound),
            Error::Unreachable {
                why: Offline::HostNotFound,
                ..
            }
        ));
        assert!(matches!(
            Error::from_ureq(url, ureq::Error::ConnectionFailed),
            Error::Unreachable {
                why: Offline::NoConnection,
                ..
            }
        ));
        assert!(matches!(
            Error::from_ureq(url, ureq::Error::BodyExceedsLimit(10)),
            Error::TooLarge { limit: 10, .. }
        ));
        assert!(matches!(
            Error::from_ureq(url, ureq::Error::TooManyRedirects),
            Error::Request { .. }
        ));
    }

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_url("example.com/feed").unwrap().as_str(),
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

    #[test]
    fn a_body_over_the_limit_is_too_large() {
        let url = format!(
            "{}/big",
            serve("text/plain", b"0123456789abcdefghij".to_vec(), 1)
        );
        let Err(err) = get(&agent(), &url, "*/*", &[], 10) else {
            panic!("the body wasn't limited");
        };
        assert!(matches!(err, Error::TooLarge { limit: 10, .. }), "{err}");
    }

    #[test]
    fn a_malformed_address_is_invalid_for_a_page() {
        assert!(matches!(
            fetch_page(&agent(), "not a url"),
            Err(Error::InvalidAddress { input, .. }) if input == "not a url"
        ));
    }

    #[test]
    fn a_malformed_feed_is_kept_as_the_reason_no_feed_was_found() {
        // The address and its seven common feed paths all serve the same broken feed.
        let url = serve(
            "application/rss+xml",
            b"<rss version=\"2.0\"><channel><title>Broken".to_vec(),
            8,
        );

        let err = subscribe(&agent(), &url).unwrap_err();

        assert!(
            matches!(
                err,
                Error::NoFeed {
                    parse: feed::Error::NotAFeed(_),
                    ..
                }
            ),
            "{err}"
        );
    }
}
