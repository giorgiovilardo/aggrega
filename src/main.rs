// Hide the console window on Windows release builds (future targets).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod db;
mod fetch;
mod reader;
mod text;
mod thumbs;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use db::{RefreshSummary, Store};
use reader::Block;

slint::include_modules!();

/// How many articles the list shows at most (newest first).
const MAX_ARTICLES: usize = 400;

/// How long a row takes to animate out before it is removed from the list
/// (matches the fade + collapse in `ui/article-card.slint`).
const LEAVE_ANIMATION: Duration = Duration::from_millis(560);

thread_local! {
    /// The application state lives on the UI thread; worker threads reach it
    /// through `slint::invoke_from_event_loop` + `with_app`.
    static APP: OnceCell<Rc<App>> = const { OnceCell::new() };
}

fn with_app(f: impl FnOnce(&Rc<App>)) {
    APP.with(|cell| {
        if let Some(app) = cell.get() {
            f(app)
        }
    });
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

struct Paths {
    db: PathBuf,
    thumbs: PathBuf,
}

impl Paths {
    fn new() -> Result<Self> {
        let dirs =
            directories::ProjectDirs::from("", "", "aggrega").context("no home directory found")?;
        let data = dirs.data_dir().to_path_buf();
        let thumbs = dirs.cache_dir().join("thumbs");
        std::fs::create_dir_all(&data)?;
        std::fs::create_dir_all(&thumbs)?;
        Ok(Self {
            db: data.join("aggrega.db"),
            thumbs,
        })
    }
}

struct App {
    ui: slint::Weak<AppWindow>,
    store: Store,
    paths: Paths,
    agent: ureq::Agent,
    articles: Rc<VecModel<ArticleItem>>,
    /// Publish timestamps of `articles`, row for row (for relative time labels).
    article_ts: RefCell<Vec<i64>>,
    feeds: Rc<VecModel<FeedItem>>,
    thumb_cache: RefCell<HashMap<SharedString, slint::Image>>,
    thumb_pending: RefCell<HashSet<SharedString>>,
    thumb_failed: RefCell<HashSet<SharedString>>,
    refreshing: Cell<bool>,
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
}

impl App {
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

    fn report(&self, what: &str, err: anyhow::Error) {
        eprintln!("aggrega: {what}: {err:#}");
        self.toast(format!("{what}: {err}"));
    }

    // ---- view state ------------------------------------------------------

    fn reload_all(&self) {
        self.reload_feeds();
        self.reload_articles();
    }

    fn reload_feeds(&self) {
        let feeds = match self.store.feeds() {
            Ok(f) => f,
            Err(e) => return self.report("Couldn't load sources", e),
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
            .unwrap_or_else(|| "All articles".into());
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
                tint: text::tint(&f.title),
                error: f.has_error,
            })
            .collect();
        sync_model(&self.feeds, items);
        self.update_status();
    }

    fn reload_articles(&self) {
        let ui = self.ui();
        let rows =
            match self
                .store
                .articles(self.selected_feed(), ui.get_unread_only(), MAX_ARTICLES)
            {
                Ok(r) => r,
                Err(e) => return self.report("Couldn't load articles", e),
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
                        tint: text::tint(&a.feed_title),
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
        wanted.dedup();
        self.load_thumbs(wanted);
    }

    fn update_status(&self) {
        let ui = self.ui();
        let status = if self.refreshing.get() {
            "Refreshing your sources…".to_string()
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
                0 => "All caught up".to_string(),
                n => format!("{n} unread"),
            };
            match self.last_refresh.get() {
                _ if self.offline.get() => format!("{unread}  ·  offline, showing saved stories"),
                Some(t) => format!("{unread}  ·  updated {}", text::ago(t, now())),
                None => unread,
            }
        };
        ui.set_status_text(status.into());
    }

    /// Re-renders relative time labels; runs once a minute.
    fn tick(&self) {
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
        let agent = self.agent.clone();
        let dir = self.paths.thumbs.clone();
        let urls: Vec<String> = urls.into_iter().map(Into::into).collect();
        std::thread::spawn(move || {
            fetch::par_for_each(&urls, 6, |url| {
                let thumb = thumbs::load(&agent, &dir, url);
                let url = SharedString::from(url.as_str());
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(|app| app.thumb_ready(url, thumb))
                });
            });
        });
    }

    fn thumb_ready(&self, url: SharedString, thumb: thumbs::Thumb) {
        self.thumb_pending.borrow_mut().remove(&url);
        let image = match thumb {
            thumbs::Thumb::Ready(pixels) => {
                let img = slint::Image::from_rgb8(pixels);
                self.thumb_cache
                    .borrow_mut()
                    .insert(url.clone(), img.clone());
                Some(img)
            }
            thumbs::Thumb::Broken => {
                self.thumb_failed.borrow_mut().insert(url.clone());
                None
            }
            // Offline: leave the placeholder; the next reload asks again.
            thumbs::Thumb::Unavailable => return,
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

    fn refresh(&self) {
        if self.refreshing.get() {
            return;
        }
        let jobs = match self.store.fetch_jobs() {
            Ok(j) if !j.is_empty() => j,
            Ok(_) => return,
            Err(e) => return self.report("Couldn't start refresh", e),
        };
        self.set_refreshing(true);
        let agent = self.agent.clone();
        let db_path = self.paths.db.clone();
        std::thread::spawn(move || {
            let results = fetch::fetch_all(&agent, &jobs);
            let summary = Store::open(&db_path)
                .and_then(|s| s.apply_refresh(results))
                .map_err(|e| format!("{e:#}"));
            let _ =
                slint::invoke_from_event_loop(move || with_app(|app| app.refresh_done(summary)));
        });
    }

    fn set_refreshing(&self, on: bool) {
        self.refreshing.set(on);
        self.ui().set_refreshing(on);
        self.update_status();
    }

    fn refresh_done(&self, summary: Result<RefreshSummary, String>) {
        let offline = matches!(&summary, Ok(s) if s.offline());
        self.offline.set(offline);
        // Saved stories stay available offline; just don't pretend we updated.
        if !offline {
            let t = now();
            self.last_refresh.set(Some(t));
            let _ = self.store.set_setting("last_refresh", &t.to_string());
        }
        self.set_refreshing(false);
        self.reload_all();
        match summary {
            Ok(_) if offline => {
                self.toast("You're offline  ·  showing your saved stories");
            }
            Ok(s) => {
                let mut msg = match s.new_articles {
                    0 => "You're up to date".to_string(),
                    1 => "1 new article".to_string(),
                    n => format!("{n} new articles"),
                };
                let problems = s.failed + s.unreachable;
                if problems > 0 {
                    msg += &format!(
                        "  ·  {} source{} failed",
                        problems,
                        if problems == 1 { "" } else { "s" }
                    );
                }
                self.toast(msg);
            }
            Err(e) => self.toast(format!("Refresh failed: {e}")),
        }
    }

    fn open_article(&self, row: usize) {
        let Some(item) = self.articles.row_data(row) else {
            return;
        };
        self.show_reader(item.id as i64);
        if !item.read {
            self.set_read(row, true);
        }
    }

    // ---- reader view -------------------------------------------------------

    /// Opens the reader with the feed's copy of the article, then fetches the
    /// full page in the background if the feed only had a summary.
    fn show_reader(&self, id: i64) {
        let article = match self.store.reader_article(id) {
            Ok(Some(a)) => a,
            Ok(None) => return,
            Err(e) => return self.report("Couldn't open the article", e),
        };
        let ui = self.ui();
        let lead = article.image_url.clone();
        let blocks = reader::tidy(
            reader::decode(article.body.as_deref().unwrap_or("")),
            &article.title,
            lead.as_deref(),
        );
        let fetch_page = (article.body.is_none() || reader::needs_full_page(&blocks))
            && reader::is_scrapable(&article.link);

        self.reader_id.set(Some(id));
        *self.reader_link.borrow_mut() = article.link.clone();
        *self.reader_lead.borrow_mut() = lead.clone();
        ui.set_reader(ReaderInfo {
            id: id as i32,
            title: article.title.as_str().into(),
            source: article.feed_title.as_str().into(),
            time: text::ago(article.published, now()).into(),
            host: host_of(&article.link).into(),
            tint: text::tint(&article.feed_title),
            read: true,
            lead: Default::default(),
        });
        ui.set_reader_loading(fetch_page);
        ui.set_reader_note(
            if !fetch_page && blocks.is_empty() {
                "This story has no text in its feed. Open the original to read it."
            } else {
                ""
            }
            .into(),
        );
        self.set_reader_blocks(&blocks);
        self.load_pictures(id, lead.into_iter().collect());
        ui.invoke_show_reader();

        if fetch_page {
            let agent = self.agent.clone();
            let link = article.link;
            std::thread::spawn(move || {
                let result = fetch::fetch_page(&agent, &link).map(|page| {
                    let base = url::Url::parse(&link).expect("scrapable links are valid URLs");
                    reader::extract_article(&page, &base)
                });
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(|app| app.page_ready(id, result))
                });
            });
        }
    }

    /// Shows `blocks` in the reader and starts loading their images.
    fn set_reader_blocks(&self, blocks: &[Block]) {
        self.reader_len.set(reader::text_len(blocks));
        let mut images = Vec::new();
        let rows: Vec<ReaderBlock> = blocks
            .iter()
            .map(|b| {
                let (kind, text) = match b {
                    Block::Paragraph(t) => (BlockKind::Paragraph, t),
                    Block::Heading(t) => (BlockKind::Heading, t),
                    Block::Quote(t) => (BlockKind::Quote, t),
                    Block::Bullet(t) => (BlockKind::Bullet, t),
                    Block::Code(t) => (BlockKind::Code, t),
                    Block::Image(u) => {
                        images.push(u.clone());
                        return ReaderBlock {
                            kind: BlockKind::Image,
                            image_url: u.as_str().into(),
                            ..Default::default()
                        };
                    }
                };
                ReaderBlock {
                    kind,
                    text: text.as_str().into(),
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
        let current = self.reader_id.get() == Some(id);
        let shown = if current { self.reader_len.get() } else { 0 };
        let note = match result {
            Ok(page) if reader::text_len(&page) > shown => {
                // Keep it, so the article opens instantly (and offline) next time.
                if let Err(e) = self.store.set_body(id, &reader::encode(&page)) {
                    eprintln!("aggrega: couldn't save article text: {e:#}");
                }
                if current {
                    let title = self.ui().get_reader().title;
                    let lead = self.reader_lead.borrow().clone();
                    self.set_reader_blocks(&reader::tidy(page, &title, lead.as_deref()));
                }
                ""
            }
            Ok(_) if shown == 0 => {
                "Couldn't find the story on its page. Open the original to read it."
            }
            Ok(_) => "",
            Err(e) if fetch::is_unreachable(&e) => {
                "You're offline, so this is the summary from the feed."
            }
            Err(_) => "The full story couldn't be loaded, so this is the summary from the feed.",
        };
        if current {
            let ui = self.ui();
            ui.set_reader_loading(false);
            ui.set_reader_note(note.into());
        }
    }

    fn load_pictures(&self, id: i64, urls: Vec<String>) {
        if urls.is_empty() {
            return;
        }
        let agent = self.agent.clone();
        std::thread::spawn(move || {
            fetch::par_for_each(&urls, 4, |url| {
                let picture = thumbs::load_picture(&agent, url);
                let url = url.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(|app| app.picture_ready(id, &url, picture))
                });
            });
        });
    }

    fn picture_ready(&self, id: i64, url: &str, picture: Option<thumbs::Picture>) {
        if self.reader_id.get() != Some(id) {
            return;
        }
        let image = picture.map(slint::Image::from_rgba8);
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

    fn close_reader(&self) {
        self.reader_id.set(None);
        // Free the pictures; the list underneath is still as the user left it.
        self.reader_blocks.set_vec(Vec::new());
        let ui = self.ui();
        let mut info = ui.get_reader();
        info.lead = Default::default();
        ui.set_reader(info);
    }

    fn open_original(&self) {
        let link = self.reader_link.borrow().clone();
        if link.is_empty() {
            return;
        }
        if let Err(e) = open::that_detached(&link) {
            self.toast(format!("Couldn't open the browser: {e}"));
        }
    }

    fn reader_toggle_read(&self) {
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
                    return self.report("Couldn't save read state", e);
                }
                // Marked unread again after it left the Unread list: bring it back.
                self.reload_all();
            }
        }
        info.read = read;
        ui.set_reader(info);
    }

    fn toggle_read(&self, row: usize) {
        if let Some(item) = self.articles.row_data(row) {
            self.set_read(row, !item.read);
        }
    }

    fn set_read(&self, row: usize, read: bool) {
        let Some(mut item) = self.articles.row_data(row) else {
            return;
        };
        if let Err(e) = self.store.set_read(item.id as i64, read) {
            return self.report("Couldn't save read state", e);
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
    }

    fn mark_all_read(&self) {
        match self.store.mark_all_read(self.selected_feed()) {
            Ok(0) => self.toast("Nothing left to read here"),
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
                self.toast(format!(
                    "Marked {n} article{} as read",
                    if n == 1 { "" } else { "s" }
                ));
            }
            Err(e) => self.report("Couldn't mark as read", e),
        }
    }

    fn add_feed(&self, input: SharedString) {
        let ui = self.ui();
        if ui.get_adding() {
            return;
        }
        ui.set_adding(true);
        ui.set_add_error("".into());
        let agent = self.agent.clone();
        let db_path = self.paths.db.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<(i64, String, usize)> {
                let (url, fetched) = fetch::subscribe(&agent, &input)?;
                let (id, n) = Store::open(&db_path)?.add_feed(&url, &fetched)?;
                Ok((id, fetched.title, n))
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = slint::invoke_from_event_loop(move || with_app(|app| app.add_done(result)));
        });
    }

    fn add_done(&self, result: Result<(i64, String, usize), String>) {
        let ui = self.ui();
        ui.set_adding(false);
        match result {
            Ok((id, title, n)) => {
                ui.invoke_close_add_dialog();
                ui.set_selected_feed(id as i32);
                self.reload_all();
                self.toast(format!("Added {title}  ·  {n} articles"));
            }
            Err(e) => {
                let mut e = e;
                if let Some(first) = e.get_mut(0..1) {
                    first.make_ascii_uppercase();
                }
                ui.set_add_error(e.into());
            }
        }
    }

    fn remove_feed(&self, id: i32) {
        let title = self
            .feeds
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.title.to_string())
            .unwrap_or_default();
        if let Err(e) = self.store.remove_feed(id as i64) {
            return self.report("Couldn't remove source", e);
        }
        self.reload_all();
        self.toast(format!("Removed {title}"));
    }
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

