//! The application state behind the window: the list and reader models,
//! caches and refresh state, and every action the UI can trigger. It lives
//! on the UI thread; background work reports back through `with_app`.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use slint::{
    ComponentHandle, Model, ModelRc, Rgb8Pixel, Rgba8Pixel, SharedPixelBuffer, SharedString,
    VecModel,
};

use crate::db::{Article, ArticleFilter, RefreshSummary, SCHEMA_VERSION, Stats, Store};
use crate::feed;
use crate::i18n::{self, Lang, tr, trn};
use crate::ops::{self, AddedSource, Core, ImportSummary};
use crate::profile::Paths;
use crate::reader::{self, Block, Kind};
use crate::search;
use crate::{
    AppWindow, ArticleItem, BlockKind, FeedItem, ReaderBlock, ReaderInfo, SyncPeer, pool, sync,
    text, thumbs,
};

/// How many articles the list shows at most (newest first).
const MAX_ARTICLES: usize = 400;

/// How long a row takes to animate out before it is removed from the list
/// (matches the fade + collapse in `ui/article-card.slint`).
const LEAVE_ANIMATION: Duration = Duration::from_millis(560);

/// Pause after the last keystroke before the search runs.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// Background refresh interval choices in minutes, offered by the stepper in
/// Settings › Updates. Saved values are clamped to the first and last.
const REFRESH_STEPS: [i64; 9] = [5, 10, 15, 20, 30, 45, 60, 90, 120];
const DEFAULT_REFRESH_MINUTES: i64 = 20;

/// Zoom levels in percent, stepped through by Ctrl/Cmd + and -.
const ZOOM_STEPS: [i64; 7] = [80, 90, 100, 110, 125, 140, 160];
const DEFAULT_ZOOM: i64 = 100;

thread_local! {
    /// The application state lives on the UI thread; worker threads reach it
    /// through `slint::invoke_from_event_loop` + `with_app`.
    static APP: OnceCell<Rc<App>> = const { OnceCell::new() };
}

pub fn with_app(f: impl FnOnce(&Rc<App>)) {
    APP.with(|cell| {
        if let Some(app) = cell.get() {
            f(app)
        }
    });
}

/// A finished thumbnail, converted to Slint pixels on the worker thread so the
/// UI thread never copies image data.
enum ThumbResult {
    Ready(SharedPixelBuffer<Rgb8Pixel>),
    Broken,
    Unavailable,
}

fn rgb_buffer(img: &image::RgbImage) -> SharedPixelBuffer<Rgb8Pixel> {
    SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height())
}

fn rgba_buffer(img: &image::RgbaImage) -> SharedPixelBuffer<Rgba8Pixel> {
    SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height())
}

