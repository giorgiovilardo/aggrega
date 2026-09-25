# Aggrega

**All your feeds, one edition.** Aggrega is a fast, lightweight desktop feed reader with an editorial, magazine-style design. It runs entirely on your machine, with no account and no cloud sync. It's written in Rust with a [Slint](https://slint.dev) UI.

![Aggrega, light theme](docs/screenshots/light.png)

| Dark theme | Adding a source |
|---|---|
| ![Dark theme](docs/screenshots/dark.png) | ![Add source dialog](docs/screenshots/add-source.png) |

## Features

- **No login, local only.** Your subscriptions and read state live in a single SQLite file on your disk.
- **No telemetry, tracking or analytics.** There is no telemetry, tracking or analytics code in Aggrega. The only network traffic is fetching the feeds, thumbnails and articles you asked for; nothing is reported anywhere.
- **Unlimited sources.** RSS 0.9x/1.0/2.0, Atom and JSON Feed are all supported.
- **Smart "Add source".** Paste a feed URL, or just a website such as `theverge.com`. Aggrega finds the feed from the page's `<link rel="alternate">` tags or from common paths like `/feed` and `/rss.xml`.
- **Newest posts first.** The list merges every source into one feed sorted by date. You can also focus on a single source from the sidebar.
- **Works offline.** Everything you've downloaded, including stories and thumbnails, stays readable without a connection. When no source can be reached, Aggrega says it's offline instead of flagging your sources as broken, and it retries on the next refresh.
- **Refreshes on launch and on demand.** Cached articles show up immediately, and fresh ones are fetched in parallel in the background. Aggrega sends conditional requests (ETag / Last-Modified), so feeds that haven't changed cost almost nothing.
- **Unread highlighting.** Unread stories get a red dot before the kicker and a black-weight headline. Read ones drop to a lighter grey headline with a dimmed photo. Each source shows its unread count. The **Unread** tab is the default view; stories you read there slide out of the list right away. *All* shows everything, and *Mark all as read* clears the view.
- **Reader view.** Clicking an article opens it inside Aggrega as a clean, single-column page with its headline, lead photo, text, quotes, lists and pictures, and marks it as read. When the feed only carries a summary, Aggrega fetches the article's page and pulls out the story, leaving menus, share bars and comments behind. The result is saved, so the story opens instantly next time, even offline. **Read on …** or `O` opens the original in your browser. Hover a story in the list to toggle read/unread without opening it.
- **Thumbnails.** Thumbnails are pulled from the feed's media tags or the article's first image. They are shrunk and cached on disk, and they fade in without blocking the UI.
- **Editorial design.** Newsprint paper and ink colours, heavy serif headlines (Source Serif 4), small-caps kickers in each source's colour (Libre Franklin), hairline rules, and a lead story at the top of the list. The fonts are bundled, so the app looks the same on every machine.
- **Opening sequence.** At launch the masthead is set in about 2.8 seconds and then lifts like a curtain. Click or press any key to skip it.
- **Fits the window.** The layout adapts to the window width. Narrow windows get a slimmer sidebar, tighter margins, smaller headlines and thumbnails, and wide ones cap the column at a comfortable reading width.
- **Custom title bar.** The window has no system frame. Drag the header or the sidebar masthead to move it, double-click to maximise, and resize from any edge.
- **Light and dark themes.** Aggrega follows your desktop by default. The toggle cross-fades the whole UI and remembers your choice.
- **Fluid animations.** Rows glide in, thumbnails zoom gently on hover, headlines turn red, the tab underline and theme switch use spring easing, and a red progress sweep runs while refreshing.

### Keyboard shortcuts

| Keys | Action |
|---|---|
| `F5` / `Ctrl+R` | Refresh all sources |
| `Ctrl+N` | Add a source |
| `Enter` | Confirm in the "Add source" dialog |
| `Esc` | Close dialogs, or close the reader |
| `Backspace` / `←` | Back to the list (reader) |
| `↑` / `↓`, `Space` / `Shift+Space`, `PgUp` / `PgDn`, `Home` / `End` | Scroll the article (reader) |
| `O` | Open the original page in your browser (reader) |
| `M` | Toggle read / unread (reader) |
| any key / click | Skip the opening animation |

## Quick start (Arch Linux)

```bash
sudo pacman -S --needed base-devel rustup fontconfig freetype2 libxkbcommon wayland libglvnd
rustup default stable
cargo run --release
```

To install it as a proper package, with a launcher entry and icon:

```bash
cd packaging/arch && makepkg -si
```

[docs/BUILDING.md](docs/BUILDING.md) has the full build, run, test and packaging guide, plus troubleshooting.

## Where is my data?

| What | Path |
|---|---|
| Subscriptions, articles, read state, settings | `~/.local/share/aggrega/aggrega.db` |
| Thumbnail cache (safe to delete) | `~/.cache/aggrega/thumbs/` |

Both paths honour `XDG_DATA_HOME` / `XDG_CACHE_HOME`. To start fresh, quit Aggrega and delete the two directories. Read articles older than 90 days are pruned automatically.

## Documentation

- [docs/BUILDING.md](docs/BUILDING.md): prerequisites, dev and release builds, tests, packaging, troubleshooting
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): how the code is organised, the threading model, the storage schema and performance notes

## Project layout

```
aggrega/
├── Cargo.toml          # dependencies + release profile (LTO, strip)
├── build.rs            # compiles the .slint UI into Rust at build time
├── src/
│   ├── main.rs         # app state, UI wiring, background jobs
│   ├── fetch.rs        # HTTP, feed parsing, feed discovery, parallel fetch
│   ├── reader.rs       # reader view: HTML → text blocks, article extraction
│   ├── db.rs           # SQLite storage
│   ├── thumbs.rs       # thumbnail download/resize/disk cache, reader pictures
│   └── text.rs         # HTML→text, relative dates, avatar colours
├── ui/
│   ├── app.slint       # main window
│   ├── theme.slint     # fonts, design tokens (light/dark), icon set
│   ├── widgets.slint   # buttons, tabs, switch, window controls…
│   ├── sidebar.slint   # sources list
│   ├── article-card.slint
│   ├── reader.slint    # in-app reader view
│   ├── dialogs.slint   # add-source + remove-confirmation modals
│   └── icons/          # SVG icons
├── assets/
│   ├── aggrega.svg     # app icon
│   └── fonts/          # bundled OFL fonts (see FONTS.md)
├── packaging/          # .desktop file + Arch PKGBUILD
└── docs/
```

## Roadmap ideas

- More build targets: [Windows (#3)](https://github.com/moebiusmania/aggrega/issues/3), [macOS (#4)](https://github.com/moebiusmania/aggrega/issues/4), [AppImage (#5)](https://github.com/moebiusmania/aggrega/issues/5), [Flatpak (#6)](https://github.com/moebiusmania/aggrega/issues/6)
- [OPML import/export (#7)](https://github.com/moebiusmania/aggrega/issues/7)
- [Folders/categories (#8)](https://github.com/moebiusmania/aggrega/issues/8), [search (#9)](https://github.com/moebiusmania/aggrega/issues/9)
- [Periodic background refresh (#10)](https://github.com/moebiusmania/aggrega/issues/10)
- [Next/previous story from the reader (#11)](https://github.com/moebiusmania/aggrega/issues/11)

## Licensing

Aggrega doesn't have a license yet. Choose one before you distribute it. Slint itself is available under GPLv3, a royalty-free license for desktop apps (which requires Slint attribution, such as an "About Slint" notice or badge), or a commercial license. Your choice for Aggrega decides which of these you can use. See <https://slint.dev/pricing>.