/// Creates the app state for `ui` and wires up every callback.
fn setup(ui: &AppWindow, store: Store, paths: Paths) -> Result<Rc<App>> {
    // Theme: saved preference wins, otherwise follow the desktop.
    if let Some(theme) = store.setting("theme")? {
        ui.global::<Theme>().set_dark(theme == "dark");
    }
    ui.invoke_apply_theme();
    ui.set_today(
        chrono::Local::now()
            .format("%A, %B %-d, %Y")
            .to_string()
            .into(),
    );

    ui.set_version(env!("CARGO_PKG_VERSION").into());

    let articles = Rc::new(VecModel::default());
    let feeds = Rc::new(VecModel::default());
    let reader_blocks = Rc::new(VecModel::default());
    ui.set_articles(ModelRc::from(articles.clone()));
    ui.set_feeds(ModelRc::from(feeds.clone()));
    ui.set_reader_blocks(ModelRc::from(reader_blocks.clone()));

    let last_refresh = store.setting("last_refresh")?.and_then(|v| v.parse().ok());
    let app = Rc::new(App {
        ui: ui.as_weak(),
        store,
        paths,
        agent: fetch::agent(),
        articles,
        article_ts: RefCell::default(),
        feeds,
        thumb_cache: RefCell::default(),
        thumb_pending: RefCell::default(),
        thumb_failed: RefCell::default(),
        refreshing: Cell::new(false),
        last_refresh: Cell::new(last_refresh),
        offline: Cell::new(false),
        reader_blocks,
        reader_id: Cell::new(None),
        reader_link: RefCell::default(),
        reader_lead: RefCell::default(),
        reader_len: Cell::new(0),
    });
    APP.with(|cell| {
        let _ = cell.set(app.clone());
    });

    ui.on_refresh(|| with_app(|a| a.refresh()));
    ui.on_open_article(|row| with_app(|a| a.open_article(row as usize)));
    ui.on_toggle_read(|row| with_app(|a| a.toggle_read(row as usize)));
    ui.on_filter_changed(|| {
        with_app(|a| {
            a.reload_feeds();
            a.reload_articles();
        })
    });
    ui.on_mark_all_read(|| with_app(|a| a.mark_all_read()));
    ui.on_add_feed(|url| with_app(|a| a.add_feed(url)));
    ui.on_remove_feed(|id| with_app(|a| a.remove_feed(id)));
    ui.on_close_reader(|| with_app(|a| a.close_reader()));
    ui.on_open_original(|| with_app(|a| a.open_original()));
    ui.on_reader_toggle_read(|| with_app(|a| a.reader_toggle_read()));
    ui.on_theme_changed(|dark| {
        with_app(|a| {
            let _ = a
                .store
                .set_setting("theme", if dark { "dark" } else { "light" });
        })
    });

    // Custom title bar: minimise/maximise are handled in Slint, closing here.
    let weak = ui.as_weak();
    ui.on_close_window(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });

    Ok(app)
}