/// Switches the language everywhere: Rust strings and Slint's bundled
/// translations. Needs the window to exist (it registers the bundled ones).
fn select_language(lang: Lang) {
    i18n::set_lang(lang);
    if let Err(e) = slint::select_bundled_translation(lang.code()) {
        eprintln!("aggrega: couldn't switch the interface language: {e}");
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn tint(seed: &str) -> slint::Color {
    let (r, g, b) = text::tint(seed);
    slint::Color::from_rgb_u8(r, g, b)
}

/// `path` for display, with the home directory shortened to `~`.
fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}

pub struct App {
    ui: slint::Weak<AppWindow>,
    store: Store,
    core: Core,
    articles: Rc<VecModel<ArticleItem>>,
    /// Publish timestamps of `articles`, row for row (for relative time labels).
    article_ts: RefCell<Vec<i64>>,
    feeds: Rc<VecModel<FeedItem>>,
    thumb_cache: RefCell<HashMap<SharedString, slint::Image>>,
    thumb_pending: RefCell<HashSet<SharedString>>,
    thumb_failed: RefCell<HashSet<SharedString>>,
    refreshing: Cell<bool>,
    /// The running refresh was started by `auto_refresh`, so it stays quiet
    /// unless it finds something.
    background: Cell<bool>,
    /// Fires every `refresh_minutes`, counted from the end of the last refresh.
    auto_refresh: slint::Timer,
    refresh_minutes: Cell<i64>,
    last_refresh: Cell<Option<i64>>,
    /// The last refresh couldn't reach any source.
    offline: Cell<bool>,
    reader_blocks: Rc<VecModel<ReaderBlock>>,
    /// The article open in the reader; async results for any other are dropped.
    reader_id: Cell<Option<i64>>,
    /// Its link and lead image, and how much text the reader currently shows.
    reader_link: RefCell<String>,
    reader_lead: RefCell<Option<String>>,
    reader_len: Cell<usize>,
    /// The ids of the list the reader was opened from, as it was then (read
    /// stories leave the Unread list, but next/previous still walk past them),
    /// and the open story's position in it.
    reader_list: RefCell<Vec<i64>>,
    reader_pos: Cell<usize>,
    /// Page downloads still running, so stepping onto a story being
    /// prefetched waits for that download instead of starting another.
    pages_pending: RefCell<HashSet<i64>>,
    /// The next story, loaded ahead of time: its id and the pictures that
    /// arrived for it (`None` for broken ones), or are still on their way.
    prefetch_id: Cell<Option<i64>>,
    prefetch_pictures: RefCell<HashMap<String, Option<slint::Image>>>,
    prefetch_pending: RefCell<HashSet<String>>,
    sync_config: RefCell<sync::Config>,
    /// Discovery and serving, while the Sync tab is open.
    sync: RefCell<Option<sync::Session>>,
    /// Bumped whenever the Sync tab opens or closes; results from an older
    /// visit are dropped.
    sync_gen: Cell<u64>,
    sync_peers: Rc<VecModel<SyncPeer>>,
    /// The computer the confirmation dialog asks about, and its name.
    sync_pending: RefCell<Option<(SocketAddr, String)>>,
    /// A pull is replacing the database.
    pulling: Cell<bool>,
    /// Zoom in percent, saved as the local setting `zoom`.
    zoom: Cell<i64>,
    /// The scale factor last dispatched for zoom, to tell our own changes
    /// from the window system resetting it (say, on a monitor change).
    applied_scale: Cell<Option<f32>>,
    /// The article search vocabulary, folded for typo tolerance (see `load_terms`).
    terms: RefCell<Option<search::Terms>>,
    /// The `data_version` the vocabulary was read at, or `None` when it is stale.
    terms_stamp: Cell<Option<i64>>,
    terms_loading: Cell<bool>,
    /// This connection wrote indexed text since the running read started.
    terms_dirty: Cell<bool>,
    /// Runs the search once typing pauses.
    search_timer: slint::Timer,
}

impl App {
    /// Creates the app state for `ui`, binds its models to the window and
    /// makes it reachable through `with_app`.
    pub fn install(ui: &AppWindow, store: Store, paths: Paths) -> Result<Rc<App>> {
        let articles = Rc::new(VecModel::default());
        let feeds = Rc::new(VecModel::default());
        let reader_blocks = Rc::new(VecModel::default());
        let sync_peers = Rc::new(VecModel::default());
        ui.set_sync_peers(ModelRc::from(sync_peers.clone()));
        ui.set_articles(ModelRc::from(articles.clone()));
        ui.set_feeds(ModelRc::from(feeds.clone()));
        ui.set_reader_blocks(ModelRc::from(reader_blocks.clone()));
        let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
        ui.set_db_path(display_path(&paths.db, home.as_deref()).into());
        ui.set_thumbs_path(display_path(&paths.thumbs, home.as_deref()).into());

        let last_refresh = store.setting("last_refresh")?.and_then(|v| v.parse().ok());
        let refresh_minutes = store
            .setting("refresh_interval")?
            .and_then(|v| v.parse().ok())
            .map_or(DEFAULT_REFRESH_MINUTES, clamp_refresh_minutes);
        ui.set_refresh_minutes(refresh_minutes as i32);
        let zoom = store
            .setting("zoom")?
            .and_then(|v| v.parse().ok())
            .map_or(DEFAULT_ZOOM, clamp_zoom);
        ui.set_zoom_percent(zoom as i32);
        // English unless another language was picked.
        let lang = store
            .setting("language")?
            .and_then(|code| Lang::from_code(&code))
            .unwrap_or_default();
        select_language(lang);
        ui.set_language(lang.code().into());
        ui.set_today(i18n::long_date(chrono::Local::now()).into());
        let app = Rc::new(App {
            ui: ui.as_weak(),
            store,
            core: Core::new(paths),
            articles,
            article_ts: RefCell::default(),
            feeds,
            thumb_cache: RefCell::default(),
            thumb_pending: RefCell::default(),
            thumb_failed: RefCell::default(),
            refreshing: Cell::new(false),
            background: Cell::new(false),
            auto_refresh: slint::Timer::default(),
            refresh_minutes: Cell::new(refresh_minutes),
            last_refresh: Cell::new(last_refresh),
            offline: Cell::new(false),
            reader_blocks,
            reader_id: Cell::new(None),
            reader_link: RefCell::default(),
            reader_lead: RefCell::default(),
            reader_len: Cell::new(0),
            reader_list: RefCell::default(),
            reader_pos: Cell::new(0),
            pages_pending: RefCell::default(),
            prefetch_id: Cell::new(None),
            prefetch_pictures: RefCell::default(),
            prefetch_pending: RefCell::default(),
            sync_config: RefCell::default(),
            sync: RefCell::default(),
            sync_gen: Cell::new(0),
            sync_peers,
            sync_pending: RefCell::default(),
            pulling: Cell::new(false),
            zoom: Cell::new(zoom),
            applied_scale: Cell::new(None),
            terms: RefCell::default(),
            terms_stamp: Cell::new(None),
            terms_loading: Cell::new(false),
            terms_dirty: Cell::new(false),
            search_timer: slint::Timer::default(),
        });
        APP.with(|cell| {
            let _ = cell.set(app.clone());
        });
        app.auto_refresh
            .start(slint::TimerMode::Repeated, minutes(refresh_minutes), || {
                with_app(|a| a.auto_refresh())
            });
        Ok(app)
    }

    fn ui(&self) -> AppWindow {
        self.ui
            .upgrade()
            .expect("UI is alive while the event loop runs")
    }

    fn selected_feed(&self) -> Option<i64> {
        let id = self.ui().get_selected_feed();
        (id >= 0).then_some(id as i64)
    }

    fn toast(&self, msg: impl Into<SharedString>) {
        self.ui().invoke_show_toast(msg.into());
    }

    /// `what` is already translated; the error itself stays as it came.
    fn report(&self, what: String, err: anyhow::Error) {
        eprintln!("aggrega: {what}: {err:#}");
        self.toast(format!("{what}: {err}"));
    }

    // ---- view state ------------------------------------------------------

    pub fn reload_all(&self) {
        self.reload_feeds();
        self.reload_articles();
    }

    pub fn reload_feeds(&self) {
        let feeds = match self.store.feeds() {
            Ok(f) => f,
            Err(e) => return self.report(tr!("Couldn't load sources"), e),
        };
        let ui = self.ui();
        let mut selected = ui.get_selected_feed();
        if selected >= 0 && !feeds.iter().any(|f| f.id as i32 == selected) {
            selected = -1;
            ui.set_selected_feed(-1);
        }
        let title = feeds
            .iter()
            .find(|f| f.id as i32 == selected)
            .map(|f| f.title.clone())
            .unwrap_or_else(|| tr!("All articles"));
        ui.set_view_title(title.into());
        ui.set_total_unread(feeds.iter().map(|f| f.unread).sum::<i64>() as i32);
        ui.set_has_feeds(!feeds.is_empty());

        let items: Vec<FeedItem> = feeds
            .iter()
            .map(|f| FeedItem {
                id: f.id as i32,
                title: f.title.as_str().into(),
                unread: f.unread as i32,
                letter: text::initial(&f.title).into(),
                tint: tint(&f.title),
                error: f.has_error,
            })
            .collect();
        sync_model(&self.feeds, items);
        self.update_status();
    }

    pub fn reload_articles(&self) {
        let ui = self.ui();
        let filter = ArticleFilter {
            feed: self.selected_feed(),
            unread_only: ui.get_unread_only(),
        };
        let text = ui.get_search_text();
        let searching = !text.trim().is_empty();
        ui.set_searching(searching);
        let rows = if searching {
            self.search_articles(filter, &text)
        } else {
            self.store.articles(filter, MAX_ARTICLES)
        };
        let rows = match rows {
            Ok(r) => r,
            Err(e) => return self.report(tr!("Couldn't load articles"), e),
        };
        let now = now();
        let mut wanted = Vec::new();
        let items: Vec<ArticleItem> = {
            let cache = self.thumb_cache.borrow();
            let pending = self.thumb_pending.borrow();
            let failed = self.thumb_failed.borrow();
            rows.iter()
                .map(|a| {
                    let image_url: SharedString = a.image_url.as_deref().unwrap_or("").into();
                    let thumb = cache.get(&image_url).cloned();
                    let usable = !image_url.is_empty() && !failed.contains(&image_url);
                    if usable && thumb.is_none() && !pending.contains(&image_url) {
                        wanted.push(image_url.clone());
                    }
                    ArticleItem {
                        id: a.id as i32,
                        title: a.title.as_str().into(),
                        source: a.feed_title.as_str().into(),
                        time: text::ago(a.published, now).into(),
                        snippet: a.snippet.as_str().into(),
                        link: a.link.as_str().into(),
                        read: a.read,
                        has_thumb: usable,
                        thumb: thumb.unwrap_or_default(),
                        image_url,
                        letter: text::initial(&a.feed_title).into(),
                        tint: tint(&a.feed_title),
                        leaving: false,
                    }
                })
                .collect()
        };

        // Drop decoded thumbnails that are no longer on screen to keep memory low.
        {
            let visible: HashSet<&SharedString> = items.iter().map(|i| &i.image_url).collect();
            self.thumb_cache
                .borrow_mut()
                .retain(|k, _| visible.contains(k));
        }
        *self.article_ts.borrow_mut() = rows.iter().map(|a| a.published).collect();
        self.articles.set_vec(items);
        self.update_status();
        wanted.dedup();
        self.load_thumbs(wanted);
    }

    /// Articles matching `text` in the view `filter` describes. Reads the
    /// vocabulary again first if it is stale; this search uses what is cached.
    fn search_articles(&self, filter: ArticleFilter, text: &str) -> Result<Vec<Article>> {
        if self
            .terms_stamp
            .get()
            .is_none_or(|v| self.store.data_version().ok() != Some(v))
        {
            self.load_terms();
        }
        match search::compile(text, self.terms.borrow().as_ref()) {
            Some(query) => self.store.search(filter, &query, MAX_ARTICLES),
            None => Ok(Vec::new()),
        }
    }

    /// Reads the search vocabulary on a worker. Does nothing while a read is
    /// running; `terms_ready` stores the result.
    pub fn load_terms(&self) {
        if self.terms_loading.get() {
            return;
        }
        let Ok(version) = self.store.data_version() else {
            return;
        };
        self.terms_loading.set(true);
        self.terms_dirty.set(false);
        let db_path = self.core.paths.db.clone();
        std::thread::spawn(move || {
            let terms = Store::open(&db_path)
                .and_then(|s| s.search_terms())
                .map(search::Terms::new)
                .map_err(|e| format!("{e:#}"));
            let _ = slint::invoke_from_event_loop(move || {
                with_app(|app| app.terms_ready(version, terms))
            });
        });
    }

    fn terms_ready(&self, version: i64, terms: Result<search::Terms, String>) {
        self.terms_loading.set(false);
        let terms = match terms {
            Ok(terms) => terms,
            Err(e) => {
                eprintln!("aggrega: couldn't read the search vocabulary: {e}");
                return;
            }
        };
        *self.terms.borrow_mut() = Some(terms);
        // Indexed text written here while the read ran may be missing from it.
        self.terms_stamp
            .set((!self.terms_dirty.get()).then_some(version));
        if self.ui().get_searching() {
            self.reload_articles();
        }
    }

    /// Writes on this connection don't change `data_version`, so the cached
    /// vocabulary is marked stale by hand. The next search reads it again.
    fn terms_changed(&self) {
        self.terms_stamp.set(None);
        self.terms_dirty.set(true);
    }

    /// The search field was edited. Clearing it shows the list at once;
    /// typing waits for a short pause first.
    pub fn search_changed(&self) {
        if self.ui().get_search_text().trim().is_empty() {
            self.search_timer.stop();
            self.reload_articles();
        } else {
            self.search_timer
                .start(slint::TimerMode::SingleShot, SEARCH_DEBOUNCE, || {
                    with_app(|a| a.reload_articles())
                });
        }
    }

    fn update_status(&self) {
        let ui = self.ui();
        let status = if ui.get_searching() {
            let n = self.articles.iter().filter(|a| !a.leaving).count();
            trn!(n, "{n} match", "{n} matches")
        } else if self.refreshing.get() {
            tr!("Refreshing your sources…")
        } else {
            let unread = if ui.get_selected_feed() < 0 {
                ui.get_total_unread()
            } else {
                let sel = ui.get_selected_feed();
                self.feeds
                    .iter()
                    .find(|f| f.id == sel)
                    .map_or(0, |f| f.unread)
            };
            let unread = match unread {
                0 => tr!("All caught up"),
                n => tr!("{} unread", n),
            };
            match self.last_refresh.get() {
                _ if self.offline.get() => tr!("{}  ·  offline, showing saved stories", unread),
                Some(t) => tr!("{}  ·  updated {}", unread, text::ago(t, now())),
                None => unread,
            }
        };
        ui.set_status_text(status.into());
    }

    /// Re-renders relative time labels; runs once a minute.
    pub fn tick(&self) {
        let now = now();
        let ts = self.article_ts.borrow();
        for (i, t) in ts.iter().enumerate() {
            if let Some(mut row) = self.articles.row_data(i) {
                let label: SharedString = text::ago(*t, now).into();
                if row.time != label {
                    row.time = label;
                    self.articles.set_row_data(i, row);
                }
            }
        }
        self.update_status();
    }

    // ---- thumbnails --------------------------------------------------------

    fn load_thumbs(&self, urls: Vec<SharedString>) {
        if urls.is_empty() {
            return;
        }
        self.thumb_pending.borrow_mut().extend(urls.iter().cloned());
        let agent = self.core.agent().clone();
        let dir = self.core.paths.thumbs.clone();
        let urls: Vec<String> = urls.into_iter().map(Into::into).collect();
        std::thread::spawn(move || {
            pool::par_for_each(&urls, 6, |url| {
                let thumb = match thumbs::load(&agent, &dir, url) {
                    thumbs::Thumb::Ready(img) => ThumbResult::Ready(rgb_buffer(&img)),
                    thumbs::Thumb::Broken => ThumbResult::Broken,
                    thumbs::Thumb::Unavailable => ThumbResult::Unavailable,
                };
                let url = SharedString::from(url.as_str());
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(|app| app.thumb_ready(url, thumb))
                });
            });
        });
    }

    fn thumb_ready(&self, url: SharedString, thumb: ThumbResult) {
        self.thumb_pending.borrow_mut().remove(&url);
        let image = match thumb {
            ThumbResult::Ready(pixels) => {
                let img = slint::Image::from_rgb8(pixels);
                self.thumb_cache
                    .borrow_mut()
                    .insert(url.clone(), img.clone());
                Some(img)
            }
            ThumbResult::Broken => {
                self.thumb_failed.borrow_mut().insert(url.clone());
                None
            }
            // Offline: leave the placeholder; the next reload asks again.
            ThumbResult::Unavailable => return,
        };
        for i in 0..self.articles.row_count() {
            let Some(mut row) = self.articles.row_data(i) else {
                continue;
            };
            if row.image_url != url {
                continue;
            }
            match &image {
                Some(img) => row.thumb = img.clone(),
                None => row.has_thumb = false,
            }
            self.articles.set_row_data(i, row);
        }
    }

    // ---- actions -----------------------------------------------------------

    pub fn refresh(&self) {
        self.start_refresh(false);
    }

    /// Runs every `refresh_minutes`. Skipped while offline: only a refresh
    /// the user asks for tries the network again.
    fn auto_refresh(&self) {
        if !self.offline.get() {
            self.start_refresh(true);
        }
    }

    fn start_refresh(&self, background: bool) {
        if self.refreshing.get() || self.pulling.get() {
            return;
        }
        let jobs = match self.store.fetch_jobs() {
            Ok(j) if !j.is_empty() => j,
            Ok(_) => return,
            Err(e) => return self.report(tr!("Couldn't start refresh"), e),
        };
        self.background.set(background);
        self.set_refreshing(true);
        let core = self.core.clone();
        std::thread::spawn(move || {
            let summary = core.refresh(&jobs);
            let _ =
                slint::invoke_from_event_loop(move || with_app(|app| app.refresh_done(summary)));
        });
    }

    fn set_refreshing(&self, on: bool) {
        self.refreshing.set(on);
        self.ui().set_refreshing(on);
        self.update_status();
    }

    fn refresh_done(&self, summary: Result<RefreshSummary>) {
        let offline = matches!(&summary, Ok(s) if s.offline());
        self.offline.set(offline);
        // Saved stories stay available offline; just don't pretend we updated.
        if !offline {
            let t = now();
            self.last_refresh.set(Some(t));
            let _ = self.store.set_setting("last_refresh", &t.to_string());
        }
        self.set_refreshing(false);
        // The next automatic refresh is a full interval after this one.
        self.auto_refresh.restart();
        self.reload_all();
        self.load_terms();
        match summary {
            // In the background, only news is worth a toast; the status line
            // already tells when it last updated, or that it's offline.
            Ok(s) if self.background.get() => {
                if s.new_articles > 0 {
                    self.toast(new_articles(s.new_articles));
                }
            }
            Err(_) if self.background.get() => {}
            Ok(_) if offline => {
                self.toast(tr!("You're offline  ·  showing your saved stories"));
            }
            Ok(s) => {
                let mut msg = match s.new_articles {
                    0 => tr!("You're up to date"),
                    n => new_articles(n),
                };
                let problems = s.failed + s.unreachable;
                if problems > 0 {
                    msg += "  ·  ";
                    msg += &trn!(problems, "{n} source failed", "{n} sources failed");
                }
                self.toast(msg);
            }
            Err(e) => self.toast(tr!("Refresh failed: {}", format!("{e:#}"))),
        }
    }

    pub fn open_article(&self, row: usize) {
        let Some(item) = self.articles.row_data(row) else {
            return;
        };
        // Rows already animating out of the Unread list don't count.
        let list: Vec<i64> = self
            .articles
            .iter()
            .filter(|r| !r.leaving || r.id == item.id)
            .map(|r| r.id as i64)
            .collect();
        let pos = list
            .iter()
            .position(|&id| id == item.id as i64)
            .unwrap_or(0);
        *self.reader_list.borrow_mut() = list;
        self.reader_pos.set(pos);
        if self.show_reader(item.id as i64) {
            self.opened(item.id as i64);
        }
    }

    /// Opens the story `delta` places away in the list the reader was opened
    /// from, skipping any that have been deleted since.
    pub fn reader_step(&self, delta: i32) {
        if self.reader_id.get().is_none() {
            return;
        }
        let len = self.reader_list.borrow().len();
        let mut pos = self.reader_pos.get();
        loop {
            pos = match pos.checked_add_signed(delta as isize) {
                Some(p) if p < len => p,
                _ => return,
            };
            let id = self.reader_list.borrow()[pos];
            if self.show_reader(id) {
                self.reader_pos.set(pos);
                self.opened(id);
                return;
            }
        }
    }

    /// Marks the story just shown in the reader as read, then gets the next
    /// one ready.
    fn opened(&self, id: i64) {
        let row = (0..self.articles.row_count())
            .find(|&i| self.articles.row_data(i).is_some_and(|r| r.id as i64 == id));
        match row {
            Some(row) => {
                if self.articles.row_data(row).is_some_and(|r| !r.read) {
                    self.set_read(row, true);
                }
            }
            // Not in the list any more (say, a refresh reloaded it).
            None => match self.store.set_read(id, true) {
                Ok(()) => self.reload_feeds(),
                Err(e) => self.report(tr!("Couldn't save read state"), e),
            },
        }
        let pos = self.reader_pos.get();
        let list = self.reader_list.borrow();
        let ui = self.ui();
        ui.set_reader_has_prev(pos > 0);
        ui.set_reader_has_next(pos + 1 < list.len());
        let next = list.get(pos + 1).copied();
        drop(list);
        self.prefetch(next);
    }

    // ---- reader view -------------------------------------------------------

    /// Opens the reader with the feed's copy of the article, then fetches the
    /// full page in the background if the feed only had a summary. False if
    /// the article is gone.
    fn show_reader(&self, id: i64) -> bool {
        let article = match self.store.reader_article(id) {
            Ok(Some(a)) => a,
            Ok(None) => return false,
            Err(e) => {
                self.report(tr!("Couldn't open the article"), e);
                return false;
            }
        };
        let ui = self.ui();
        let ops::ReaderContent {
            blocks,
            lead,
            needs_page: fetch_page,
        } = ops::reader_content(&article);

        self.reader_id.set(Some(id));
        *self.reader_link.borrow_mut() = article.link.clone();
        *self.reader_lead.borrow_mut() = lead.clone();
        ui.set_reader(ReaderInfo {
            id: id as i32,
            title: article.title.as_str().into(),
            source: article.feed_title.as_str().into(),
            time: text::ago(article.published, now()).into(),
            host: host_of(&article.link).into(),
            link: article.link.as_str().into(),
            tint: tint(&article.feed_title),
            read: true,
            lead: Default::default(),
        });
        ui.set_reader_loading(fetch_page);
        ui.set_reader_note(
            if !fetch_page && blocks.is_empty() {
                tr!("This story has no text in its feed. Open the original to read it.")
            } else {
                String::new()
            }
            .into(),
        );
        self.set_reader_blocks(&blocks);
        self.load_pictures(id, lead.into_iter().collect());
        ui.invoke_show_reader();

        if fetch_page {
            self.fetch_page(id, article.link);
        }
        true
    }

    /// Downloads the article's page on a worker, unless that's already
    /// happening; `page_ready` gets the result.
    fn fetch_page(&self, id: i64, link: String) {
        if !self.pages_pending.borrow_mut().insert(id) {
            return;
        }
        let core = self.core.clone();
        std::thread::spawn(move || {
            let result = core.full_text(&link);
            let _ =
                slint::invoke_from_event_loop(move || with_app(|app| app.page_ready(id, result)));
        });
    }

    /// Loads the story after the open one ahead of time: its full page, if the
    /// feed only had a summary, and its pictures, so it opens instantly.
    fn prefetch(&self, id: Option<i64>) {
        if self.prefetch_id.get() == id {
            return;
        }
        self.prefetch_id.set(id);
        self.prefetch_pictures.borrow_mut().clear();
        self.prefetch_pending.borrow_mut().clear();
        let Some(id) = id else {
            return;
        };
        let Ok(Some(article)) = self.store.reader_article(id) else {
            return;
        };
        let content = ops::reader_content(&article);
        if content.needs_page {
            // Its pictures are fetched once the page is in (see `page_ready`).
            self.fetch_page(id, article.link);
            self.load_pictures(id, content.lead.into_iter().collect());
        } else {
            self.load_pictures(
                id,
                content
                    .lead
                    .into_iter()
                    .chain(image_urls(&content.blocks))
                    .collect(),
            );
        }
    }

    /// Shows `blocks` in the reader and starts loading their images.
    fn set_reader_blocks(&self, blocks: &[Block]) {
        self.reader_len.set(reader::text_len(blocks));
        let mut images = Vec::new();
        let rows: Vec<ReaderBlock> = blocks
            .iter()
            .map(|b| {
                let kind = match b.kind {
                    Kind::Paragraph => BlockKind::Paragraph,
                    Kind::Heading => BlockKind::Heading,
                    Kind::Quote => BlockKind::Quote,
                    Kind::Bullet => BlockKind::Bullet,
                    Kind::Code => BlockKind::Code,
                    Kind::Image => {
                        images.push(b.text.clone());
                        return ReaderBlock {
                            kind: BlockKind::Image,
                            image_url: b.text.as_str().into(),
                            ..Default::default()
                        };
                    }
                };
                ReaderBlock {
                    kind,
                    text: b.text.as_str().into(),
                    ..Default::default()
                }
            })
            .collect();
        self.reader_blocks.set_vec(rows);
        if let Some(id) = self.reader_id.get() {
            self.load_pictures(id, images);
        }
    }

    fn page_ready(&self, id: i64, result: Result<Vec<Block>>) {
        self.pages_pending.borrow_mut().remove(&id);
        let current = self.reader_id.get() == Some(id);
        let shown = if current {
            self.reader_len.get()
        } else {
            self.stored_len(id)
        };
        let note = match result {
            Ok(page) if reader::text_len(&page) > shown => {
                // Keep it, so the article opens instantly (and offline) next time.
                self.terms_changed();
                if let Err(e) = self.store.set_body(id, &reader::encode(&page)) {
                    eprintln!("aggrega: couldn't save article text: {e:#}");
                }
                if current {
                    let title = self.ui().get_reader().title;
                    let lead = self.reader_lead.borrow().clone();
                    self.set_reader_blocks(&reader::tidy(page, &title, lead.as_deref()));
                } else if self.prefetch_id.get() == Some(id) {
                    self.load_pictures(id, image_urls(&page).collect());
                }
                String::new()
            }
            Ok(_) if shown == 0 => {
                tr!("Couldn't find the story on its page. Open the original to read it.")
            }
            Ok(_) => String::new(),
            Err(e) if feed::is_unreachable(&e) => {
                tr!("You're offline, so this is the summary from the feed.")
            }
            Err(_) => {
                tr!("The full story couldn't be loaded, so this is the summary from the feed.")
            }
        };
        if current {
            let ui = self.ui();
            ui.set_reader_loading(false);
            ui.set_reader_note(note.into());
        }
    }

    /// How much text the stored copy of an article has.
    fn stored_len(&self, id: i64) -> usize {
        match self.store.reader_article(id) {
            Ok(Some(a)) => reader::text_len(&reader::decode(a.body.as_deref().unwrap_or(""))),
            _ => 0,
        }
    }

    /// Downloads pictures for article `id`. Those already loaded or on their
    /// way for the prefetched story are taken from there instead.
    fn load_pictures(&self, id: i64, mut urls: Vec<String>) {
        if self.prefetch_id.get() == Some(id) {
            let ready: Vec<(String, Option<slint::Image>)> = {
                let cache = self.prefetch_pictures.borrow();
                urls.iter()
                    .filter_map(|u| cache.get(u).map(|img| (u.clone(), img.clone())))
                    .collect()
            };
            for (url, image) in ready {
                self.show_picture(&url, image);
            }
            let mut pending = self.prefetch_pending.borrow_mut();
            let cache = self.prefetch_pictures.borrow();
            urls.retain(|u| !cache.contains_key(u) && pending.insert(u.clone()));
        }
        if urls.is_empty() {
            return;
        }
        let agent = self.core.agent().clone();
        std::thread::spawn(move || {
            pool::par_for_each(&urls, 4, |url| {
                let picture = thumbs::load_picture(&agent, url).map(|img| rgba_buffer(&img));
                let url = url.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(|app| app.picture_ready(id, &url, picture))
                });
            });
        });
    }

    fn picture_ready(&self, id: i64, url: &str, picture: Option<SharedPixelBuffer<Rgba8Pixel>>) {
        let image = picture.map(slint::Image::from_rgba8);
        if self.prefetch_id.get() == Some(id) {
            self.prefetch_pending.borrow_mut().remove(url);
            self.prefetch_pictures
                .borrow_mut()
                .insert(url.to_owned(), image.clone());
        }
        if self.reader_id.get() == Some(id) {
            self.show_picture(url, image);
        }
    }

    /// Puts a downloaded picture in the open article (or drops its block).
    fn show_picture(&self, url: &str, image: Option<slint::Image>) {
        if self.reader_lead.borrow().as_deref() == Some(url) {
            let ui = self.ui();
            let mut info = ui.get_reader();
            info.lead = image.clone().unwrap_or_default();
            ui.set_reader(info);
        }
        for i in (0..self.reader_blocks.row_count()).rev() {
            let Some(mut row) = self.reader_blocks.row_data(i) else {
                continue;
            };
            if row.kind != BlockKind::Image || row.image_url != url {
                continue;
            }
            match &image {
                Some(img) => {
                    row.image = img.clone();
                    self.reader_blocks.set_row_data(i, row);
                }
                // Broken images just disappear.
                None => {
                    self.reader_blocks.remove(i);
                }
            }
        }
    }

    pub fn close_reader(&self) {
        self.reader_id.set(None);
        self.reader_list.borrow_mut().clear();
        self.prefetch(None);
        // Free the pictures; the list underneath is still as the user left it.
        self.reader_blocks.set_vec(Vec::new());
        let ui = self.ui();
        let mut info = ui.get_reader();
        info.lead = Default::default();
        ui.set_reader(info);
    }

    pub fn open_original(&self) {
        let link = self.reader_link.borrow().clone();
        if link.is_empty() {
            return;
        }
        if let Err(e) = open::that_detached(&link) {
            self.toast(tr!("Couldn't open the browser: {}", e));
        }
    }

    /// Opens a link from the About tab in the browser.
    pub fn open_link(&self, url: SharedString) {
        if let Err(e) = open::that_detached(url.as_str()) {
            self.toast(tr!("Couldn't open the browser: {}", e));
        }
    }

    /// Shows the database folder, or the thumbnail cache, in the file manager.
    pub fn open_data_folder(&self, thumbs: bool) {
        let dir = if thumbs {
            self.core.paths.thumbs.as_path()
        } else {
            self.core.paths.db.parent().unwrap_or(&self.core.paths.db)
        };
        if let Err(e) = open::that_detached(dir) {
            self.toast(tr!("Couldn't open the folder: {}", e));
        }
    }

    pub fn reader_toggle_read(&self) {
        let Some(id) = self.reader_id.get() else {
            return;
        };
        let ui = self.ui();
        let mut info = ui.get_reader();
        let read = !info.read;
        let row = (0..self.articles.row_count())
            .find(|&i| self.articles.row_data(i).is_some_and(|r| r.id as i64 == id));
        match row {
            // Also updates the list (and animates the row out in the Unread view).
            Some(row) => self.set_read(row, read),
            None => {
                if let Err(e) = self.store.set_read(id, read) {
                    return self.report(tr!("Couldn't save read state"), e);
                }
                // Marked unread again after it left the Unread list: bring it back.
                self.reload_all();
            }
        }
        info.read = read;
        ui.set_reader(info);
    }

    pub fn toggle_read(&self, row: usize) {
        if let Some(item) = self.articles.row_data(row) {
            self.set_read(row, !item.read);
        }
    }

    fn set_read(&self, row: usize, read: bool) {
        let Some(mut item) = self.articles.row_data(row) else {
            return;
        };
        if let Err(e) = self.store.set_read(item.id as i64, read) {
            return self.report(tr!("Couldn't save read state"), e);
        }
        item.read = read;
        // In the Unread view a freshly read story animates out of the list.
        let leaving = read && self.ui().get_unread_only();
        item.leaving = leaving;
        let id = item.id;
        self.articles.set_row_data(row, item);
        self.reload_feeds();
        if leaving {
            slint::Timer::single_shot(LEAVE_ANIMATION, move || {
                with_app(|a| a.remove_leaving(Some(id)))
            });
        }
    }

    /// Drops rows whose leave animation has finished: the given article, or
    /// every leaving row when `id` is `None`.
    fn remove_leaving(&self, id: Option<i32>) {
        let mut ts = self.article_ts.borrow_mut();
        for i in (0..self.articles.row_count()).rev() {
            let Some(row) = self.articles.row_data(i) else {
                continue;
            };
            if row.leaving && id.is_none_or(|id| row.id == id) {
                self.articles.remove(i);
                if i < ts.len() {
                    ts.remove(i);
                }
            }
        }
        drop(ts);
        self.update_status();
    }

    pub fn mark_all_read(&self) {
        match self.store.mark_all_read(self.selected_feed()) {
            Ok(0) => self.toast(tr!("Nothing left to read here")),
            Ok(n) => {
                let leaving = self.ui().get_unread_only();
                for i in 0..self.articles.row_count() {
                    if let Some(mut row) = self.articles.row_data(i).filter(|r| !r.read) {
                        row.read = true;
                        row.leaving = leaving;
                        self.articles.set_row_data(i, row);
                    }
                }
                self.reload_feeds();
                if leaving {
                    slint::Timer::single_shot(LEAVE_ANIMATION, || {
                        with_app(|a| a.remove_leaving(None))
                    });
                }
                self.toast(trn!(
                    n,
                    "Marked {n} article as read",
                    "Marked {n} articles as read"
                ));
            }
            Err(e) => self.report(tr!("Couldn't mark as read"), e),
        }
    }

    pub fn add_feed(&self, input: SharedString) {
        let ui = self.ui();
        if ui.get_adding() {
            return;
        }
        ui.set_adding(true);
        ui.set_add_error("".into());
        let core = self.core.clone();
        std::thread::spawn(move || {
            let result = core.add_source(&input);
            let _ = slint::invoke_from_event_loop(move || with_app(|app| app.add_done(result)));
        });
    }

    fn add_done(&self, result: Result<AddedSource>) {
        let ui = self.ui();
        ui.set_adding(false);
        match result {
            Ok(added) => {
                let n = added.articles;
                ui.invoke_close_add_dialog();
                ui.set_selected_feed(added.id as i32);
                self.reload_all();
                self.toast(trn!(
                    n,
                    "Added {}  ·  {n} article",
                    "Added {}  ·  {n} articles",
                    added.title
                ));
            }
            Err(e) => {
                let mut e = format!("{e:#}");
                if let Some(first) = e.get_mut(0..1) {
                    first.make_ascii_uppercase();
                }
                ui.set_add_error(e.into());
            }
        }
    }

    pub fn remove_feed(&self, id: i32) {
        let title = self
            .feeds
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.title.to_string())
            .unwrap_or_default();
        self.terms_changed();
        if let Err(e) = self.store.remove_feed(id as i64) {
            return self.report(tr!("Couldn't remove source"), e);
        }
        self.reload_all();
        self.toast(tr!("Removed {}", title));
    }

    // ---- OPML import / export ----------------------------------------------

    /// Asks for an OPML file and subscribes to the sources in it.
    pub fn import_sources(&self) {
        if self.ui().get_importing() || self.pulling.get() {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .set_title(tr!("Import sources"))
            .add_filter("OPML", &["opml", "xml"])
            .pick_file()
        else {
            return;
        };
        self.import_file(&path);
    }

    /// Subscribes, in the background, to every source in the OPML file at
    /// `path` that isn't followed yet.
    fn import_file(&self, path: &Path) {
        let text = match std::fs::read(path)
            .with_context(|| format!("couldn't read {}", path.display()))
        {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => return self.report(tr!("Couldn't import"), e),
        };
        let plan = match ops::plan_import(&self.store, &text) {
            Ok(p) => p,
            Err(e) => return self.report(tr!("Couldn't import"), e),
        };
        if plan.fresh.is_empty() {
            return self.toast(match plan.total {
                0 => tr!("No sources found in that file"),
                n => trn!(
                    n,
                    "You already follow {n} source",
                    "You already follow {n} sources"
                ),
            });
        }

        self.ui().set_importing(true);
        self.toast(trn!(
            plan.fresh.len(),
            "Importing {n} source…",
            "Importing {n} sources…"
        ));
        let core = self.core.clone();
        std::thread::spawn(move || {
            let summary = core.import(plan);
            let _ = slint::invoke_from_event_loop(move || with_app(|app| app.import_done(summary)));
        });
    }

    fn import_done(&self, summary: Result<ImportSummary>) {
        self.ui().set_importing(false);
        self.reload_all();
        match summary {
            Ok(s) => self.toast(import_message(&s)),
            Err(e) => self.toast(tr!("Import failed: {}", format!("{e:#}"))),
        }
    }

    /// Asks where to save, then writes every source there as OPML.
    pub fn export_sources(&self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title(tr!("Export sources"))
            .set_file_name("aggrega-sources.opml")
            .add_filter("OPML", &["opml"])
            .save_file()
        else {
            return;
        };
        match self.export_to(&path) {
            Ok(n) => {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                self.toast(trn!(
                    n,
                    "Exported {n} source to {}",
                    "Exported {n} sources to {}",
                    name
                ));
            }
            Err(e) => self.report(tr!("Couldn't export"), e),
        }
    }

    /// Writes every source to `path` as OPML; returns how many.
    fn export_to(&self, path: &Path) -> Result<usize> {
        let (doc, count) = ops::export_opml(&self.store)?;
        std::fs::write(path, doc).with_context(|| format!("couldn't write {}", path.display()))?;
        Ok(count)
    }

    // ---- sync ----------------------------------------------------------------

    /// Starts discovery and serving when the Sync tab comes on screen, and
    /// stops both when it goes away (other tab, or Settings closed).
    pub fn sync_active(&self, on: bool) {
        self.sync_gen.set(self.sync_gen.get() + 1);
        // Dropping the old session stops its threads.
        self.sync.replace(None);
        self.sync_pending.replace(None);
        self.sync_peers.set_vec(Vec::new());
        let ui = self.ui();
        if !self.pulling.get() {
            ui.set_sync_busy(false);
            self.sync_note("", false);
        }
        if !on {
            return;
        }

        let name = sync::device_name();
        let gen_ = self.sync_gen.get();
        let session = sync::Session::start(
            &self.sync_config.borrow(),
            self.core.paths.db.clone(),
            name.clone(),
            move |peers| {
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(|app| app.sync_peers_found(gen_, peers))
                });
            },
        );
        let device = match (sync::local_ip(), session.port) {
            (Some(ip), Some(port)) => {
                format!("{name}  ·  {}", sync::display_addr((ip, port).into()))
            }
            (None, _) => tr!("{}  ·  not connected to a network", name),
            (Some(ip), None) => format!("{name}  ·  {ip}"),
        };
        ui.set_sync_device(device.into());
        if !session.problems.is_empty() && !self.pulling.get() {
            self.sync_note(&session.problems.join(" "), true);
        }
        self.sync.replace(Some(session));
    }

    fn sync_peers_found(&self, gen_: u64, peers: Vec<sync::Peer>) {
        if gen_ != self.sync_gen.get() {
            return;
        }
        let items = peers
            .into_iter()
            .map(|p| SyncPeer {
                name: p.name.into(),
                address: sync::display_addr(p.addr).into(),
                target: p.addr.to_string().into(),
            })
            .collect();
        sync_model(&self.sync_peers, items);
    }

    fn sync_note(&self, note: &str, error: bool) {
        let ui = self.ui();
        ui.set_sync_note(note.into());
        ui.set_sync_note_error(error);
    }

    /// Asks the computer at `address` what it has, then lets the user
    /// confirm before anything is replaced.
    pub fn sync_connect(&self, address: SharedString) {
        let ui = self.ui();
        if ui.get_sync_busy() {
            return;
        }
        if self.refreshing.get() || ui.get_importing() {
            return self.sync_note(
                &tr!("Wait for Aggrega to finish updating, then try again."),
                true,
            );
        }
        ui.set_sync_busy(true);
        self.sync_note(&tr!("Connecting to {}…", address), false);
        let gen_ = self.sync_gen.get();
        std::thread::spawn(move || {
            let result = sync::resolve(&address)
                .and_then(|addr| Ok((addr, sync::info(addr)?)))
                .map_err(|e| format!("{e:#}"));
            let _ = slint::invoke_from_event_loop(move || {
                with_app(|app| app.sync_info_ready(gen_, result))
            });
        });
    }

    fn sync_info_ready(&self, gen_: u64, result: Result<(SocketAddr, sync::Remote), String>) {
        if gen_ != self.sync_gen.get() {
            return;
        }
        self.ui().set_sync_busy(false);
        let (addr, remote) = match result {
            Ok(r) => r,
            Err(e) => return self.sync_note(&capitalize(&e), true),
        };
        if remote.schema > SCHEMA_VERSION {
            return self.sync_note(
                &tr!(
                    "{} runs a newer version of Aggrega. Update this one first.",
                    remote.name
                ),
                true,
            );
        }
        let ours = match self.store.stats() {
            Ok(s) => s,
            Err(e) => return self.report(tr!("Couldn't read this computer's sources"), e),
        };
        self.sync_note("", false);
        let detail = tr!(
            "Its {} will replace the {} on this computer. This can't be undone.",
            describe(remote.stats, true),
            describe(ours, false),
        );
        self.sync_pending.replace(Some((addr, remote.name.clone())));
        self.ui().invoke_ask_sync(remote.name.into(), detail.into());
    }

    /// The user confirmed: replaces this computer's database with the one
    /// asked about.
    pub fn sync_confirmed(&self) {
        let Some((addr, name)) = self.sync_pending.take() else {
            return;
        };
        if self.refreshing.get() || self.ui().get_importing() || self.pulling.get() {
            return self.sync_note(
                &tr!("Wait for Aggrega to finish updating, then try again."),
                true,
            );
        }
        self.pulling.set(true);
        self.ui().set_sync_busy(true);
        self.sync_note(&tr!("Pulling everything from {}…", name), false);
        let db = self.core.paths.db.clone();
        std::thread::spawn(move || {
            let result = sync::pull(addr, &db).map_err(|e| format!("{e:#}"));
            let _ = slint::invoke_from_event_loop(move || {
                with_app(|app| app.sync_pulled(name, result))
            });
        });
    }

    fn sync_pulled(&self, name: String, result: Result<Stats, String>) {
        self.pulling.set(false);
        let ui = self.ui();
        ui.set_sync_busy(false);
        let stats = match result {
            Ok(s) => s,
            Err(e) => {
                let msg = tr!("Couldn't pull from {}: {}", name, e);
                self.sync_note(&msg, true);
                return self.toast(msg);
            }
        };
        // Everything on screen came from the old database.
        ui.invoke_hide_reader();
        ui.set_selected_feed(-1);
        self.thumb_failed.borrow_mut().clear();
        self.terms_changed();
        self.load_terms();
        self.offline.set(false);
        self.last_refresh.set(
            self.store
                .setting("last_refresh")
                .ok()
                .flatten()
                .and_then(|v| v.parse().ok()),
        );
        self.reload_all();
        let msg = tr!("Pulled {} from {}", describe(stats, false), name);
        self.sync_note(&msg, false);
        self.toast(msg);
    }

    /// Settings › Updates stepper: the next longer or shorter interval.
    pub fn step_refresh_interval(&self, longer: bool) {
        let current = self.refresh_minutes.get();
        let next = if longer {
            REFRESH_STEPS.iter().copied().find(|&m| m > current)
        } else {
            REFRESH_STEPS.iter().rev().copied().find(|&m| m < current)
        };
        let Some(next) = next else { return };
        self.refresh_minutes.set(next);
        self.ui().set_refresh_minutes(next as i32);
        // Restarts the countdown, so a new interval applies from now.
        self.auto_refresh.set_interval(minutes(next));
        if let Err(e) = self
            .store
            .set_setting("refresh_interval", &next.to_string())
        {
            self.report(tr!("Couldn't save the refresh interval"), e);
        }
    }

    /// Ctrl/Cmd + and -: the next zoom level in `step`'s direction, or the
    /// default for 0 (Ctrl/Cmd + 0).
    pub fn zoom(&self, step: i32) {
        let next = next_zoom(self.zoom.get(), step);
        if next != self.zoom.get() {
            self.set_zoom(next);
        }
    }

    /// Scales the whole window to `percent`. Zoom is the window's scale
    /// factor times this, so everything reflows like on a high-DPI screen.
    fn set_zoom(&self, percent: i64) {
        let ui = self.ui();
        let window = ui.window();
        let current = window.scale_factor();
        let base = match self.applied_scale.get() {
            // Still our last scale: undo the old zoom to find the system one.
            Some(applied) if (current - applied).abs() < 1e-3 => {
                current / (self.zoom.get() as f32 / 100.)
            }
            // Reset by the window system (say, moved to another monitor).
            _ => current,
        };
        let scale = base * percent as f32 / 100.;
        let physical = window.size();
        window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
        // The native window keeps its physical size, so its logical size
        // shrinks as the scale grows. Say so, or the backend takes the stale
        // logical size for a resize request and grows the window instead of
        // reflowing the content.
        window.dispatch_event(slint::platform::WindowEvent::Resized {
            size: physical.to_logical(scale),
        });
        self.applied_scale.set(Some(scale));
        self.zoom.set(percent);
        ui.set_zoom_percent(percent as i32);
        if let Err(e) = self.store.set_setting("zoom", &percent.to_string()) {
            self.report(tr!("Couldn't save the zoom level"), e);
        }
    }

    /// Applies the saved zoom at startup. Runs from the event loop right after
    /// launch (queued in `main`): the window system only reports the monitor's
    /// scale factor once the native window exists, and that would overwrite an
    /// earlier zoom.
    pub fn restore_zoom(&self) {
        let percent = self.zoom.get();
        if percent != DEFAULT_ZOOM {
            self.set_zoom(percent);
        }
    }

    /// The language switch: applies `code` at once, everywhere, and saves it.
    pub fn change_language(&self, code: SharedString) {
        let Some(lang) = Lang::from_code(&code) else {
            return;
        };
        select_language(lang);
        let ui = self.ui();
        ui.set_language(lang.code().into());
        ui.set_today(i18n::long_date(chrono::Local::now()).into());
        // Text built in Rust, re-rendered in place so the list doesn't move:
        // the view title, the status line and every row's date.
        self.reload_feeds();
        self.tick();
        if let Some(id) = self.reader_id.get()
            && let Ok(Some(article)) = self.store.reader_article(id)
        {
            let mut info = ui.get_reader();
            info.time = text::ago(article.published, now()).into();
            ui.set_reader(info);
        }
        if let Err(e) = self.store.set_setting("language", lang.code()) {
            self.report(tr!("Couldn't save the language"), e);
        }
    }

    pub fn save_theme(&self, dark: bool) {
        let _ = self
            .store
            .set_setting("theme", if dark { "dark" } else { "light" });
    }
}

