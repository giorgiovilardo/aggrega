//! Blocking operations on a profile, shared by every frontend. Never touches a UI and never calls `tr!`.

use anyhow::Result;

use crate::db::{RefreshSummary, Store};
use crate::feed::FeedJob;
use crate::fetch;
use crate::profile::Paths;

/// A source that `Core::add_source` stored, with its new row id and how many
/// articles it came with.
#[derive(Debug, PartialEq)]
pub struct AddedSource {
    pub id: i64,
    pub title: String,
    pub articles: usize,
}

#[derive(Clone)]
pub struct Core {
    pub paths: Paths,
    agent: ureq::Agent,
}

impl Core {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            agent: fetch::agent(),
        }
    }

    pub fn agent(&self) -> &ureq::Agent {
        &self.agent
    }

    /// A new connection to the profile's database.
    pub fn store(&self) -> Result<Store> {
        Store::open(&self.paths.db)
    }

    /// Fetches every job and stores the results. Blocking, so meant for a
    /// worker thread. Doesn't touch `last_refresh`: the frontend records it.
    pub fn refresh(&self, jobs: &[FeedJob]) -> Result<RefreshSummary> {
        let results = fetch::fetch_all(&self.agent, jobs);
        self.store()?.apply_refresh(results)
    }

    /// Subscribes to the feed `input` points at and stores it. Blocking, so
    /// meant for a worker thread.
    pub fn add_source(&self, input: &str) -> Result<AddedSource> {
        let sub = fetch::subscribe(&self.agent, input)?;
        let added = self.store()?.add_feed(&sub.url, &sub.fetched)?;
        Ok(AddedSource {
            id: added.id,
            title: sub.fetched.title,
            articles: added.articles,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::{self, Fetched, NewArticle};
    use crate::reader;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// A feed on a port nothing listens on, so the fetch fails fast.
    fn unreachable_feed(store: &Store) {
        store
            .add_feed(
                "http://127.0.0.1:9/feed.xml",
                &Fetched {
                    title: "Offline Blog".into(),
                    site_url: None,
                    etag: None,
                    last_modified: None,
                    articles: vec![NewArticle {
                        guid: "g0".into(),
                        title: "Story 0".into(),
                        link: "http://127.0.0.1:9/story/0".into(),
                        snippet: "Snippet".into(),
                        image_url: None,
                        published: 1_700_000_000,
                        body: reader::encode(&[]),
                    }],
                },
            )
            .unwrap();
    }

    #[test]
    fn refresh_reports_an_unreachable_feed_as_offline() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());
        let store = core.store().unwrap();
        unreachable_feed(&store);
        let jobs = store.fetch_jobs().unwrap();
        let before = store.stats().unwrap().articles;

        let summary = core.refresh(&jobs).unwrap();

        assert!(summary.offline());
        assert_eq!(summary.new_articles, 0);
        assert_eq!(store.stats().unwrap().articles, before);
    }

    #[test]
    fn refresh_with_no_jobs_does_nothing() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());
        core.store().unwrap();

        let summary = core.refresh(&[]).unwrap();

        assert_eq!(summary.total, 0);
        assert_eq!(summary.new_articles, 0);
        assert!(!summary.offline());
    }

    #[test]
    fn add_source_reports_an_unreachable_address_and_stores_nothing() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());

        let err = core.add_source("http://127.0.0.1:9/feed.xml").unwrap_err();

        assert!(feed::is_unreachable(&err), "{err:#}");
        assert!(core.store().unwrap().sources().unwrap().is_empty());
    }

    #[test]
    fn add_source_stores_a_feed_served_over_http() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/feed.xml", listener.local_addr().unwrap());
        let xml: &[u8] = include_bytes!("../tests/fixtures/feeds/rss_2.0_spec_1.xml");
        // Serves two requests: the add, then the same add again.
        std::thread::spawn(move || {
            for mut stream in listener.incoming().take(2).flatten() {
                let _request: Vec<_> = BufReader::new(&stream)
                    .lines()
                    .map_while(Result::ok)
                    .take_while(|l| !l.is_empty())
                    .collect();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    xml.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(xml);
            }
        });

        let added = core.add_source(&url).unwrap();

        assert_eq!(added.title, "Scripting News");
        assert_eq!(added.articles, 2);
        let sources = core.store().unwrap().sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].xml_url, url);
        let again = core.add_source(&url).unwrap_err();
        assert_eq!(format!("{again:#}"), "you're already following this source");
    }

    #[test]
    fn core_opens_the_store_repeatedly() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());
        core.store().unwrap();
        core.store().unwrap();
        assert!(core.paths.db.exists());
    }
}
