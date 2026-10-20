//! Blocking operations on a profile, shared by every frontend. Never touches a UI and never calls `tr!`.

use anyhow::Result;

use crate::db::{RefreshSummary, Store};
use crate::feed::FeedJob;
use crate::fetch;
use crate::profile::Paths;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::{Fetched, NewArticle};
    use crate::reader;

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