fn import_message(s: &ImportSummary) -> String {
    if s.added == 0 && s.failed == 0 && s.unreachable > 0 {
        return tr!("You're offline  ·  nothing was imported");
    }
    let mut parts = vec![trn!(s.added, "Imported {n} source", "Imported {n} sources")];
    if s.skipped > 0 {
        parts.push(trn!(
            s.skipped,
            "{n} already followed",
            "{n} already followed"
        ));
    }
    let failed = s.failed + s.unreachable;
    if failed > 0 {
        parts.push(trn!(
            failed,
            "{n} couldn't be added",
            "{n} couldn't be added"
        ));
    }
    parts.join("  ·  ")
}

fn minutes(n: i64) -> Duration {
    Duration::from_secs(n as u64 * 60)
}

fn clamp_refresh_minutes(n: i64) -> i64 {
    n.clamp(REFRESH_STEPS[0], REFRESH_STEPS[REFRESH_STEPS.len() - 1])
}

fn clamp_zoom(n: i64) -> i64 {
    n.clamp(ZOOM_STEPS[0], ZOOM_STEPS[ZOOM_STEPS.len() - 1])
}

/// The zoom level one step in (`step` > 0) or out (`step` < 0) from
/// `current`, or back to the default for `step` == 0. Stays put at the ends.
fn next_zoom(current: i64, step: i32) -> i64 {
    match step.cmp(&0) {
        std::cmp::Ordering::Greater => ZOOM_STEPS
            .iter()
            .copied()
            .find(|&z| z > current)
            .unwrap_or(clamp_zoom(current)),
        std::cmp::Ordering::Less => ZOOM_STEPS
            .iter()
            .rev()
            .copied()
            .find(|&z| z < current)
            .unwrap_or(clamp_zoom(current)),
        std::cmp::Ordering::Equal => DEFAULT_ZOOM,
    }
}