fn main() -> Result<()> {
    let paths = Paths::new()?;
    let store =
        Store::open(&paths.db).with_context(|| format!("opening {}", paths.db.display()))?;
    let ui = AppWindow::new()?;
    // Lets the desktop match the window to aggrega.desktop (taskbar icon/name).
    // Needs the platform created by `AppWindow::new`, and must precede `run`.
    slint::set_xdg_app_id("aggrega")?;

    let app = setup(&ui, store, paths)?;

    // Show cached articles instantly, then fetch fresh ones in the background.
    app.reload_all();
    app.refresh();

    let clock = slint::Timer::default();
    clock.start(slint::TimerMode::Repeated, Duration::from_secs(60), || {
        with_app(|a| a.tick())
    });

    drop(app);
    ui.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Drives the real window headlessly (Slint's testing backend) against a
    //! temporary database. Each test runs on its own thread, so each gets its
    //! own backend and `APP`.

    use super::*;
    use fetch::{Fetched, NewArticle};
    use i_slint_backend_testing::{self as testing, ElementHandle};
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
        let mut blocks = vec![Block::Heading("Story 1".into())];
        blocks.extend((0..12).map(|_| Block::Paragraph(LONG.into())));
        blocks.push(Block::Quote("A quote".into()));
        blocks.push(Block::Image("http://127.0.0.1:9/pic.png".into()));
        blocks
    }

    /// A window and app over a fresh database holding one feed:
    /// "Story 1" (full text in the feed) and "Story 0" (summary only).
    fn start(name: &str) -> (AppWindow, Rc<App>, PathBuf) {
        testing::init_no_event_loop();
        let dir = std::env::temp_dir().join(format!("aggrega-ui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("thumbs")).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        store
            .add_feed(
                "https://blog.example/rss",
                &Fetched {
                    title: "Example Blog".into(),
                    site_url: None,
                    etag: None,
                    last_modified: None,
                    articles: vec![
                        article(0, &[Block::Paragraph("Just a teaser…".into())]),
                        article(1, &full_text()),
                    ],
                },
            )
            .unwrap();
        let paths = Paths {
            db: dir.join("t.db"),
            thumbs: dir.join("thumbs"),
        };
        let ui = AppWindow::new().unwrap();
        let app = setup(&ui, store, paths).unwrap();
        app.reload_all();
        // Headless windows start at 0×0, where every element counts as clipped away.
        ui.window().set_size(slint::LogicalSize::new(1240., 820.));
        // Let the opening animation finish so key presses aren't swallowed by it.
        testing::mock_elapsed_time(Duration::from_secs(3));
        (ui, app, dir)
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

    #[test]
    fn layout_adapts_to_window_width() {
        let (ui, _app, dir) = start("layout");

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
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opening_an_article_shows_the_feed_text() {
        let (ui, app, dir) = start("open");
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
        assert!(app.store.reader_article(id).unwrap().unwrap().read);
        assert!(app.articles.row_data(row).unwrap().read);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn summaries_are_replaced_by_the_full_page() {
        let (ui, app, dir) = start("fetch");
        app.open_article(row_of(&app, "Story 0"));
        let id = ui.get_reader().id as i64;
        // Only a teaser in the feed: show it while the page loads.
        assert!(ui.get_reader_loading());
        assert_eq!(kinds(&app), vec![BlockKind::Paragraph]);

        let page = vec![
            Block::Paragraph(LONG.into()),
            Block::Heading("More".into()),
            Block::Paragraph(LONG.into()),
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
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_page_loads_keep_the_summary() {
        let (ui, app, dir) = start("offline");
        app.open_article(row_of(&app, "Story 0"));
        let id = ui.get_reader().id as i64;

        let offline = anyhow::Error::new(fetch::Unreachable("offline".into()));
        app.page_ready(id, Err(offline));
        assert!(!ui.get_reader_loading());
        assert!(ui.get_reader_note().contains("offline"));
        assert_eq!(kinds(&app), vec![BlockKind::Paragraph]);

        // A page with less text than the feed doesn't replace it.
        app.page_ready(id, Ok(vec![]));
        assert_eq!(kinds(&app), vec![BlockKind::Paragraph]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn late_results_for_another_article_are_ignored() {
        let (ui, app, dir) = start("stale");
        app.open_article(row_of(&app, "Story 0"));
        let teaser_id = ui.get_reader().id as i64;
        app.open_article(row_of(&app, "Story 1"));

        app.page_ready(teaser_id, Ok(vec![Block::Paragraph(LONG.into())]));
        app.picture_ready(teaser_id, "http://127.0.0.1:9/pic.png", None);
        assert_eq!(ui.get_reader().title, "Story 1");
        assert_eq!(app.reader_blocks.row_count(), 14, "Story 1 is untouched");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pictures_fill_in_or_disappear() {
        let (_ui, app, dir) = start("pictures");
        app.open_article(row_of(&app, "Story 1"));
        let id = app.reader_id.get().unwrap();
        let url = "http://127.0.0.1:9/pic.png";
        let last = app.reader_blocks.row_count() - 1;
        assert_eq!(
            app.reader_blocks.row_data(last).unwrap().image.size().width,
            0
        );

        app.picture_ready(id, url, Some(thumbs::Picture::new(4, 3)));
        assert_eq!(
            app.reader_blocks.row_data(last).unwrap().image.size().width,
            4
        );

        app.picture_ready(id, url, None);
        assert_eq!(app.reader_blocks.row_count(), last, "broken image removed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn toggling_read_from_the_reader() {
        let (ui, app, dir) = start("toggle");
        // "All" view, so the row stays in the list.
        ui.set_unread_only(false);
        app.reload_articles();
        let row = row_of(&app, "Story 1");
        app.open_article(row);
        let id = app.reader_id.get().unwrap();

        app.reader_toggle_read();
        assert!(!ui.get_reader().read);
        assert!(!app.store.reader_article(id).unwrap().unwrap().read);
        assert!(!app.articles.row_data(row).unwrap().read);

        app.reader_toggle_read();
        assert!(ui.get_reader().read);
        assert!(app.store.reader_article(id).unwrap().unwrap().read);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn escape_and_back_button_close_the_reader() {
        let (ui, app, dir) = start("close");
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
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reader_toolbar_buttons_are_wired() {
        let (ui, app, dir) = start("toolbar");
        ui.set_unread_only(false);
        app.reload_articles();
        app.open_article(row_of(&app, "Story 1"));
        let id = app.reader_id.get().unwrap();
        let mark = ElementHandle::find_by_accessible_label(&ui, "Mark as unread")
            .next()
            .expect("mark as unread button");
        mark.invoke_accessible_default_action();
        assert!(!app.store.reader_article(id).unwrap().unwrap().read);
        // The label follows the state.
        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Mark as read")
                .next()
                .is_some()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn host_names_for_the_reader() {
        assert_eq!(host_of("https://www.example.com/a/b"), "example.com");
        assert_eq!(host_of("https://blog.example.org/"), "blog.example.org");
        assert_eq!(host_of("not a link"), "");
    }
}
