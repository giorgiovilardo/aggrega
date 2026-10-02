// Hide the console window on Windows release builds (future targets).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod db;
mod feed;
mod fetch;
mod html;
mod pool;
mod reader;
mod text;
mod thumbs;

use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use slint::ComponentHandle;

use app::{App, Paths, with_app};
use db::Store;

slint::include_modules!();

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
    ui.set_repository(env!("CARGO_PKG_REPOSITORY").into());

    let app = App::install(ui, store, paths)?;

    ui.on_refresh(|| with_app(|a| a.refresh()));
    ui.on_open_article(|row| with_app(|a| a.open_article(row as usize)));
    ui.on_toggle_read(|row| with_app(|a| a.toggle_read(row as usize)));
    ui.on_filter_changed(|| with_app(|a| a.reload_all()));
    ui.on_mark_all_read(|| with_app(|a| a.mark_all_read()));
    ui.on_add_feed(|url| with_app(|a| a.add_feed(url)));
    ui.on_remove_feed(|id| with_app(|a| a.remove_feed(id)));
    ui.on_close_reader(|| with_app(|a| a.close_reader()));
    ui.on_open_original(|| with_app(|a| a.open_original()));
    ui.on_reader_toggle_read(|| with_app(|a| a.reader_toggle_read()));
    ui.on_open_link(|url| with_app(|a| a.open_link(url)));
    ui.on_open_data_folder(|thumbs| with_app(|a| a.open_data_folder(thumbs)));
    ui.on_theme_changed(|dark| with_app(|a| a.save_theme(dark)));

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
    // Lets release builds check the compiled-in version against the tag.
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("aggrega {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

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