fn new_articles(n: usize) -> String {
    trn!(n, "{n} new article", "{n} new articles")
}

/// "3 sources and 120 articles", optionally with "(12 unread)".
fn describe(s: Stats, unread: bool) -> String {
    let mut text = tr!(
        "{} and {}",
        trn!(s.feeds, "{n} source", "{n} sources"),
        trn!(s.articles, "{n} article", "{n} articles")
    );
    if unread && s.articles > 0 {
        text += " ";
        text += &trn!(s.unread, "({n} unread)", "({n} unread)");
    }
    text
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// The pictures among `blocks`.
fn image_urls(blocks: &[Block]) -> impl Iterator<Item = String> + '_ {
    blocks
        .iter()
        .filter(|b| b.kind == Kind::Image)
        .map(|b| b.text.clone())
}

/// The site name shown in the reader, e.g. "example.com".
fn host_of(link: &str) -> String {
    url::Url::parse(link)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("www.").to_string())
        })
        .unwrap_or_default()
}

/// Updates a model in place when the shape is unchanged (keeps scroll
/// position and avoids re-creating delegates), otherwise replaces it.
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, items: Vec<T>) {
    if model.row_count() != items.len() {
        model.set_vec(items);
        return;
    }
    for (i, item) in items.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&item) {
            model.set_row_data(i, item);
        }
    }
}

