//! Blocking operations on a profile, shared by every frontend. Never touches a UI and never calls `tr!`.

use std::collections::HashSet;

use crate::db::{self, ReaderArticle, RefreshSummary, Store};
use crate::error;
use crate::feed::{FeedJob, Source};
use crate::fetch;
use crate::opml;
use crate::pool;
use crate::profile::Paths;
use crate::reader::{self, Block};

/// A source that `Core::add_source` stored, with its new row id and how many
/// articles it came with.
#[derive(Debug, PartialEq)]
pub struct AddedSource {
    pub id: i64,
    pub title: String,
    pub articles: usize,
}

/// What the reader shows for a stored article, before any page is fetched.
pub struct ReaderContent {
    pub blocks: Vec<Block>,
    /// The article's picture, shown above the text.
    pub lead: Option<String>,
    /// Whether the feed's copy is too short to read, so the full page is wanted.
    pub needs_page: bool,
}

/// Decodes a stored article into reader blocks and works out whether its full
/// page should be fetched. Makes no network calls.
pub fn reader_content(article: &ReaderArticle) -> ReaderContent {
    let lead = article.image_url.clone();
    let blocks = reader::tidy(
        reader::decode(article.body.as_deref().unwrap_or("")),
        &article.title,
        lead.as_deref(),
    );
    let needs_page = (article.body.is_none() || reader::needs_full_page(&blocks))
        && reader::is_scrapable(&article.link);
    ReaderContent {
        blocks,
        lead,
        needs_page,
    }
}

/// What an OPML import will subscribe to, worked out before anything is fetched.
pub struct ImportPlan {
    /// Sources that aren't followed yet.
    pub fresh: Vec<Source>,
    /// Sources that are already followed.
    pub skipped: usize,
    /// Every source in the file.
    pub total: usize,
}

/// What an OPML import did.
#[derive(Debug, Default, PartialEq)]
pub struct ImportSummary {
    pub added: usize,
    /// Already followed, or leading to a feed that is.
    pub skipped: usize,
    /// Answered with an error, or not a feed.
    pub failed: usize,
    /// Couldn't be reached at all.
    pub unreachable: usize,
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
    pub fn store(&self) -> Result<Store, db::Error> {
        Store::open(&self.paths.db)
    }

    /// Fetches every job and stores the results. Blocking, so meant for a
    /// worker thread. Doesn't touch `last_refresh`: the frontend records it.
    pub fn refresh(&self, jobs: &[FeedJob]) -> Result<RefreshSummary, db::Error> {
        let results = fetch::fetch_all(&self.agent, jobs);
        self.store()?.apply_refresh(results)
    }

    /// Subscribes to the feed `input` points at and stores it. Blocking, so
    /// meant for a worker thread.
    pub fn add_source(&self, input: &str) -> Result<AddedSource, error::Error> {
        let sub = fetch::subscribe(&self.agent, input)?;
        let added = self.store()?.add_feed(&sub.url, &sub.fetched)?;
        Ok(AddedSource {
            id: added.id,
            title: sub.fetched.title,
            articles: added.articles,
        })
    }

    /// Downloads the web page at `link` and extracts its article text. Blocking,
    /// so meant for a worker thread. Doesn't save anything.
    pub fn full_text(&self, link: &str) -> Result<Vec<Block>, fetch::Error> {
        let (base, page) = fetch::fetch_page(&self.agent, link)?;
        Ok(reader::extract_article(&page, &base))
    }

    /// Subscribes to the planned sources in parallel (with feed discovery,
    /// like the Add dialog), then stores each one that works, with its first
    /// articles. Blocking, so meant for a worker thread.
    pub fn import(&self, plan: ImportPlan) -> Result<ImportSummary, db::Error> {
        let agent = &self.agent;
        let results = pool::par_map(&plan.fresh, 8, |s| fetch::subscribe(agent, &s.xml_url));
        let store = self.store()?;
        // Several entries can lead to one feed, and one may have been added meanwhile.
        let mut known: HashSet<String> = store.sources()?.into_iter().map(|s| s.xml_url).collect();
        let mut summary = ImportSummary::default();
        for result in results {
            match result {
                Ok(sub) if known.contains(&sub.url) => summary.skipped += 1,
                Ok(sub) => match store.add_feed(&sub.url, &sub.fetched) {
                    Ok(_) => {
                        known.insert(sub.url);
                        summary.added += 1;
                    }
                    // Added from the Add dialog while this import ran.
                    Err(db::Error::AlreadyFollowing { .. }) => summary.skipped += 1,
                    Err(e) => return Err(e),
                },
                Err(fetch::Error::Unreachable { .. }) => summary.unreachable += 1,
                Err(_) => summary.failed += 1,
            }
        }
        summary.skipped += plan.skipped;
        Ok(summary)
    }
}

/// Parses an OPML document and sorts its sources into those already followed
/// and those that are new. Makes no network calls.
pub fn plan_import(store: &Store, opml_text: &str) -> Result<ImportPlan, error::Error> {
    let sources = opml::parse(opml_text)?;
    let known: HashSet<String> = store.sources()?.into_iter().map(|s| s.xml_url).collect();
    let total = sources.len();
    let fresh: Vec<Source> = sources
        .into_iter()
        .filter(|s| {
            let url =
                fetch::normalize_url(&s.xml_url).map_or_else(|_| s.xml_url.clone(), String::from);
            !known.contains(&url)
        })
        .collect();
    let skipped = total - fresh.len();
    Ok(ImportPlan {
        fresh,
        skipped,
        total,
    })
}

