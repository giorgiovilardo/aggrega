//! Local persistence in a single SQLite file (WAL mode).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::backup::Backup;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, params};

use crate::feed::{FeedJob, Fetched, Source};
use crate::fetch::{self, FeedRefresh};
use crate::search;

pub const SCHEMA_VERSION: i32 = 3;

/// Settings that belong to this computer, kept when a sync pull replaces
/// the database.
const LOCAL_SETTINGS: [&str; 4] = ["theme", "refresh_interval", "language", "zoom"];

/// Why a database operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `add_feed` for a URL that's already subscribed.
    #[error("already following {url}")]
    AlreadyFollowing { url: String },
    /// The database (this profile's, or a snapshot) has a newer schema than this build understands.
    #[error(
        "the database was made by a newer version of Aggrega (schema {found}, this one knows {SCHEMA_VERSION})"
    )]
    NewerSchema { found: i32 },
    /// A snapshot that isn't an Aggrega database at all.
    #[error("not an Aggrega database")]
    NotAnAggregaDatabase,
    /// SQLite's integrity check found problems in a snapshot; `report` is what it said.
    #[error("the snapshot is damaged: {report}")]
    Damaged { report: String },
    /// SQLite couldn't read a snapshot because it's corrupt.
    #[error("the snapshot is corrupt")]
    Corrupt(#[source] rusqlite::Error),
    #[error("{} isn't a valid UTF-8 path", path.display())]
    NonUtf8Path { path: PathBuf },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

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

/// Which articles `Store::articles` returns.
#[derive(Clone, Copy, Default)]
pub struct ArticleFilter {
    /// Only this feed's articles; all feeds when `None`.
    pub feed: Option<i64>,
    /// Only unread articles.
    pub unread_only: bool,
}

/// Everything the reader view needs for one article.
#[derive(Debug, Clone)]
pub struct ReaderArticle {
    pub title: String,
    pub link: String,
    pub image_url: Option<String>,
    pub published: i64,
    pub feed_title: String,
    /// `reader::encode`d blocks; `None` for articles stored before the reader existed.
    pub body: Option<String>,
}

/// What `Store::add_feed` stored for a new subscription.
#[derive(Debug, Clone, Copy)]
pub struct AddedFeed {
    pub id: i64,
    pub articles: usize,
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
    pub fn open(path: &Path) -> Result<Self, Error> {
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

    fn migrate(&self) -> Result<(), Error> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(Error::NewerSchema { found: version });
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
        if version < 2 {
            // Reader view content. NULL for older articles: the reader fetches the page instead.
            self.conn.execute_batch(
                "ALTER TABLE articles ADD COLUMN body TEXT;
                 PRAGMA user_version = 2;",
            )?;
        }
        if version < 3 {
            // Full-text index for search. It is contentless (the text lives only
            // in the index), filled from Rust as plain text by `index_article`, and
            // the trigger removes a row whenever its article is deleted. The
            // backfill runs in the same transaction, so a failure leaves v2 intact.
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE VIRTUAL TABLE articles_fts USING fts5(
                     title, snippet, body,
                     content='', contentless_delete=1,
                     tokenize='unicode61 remove_diacritics 2');
                 CREATE VIRTUAL TABLE articles_fts_vocab USING fts5vocab(articles_fts, row);
                 CREATE TRIGGER articles_fts_delete AFTER DELETE ON articles
                 BEGIN DELETE FROM articles_fts WHERE rowid = old.id; END;",
            )?;
            {
                let mut select = tx.prepare("SELECT id, title, snippet, body FROM articles")?;
                let mut rows = select.query([])?;
                while let Some(r) = rows.next()? {
                    let body: Option<String> = r.get(3)?;
                    index_article(
                        &tx,
                        r.get(0)?,
                        &r.get::<_, String>(1)?,
                        &r.get::<_, String>(2)?,
                        &search::body_text(body.as_deref().unwrap_or("")),
                    )?;
                }
            }
            tx.execute_batch("PRAGMA user_version = 3;")?;
            tx.commit()?;
        }
        Ok(())
    }

    // ---- settings -------------------------------------------------------

    pub fn setting(&self, key: &str) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- feeds ----------------------------------------------------------

    pub fn feeds(&self) -> Result<Vec<Feed>, Error> {
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

    /// Every subscription with its URLs, by title (for OPML export and import).
    pub fn sources(&self) -> Result<Vec<Source>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT title, url, site_url FROM feeds ORDER BY title COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Source {
                title: r.get(0)?,
                xml_url: r.get(1)?,
                html_url: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn fetch_jobs(&self) -> Result<Vec<FeedJob>, Error> {
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
    pub fn add_feed(&self, url: &str, fetched: &Fetched) -> Result<AddedFeed, Error> {
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
        )
        .map_err(|e| match e {
            // The UNIQUE constraint on url also holds against a concurrent add.
            rusqlite::Error::SqliteFailure(f, _)
                if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                Error::AlreadyFollowing { url: url.into() }
            }
            e => Error::Sqlite(e),
        })?;
        let id = tx.last_insert_rowid();
        let n = insert_articles(&tx, id, fetched)?;
        tx.commit()?;
        Ok(AddedFeed { id, articles: n })
    }

    pub fn remove_feed(&self, id: i64) -> Result<(), Error> {
        self.conn.execute("DELETE FROM feeds WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Stores the results of a refresh in a single transaction.
    pub fn apply_refresh(&self, results: Vec<FeedRefresh>) -> Result<RefreshSummary, Error> {
        let mut summary = RefreshSummary {
            total: results.len(),
            ..Default::default()
        };
        let ts = now();
        let tx = self.conn.unchecked_transaction()?;
        for FeedRefresh { id, result } in results {
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
                Err(fetch::Error::Unreachable { .. }) => summary.unreachable += 1,
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
    pub fn articles(&self, filter: ArticleFilter, limit: usize) -> Result<Vec<Article>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT a.id, a.title, a.link, a.snippet, a.image_url, a.published, a.read, f.title
             FROM articles a JOIN feeds f ON f.id = a.feed_id
             WHERE (?1 IS NULL OR a.feed_id = ?1) AND (?2 = 0 OR a.read = 0)
             ORDER BY a.published DESC, a.id DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![filter.feed, filter.unread_only, limit as i64],
            article_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Articles matching `query`, best tier first and newest first within a
    /// tier, up to `limit`. Later tiers only run while there is room left, and
    /// typo tiers only when the exact tier found fewer than `search::FUZZY_BELOW`.
    pub fn search(
        &self,
        filter: ArticleFilter,
        query: &search::Query,
        limit: usize,
    ) -> Result<Vec<Article>, Error> {
        let mut found = Vec::new();
        for (tier, expr) in query.tiers().iter().enumerate() {
            if found.len() >= limit {
                break;
            }
            let mut stmt = self.conn.prepare_cached(
                "SELECT a.id, a.title, a.link, a.snippet, a.image_url, a.published, a.read, f.title
                 FROM articles a JOIN feeds f ON f.id = a.feed_id
                 WHERE (?1 IS NULL OR a.feed_id = ?1) AND (?2 = 0 OR a.read = 0)
                   AND a.id IN (SELECT rowid FROM articles_fts WHERE articles_fts MATCH ?3)
                 ORDER BY a.published DESC, a.id DESC
                 LIMIT ?4",
            )?;
            let rows = stmt.query_map(
                params![
                    filter.feed,
                    filter.unread_only,
                    expr,
                    (limit - found.len()) as i64
                ],
                article_row,
            )?;
            found.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
            if tier == 0 && found.len() >= search::FUZZY_BELOW {
                break;
            }
        }
        Ok(found)
    }

    /// Every distinct word in the search index, folded as the index folds it.
    pub fn search_terms(&self) -> Result<Vec<String>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT term FROM articles_fts_vocab")?;
        Ok(stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Changes whenever another connection writes to the database, so a
    /// reader can tell that its cached data has gone stale.
    pub fn data_version(&self) -> Result<i64, Error> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?)
    }

    pub fn reader_article(&self, id: i64) -> Result<Option<ReaderArticle>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT a.title, a.link, a.image_url, a.published, f.title, a.body
                 FROM articles a JOIN feeds f ON f.id = a.feed_id
                 WHERE a.id = ?1",
                [id],
                |r| {
                    Ok(ReaderArticle {
                        title: r.get(0)?,
                        link: r.get(1)?,
                        image_url: r.get(2)?,
                        published: r.get(3)?,
                        feed_title: r.get(4)?,
                        body: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    /// Saves reader content fetched from the article's web page.
    pub fn set_body(&self, article: i64, body: &str) -> Result<(), Error> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE articles SET body = ?2 WHERE id = ?1",
            params![article, body],
        )?;
        tx.execute("DELETE FROM articles_fts WHERE rowid = ?1", [article])?;
        tx.execute(
            "INSERT INTO articles_fts(rowid, title, snippet, body)
             SELECT id, title, snippet, ?2 FROM articles WHERE id = ?1",
            params![article, search::body_text(body)],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_read(&self, article: i64, read: bool) -> Result<(), Error> {
        self.conn.execute(
            "UPDATE articles SET read = ?2 WHERE id = ?1",
            params![article, read],
        )?;
        Ok(())
    }

    pub fn mark_all_read(&self, feed: Option<i64>) -> Result<usize, Error> {
        Ok(self.conn.execute(
            "UPDATE articles SET read = 1 WHERE read = 0 AND (?1 IS NULL OR feed_id = ?1)",
            params![feed],
        )?)
    }

    // ---- sync -----------------------------------------------------------

    pub fn stats(&self) -> Result<Stats, Error> {
        Ok(self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM feeds), COUNT(*), COALESCE(SUM(read = 0), 0)
             FROM articles",
            [],
            |r| {
                Ok(Stats {
                    feeds: r.get(0)?,
                    articles: r.get(1)?,
                    unread: r.get(2)?,
                })
            },
        )?)
    }

    /// Writes a consistent, compacted copy of the database to `path`, which
    /// must not exist yet. Safe while other connections write.
    pub fn snapshot_to(&self, path: &Path) -> Result<(), Error> {
        let path = path.to_str().ok_or_else(|| Error::NonUtf8Path {
            path: path.to_path_buf(),
        })?;
        self.conn.execute("VACUUM INTO ?1", [path])?;
        Ok(())
    }

    /// Replaces everything in this database with the snapshot at `path`
    /// (from `snapshot_to`, possibly on another computer), keeping only this
    /// computer's `LOCAL_SETTINGS`. Other connections see the new contents
    /// right away.
    pub fn replace_with(&mut self, path: &Path) -> Result<(), Error> {
        let src = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(snapshot_error)?;
        let check: String = src
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(snapshot_error)?;
        if check != "ok" {
            return Err(Error::Damaged { report: check });
        }
        let version: i32 = src.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(Error::NewerSchema { found: version });
        }
        let tables: i64 = src.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN ('feeds', 'articles', 'settings')",
            [],
            |r| r.get(0),
        )?;
        if version < 1 || tables != 3 {
            return Err(Error::NotAnAggregaDatabase);
        }

        let local = LOCAL_SETTINGS
            .iter()
            .map(|key| Ok((*key, self.setting(key)?)))
            .collect::<Result<Vec<_>, Error>>()?;
        // One step copies every page under a single lock, so readers never
        // see a half-replaced database.
        Backup::new(&src, &mut self.conn)?.step(-1)?;
        drop(src);
        self.migrate()?;
        for (key, value) in local {
            match value {
                Some(v) => self.set_setting(key, &v)?,
                None => {
                    self.conn
                        .execute("DELETE FROM settings WHERE key = ?1", [key])?;
                }
            }
        }
        Ok(())
    }
}

/// What a database holds, to show before replacing one with another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub feeds: i64,
    pub articles: i64,
    pub unread: i64,
}

fn insert_articles(conn: &Connection, feed_id: i64, fetched: &Fetched) -> Result<usize, Error> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO articles(feed_id, guid, title, link, snippet, image_url, published, fetched_at, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(feed_id, guid) DO NOTHING",
    )?;
    let ts = now();
    let mut n = 0;
    for a in &fetched.articles {
        // Only new rows are indexed: a repeated guid is ignored, and must not
        // add a second index row.
        let inserted = stmt.execute(params![
            feed_id,
            a.guid,
            a.title,
            a.link,
            a.snippet,
            a.image_url,
            a.published,
            ts,
            a.body
        ])?;
        if inserted == 1 {
            index_article(
                conn,
                conn.last_insert_rowid(),
                &a.title,
                &a.snippet,
                &search::body_text(&a.body),
            )?;
        }
        n += inserted;
    }
    Ok(n)
}

/// Classifies an error from opening or checking a snapshot file.
fn snapshot_error(e: rusqlite::Error) -> Error {
    match e.sqlite_error_code() {
        Some(ErrorCode::NotADatabase) => Error::NotAnAggregaDatabase,
        Some(ErrorCode::DatabaseCorrupt) => Error::Corrupt(e),
        _ => Error::Sqlite(e),
    }
}

/// Adds one article to the search index. `body` is already plain text.
fn index_article(
    conn: &Connection,
    id: i64,
    title: &str,
    snippet: &str,
    body: &str,
) -> Result<(), Error> {
    conn.prepare_cached(
        "INSERT INTO articles_fts(rowid, title, snippet, body) VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute(params![id, title, snippet, body])?;
    Ok(())
}

fn article_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Article> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::NewArticle;
    use crate::reader::{self, Block, Kind};
    use crate::search::corpus;
    use std::collections::{BTreeMap, HashMap};

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
                    body: format!("p Body {i}\n"),
                })
                .collect(),
        }
    }

    fn temp_dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("aggrega-{name}-"))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn reader_body_is_stored_and_updatable() -> Result<(), Error> {
        let dir = temp_dir("reader");
        let store = Store::open(&dir.path().join("t.db"))?;
        let feed = store.add_feed("https://x.org/rss", &sample(2))?.id;
        let id = store.articles(
            ArticleFilter {
                feed: Some(feed),
                ..Default::default()
            },
            1,
        )?[0]
            .id;

        let a = store.reader_article(id)?.expect("article exists");
        assert_eq!(a.title, "Post 1");
        assert_eq!(a.feed_title, "Sample");
        assert_eq!(a.link, "https://x.org/1");
        assert_eq!(a.body.as_deref(), Some("p Body 1\n"));

        store.set_body(id, "p Full page\n")?;
        let a = store.reader_article(id)?.unwrap();
        assert_eq!(a.body.as_deref(), Some("p Full page\n"));
        assert!(store.reader_article(9999)?.is_none());
        Ok(())
    }

    #[test]
    fn migrates_v1_databases() -> Result<(), Error> {
        let dir = temp_dir("migrate");
        let path = dir.path().join("t.db");
        {
            // A database as the first release created it.
            let conn = Connection::open(&path)?;
            conn.execute_batch(
                "CREATE TABLE feeds (id INTEGER PRIMARY KEY, url TEXT NOT NULL UNIQUE,
                    title TEXT NOT NULL, site_url TEXT, etag TEXT, last_modified TEXT,
                    last_fetched INTEGER, last_error TEXT, added_at INTEGER NOT NULL);
                 CREATE TABLE articles (id INTEGER PRIMARY KEY,
                    feed_id INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
                    guid TEXT NOT NULL, title TEXT NOT NULL, link TEXT NOT NULL,
                    snippet TEXT NOT NULL, image_url TEXT, published INTEGER NOT NULL,
                    fetched_at INTEGER NOT NULL, read INTEGER NOT NULL DEFAULT 0,
                    UNIQUE (feed_id, guid));
                 CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO feeds VALUES (1, 'https://x.org/rss', 'Old', NULL, NULL, NULL, NULL, NULL, 0);
                 INSERT INTO articles (feed_id, guid, title, link, snippet, published, fetched_at)
                    VALUES (1, 'g', 'Old post', 'https://x.org/old', '', 0, 0);
                 PRAGMA user_version = 1;",
            )?;
        }
        let store = Store::open(&path)?;
        let id = store.articles(ArticleFilter::default(), 10)?[0].id;
        let a = store.reader_article(id)?.unwrap();
        assert_eq!(a.title, "Old post");
        assert_eq!(a.body, None, "old articles have no stored body");
        drop(store);
        // Opening again is a no-op.
        Store::open(&path)?;
        Ok(())
    }

    #[test]
    fn add_list_read_remove() -> Result<(), Error> {
        let dir = temp_dir("test");
        let store = Store::open(&dir.path().join("t.db"))?;
        let added = store.add_feed("https://x.org/rss", &sample(3))?;
        let id = added.id;
        assert_eq!(added.articles, 3);
        let before = store.stats()?;
        match store.add_feed("https://x.org/rss", &sample(1)) {
            Err(Error::AlreadyFollowing { url }) => assert_eq!(url, "https://x.org/rss"),
            other => panic!("expected AlreadyFollowing, got {other:?}"),
        }
        assert_eq!(store.stats()?, before, "nothing else changed");

        let all = store.articles(ArticleFilter::default(), 10)?;
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].title, "Post 2", "newest first");

        store.set_read(all[0].id, true)?;
        let unread_here = ArticleFilter {
            feed: Some(id),
            unread_only: true,
        };
        assert_eq!(store.articles(unread_here, 10)?.len(), 2);
        assert_eq!(store.feeds()?[0].unread, 2);

        // Refreshing with the same items adds nothing.
        let s = store.apply_refresh(vec![FeedRefresh {
            id,
            result: Ok(Some(sample(4))),
        }])?;
        assert_eq!(s.new_articles, 1);

        // Being offline neither marks the source as broken nor loses articles.
        let before = store.articles(ArticleFilter::default(), 10)?.len();
        let offline = fetch::Error::Unreachable {
            url: "http://127.0.0.1:9/".into(),
            why: fetch::Offline::HostNotFound,
            source: ureq::Error::HostNotFound,
        };
        let s = store.apply_refresh(vec![FeedRefresh {
            id,
            result: Err(offline),
        }])?;
        assert!(s.offline());
        assert!(!store.feeds()?[0].has_error);
        assert_eq!(store.articles(ArticleFilter::default(), 10)?.len(), before);

        store.remove_feed(id)?;
        assert!(store.articles(ArticleFilter::default(), 10)?.is_empty());
        Ok(())
    }

    #[test]
    fn replaces_everything_from_a_snapshot() -> Result<(), Error> {
        let dir = temp_dir("replace");
        let theirs = Store::open(&dir.path().join("theirs.db"))?;
        let feed = theirs.add_feed("https://x.org/rss", &sample(3))?.id;
        let first = theirs.articles(
            ArticleFilter {
                feed: Some(feed),
                ..Default::default()
            },
            1,
        )?[0]
            .id;
        theirs.set_read(first, true)?;
        theirs.set_setting("theme", "dark")?;
        theirs.set_setting("last_refresh", "123")?;
        theirs.set_setting("refresh_interval", "60")?;
        theirs.set_setting("zoom", "125")?;
        assert_eq!(
            theirs.stats()?,
            Stats {
                feeds: 1,
                articles: 3,
                unread: 2
            }
        );
        let snapshot = dir.path().join("snapshot.db");
        theirs.snapshot_to(&snapshot)?;

        let mut ours = Store::open(&dir.path().join("ours.db"))?;
        ours.add_feed("https://y.org/feed", &sample(5))?;
        ours.set_setting("theme", "light")?;
        ours.set_setting("zoom", "90")?;
        // Another connection, like the UI thread's, sees the new contents.
        let ui = Store::open(&dir.path().join("ours.db"))?;
        ours.replace_with(&snapshot)?;
        assert_eq!(ui.stats()?, theirs.stats()?);
        assert_eq!(ui.sources()?, theirs.sources()?);
        assert_eq!(ui.setting("last_refresh")?.as_deref(), Some("123"));
        assert_eq!(
            ui.setting("theme")?.as_deref(),
            Some("light"),
            "theme stays"
        );
        assert_eq!(ui.setting("refresh_interval")?, None, "interval stays");
        assert_eq!(ui.setting("zoom")?.as_deref(), Some("90"), "zoom stays");
        let mode: String = ui.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        assert_eq!(mode, "wal");
        // Still writable through either connection.
        ui.mark_all_read(None)?;
        assert_eq!(ours.stats()?.unread, 0);
        Ok(())
    }

    #[test]
    fn refuses_snapshots_it_cannot_use() -> Result<(), Error> {
        let dir = temp_dir("refuse");
        let mut ours = Store::open(&dir.path().join("ours.db"))?;
        ours.add_feed("https://y.org/feed", &sample(2))?;
        let before = ours.stats()?;

        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, b"definitely not sqlite, just some bytes here").unwrap();
        assert!(matches!(
            ours.replace_with(&junk),
            Err(Error::NotAnAggregaDatabase)
        ));

        let newer = dir.path().join("newer.db");
        Store::open(&newer)?
            .conn
            .execute_batch("PRAGMA user_version = 99")?;
        assert!(matches!(
            ours.replace_with(&newer),
            Err(Error::NewerSchema { found: 99 })
        ));

        let other = dir.path().join("other.db");
        Connection::open(&other)?
            .execute_batch("CREATE TABLE notes (x); PRAGMA user_version = 1;")?;
        assert!(matches!(
            ours.replace_with(&other),
            Err(Error::NotAnAggregaDatabase)
        ));

        assert_eq!(ours.stats()?, before, "nothing was touched");
        Ok(())
    }

    #[test]
    fn refuses_damaged_snapshots() -> Result<(), Error> {
        let dir = temp_dir("damaged");
        let theirs = Store::open(&dir.path().join("theirs.db"))?;
        theirs.add_feed("https://x.org/rss", &sample(300))?;
        let snapshot = dir.path().join("snapshot.db");
        theirs.snapshot_to(&snapshot)?;
        let mut ours = Store::open(&dir.path().join("ours.db"))?;

        // Page 2 overwritten: the integrity check reports it.
        let mut bytes = std::fs::read(&snapshot).unwrap();
        bytes[4096..8192].fill(0xff);
        let garbled = dir.path().join("garbled.db");
        std::fs::write(&garbled, &bytes).unwrap();
        assert!(matches!(
            ours.replace_with(&garbled),
            Err(Error::Damaged { .. })
        ));

        // Cut in half: SQLite finds pages past the end of the file.
        let mut bytes = std::fs::read(&snapshot).unwrap();
        bytes.truncate(bytes.len() / 2);
        let cut = dir.path().join("cut.db");
        std::fs::write(&cut, &bytes).unwrap();
        assert!(matches!(ours.replace_with(&cut), Err(Error::Corrupt(_))));
        Ok(())
    }

    #[test]
    fn refuses_to_open_a_newer_database() -> Result<(), Error> {
        let dir = temp_dir("newer-open");
        let path = dir.path().join("t.db");
        Connection::open(&path)?.execute_batch("PRAGMA user_version = 99")?;
        assert!(matches!(
            Store::open(&path),
            Err(Error::NewerSchema { found: 99 })
        ));
        Ok(())
    }

    #[test]
    fn lists_sources_for_export() -> Result<(), Error> {
        let dir = temp_dir("sources");
        let store = Store::open(&dir.path().join("t.db"))?;
        let mut zine = sample(0);
        zine.title = "a zine".into();
        zine.site_url = Some("https://y.org/".into());
        store.add_feed("https://x.org/rss", &sample(1))?;
        store.add_feed("https://y.org/feed", &zine)?;
        assert_eq!(
            store.sources()?,
            [
                Source {
                    title: "a zine".into(),
                    xml_url: "https://y.org/feed".into(),
                    html_url: Some("https://y.org/".into()),
                },
                Source {
                    title: "Sample".into(),
                    xml_url: "https://x.org/rss".into(),
                    html_url: None,
                },
            ]
        );
        Ok(())
    }

    /// A feed whose articles have these titles; later entries are newer.
    fn titled(title: &str, articles: &[&str]) -> Fetched {
        Fetched {
            title: title.into(),
            site_url: None,
            etag: None,
            last_modified: None,
            articles: articles
                .iter()
                .enumerate()
                .map(|(i, t)| NewArticle {
                    guid: t.to_string(),
                    title: t.to_string(),
                    link: format!("https://x.org/{i}"),
                    snippet: String::new(),
                    image_url: None,
                    published: 1_700_000_000 + i as i64,
                    body: String::new(),
                })
                .collect(),
        }
    }

    /// A database as release 2 created it: the v1 tables plus `articles.body`.
    fn write_v2_database(path: &std::path::Path) -> Result<(), Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE feeds (id INTEGER PRIMARY KEY, url TEXT NOT NULL UNIQUE,
                title TEXT NOT NULL, site_url TEXT, etag TEXT, last_modified TEXT,
                last_fetched INTEGER, last_error TEXT, added_at INTEGER NOT NULL);
             CREATE TABLE articles (id INTEGER PRIMARY KEY,
                feed_id INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
                guid TEXT NOT NULL, title TEXT NOT NULL, link TEXT NOT NULL,
                snippet TEXT NOT NULL, image_url TEXT, published INTEGER NOT NULL,
                fetched_at INTEGER NOT NULL, read INTEGER NOT NULL DEFAULT 0,
                body TEXT, UNIQUE (feed_id, guid));
             CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO feeds VALUES (1, 'https://x.org/rss', 'Old', NULL, NULL, NULL, NULL, NULL, 0);
             PRAGMA user_version = 2;",
        )?;
        let body = reader::encode(&[
            Block::new(Kind::Heading, "Heading words"),
            Block::new(Kind::Paragraph, "Prose about lighthouses."),
            Block::new(Kind::Code, "secretcodeword()"),
        ]);
        conn.execute(
            "INSERT INTO articles (feed_id, guid, title, link, snippet, published, fetched_at, body)
             VALUES (1, 'a', 'Ferrovie nuove', 'https://x.org/a', 'Snippet here', 300, 0, NULL)",
            [],
        )?;
        conn.execute(
            "INSERT INTO articles (feed_id, guid, title, link, snippet, published, fetched_at, body)
             VALUES (1, 'b', 'With body', 'https://x.org/b', 'Summary', 200, 0, ?1)",
            params![body],
        )?;
        Ok(())
    }

    /// Titles of the articles `q` finds, searched the way the UI does: with
    /// the store's own vocabulary for typo tolerance.
    fn found_in(
        store: &Store,
        q: &str,
        filter: ArticleFilter,
        limit: usize,
    ) -> Result<Vec<String>, Error> {
        let terms = search::Terms::new(store.search_terms()?);
        let query = search::compile(q, Some(&terms)).expect("query has tokens");
        Ok(store
            .search(filter, &query, limit)?
            .into_iter()
            .map(|a| a.title)
            .collect())
    }

    fn found(store: &Store, q: &str) -> Result<Vec<String>, Error> {
        found_in(store, q, ArticleFilter::default(), 50)
    }

    /// Every `(term, doc, cnt)` row of the vocabulary table. Not asserted on:
    /// it shows what the index keeps after deletes.
    fn vocab(store: &Store) -> Result<Vec<(String, i64, i64)>, Error> {
        Ok(store
            .conn
            .prepare("SELECT term, doc, cnt FROM articles_fts_vocab")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Rows of the search index that contain `term`.
    fn index_docs(store: &Store, term: &str) -> Result<i64, Error> {
        Ok(store
            .conn
            .query_row(
                "SELECT doc FROM articles_fts_vocab WHERE term = ?1",
                [term],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    #[test]
    fn migrating_v2_builds_a_search_index() -> Result<(), Error> {
        let dir = temp_dir("migrate-v2");
        let path = dir.path().join("t.db");
        write_v2_database(&path)?;
        let store = Store::open(&path)?;
        let version: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        assert_eq!(version, SCHEMA_VERSION);

        // A NULL body is searchable by title and snippet.
        assert_eq!(found(&store, "ferrovie")?, ["Ferrovie nuove"]);
        assert_eq!(found(&store, "snippet")?, ["Ferrovie nuove"]);
        // Encoded bodies are indexed as prose, but not their code blocks.
        assert_eq!(found(&store, "lighthouses")?, ["With body"]);
        assert_eq!(found(&store, "heading")?, ["With body"]);
        assert!(found(&store, "secretcodeword")?.is_empty());
        Ok(())
    }

    #[test]
    fn replacing_with_a_v2_snapshot_indexes_it() -> Result<(), Error> {
        let dir = temp_dir("replace-v2");
        let snapshot = dir.path().join("v2.db");
        write_v2_database(&snapshot)?;
        let mut ours = Store::open(&dir.path().join("ours.db"))?;
        ours.add_feed("https://y.org/feed", &titled("Mine", &["Local story"]))?;
        ours.replace_with(&snapshot)?;
        assert_eq!(found(&ours, "lighthouses")?, ["With body"]);
        assert_eq!(found(&ours, "ferrovie")?, ["Ferrovie nuove"]);
        assert!(found(&ours, "local")?.is_empty());
        Ok(())
    }

    #[test]
    fn new_articles_are_indexed_once() -> Result<(), Error> {
        let dir = temp_dir("index-once");
        let store = Store::open(&dir.path().join("t.db"))?;
        let feed = store.add_feed("https://x.org/rss", &sample(3))?.id;
        assert_eq!(index_docs(&store, "post")?, 3);
        assert_eq!(index_docs(&store, "body")?, 3);

        // The same guids again: nothing new, and no second index row.
        store.apply_refresh(vec![FeedRefresh {
            id: feed,
            result: Ok(Some(sample(3))),
        }])?;
        assert_eq!(index_docs(&store, "post")?, 3);

        store.apply_refresh(vec![FeedRefresh {
            id: feed,
            result: Ok(Some(sample(4))),
        }])?;
        assert_eq!(index_docs(&store, "post")?, 4);
        assert_eq!(found(&store, "post 3")?, ["Post 3"]);
        Ok(())
    }

    #[test]
    fn set_body_reindexes_the_article() -> Result<(), Error> {
        let dir = temp_dir("set-body");
        let store = Store::open(&dir.path().join("t.db"))?;
        let feed = store.add_feed("https://x.org/rss", &sample(2))?.id;
        let id = store.articles(
            ArticleFilter {
                feed: Some(feed),
                ..Default::default()
            },
            1,
        )?[0]
            .id;
        assert_eq!(found(&store, "body")?.len(), 2);

        let page = reader::encode(&[Block::new(Kind::Paragraph, "Full page about lighthouses")]);
        store.set_body(id, &page)?;
        assert_eq!(found(&store, "lighthouses")?, ["Post 1"]);
        assert_eq!(found(&store, "body")?, ["Post 0"], "the old body is gone");
        assert_eq!(
            found(&store, "post")?.len(),
            2,
            "the title is still indexed"
        );
        Ok(())
    }

    #[test]
    fn removing_feeds_and_pruning_drop_index_rows() -> Result<(), Error> {
        let dir = temp_dir("index-delete");
        let store = Store::open(&dir.path().join("t.db"))?;
        let keep = store.add_feed("https://x.org/rss", &titled("Keep", &["Lighthouse keeper"]))?;
        let gone = store.add_feed("https://y.org/feed", &titled("Gone", &["Lighthouse tower"]))?;
        assert_eq!(found(&store, "lighthouse")?.len(), 2);

        store.remove_feed(gone.id)?;
        assert_eq!(found(&store, "lighthouse")?, ["Lighthouse keeper"]);
        println!("vocab after remove_feed: {:?}", vocab(&store)?);

        // Read articles older than 90 days are pruned by the next refresh.
        let article = store.articles(
            ArticleFilter {
                feed: Some(keep.id),
                ..Default::default()
            },
            1,
        )?[0]
            .id;
        store.set_read(article, true)?;
        store.apply_refresh(vec![])?;
        assert!(found(&store, "lighthouse")?.is_empty());

        println!("vocab after prune: {:?}", vocab(&store)?);
        Ok(())
    }

    #[test]
    fn exact_matches_come_before_typos_and_newest_first_within_a_tier() -> Result<(), Error> {
        let dir = temp_dir("tiers");
        let store = Store::open(&dir.path().join("t.db"))?;
        store.add_feed(
            "https://x.org/rss",
            &titled("Feed", &["Kernel old", "Kernal typo", "Kernel new"]),
        )?;
        let other = store.add_feed(
            "https://y.org/feed",
            &titled("Other", &["Kernel elsewhere"]),
        )?;
        let all = ArticleFilter::default();
        assert_eq!(
            found(&store, "kernel")?,
            [
                "Kernel new",
                "Kernel elsewhere",
                "Kernel old",
                "Kernal typo"
            ]
        );
        assert_eq!(
            found_in(&store, "kernel", all, 2)?,
            ["Kernel new", "Kernel elsewhere"]
        );
        assert!(found_in(&store, "kernel", all, 0)?.is_empty());

        let only_other = ArticleFilter {
            feed: Some(other.id),
            ..Default::default()
        };
        assert_eq!(
            found_in(&store, "kernel", only_other, 50)?,
            ["Kernel elsewhere"]
        );

        let newest = store.articles(ArticleFilter::default(), 1)?[0].id;
        store.set_read(newest, true)?;
        let unread = ArticleFilter {
            unread_only: true,
            ..Default::default()
        };
        let titles = found_in(&store, "kernel", unread, 50)?;
        assert!(!titles.contains(&"Kernel new".to_string()), "{titles:?}");
        assert!(titles.contains(&"Kernal typo".to_string()), "{titles:?}");
        Ok(())
    }

    #[test]
    fn typo_tiers_only_run_when_exact_matches_are_scarce() -> Result<(), Error> {
        let dir = temp_dir("fuzzy-below");
        let store = Store::open(&dir.path().join("t.db"))?;
        let exact: Vec<String> = (0..10).map(|i| format!("Kernel {i}")).collect();
        let mut titles: Vec<&str> = exact.iter().map(String::as_str).collect();
        titles.push("Kernal typo");
        store.add_feed("https://x.org/rss", &titled("Feed", &titles))?;
        let got = found(&store, "kernel")?;
        assert_eq!(got.len(), 10, "{got:?}");
        assert!(!got.contains(&"Kernal typo".to_string()), "{got:?}");

        // One exact match fewer: the typo follows the exact ones.
        let dir = temp_dir("fuzzy-below-9");
        let store = Store::open(&dir.path().join("t.db"))?;
        store.add_feed("https://x.org/rss", &titled("Feed", &titles[1..]))?;
        let got = found(&store, "kernel")?;
        assert_eq!(got.len(), 10, "{got:?}");
        assert_eq!(got.last().map(String::as_str), Some("Kernal typo"));
        Ok(())
    }

    #[test]
    fn folded_spelling_matches_in_the_exact_tier() -> Result<(), Error> {
        let dir = temp_dir("folded-prefix");
        let store = Store::open(&dir.path().join("t.db"))?;
        store.add_feed(
            "https://x.org/rss",
            &titled("Feed", &["Straßenbahn fährt", "Strasze typo"]),
        )?;
        // The typo is newer, so it would come first if it were in the exact tier.
        assert_eq!(
            found(&store, "strasse")?,
            ["Straßenbahn fährt", "Strasze typo"]
        );
        Ok(())
    }

    #[test]
    fn data_version_changes_when_another_connection_writes() -> Result<(), Error> {
        let dir = temp_dir("data-version");
        let path = dir.path().join("t.db");
        let store = Store::open(&path)?;
        let other = Store::open(&path)?;
        let before = store.data_version()?;
        other.set_setting("zoom", "100")?;
        assert_ne!(store.data_version()?, before);
        Ok(())
    }

    /// Runs every corpus case through the UI's search path and prints a score
    /// table. The exact, diacritics, prefix and cyrillic kinds must all pass.
    #[test]
    fn corpus_search_scores() -> Result<(), Error> {
        let dir = temp_dir("corpus");
        let store = Store::open(&dir.path().join("t.db"))?;
        let docs = corpus::docs();
        store.add_feed(
            "https://corpus.org/rss",
            &Fetched {
                title: "Corpus".into(),
                site_url: None,
                etag: None,
                last_modified: None,
                articles: docs
                    .iter()
                    .map(|d| NewArticle {
                        guid: d.id.to_string(),
                        title: d.title.clone(),
                        link: format!("https://corpus.org/{}", d.id),
                        snippet: d.snippet.clone(),
                        image_url: None,
                        published: 1_700_000_000 + d.id,
                        body: reader::encode(&[Block::new(Kind::Paragraph, d.body.clone())]),
                    })
                    .collect(),
            },
        )?;
        // Article row id -> corpus id, which was stored as the guid.
        let corpus_id: HashMap<i64, i64> = {
            let mut stmt = store.conn.prepare("SELECT id, guid FROM articles")?;
            let rows = stmt.query_map([], |r| {
                let guid: String = r.get(1)?;
                Ok((r.get(0)?, guid.parse().unwrap_or(-1)))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };

        let terms = search::Terms::new(store.search_terms()?);
        println!("vocabulary: {} terms", terms.len());
        let strict = ["exact", "diacritics", "prefix", "cyrillic"];
        let mut table: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        let mut failures: Vec<String> = Vec::new();
        let mut strict_failures: Vec<&str> = Vec::new();
        for c in corpus::cases() {
            let query = search::compile(c.query, Some(&terms)).expect("corpus queries have tokens");
            let results: Vec<i64> = store
                .search(ArticleFilter::default(), &query, 50)?
                .iter()
                .filter_map(|a| corpus_id.get(&a.id).copied())
                .collect();
            let entry = table.entry(c.kind).or_default();
            entry.1 += 1;
            if corpus::score(&results, &c) {
                entry.0 += 1;
            } else {
                failures.push(format!(
                    "[{}] {:?} expect {:?} got {:?}\n      tiers {:?}",
                    c.kind,
                    c.query,
                    c.expect,
                    results,
                    query.tiers()
                ));
                if strict.contains(&c.kind) {
                    strict_failures.push(c.query);
                }
            }
        }
        println!("{:<20} {:>6} {:>6}", "kind", "pass", "total");
        for (kind, (pass, total)) in &table {
            println!("{kind:<20} {pass:>6} {total:>6}");
        }
        println!("failing cases ({}):", failures.len());
        for f in &failures {
            println!("  {f}");
        }
        assert!(strict_failures.is_empty(), "failed: {strict_failures:?}");
        Ok(())
    }
}