#[cfg(test)]
mod tests {
    //! Drives the real window headlessly (Slint's testing backend) against a
    //! temporary database. Each test runs on its own thread, so each gets its
    //! own backend and `APP`.

    use super::*;
    use crate::feed::{Fetched, NewArticle, Source};
    use crate::opml;
    use crate::setup;
    use i_slint_backend_testing::{self as testing, AccessibleRole, ElementHandle};
    use slint::platform::{Key, WindowEvent};

    const LONG: &str = "Reading is the whole point of an aggregator, and this paragraph is long enough to count as the full text of the story.";

    fn article(i: usize, body: &[Block]) -> NewArticle {
        NewArticle {
            guid: format!("g{i}"),
            title: format!("Story {i}"),
            // Nothing listens on port 9, so page fetches fail fast.
            link: format!("http://127.0.0.1:9/story/{i}"),
            snippet: "Snippet".into(),
            image_url: None,
            published: 1_700_000_000 + i as i64,
            body: reader::encode(body),
        }
    }

    fn full_text() -> Vec<Block> {
        let mut blocks = vec![Block::new(Kind::Heading, "Story 1")];
        blocks.extend((0..12).map(|_| Block::new(Kind::Paragraph, LONG)));
        blocks.push(Block::new(Kind::Quote, "A quote"));
        blocks.push(Block::new(Kind::Image, "http://127.0.0.1:9/pic.png"));
        blocks
    }

    /// A window and app over a fresh database holding one feed:
    /// "Story 1" (full text in the feed) and "Story 0" (summary only).
    fn start(name: &str) -> (AppWindow, Rc<App>, tempfile::TempDir) {
        testing::init_no_event_loop();
        let dir = tempfile::Builder::new()
            .prefix(&format!("aggrega-ui-{name}-"))
            .tempdir()
            .unwrap();
        let paths = Paths::in_dir(dir.path()).unwrap();
        let store = Store::open(&paths.db).unwrap();
        store
            .add_feed(
                "https://blog.example/rss",
                &Fetched {
                    title: "Example Blog".into(),
                    site_url: None,
                    etag: None,
                    last_modified: None,
                    articles: vec![
                        article(0, &[Block::new(Kind::Paragraph, "Just a teaser…")]),
                        article(1, &full_text()),
                    ],
                },
            )
            .unwrap();
        let ui = AppWindow::new().unwrap();
        let app = setup(&ui, store, paths).unwrap();
        // Sync never leaves this computer in tests.
        app.sync_config.replace(loopback());
        app.reload_all();
        // Headless windows start at 0×0, where every element counts as clipped away.
        ui.window().set_size(slint::LogicalSize::new(1240., 820.));
        // Let the opening animation finish so key presses aren't swallowed by it.
        testing::mock_elapsed_time(Duration::from_secs(3));
        (ui, app, dir)
    }

    fn loopback() -> sync::Config {
        sync::Config {
            beacon_port: 0,
            service: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            announce_to: std::net::Ipv4Addr::LOCALHOST.into(),
        }
    }

    fn resize(ui: &AppWindow, w: f32, h: f32) {
        ui.window().set_size(slint::LogicalSize::new(w, h));
        // The sidebar animates its width.
        testing::mock_elapsed_time(Duration::from_secs(1));
    }

