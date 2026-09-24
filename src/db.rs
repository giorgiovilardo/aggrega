//! Local persistence in a single SQLite file (WAL mode).

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::fetch::{FeedJob, FetchResult, Fetched};

const SCHEMA_VERSION: i32 = 1;

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct Feed {
    pub id: i64,
    pub title: String,
    pub unread: i64,
    pub has_error: bool,
}

#[derive(Debug, Clone)]
pub struct Article {
    pub id: i64,
    pub title: String,
    pub link: String,
    pub snippet: String,
    pub image_url: Option<String>,
    pub published: i64,
    pub read: bool,
    pub feed_title: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RefreshSummary {
    pub new_articles: usize,
    /// Sources that answered with an error (bad feed, HTTP 404…).
    pub failed: usize,
    /// Sources that couldn't be reached at all (network down, DNS, timeout).
    pub unreachable: usize,
    pub total: usize,
}

impl RefreshSummary {
    /// Nothing could be reached: most likely this computer is offline.
    pub fn offline(&self) -> bool {
        self.total > 0 && self.unreachable == self.total
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            bail!("database was created by a newer version of Aggrega");
        }
        if version < 1 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS feeds (
                    id            INTEGER PRIMARY KEY,
                    url           TEXT NOT NULL UNIQUE,
                    title         TEXT NOT NULL,
                    site_url      TEXT,
                    etag          TEXT,
                    last_modified TEXT,
                    last_fetched  INTEGER,
                    last_error    TEXT,
                    added_at      INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS articles (
                    id         INTEGER PRIMARY KEY,
                    feed_id    INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
                    guid       TEXT NOT NULL,
                    title      TEXT NOT NULL,
                    link       TEXT NOT NULL,
                    snippet    TEXT NOT NULL,
                    image_url  TEXT,
                    published  INTEGER NOT NULL,
                    fetched_at INTEGER NOT NULL,
                    read       INTEGER NOT NULL DEFAULT 0,
                    UNIQUE (feed_id, guid)
                );
                CREATE INDEX IF NOT EXISTS idx_articles_published ON articles(published DESC);
                CREATE INDEX IF NOT EXISTS idx_articles_feed ON articles(feed_id, published DESC);
                CREATE INDEX IF NOT EXISTS idx_articles_unread ON articles(feed_id, read);
                CREATE TABLE IF NOT EXISTS settings (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                PRAGMA user_version = 1;",
            )?;
        }
        Ok(())
    }

    // ---- settings -------------------------------------------------------

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- feeds ----------------------------------------------------------

