// Hide the console window on Windows release builds (future targets).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod db;
mod fetch;
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
        if let Err(e) = open::that_detached(item.link.as_str()) {
            self.toast(format!("Couldn't open the browser: {e}"));
        }
        if !item.read {
            self.set_read(row, true);
        }
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

fn main() -> Result<()> {
    let paths = Paths::new()?;
    let store =
        Store::open(&paths.db).with_context(|| format!("opening {}", paths.db.display()))?;
    let ui = AppWindow::new()?;
    // Lets the desktop match the window to aggrega.desktop (taskbar icon/name).
    // Needs the platform created by `AppWindow::new`, and must precede `run`.
    slint::set_xdg_app_id("aggrega")?;

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
    ui.set_articles(ModelRc::from(articles.clone()));
    ui.set_feeds(ModelRc::from(feeds.clone()));

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