    fn press(ui: &AppWindow, key: impl Into<SharedString>) {
        let text = key.into();
        ui.window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text });
    }

    fn row_of(app: &App, title: &str) -> usize {
        (0..app.articles.row_count())
            .find(|&i| app.articles.row_data(i).unwrap().title == title)
            .unwrap()
    }

    fn kinds(app: &App) -> Vec<BlockKind> {
        app.reader_blocks.iter().map(|b| b.kind).collect()
    }

    /// The read flag as stored, bypassing the list model.
    fn read_in_db(app: &App, id: i64) -> bool {
        app.store
            .articles(ArticleFilter::default(), 10)
            .unwrap()
            .into_iter()
            .find(|a| a.id == id)
            .expect("article is in the database")
            .read
    }

    #[test]
    fn layout_adapts_to_window_width() {
        let (ui, _app, _dir) = start("layout");

        resize(&ui, 1400., 900.);
        assert!(!ui.get_compact());
        assert_eq!(ui.get_sidebar_width(), 272.);
        assert_eq!(ui.get_column_width(), 980., "wide windows cap the column");
        assert_eq!(
            ui.get_reader_column_width(),
            720.,
            "reader keeps a readable measure"
        );

        resize(&ui, 1100., 800.);
        assert!(!ui.get_compact());
        // 1100 - 272 sidebar - 72 margins
        assert_eq!(ui.get_column_width(), 756.);

        resize(&ui, 800., 600.);
        assert!(ui.get_compact());
        assert_eq!(ui.get_sidebar_width(), 224.);
        // 800 - 224 sidebar - 40 (tighter margins below 640px)
        assert_eq!(ui.get_column_width(), 536.);
        assert_eq!(ui.get_reader_column_width(), 536.);
    }

    #[test]
    fn opening_an_article_shows_the_feed_text() {
        let (ui, app, _dir) = start("open");
        assert!(!ui.get_reader_open());
        let row = row_of(&app, "Story 1");
        app.open_article(row);

        assert!(ui.get_reader_open());
        let info = ui.get_reader();
        assert_eq!(info.title, "Story 1");
        assert_eq!(info.source, "Example Blog");
        assert_eq!(info.host, "127.0.0.1");
        assert!(info.read);
        // Full text in the feed: nothing to fetch.
        assert!(!ui.get_reader_loading());
        assert_eq!(ui.get_reader_note(), "");
        // The heading repeating the title is dropped.
        let k = kinds(&app);
        assert_eq!(k.len(), 14);
        assert_eq!(k[0], BlockKind::Paragraph);
        assert_eq!(k[12], BlockKind::Quote);
        assert_eq!(k[13], BlockKind::Image);
        assert_eq!(app.reader_blocks.row_data(0).unwrap().text, LONG);

        // Opening marks the story read, in the database and in the list.
        let id = info.id as i64;
        assert!(read_in_db(&app, id));
        assert!(app.articles.row_data(row).unwrap().read);
    }

    #[test]
    fn summaries_are_replaced_by_the_full_page() {
        let (ui, app, _dir) = start("fetch");
        app.open_article(row_of(&app, "Story 0"));
        let id = ui.get_reader().id as i64;
        // Only a teaser in the feed: show it while the page loads.
        assert!(ui.get_reader_loading());
        assert_eq!(kinds(&app), vec![BlockKind::Paragraph]);

        let page = vec![
            Block::new(Kind::Paragraph, LONG),
            Block::new(Kind::Heading, "More"),
            Block::new(Kind::Paragraph, LONG),
        ];
        app.page_ready(id, Ok(page.clone()));
        assert!(!ui.get_reader_loading());
        assert_eq!(ui.get_reader_note(), "");
        assert_eq!(
            kinds(&app),
            vec![
                BlockKind::Paragraph,
                BlockKind::Heading,
                BlockKind::Paragraph
            ]
        );
        // Saved, so it opens instantly (and offline) next time.
        let stored = app.store.reader_article(id).unwrap().unwrap().body.unwrap();
        assert_eq!(reader::decode(&stored), page);
    }

    #[test]
    fn failed_page_loads_keep_the_summary() {
        let (ui, app, _dir) = start("offline");
        app.open_article(row_of(&app, "Story 0"));
        let id = ui.get_reader().id as i64;

        let offline = anyhow::Error::new(feed::Unreachable("offline".into()));
        app.page_ready(id, Err(offline));
        assert!(!ui.get_reader_loading());
        assert!(ui.get_reader_note().contains("offline"));
        assert_eq!(kinds(&app), vec![BlockKind::Paragraph]);

        // A page with less text than the feed doesn't replace it.
        app.page_ready(id, Ok(vec![]));
        assert_eq!(kinds(&app), vec![BlockKind::Paragraph]);
    }

    #[test]
    fn late_results_for_another_article_are_ignored() {
        let (ui, app, _dir) = start("stale");
        app.open_article(row_of(&app, "Story 0"));
        let teaser_id = ui.get_reader().id as i64;
        app.open_article(row_of(&app, "Story 1"));

        app.page_ready(teaser_id, Ok(vec![Block::new(Kind::Paragraph, LONG)]));
        app.picture_ready(teaser_id, "http://127.0.0.1:9/pic.png", None);
        assert_eq!(ui.get_reader().title, "Story 1");
        assert_eq!(app.reader_blocks.row_count(), 14, "Story 1 is untouched");
    }

    #[test]
    fn pictures_fill_in_or_disappear() {
        let (_ui, app, _dir) = start("pictures");
        app.open_article(row_of(&app, "Story 1"));
        let id = app.reader_id.get().unwrap();
        let url = "http://127.0.0.1:9/pic.png";
        let last = app.reader_blocks.row_count() - 1;
        assert_eq!(
            app.reader_blocks.row_data(last).unwrap().image.size().width,
            0
        );

        app.picture_ready(id, url, Some(SharedPixelBuffer::new(4, 3)));
        assert_eq!(
            app.reader_blocks.row_data(last).unwrap().image.size().width,
            4
        );

        app.picture_ready(id, url, None);
        assert_eq!(app.reader_blocks.row_count(), last, "broken image removed");
    }

    #[test]
    fn toggling_read_from_the_reader() {
        let (ui, app, _dir) = start("toggle");
        // "All" view, so the row stays in the list.
        ui.set_unread_only(false);
        app.reload_articles();
        let row = row_of(&app, "Story 1");
        app.open_article(row);
        let id = app.reader_id.get().unwrap();

        app.reader_toggle_read();
        assert!(!ui.get_reader().read);
        assert!(!read_in_db(&app, id));
        assert!(!app.articles.row_data(row).unwrap().read);

        app.reader_toggle_read();
        assert!(ui.get_reader().read);
        assert!(read_in_db(&app, id));
    }

    #[test]
    fn escape_and_back_button_close_the_reader() {
        let (ui, app, _dir) = start("close");
        app.open_article(row_of(&app, "Story 1"));
        press(&ui, Key::Escape);
        assert!(!ui.get_reader_open());
        assert_eq!(app.reader_blocks.row_count(), 0, "content is released");
        assert_eq!(app.reader_id.get(), None);

        app.open_article(row_of(&app, "Story 0"));
        assert!(ui.get_reader_open());
        let back = ElementHandle::find_by_accessible_label(&ui, "Back to the list")
            .next()
            .expect("back button");
        back.invoke_accessible_default_action();
        assert!(!ui.get_reader_open());
    }

    #[test]
    fn reader_toolbar_buttons_are_wired() {
        let (ui, app, _dir) = start("toolbar");
        ui.set_unread_only(false);
        app.reload_articles();
        app.open_article(row_of(&app, "Story 1"));
        let id = app.reader_id.get().unwrap();
        let mark = ElementHandle::find_by_accessible_label(&ui, "Mark as unread")
            .next()
            .expect("mark as unread button");
        mark.invoke_accessible_default_action();
        assert!(!read_in_db(&app, id));
        // The label follows the state.
        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Mark as read")
                .next()
                .is_some()
        );
    }

    #[test]
    fn picking_a_source_closes_the_reader() {
        let (ui, app, _dir) = start("sidebar");
        let source = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .find(|e| e.accessible_role() == Some(AccessibleRole::Button))
                .unwrap_or_else(|| panic!("{label} in the sidebar"))
        };
        app.open_article(row_of(&app, "Story 1"));
        source("Example Blog").invoke_accessible_default_action();
        assert!(!ui.get_reader_open());
        assert_eq!(app.reader_id.get(), None);
        assert!(ui.get_selected_feed() >= 0);

        // Even the source already shown.
        app.open_article(0);
        source("Example Blog").invoke_accessible_default_action();
        assert!(!ui.get_reader_open());
        app.open_article(0);
        source("All articles").invoke_accessible_default_action();
        assert!(!ui.get_reader_open());
        assert_eq!(ui.get_selected_feed(), -1);
    }

    #[test]
    fn copy_link_confirms_with_a_toast() {
        let (ui, app, _dir) = start("copy");
        app.open_article(row_of(&app, "Story 1"));
        assert_eq!(ui.get_reader().link, "http://127.0.0.1:9/story/1");
        ElementHandle::find_by_accessible_label(&ui, "Copy link")
            .next()
            .expect("copy link button")
            .invoke_accessible_default_action();
        // The toast slides up from below the window.
        testing::mock_elapsed_time(Duration::from_secs(1));
        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Link copied")
                .next()
                .is_some(),
            "a toast says so"
        );
    }

    #[test]
    fn next_and_previous_walk_the_list_the_reader_came_from() {
        let (ui, app, _dir) = start("step");
        // The Unread view: stories leave it once read, but stepping doesn't skip.
        assert!(ui.get_unread_only());
        app.open_article(row_of(&app, "Story 1"));
        let first = app.reader_id.get().unwrap();
        assert!(!ui.get_reader_has_prev());
        assert!(ui.get_reader_has_next());
        testing::mock_elapsed_time(Duration::from_secs(1));
        assert_eq!(app.articles.row_count(), 1, "Story 1 left the Unread list");

        press(&ui, "j");
        assert_eq!(ui.get_reader().title, "Story 0");
        let second = app.reader_id.get().unwrap();
        assert!(read_in_db(&app, second), "stepping onto a story reads it");
        assert!(ui.get_reader_has_prev());
        assert!(!ui.get_reader_has_next());
        press(&ui, Key::RightArrow);
        assert_eq!(ui.get_reader().title, "Story 0", "stays put at the end");

        press(&ui, "k");
        assert_eq!(ui.get_reader().title, "Story 1");
        assert_eq!(app.reader_id.get(), Some(first));
        press(&ui, "p");
        assert_eq!(ui.get_reader().title, "Story 1", "stays put at the start");

        let button = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .unwrap_or_else(|| panic!("{label} button"))
        };
        button("Previous story").invoke_accessible_default_action();
        assert_eq!(ui.get_reader().title, "Story 1", "disabled at the start");
        button("Next story").invoke_accessible_default_action();
        assert_eq!(ui.get_reader().title, "Story 0");
        button("Previous story").invoke_accessible_default_action();
        assert_eq!(ui.get_reader().title, "Story 1");

        press(&ui, Key::Escape);
        press(&ui, "j");
        assert!(!ui.get_reader_open(), "no stepping with the reader closed");
    }

    #[test]
    fn the_next_story_is_loaded_ahead() {
        let (ui, app, _dir) = start("prefetch");
        app.open_article(row_of(&app, "Story 1"));
        // Story 0 only has a teaser: its page is already being fetched.
        let next = app.prefetch_id.get().expect("next story prefetched");
        assert!(app.pages_pending.borrow().contains(&next));
        let mut page = full_text();
        page.push(Block::new(Kind::Image, "http://127.0.0.1:9/a.png"));
        let blocks = page.len();
        app.page_ready(next, Ok(page));
        assert!(!read_in_db(&app, next), "prefetching doesn't read it");
        // Its pictures are on their way too.
        assert!(
            app.prefetch_pending
                .borrow()
                .contains("http://127.0.0.1:9/a.png")
        );

        app.reader_step(1);
        assert_eq!(app.reader_id.get(), Some(next));
        assert!(!ui.get_reader_loading(), "opens with the full text");
        assert_eq!(app.reader_blocks.row_count(), blocks);
        assert_eq!(kinds(&app).last(), Some(&BlockKind::Image));
        assert_eq!(app.prefetch_id.get(), None, "nothing after the last story");
    }

    #[test]
    fn a_page_still_loading_ahead_is_not_fetched_twice() {
        let (ui, app, _dir) = start("prefetch-wait");
        app.open_article(row_of(&app, "Story 1"));
        let next = app.prefetch_id.get().unwrap();
        app.reader_step(1);
        assert!(ui.get_reader_loading(), "waits for the prefetch");
        assert_eq!(app.pages_pending.borrow().len(), 1);
        app.page_ready(next, Ok(full_text()));
        assert!(!ui.get_reader_loading());
        assert!(
            app.reader_blocks.row_count() > 1,
            "the page replaced the teaser"
        );
    }

    #[test]
    fn settings_open_from_the_sidebar_and_switch_tabs() {
        let (ui, _app, dir) = start("settings");
        let button = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .unwrap_or_else(|| panic!("{label} button"))
        };
        let shows = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_some()
        };
        assert!(!ui.get_settings_open());

        button("Settings").invoke_accessible_default_action();
        assert!(ui.get_settings_open());
        // Let the modal fade in.
        testing::mock_elapsed_time(Duration::from_secs(1));
        assert!(
            shows("Take your edition with you"),
            "opens on Import & export"
        );
        assert!(shows("Import…") && shows("Export…"));
        assert!(!shows(env!("CARGO_PKG_VERSION")));

        button("About").invoke_accessible_default_action();
        assert!(shows(env!("CARGO_PKG_VERSION")));
        assert!(shows(env!("CARGO_PKG_REPOSITORY")));
        assert!(shows(dir.path().join("aggrega.db").to_str().unwrap()));
        assert!(shows("License: Read"), "links to the license text");
        assert!(!shows("Take your edition with you"));
        // Slint Royalty-free License attribution, at the bottom of the pane.
        ui.window().dispatch_event(WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(700., 400.),
            delta_x: 0.,
            delta_y: -2000.,
        });
        assert!(shows("#MadeWithSlint"));

        press(&ui, Key::Escape);
        assert!(!ui.get_settings_open());

        press(&ui, ",");
        assert!(!ui.get_settings_open(), "plain comma does nothing");
        button("Settings").invoke_accessible_default_action();
        button("Close settings").invoke_accessible_default_action();
        assert!(!ui.get_settings_open());
    }

    #[test]
    fn sync_runs_only_while_its_tab_is_open() {
        let (ui, app, _dir) = start("sync-tab");
        let button = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .unwrap_or_else(|| panic!("{label} button"))
        };
        // `changed` handlers run on the next pass of the event loop.
        let running = || {
            testing::mock_elapsed_time(Duration::from_millis(16));
            app.sync.borrow().is_some()
        };

        button("Settings").invoke_accessible_default_action();
        testing::mock_elapsed_time(Duration::from_secs(1));
        assert!(!running(), "not on the Import & export tab");

        button("Sync").invoke_accessible_default_action();
        assert!(running());
        assert!(ui.get_sync_device().contains(&sync::device_name()));
        assert_eq!(ui.get_sync_note(), "", "no problems on the loopback");

        button("About").invoke_accessible_default_action();
        assert!(!running(), "switching tabs stops it");

        button("Sync").invoke_accessible_default_action();
        assert!(running());
        press(&ui, Key::Escape);
        assert!(!ui.get_settings_open());
        assert!(!running(), "closing Settings stops it");

        // Settings reopens on the Sync tab, so it starts again.
        button("Settings").invoke_accessible_default_action();
        assert!(running());
        button("Close settings").invoke_accessible_default_action();
        assert!(!running());
    }

    #[test]
    fn refresh_interval_steps_between_limits_and_is_saved() {
        let (ui, app, _dir) = start("interval");
        let button = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .unwrap_or_else(|| panic!("{label} button"))
        };
        let shows = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_some()
        };
        let saved = || app.store.setting("refresh_interval").unwrap();
        assert_eq!(ui.get_refresh_minutes(), 20, "default");
        assert_eq!(app.auto_refresh.interval(), Duration::from_secs(20 * 60));
        assert_eq!(saved(), None);

        button("Settings").invoke_accessible_default_action();
        testing::mock_elapsed_time(Duration::from_secs(1));
        button("Updates").invoke_accessible_default_action();
        assert!(shows("Fresh stories, on their own"));
        assert!(shows("Refresh every 20 minutes"));

        button("Refresh less often").invoke_accessible_default_action();
        assert!(shows("Refresh every 30 minutes"));
        assert_eq!(saved().as_deref(), Some("30"));
        assert_eq!(app.auto_refresh.interval(), Duration::from_secs(30 * 60));
        for _ in 0..10 {
            button("Refresh less often").invoke_accessible_default_action();
        }
        assert_eq!(ui.get_refresh_minutes(), 120, "at most two hours");
        for _ in 0..10 {
            button("Refresh more often").invoke_accessible_default_action();
        }
        assert_eq!(ui.get_refresh_minutes(), 5, "at least five minutes");
        assert_eq!(saved().as_deref(), Some("5"));

        // Saved values outside the range are pulled back into it.
        assert_eq!(clamp_refresh_minutes(1), 5);
        assert_eq!(clamp_refresh_minutes(600), 120);
    }

    #[test]
    fn zoom_steps_between_limits() {
        assert_eq!(next_zoom(100, 1), 110);
        assert_eq!(next_zoom(100, -1), 90);
        assert_eq!(next_zoom(160, 1), 160, "at most 160%");
        assert_eq!(next_zoom(80, -1), 80, "at least 80%");
        assert_eq!(next_zoom(125, 0), DEFAULT_ZOOM, "Ctrl+0 resets");
        assert_eq!(
            next_zoom(85, 1),
            90,
            "off-step values move to the next step"
        );
        assert_eq!(clamp_zoom(50), 80);
        assert_eq!(clamp_zoom(500), 160);
        assert_eq!(clamp_zoom(125), 125);
    }

    /// Ctrl (or Cmd) held while `key` is pressed. Slint tracks modifiers from
    /// the modifier keys' own events.
    fn chord(ui: &AppWindow, modifier: Key, key: &str) {
        let modifier: SharedString = modifier.into();
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: modifier.clone(),
        });
        press(ui, key);
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text: modifier });
    }

    #[test]
    fn zoom_scales_the_window_and_is_saved() {
        let (ui, app, dir) = start("zoom");
        let button = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .unwrap_or_else(|| panic!("{label} button"))
        };
        let shows = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_some()
        };
        let saved = || app.store.setting("zoom").unwrap();
        let base = ui.window().scale_factor();
        let scale = || ui.window().scale_factor();
        assert_eq!(ui.get_zoom_percent(), 100, "default");
        assert_eq!(saved(), None);

        let column = ui.get_column_width();
        chord(&ui, Key::Control, "=");
        assert_eq!(ui.get_zoom_percent(), 110);
        assert!((scale() - base * 1.1).abs() < 1e-3);
        assert_eq!(saved().as_deref(), Some("110"));
        assert!(
            ui.get_column_width() < column,
            "the window keeps its size, so the content reflows into less room"
        );

        chord(&ui, Key::Control, "0");
        assert_eq!(ui.get_zoom_percent(), 100);
        assert!((scale() - base).abs() < 1e-3, "back to the system scale");
        assert_eq!(saved().as_deref(), Some("100"));

        chord(&ui, Key::Control, "-");
        assert_eq!(ui.get_zoom_percent(), 90);
        assert!((scale() - base * 0.9).abs() < 1e-3);
        assert!(!ui.get_settings_open());

        // Works with Settings open too.
        button("Settings").invoke_accessible_default_action();
        testing::mock_elapsed_time(Duration::from_secs(1));
        button("UI").invoke_accessible_default_action();
        assert!(shows("Make it easier on the eyes"));
        assert!(shows("Zoom 90 percent"));
        button("Zoom in").invoke_accessible_default_action();
        assert!(shows("Zoom 100 percent"));
        assert_eq!(saved().as_deref(), Some("100"));
        chord(&ui, Key::Control, "=");
        assert_eq!(ui.get_zoom_percent(), 110);
        assert!(shows("Zoom 110 percent"));
        assert!(ui.get_settings_open());
        press(&ui, Key::Escape);
        assert!(!ui.get_settings_open());

        // The window system resets the scale (say, the window moved to another
        // monitor): zoom applies to the new scale rather than compounding.
        ui.window()
            .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 2.0 });
        chord(&ui, Key::Control, "-");
        assert_eq!(ui.get_zoom_percent(), 100);
        assert!(
            (scale() - 2.0).abs() < 1e-3,
            "2.0 at 100%, not 2.0 * 1.1 * 0.9"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn background_refreshes_skip_offline_and_stay_quiet() {
        let (ui, app, _dir) = start("auto-refresh");
        // The toast slides up from below the window, where it can't be found.
        let toast_shows = |label: &str| {
            testing::mock_elapsed_time(Duration::from_secs(1));
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_some()
        };
        app.offline.set(true);
        testing::mock_elapsed_time(Duration::from_secs(21 * 60));
        assert!(!app.refreshing.get(), "skipped while offline");

        // Nothing new: the status line is enough.
        app.background.set(true);
        app.set_refreshing(true);
        let nothing = RefreshSummary {
            total: 1,
            ..Default::default()
        };
        app.refresh_done(Ok(nothing));
        assert!(!app.offline.get());
        assert!(!toast_shows("You're up to date"));

        app.background.set(true);
        app.set_refreshing(true);
        app.refresh_done(Ok(RefreshSummary {
            new_articles: 2,
            ..nothing
        }));
        assert!(toast_shows("2 new articles"));

        // Asked for by hand, it always reports back.
        app.background.set(false);
        app.set_refreshing(true);
        app.refresh_done(Ok(nothing));
        assert!(toast_shows("You're up to date"));
    }

    #[test]
    fn pulling_asks_first_then_replaces_everything() {
        let (ui, app, dir) = start("sync-pull");
        let shows = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_some()
        };
        // Another computer, following two other sources.
        let theirs = dir.path().join("theirs.db");
        let other = Store::open(&theirs).unwrap();
        for url in ["https://a.example/rss", "https://b.example/rss"] {
            let fetched = Fetched {
                title: url.into(),
                site_url: None,
                etag: None,
                last_modified: None,
                articles: vec![article(7, &[])],
            };
            other.add_feed(url, &fetched).unwrap();
        }
        let server = sync::Session::start(&loopback(), theirs, "Desk".into(), |_| {});
        let addr: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, server.port.unwrap()).into();

        app.open_article(row_of(&app, "Story 1"));
        assert!(ui.get_reader_open());
        app.sync_active(true);
        let gen_ = app.sync_gen.get();
        // Results from an earlier visit to the tab are dropped.
        app.sync_info_ready(gen_ - 1, Ok((addr, sync::info(addr).unwrap())));
        assert!(app.sync_pending.borrow().is_none());

        app.sync_info_ready(gen_, Ok((addr, sync::info(addr).unwrap())));
        testing::mock_elapsed_time(Duration::from_secs(1));
        assert!(shows("Replace everything with “Desk”?"));
        assert!(shows(
            "Its 2 sources and 2 articles (2 unread) will replace the 1 source and 2 articles \
             on this computer. This can't be undone."
        ));
        assert_eq!(app.store.stats().unwrap().feeds, 1, "nothing changes yet");

        // What the worker does once "Replace" is pressed.
        let stats = sync::pull(addr, &app.core.paths.db).unwrap();
        app.sync_pulled("Desk".into(), Ok(stats));
        let titles: Vec<_> = app.feeds.iter().map(|f| f.title.to_string()).collect();
        assert_eq!(titles, ["https://a.example/rss", "https://b.example/rss"]);
        assert_eq!(
            ui.get_sync_note(),
            "Pulled 2 sources and 2 articles from Desk"
        );
        assert!(!ui.get_sync_note_error());
        assert!(
            !ui.get_reader_open(),
            "the reader showed an article that's gone"
        );

        app.sync_pulled("Desk".into(), Err("connection reset".into()));
        assert!(ui.get_sync_note_error());
        assert_eq!(app.feeds.row_count(), 2, "a failed pull changes nothing");
        drop(server);
    }

    #[test]
    fn sync_explains_what_it_cannot_do() {
        let (ui, app, _dir) = start("sync-errors");
        app.sync_active(true);
        let gen_ = app.sync_gen.get();
        app.sync_info_ready(gen_, Err("couldn't reach 10.0.0.9:47812".into()));
        assert_eq!(ui.get_sync_note(), "Couldn't reach 10.0.0.9:47812");
        assert!(ui.get_sync_note_error());

        let newer = sync::Remote {
            name: "Desk".into(),
            schema: SCHEMA_VERSION + 1,
            stats: Stats {
                feeds: 1,
                articles: 1,
                unread: 1,
            },
        };
        app.sync_info_ready(gen_, Ok(("127.0.0.1:9".parse().unwrap(), newer)));
        assert!(ui.get_sync_note().contains("newer version"));
        assert!(app.sync_pending.borrow().is_none(), "nothing to confirm");

        // Pulling while a refresh writes to the database waits.
        app.refreshing.set(true);
        app.sync_connect("127.0.0.1".into());
        assert!(!ui.get_sync_busy());
        assert!(ui.get_sync_note().starts_with("Wait"));
        app.refreshing.set(false);

        app.sync_active(false);
        assert_eq!(ui.get_sync_note(), "", "leaving the tab clears it");
        assert_eq!(
            describe(
                Stats {
                    feeds: 1,
                    articles: 0,
                    unread: 0
                },
                true
            ),
            "1 source and 0 articles"
        );
    }

    #[test]
    fn exports_every_source_as_opml() {
        let (_ui, app, dir) = start("export");
        let path = dir.path().join("out.opml");
        assert_eq!(app.export_to(&path).unwrap(), 1);
        let sources = opml::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            sources,
            [Source {
                title: "Example Blog".into(),
                xml_url: "https://blog.example/rss".into(),
                html_url: None,
            }]
        );
    }

    #[test]
    fn importing_followed_sources_does_nothing() {
        let (ui, app, dir) = start("import-known");
        let path = dir.path().join("in.opml");
        std::fs::write(
            &path,
            r#"<opml version="2.0"><body><outline text="Blog" xmlUrl="https://blog.example/rss"/></body></opml>"#,
        )
        .unwrap();
        app.import_file(&path);
        assert!(!ui.get_importing(), "nothing new, so no import starts");
        // Not an OPML file: reported, nothing starts either.
        std::fs::write(&path, "<rss></rss>").unwrap();
        app.import_file(&path);
        assert!(!ui.get_importing());
        assert_eq!(app.store.sources().unwrap().len(), 1);
    }

    #[test]
    fn imports_count_sources_that_fail() {
        let (_ui, app, _dir) = start("import-fail");
        let dead = Source {
            title: "Dead".into(),
            xml_url: "http://127.0.0.1:9/feed".into(),
            html_url: None,
        };
        let plan = ops::ImportPlan {
            fresh: vec![dead],
            skipped: 0,
            total: 1,
        };
        let s = app.core.import(plan).unwrap();
        assert_eq!((s.added, s.skipped, s.failed + s.unreachable), (0, 0, 1));
        assert_eq!(app.store.sources().unwrap().len(), 1, "nothing stored");
    }

    #[test]
    fn import_summaries() {
        let s = |added, skipped, failed, unreachable| {
            import_message(&ImportSummary {
                added,
                skipped,
                failed,
                unreachable,
            })
        };
        assert_eq!(s(1, 0, 0, 0), "Imported 1 source");
        assert_eq!(
            s(12, 3, 1, 1),
            "Imported 12 sources  ·  3 already followed  ·  2 couldn't be added"
        );
        assert_eq!(s(0, 2, 0, 4), "You're offline  ·  nothing was imported");
        assert_eq!(s(0, 0, 2, 0), "Imported 0 sources  ·  2 couldn't be added");
    }

    #[test]
    fn data_paths_are_shown_relative_to_home() {
        let home = Path::new("/home/ada");
        assert_eq!(
            display_path(
                Path::new("/home/ada/.local/share/aggrega/aggrega.db"),
                Some(home)
            ),
            "~/.local/share/aggrega/aggrega.db"
        );
        assert_eq!(
            display_path(Path::new("/tmp/agg/aggrega.db"), Some(home)),
            "/tmp/agg/aggrega.db"
        );
        assert_eq!(
            display_path(Path::new("/tmp/agg/thumbs"), None),
            "/tmp/agg/thumbs"
        );
    }

    #[test]
    fn host_names_for_the_reader() {
        assert_eq!(host_of("https://www.example.com/a/b"), "example.com");
        assert_eq!(host_of("https://blog.example.org/"), "blog.example.org");
        assert_eq!(host_of("not a link"), "");
    }

    #[test]
    fn language_switch_applies_at_once_and_sticks() {
        let (ui, app, dir) = start("language");
        let button = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .unwrap_or_else(|| panic!("{label} button"))
        };
        let shows = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_some()
        };
        assert_eq!(ui.get_language(), "en", "English by default");
        app.open_article(row_of(&app, "Story 1"));

        // Through the switch and its list, as a click would.
        button("Language").invoke_accessible_default_action();
        button("Italiano").invoke_accessible_default_action();
        assert_eq!(ui.get_language(), "it");
        // Slint's strings and the ones built in Rust switch together.
        assert!(shows("Impostazioni") && !shows("Settings"));
        assert!(shows("Lingua"));
        assert_eq!(ui.get_view_title(), "Tutti gli articoli");
        assert!(
            ui.get_status_text().starts_with("1 da leggere"),
            "{}",
            ui.get_status_text()
        );
        assert_eq!(ui.get_today(), i18n::long_date(chrono::Local::now()));
        assert!(
            ui.get_today().contains(" 20"),
            "Italian date: {}",
            ui.get_today()
        );
        let row = app.articles.row_data(row_of(&app, "Story 1")).unwrap();
        assert!(row.time.ends_with(" nov 2023"), "{}", row.time);
        assert_eq!(
            ui.get_reader().time,
            row.time,
            "the open article's date too"
        );

        // Remembered for next time.
        drop(app);
        let store = Store::open(&dir.path().join("aggrega.db")).unwrap();
        assert_eq!(store.setting("language").unwrap().as_deref(), Some("it"));

        with_app(|a| a.change_language("en".into()));
        assert!(shows("Settings") && !shows("Impostazioni"));
        assert_eq!(ui.get_view_title(), "All articles");
        // Unknown codes are ignored.
        with_app(|a| a.change_language("xx".into()));
        assert_eq!(ui.get_language(), "en");
    }

    fn titles(app: &App) -> Vec<String> {
        let mut titles: Vec<String> = (0..app.articles.row_count())
            .map(|i| app.articles.row_data(i).unwrap().title.to_string())
            .collect();
        titles.sort();
        titles
    }

    fn id_of(app: &App, title: &str) -> i64 {
        app.store
            .articles(ArticleFilter::default(), 10)
            .unwrap()
            .into_iter()
            .find(|a| a.title == title)
            .expect("article is in the database")
            .id
    }

    /// Types `text` into the field: the text changes, then the debounce runs.
    fn search_for(ui: &AppWindow, app: &App, text: &str) {
        ui.set_search_text(text.into());
        app.search_changed();
        testing::mock_elapsed_time(Duration::from_millis(200));
    }

    /// A second source, so the source filter has something to leave out.
    fn add_other_feed(app: &App) {
        let store = Store::open(&app.core.paths.db).unwrap();
        store
            .add_feed(
                "https://other.example/rss",
                &Fetched {
                    title: "Other Feed".into(),
                    site_url: None,
                    etag: None,
                    last_modified: None,
                    articles: vec![article(2, &[Block::new(Kind::Paragraph, LONG)])],
                },
            )
            .unwrap();
    }

    #[test]
    fn search_shows_matches_and_clearing_restores_the_list() {
        let (ui, app, _dir) = start("search");
        search_for(&ui, &app, "aggregator");
        assert!(ui.get_searching());
        assert_eq!(titles(&app), vec!["Story 1"]);

        // Whitespace is no search: the full list comes back at once.
        ui.set_search_text("  ".into());
        app.search_changed();
        assert!(!ui.get_searching());
        assert_eq!(titles(&app), vec!["Story 0", "Story 1"]);
    }

    #[test]
    fn search_follows_the_unread_tab_and_the_selected_source() {
        let (ui, app, _dir) = start("search-view");
        add_other_feed(&app);
        app.reload_all();
        search_for(&ui, &app, "aggregator");
        assert_eq!(titles(&app), vec!["Story 1", "Story 2"]);

        // Reading a match takes it out of the Unread tab only.
        app.store.set_read(id_of(&app, "Story 1"), true).unwrap();
        app.reload_all();
        assert_eq!(titles(&app), vec!["Story 2"]);
        ui.set_unread_only(false);
        app.reload_all();
        assert_eq!(titles(&app), vec!["Story 1", "Story 2"]);

        let blog = app
            .store
            .feeds()
            .unwrap()
            .into_iter()
            .find(|f| f.title == "Example Blog")
            .unwrap();
        ui.set_selected_feed(blog.id as i32);
        app.reload_all();
        assert_eq!(titles(&app), vec!["Story 1"]);
    }

    #[test]
    fn slash_and_ctrl_f_focus_the_field_and_it_takes_the_keys() {
        let (ui, app, _dir) = start("search-keys");
        press(&ui, "/");
        assert!(ui.get_search_focused());
        press(&ui, "a");
        press(&ui, "/");
        assert_eq!(ui.get_search_text(), "a/");
        testing::mock_elapsed_time(Duration::from_millis(200));

        // Escape clears the field and hands the keyboard back to the list.
        press(&ui, Key::Escape);
        assert_eq!(ui.get_search_text(), "");
        assert!(!ui.get_search_focused());
        assert!(!ui.get_searching());

        chord(&ui, Key::Control, "f");
        assert!(ui.get_search_focused());
        press(&ui, "m");
        assert_eq!(ui.get_search_text(), "m");

        // The field takes the reader's keys too: "/" closes the reader.
        press(&ui, Key::Escape);
        app.open_article(row_of(&app, "Story 1"));
        assert!(ui.get_reader_open());
        press(&ui, "/");
        assert!(!ui.get_reader_open());
        assert!(ui.get_search_focused());
        press(&ui, "j");
        assert_eq!(ui.get_search_text(), "j");
    }

    #[test]
    fn escape_closes_the_reader_first_and_then_clears_the_search() {
        let (ui, app, _dir) = start("search-esc");
        search_for(&ui, &app, "aggregator");
        app.open_article(row_of(&app, "Story 1"));
        press(&ui, Key::Escape);
        assert!(!ui.get_reader_open());
        assert_eq!(ui.get_search_text(), "aggregator");
        assert!(ui.get_searching());

        press(&ui, Key::Escape);
        assert_eq!(ui.get_search_text(), "");
        assert!(!ui.get_searching());
        assert_eq!(titles(&app), vec!["Story 0"]);
    }

    #[test]
    fn typos_match_once_the_vocabulary_is_cached() {
        let (ui, app, _dir) = start("search-typo");
        search_for(&ui, &app, "stroy");
        assert!(ui.get_searching());
        assert!(
            titles(&app).is_empty(),
            "no typo tolerance before the cache"
        );

        // The worker's job, done here so the test doesn't wait on a thread.
        let version = app.store.data_version().unwrap();
        let words = app.store.search_terms().unwrap();
        app.terms_ready(version, Ok(search::Terms::new(words)));
        assert_eq!(titles(&app), vec!["Story 0", "Story 1"]);

        // Opening the database on another connection doesn't stale the cache.
        let before = app.store.data_version().unwrap();
        Store::open(&app.core.paths.db)
            .unwrap()
            .search_terms()
            .unwrap();
        assert_eq!(app.store.data_version().unwrap(), before);
    }

    #[test]
    fn writes_on_this_connection_mark_the_vocabulary_stale() {
        let (_ui, app, _dir) = start("search-stale");
        let version = app.store.data_version().unwrap();
        app.terms_ready(version, Ok(search::Terms::new(Vec::new())));
        assert_eq!(app.terms_stamp.get(), Some(version));

        let feed = app.store.feeds().unwrap()[0].id as i32;
        app.remove_feed(feed);
        assert_eq!(app.terms_stamp.get(), None);
    }

    #[test]
    fn a_search_without_matches_empties_the_list() {
        let (ui, app, _dir) = start("search-none");
        search_for(&ui, &app, "zebra");
        assert!(ui.get_searching());
        assert!(titles(&app).is_empty());

        // Only punctuation has no words to search for.
        search_for(&ui, &app, "!?");
        assert!(ui.get_searching());
        assert!(titles(&app).is_empty());
    }

    #[test]
    fn search_field_stays_clear_of_the_header_at_narrow_widths() {
        let (ui, _app, _dir) = start("search-narrow");
        for (w, h) in [(720., 560.), (800., 600.), (1240., 820.)] {
            resize(&ui, w, h);
            let find = |id: &str| {
                ElementHandle::find_by_element_id(&ui, id)
                    .next()
                    .unwrap_or_else(|| panic!("{id} in the header"))
            };
            let left = |e: &ElementHandle| e.absolute_position().x;
            let right = |e: &ElementHandle| e.absolute_position().x + e.size().width;
            let title = find("AppWindow::title-column");
            let field = find("AppWindow::search-box");
            let tabs = find("AppWindow::header-tabs");
            assert!(
                right(&title) <= left(&field) && right(&field) <= left(&tabs),
                "at {w}px the header's title, search field and tabs overlap"
            );
            // Two 36px buttons with 8px gaps follow the tabs, then the window's edge.
            assert!(
                right(&tabs) + 88. <= w,
                "at {w}px the header buttons run off the window"
            );
        }
    }

    /// Home/End: Apple's TextInput ignores them and moves by line with
    /// Cmd+Left/Right, which Slint reports as Control+Left/Right.
    fn move_to_line_start(ui: &AppWindow) {
        if cfg!(target_vendor = "apple") {
            chord(ui, Key::Control, &SharedString::from(Key::LeftArrow));
        } else {
            press(ui, Key::Home);
        }
    }

    fn move_to_line_end(ui: &AppWindow) {
        if cfg!(target_vendor = "apple") {
            chord(ui, Key::Control, &SharedString::from(Key::RightArrow));
        } else {
            press(ui, Key::End);
        }
    }

    #[test]
    fn long_text_in_add_source_stays_in_the_box_and_follows_the_cursor() {
        fn left(e: &ElementHandle) -> f32 {
            e.absolute_position().x
        }
        fn right(e: &ElementHandle) -> f32 {
            e.absolute_position().x + e.size().width
        }

        let (ui, _app, _dir) = start("add-source-long");
        chord(&ui, Key::Control, "n");
        testing::mock_elapsed_time(Duration::from_secs(1));

        let flick = ElementHandle::find_by_element_id(&ui, "AddSourceDialog::flick")
            .next()
            .expect("add-source flickable");
        let input = ElementHandle::find_by_element_id(&ui, "AddSourceDialog::input")
            .next()
            .expect("add-source input");
        // The bordered field is the 50px-high rectangle that wraps the flickable
        // with its 14px padding. The clip rectangle has the same height but starts
        // at the flickable's left edge, so it is excluded.
        let field: Vec<ElementHandle> = ElementHandle::find_by_element_type_name(&ui, "Rectangle")
            .filter(|r| {
                let p = r.absolute_position();
                let s = r.size();
                s.height == 50.
                    && p.x < left(&flick)
                    && p.x + s.width >= right(&flick)
                    && p.y <= flick.absolute_position().y
                    && p.y + s.height >= flick.absolute_position().y + flick.size().height
            })
            .collect();
        assert_eq!(
            field.len(),
            1,
            "exactly one bordered field around the input"
        );
        let field = &field[0];

        let url = "https://www.gazzetta.it/dynamic-feed/rss/section/Calcio/Serie-A.xml?x=";
        let text: String = url.repeat(2).chars().take(120).collect();
        for c in text.chars() {
            press(&ui, c.to_string());
        }

        // A. Overflowing text: the input runs past the container, the container
        // stays inside the box (14px is the field's right padding), and the view
        // is scrolled so the cursor at the end is visible.
        assert!(
            right(&input) >= right(&flick),
            "text overflows: input right {} vs container right {}",
            right(&input),
            right(&flick)
        );
        assert!(
            (right(field) - right(&flick) - 14.).abs() < 0.5,
            "container did not grow: field right {} vs container right {}",
            right(field),
            right(&flick)
        );
        assert!(
            left(&input) < left(&flick),
            "scrolled to the cursor: input x {} vs container x {}",
            left(&input),
            left(&flick)
        );

        // B. Home scrolls back to the start.
        move_to_line_start(&ui);
        assert_eq!(left(&input), left(&flick), "Home scrolls back to the start");

        // C. End, then delete 30 characters: still overflowing, so no blank gap.
        move_to_line_end(&ui);
        for _ in 0..30 {
            press(&ui, Key::Backspace);
        }
        assert!(
            right(&input) >= right(&flick) - 6.,
            "no blank gap after deleting: input right {} vs container right {}",
            right(&input),
            right(&flick)
        );

        // D. Closing and reopening gives an empty field scrolled to the start.
        ui.invoke_close_add_dialog();
        testing::mock_elapsed_time(Duration::from_secs(1));
        chord(&ui, Key::Control, "n");
        testing::mock_elapsed_time(Duration::from_secs(1));
        assert_eq!(input.accessible_value().unwrap_or_default(), "");
        assert_eq!(
            left(&input),
            left(&flick),
            "reopened field starts at the beginning"
        );
    }
}