/// Every source in `store` as an OPML document, with how many it lists.
pub fn export_opml(store: &Store) -> Result<(String, usize), db::Error> {
    let sources = store.sources()?;
    let created = chrono::Utc::now().to_rfc2822();
    Ok((opml::write(&sources, &created), sources.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::{Fetched, NewArticle};

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

        assert!(
            matches!(err, error::Error::Fetch(fetch::Error::Unreachable { .. })),
            "{err}"
        );
        assert!(core.store().unwrap().sources().unwrap().is_empty());
    }

    #[test]
    fn add_source_stores_a_feed_served_over_http() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());
        let xml = include_bytes!("../tests/fixtures/feeds/rss_2.0_spec_1.xml");
        // Serves two requests: the add, then the same add again.
        let url = format!(
            "{}/feed.xml",
            fetch::serve("application/rss+xml", xml.to_vec(), 2)
        );

        let added = core.add_source(&url).unwrap();

        assert_eq!(added.title, "Scripting News");
        assert_eq!(added.articles, 2);
        let sources = core.store().unwrap().sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].xml_url, url);
        let again = core.add_source(&url).unwrap_err();
        assert!(matches!(
            again,
            error::Error::Db(db::Error::AlreadyFollowing { .. })
        ));
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

    fn source(xml_url: &str) -> Source {
        Source {
            title: "Some Blog".into(),
            xml_url: xml_url.into(),
            html_url: None,
        }
    }

    #[test]
    fn plan_import_separates_followed_sources_from_new_ones() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());
        let store = core.store().unwrap();
        unreachable_feed(&store);
        let text = opml::write(
            &[
                source("http://127.0.0.1:9/feed.xml"),
                source("http://127.0.0.1:9/other.xml"),
            ],
            "Mon, 01 Jan 2024 00:00:00 +0000",
        );

        let plan = plan_import(&store, &text).unwrap();

        assert_eq!(plan.fresh.len(), 1);
        assert_eq!(plan.fresh[0].xml_url, "http://127.0.0.1:9/other.xml");
        assert_eq!(plan.skipped, 1);
        assert_eq!(plan.total, 2);
    }

    #[test]
    fn plan_import_of_an_empty_list_is_empty() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());
        let store = core.store().unwrap();
        let text = opml::write(&[], "Mon, 01 Jan 2024 00:00:00 +0000");

        let plan = plan_import(&store, &text).unwrap();

        assert!(plan.fresh.is_empty());
        assert_eq!(plan.skipped, 0);
        assert_eq!(plan.total, 0);
    }

    #[test]
    fn import_counts_sources_that_fail_and_adds_the_skipped_ones() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());
        let plan = ImportPlan {
            fresh: vec![source("http://127.0.0.1:9/x.xml")],
            skipped: 2,
            total: 3,
        };

        let summary = core.import(plan).unwrap();

        assert_eq!(
            summary,
            ImportSummary {
                added: 0,
                skipped: 2,
                failed: 0,
                unreachable: 1,
            }
        );
        assert!(core.store().unwrap().sources().unwrap().is_empty());
    }

    fn stored_article(link: &str, body: Option<String>, image_url: Option<&str>) -> ReaderArticle {
        ReaderArticle {
            title: "Story".into(),
            link: link.into(),
            image_url: image_url.map(Into::into),
            published: 1_700_000_000,
            feed_title: "Some Blog".into(),
            body,
        }
    }

    #[test]
    fn reader_content_wants_the_page_of_an_article_without_a_body() {
        let article = stored_article("https://blog.org/post", None, None);

        let content = reader_content(&article);

        assert!(content.needs_page);
        assert!(content.blocks.is_empty());
    }

    #[test]
    fn reader_content_does_not_want_the_page_of_a_long_body() {
        let body = reader::encode(&[Block::new(reader::Kind::Paragraph, "word ".repeat(300))]);
        let article = stored_article("https://blog.org/post", Some(body), None);

        let content = reader_content(&article);

        assert!(!content.needs_page);
        assert_eq!(content.blocks.len(), 1);
    }

    #[test]
    fn reader_content_never_wants_the_page_of_a_video() {
        let article = stored_article("https://www.youtube.com/watch?v=x", None, None);

        assert!(!reader_content(&article).needs_page);
    }

    #[test]
    fn reader_content_passes_the_lead_picture_through() {
        let article = stored_article(
            "https://blog.org/post",
            None,
            Some("https://blog.org/a.jpg"),
        );

        let content = reader_content(&article);

        assert_eq!(content.lead.as_deref(), Some("https://blog.org/a.jpg"));
    }

    #[test]
    fn full_text_reports_an_unreachable_page() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());

        let err = core.full_text("http://127.0.0.1:9/x").unwrap_err();

        assert!(matches!(err, fetch::Error::Unreachable { .. }), "{err}");
    }

    #[test]
    fn full_text_extracts_the_article_of_a_page_served_over_http() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());
        let text = "word ".repeat(100);
        let page = format!(
            "<html><body><nav>Menu</nav><article><p>{}</p></article></body></html>",
            text.trim_end()
        );
        let url = format!("{}/post", fetch::serve("text/html", page.into_bytes(), 1));

        let blocks = core.full_text(&url).unwrap();

        assert_eq!(
            blocks,
            vec![Block::new(reader::Kind::Paragraph, text.trim_end())]
        );
    }

    #[test]
    fn export_opml_lists_every_source_and_parses_back() {
        let dir = tempfile::Builder::new()
            .prefix("aggrega-ops-")
            .tempdir()
            .unwrap();
        let core = Core::new(Paths::in_dir(dir.path()).unwrap());
        let store = core.store().unwrap();
        unreachable_feed(&store);

        let (text, count) = export_opml(&store).unwrap();

        assert_eq!(count, 1);
        let parsed = opml::parse(&text).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].xml_url, "http://127.0.0.1:9/feed.xml");
    }
}