    pub fn feeds(&self) -> Result<Vec<Feed>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT f.id, f.title, f.last_error IS NOT NULL,
                    (SELECT COUNT(*) FROM articles a WHERE a.feed_id = f.id AND a.read = 0)
             FROM feeds f ORDER BY f.title COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Feed {
                id: r.get(0)?,
                title: r.get(1)?,
                has_error: r.get(2)?,
                unread: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn fetch_jobs(&self) -> Result<Vec<FeedJob>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, url, etag, last_modified FROM feeds")?;
        let rows = stmt.query_map([], |r| {
            Ok(FeedJob {
                id: r.get(0)?,
                url: r.get(1)?,
                etag: r.get(2)?,
                last_modified: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Inserts a new subscription with its first batch of articles.
    /// Returns the feed id and how many articles were stored.
    pub fn add_feed(&self, url: &str, fetched: &Fetched) -> Result<(i64, usize)> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM feeds WHERE url = ?1)",
            [url],
            |r| r.get(0),
        )?;
        if exists {
            bail!("you're already following this source");
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO feeds(url, title, site_url, etag, last_modified, last_fetched, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                url,
                fetched.title,
                fetched.site_url,
                fetched.etag,
                fetched.last_modified,
                now()
            ],
        )?;
        let id = tx.last_insert_rowid();
        let n = insert_articles(&tx, id, fetched)?;
        tx.commit()?;
        Ok((id, n))
    }

    pub fn remove_feed(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM feeds WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Stores the results of a refresh in a single transaction.
    pub fn apply_refresh(&self, results: Vec<(i64, FetchResult)>) -> Result<RefreshSummary> {
        let mut summary = RefreshSummary {
            total: results.len(),
            ..Default::default()
        };
        let ts = now();
        let tx = self.conn.unchecked_transaction()?;
        for (id, result) in results {
            match result {
                Ok(Some(fetched)) => {
                    summary.new_articles += insert_articles(&tx, id, &fetched)?;
                    tx.execute(
                        "UPDATE feeds SET etag = ?2, last_modified = ?3, last_fetched = ?4, last_error = NULL,
                                site_url = COALESCE(?5, site_url)
                         WHERE id = ?1",
                        params![id, fetched.etag, fetched.last_modified, ts, fetched.site_url],
                    )?;
                }
                Ok(None) => {
                    tx.execute(
                        "UPDATE feeds SET last_fetched = ?2, last_error = NULL WHERE id = ?1",
                        params![id, ts],
                    )?;
                }
                // Connectivity problems aren't the source's fault: keep its state.
                Err(e) if crate::fetch::is_unreachable(&e) => summary.unreachable += 1,
                Err(e) => {
                    summary.failed += 1;
                    tx.execute(
                        "UPDATE feeds SET last_error = ?2 WHERE id = ?1",
                        params![id, e.to_string()],
                    )?;
                }
            }
        }
        // Housekeeping: forget read articles older than 90 days.
        tx.execute(
            "DELETE FROM articles WHERE read = 1 AND published < ?1",
            [ts - 90 * 86_400],
        )?;
        tx.commit()?;
        Ok(summary)
    }

    // ---- articles -------------------------------------------------------

    /// Most recent articles, optionally restricted to one feed and/or unread ones.
    pub fn articles(
        &self,
        feed: Option<i64>,
        unread_only: bool,
        limit: usize,
    ) -> Result<Vec<Article>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT a.id, a.title, a.link, a.snippet, a.image_url, a.published, a.read, f.title
             FROM articles a JOIN feeds f ON f.id = a.feed_id
             WHERE (?1 IS NULL OR a.feed_id = ?1) AND (?2 = 0 OR a.read = 0)
             ORDER BY a.published DESC, a.id DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![feed, unread_only, limit as i64], |r| {
            Ok(Article {
                id: r.get(0)?,
                title: r.get(1)?,
                link: r.get(2)?,
                snippet: r.get(3)?,
                image_url: r.get(4)?,
                published: r.get(5)?,
                read: r.get(6)?,
                feed_title: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_read(&self, article: i64, read: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE articles SET read = ?2 WHERE id = ?1",
            params![article, read],
        )?;
        Ok(())
    }

    pub fn mark_all_read(&self, feed: Option<i64>) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE articles SET read = 1 WHERE read = 0 AND (?1 IS NULL OR feed_id = ?1)",
            params![feed],
        )?)
    }
}

fn insert_articles(conn: &Connection, feed_id: i64, fetched: &Fetched) -> Result<usize> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO articles(feed_id, guid, title, link, snippet, image_url, published, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(feed_id, guid) DO NOTHING",
    )?;
    let ts = now();
    let mut n = 0;
    for a in &fetched.articles {
        n += stmt.execute(params![
            feed_id,
            a.guid,
            a.title,
            a.link,
            a.snippet,
            a.image_url,
            a.published,
            ts
        ])?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::NewArticle;

    fn sample(n: usize) -> Fetched {
        Fetched {
            title: "Sample".into(),
            site_url: None,
            etag: None,
            last_modified: None,
            articles: (0..n)
                .map(|i| NewArticle {
                    guid: format!("g{i}"),
                    title: format!("Post {i}"),
                    link: format!("https://x.org/{i}"),
                    snippet: String::new(),
                    image_url: None,
                    published: 1_700_000_000 + i as i64,
                })
                .collect(),
        }
    }

    #[test]
    fn add_list_read_remove() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("aggrega-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let store = Store::open(&dir.join("t.db"))?;
        let (id, n) = store.add_feed("https://x.org/rss", &sample(3))?;
        assert_eq!(n, 3);
        assert!(store.add_feed("https://x.org/rss", &sample(1)).is_err());

        let all = store.articles(None, false, 10)?;
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].title, "Post 2", "newest first");

        store.set_read(all[0].id, true)?;
        assert_eq!(store.articles(Some(id), true, 10)?.len(), 2);
        assert_eq!(store.feeds()?[0].unread, 2);

        // Refreshing with the same items adds nothing.
        let s = store.apply_refresh(vec![(id, Ok(Some(sample(4))))])?;
        assert_eq!(s.new_articles, 1);

        // Being offline neither marks the source as broken nor loses articles.
        let before = store.articles(None, false, 10)?.len();
        let offline = anyhow::Error::new(crate::fetch::Unreachable("offline".into()));
        let s = store.apply_refresh(vec![(id, Err(offline))])?;
        assert!(s.offline());
        assert!(!store.feeds()?[0].has_error);
        assert_eq!(store.articles(None, false, 10)?.len(), before);

        store.remove_feed(id)?;
        assert!(store.articles(None, false, 10)?.is_empty());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
