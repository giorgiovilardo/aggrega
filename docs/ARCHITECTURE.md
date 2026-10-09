# Architecture

Aggrega is a single native binary with no runtime services. The UI is declared in Slint markup, compiled to Rust at build time, and driven by a small amount of Rust state on the UI thread. All I/O runs on short-lived worker threads.

```
                ┌──────────────────────────── UI thread ─────────────────────────────┐
  user input ──▶│ Slint UI (ui/*.slint)  ──callbacks──▶  App (src/app.rs)             │
                │        ▲                                 │  owns Store (SQLite conn) │
                │        └──── VecModel<ArticleItem/FeedItem> ◀┘  + models, caches     │
                └──────────────────────────────▲────────────────────────────────────┘
                                               │ slint::invoke_from_event_loop
          ┌────────────────────────────────────┴───────────────────────────────────┐
          │ worker threads (std::thread, scoped pools of 6–8)                       │
          │  • refresh: fetch::fetch_all → Store::apply_refresh (own SQLite conn)  │
          │  • add source: fetch::subscribe (discovery) → Store::add_feed          │
          │  • thumbnails: thumbs::load (disk cache or download+resize)            │
          │  • reader: fetch::fetch_page → reader::extract_article, pictures       │
          └────────────────────────────────────────────────────────────────────────┘
```

## Modules

| File | Responsibility |
|---|---|
| `src/main.rs` | Creates the window, wires every Slint callback to an `App` method, and runs the event loop |
| `src/app.rs` | `App`, the UI-thread state (models, thumbnail cache, refresh and reader state), every action the UI triggers, and the background work those actions start. Also holds the headless UI tests |
| `src/fetch.rs` | HTTP only, via `ureq` (blocking, rustls, gzip): conditional feed fetches, subscribing (with feed discovery), article pages and images |
| `src/feed.rs` | Feeds as plain data (`FeedJob`, `Fetched`, `NewArticle`), plus everything done offline with a download: parsing with `feed-rs`, picking each entry's thumbnail, and finding feed links in an HTML page |
| `src/html.rs` | The one HTML tokenizer (forgiving tag soup, never fails) and helpers on top of it: attributes, element extents, tags by name, and HTML → plain text |
| `src/pool.rs` | `par_map` / `par_for_each`, a tiny scoped thread pool for short blocking jobs |
| `src/opml.rs` | OPML as plain data: `parse` reads the feeds out of any reader's export (nested folders flattened, duplicates dropped, via `html::tokenize`) and `write` produces an OPML 2.0 document. The import itself (subscribing in parallel, then storing) lives in `app.rs` |
| `src/db.rs` | `Store`: schema and migrations, queries, and transactional refresh writes. For sync, `snapshot_to` (`VACUUM INTO`) and `replace_with` (SQLite's backup API, in one step, keeping the local theme, refresh interval and language) |
| `src/sync.rs` | LAN sync, std networking only. A `Session` (alive only while the Sync tab is on screen) broadcasts a UDP beacon, collects the beacons of other copies, and serves database snapshots over TCP. `info` and `pull` are the client side |
| `src/thumbs.rs` | Downloads images, `resize_to_fill` to 264×184, keeps a JPEG disk cache and negative cache, and returns a `SharedPixelBuffer`. Also loads reader pictures, shrunk to fit the column (not cached) |
| `src/reader.rs` | Reader view content: HTML → blocks (paragraph, heading, quote, bullet, code, image), main-content extraction from full web pages, and the compact line format blocks are stored in |
| `src/i18n.rs` | Interface language (English or Italian, per thread like Slint's): `tr!`/`trn!` look Rust-built strings up in the same `.po` catalog Slint bundles, plus localised dates. Its tests check every marked string has a translation |
| `src/text.rs` | Whitespace collapsing, truncation, relative times ("5m ago"), avatar letter and colour |

Dependencies point one way: `html` and `text` are leaves, `reader` and `feed` build on them, `fetch` adds HTTP on top of `feed`, and `app` uses everything. `db` needs only `feed`'s data types, plus `fetch::is_unreachable` to tell an offline refresh from a broken source.
| `ui/theme.slint` | `Theme` global with every colour token, switched by `Theme.dark`, plus the `Icons` global |
| `ui/app.slint` | `AppWindow`: responsive layout, header, list, toast, keyboard shortcuts, and the public API used from Rust |
| `ui/settings.slint` | `SettingsDialog`: a large modal with a vertical tab rail (Import & export, Sync, Updates, About) and the selected tab's pane, opened from the gear in the sidebar footer or `Ctrl+,`. About shows the version, copyright, license, project links, this profile's data paths and the third-party credits, including the `AboutSlint` widget the Slint Royalty-free License requires |
| `ui/reader.slint` | `ReaderView`: toolbar, headline, lead photo and one `BlockView` per content block, in a scrollable column |

## Threading model

- **Slint objects stay on the UI thread.** `App` lives in a `thread_local!`. A worker never touches the UI directly. It posts a closure with `slint::invoke_from_event_loop`, and that closure calls `with_app(|app| …)` on the UI thread.
- **Only `Send` data crosses threads:** plain Rust structs, `String`s, and `SharedPixelBuffer`s. Thumbnails are decoded and resized on workers, so the UI thread only wraps the finished pixel buffer in a `slint::Image`.
- **SQLite:** the UI thread keeps one connection for reads and small writes, such as marking an article read. Workers open their own connection. WAL mode plus a 5 s busy timeout let reads and writes run concurrently. A refresh writes all results in a single transaction.
- **No async runtime.** Blocking I/O on a handful of threads is simpler and lighter than pulling in Tokio for a desktop app that fetches a few dozen URLs at a time.

## Data flow

**Startup:** the app opens the DB, applies the saved theme, and loads cached feeds and articles, so content shows immediately. Then it starts a background refresh.

**Refresh:** the app reads a `FeedJob` (id, url, etag, last_modified) for each feed. Up to 8 threads fetch them in parallel with `If-None-Match` / `If-Modified-Since` headers. A `304` response is recorded without parsing. New entries are inserted with `ON CONFLICT(feed_id, guid) DO NOTHING`, so an existing article never shows up as new again. Per-feed errors go into `feeds.last_error`, and the sidebar shows them as a red icon. When the refresh ends, the app reloads the models and shows a toast.

**Background refresh:** `App` owns a repeating `slint::Timer` (`auto_refresh`) set to `settings.refresh_interval` minutes (default 20, stepped between 5 and 120 in Settings › Updates). It restarts whenever a refresh ends, so the interval counts from the last refresh, manual or not. When it fires it starts an ordinary refresh, unless one is already running or the last refresh found the app offline; then it waits for the user to refresh by hand. A background refresh only shows a toast when it found new articles.

**Sync:** `AppWindow.sync-open` is true only while Settings is open on the Sync tab, and its `changed` handler calls `App::sync_active`. Turning it on starts a `sync::Session`; turning it off drops it, which stops its threads within 250 ms, so nothing listens or announces itself otherwise.
- *Discovery:* every 1.5 s the session broadcasts `AGGREGA-SYNC 1 <instance> <tcp port> <device name>` to `255.255.255.255:47811` and listens on the same port. Its own beacons (same random instance id) are ignored, and peers not heard for 5 s are dropped. The peer list goes to the UI whenever it changes. If the port is taken, the session still announces itself and the user can type an address instead.
- *Serving:* a TCP listener on port 47812 (or any free port, which the beacon then carries) answers one line per connection: `INFO` gets `OK <schema> <feeds> <articles> <unread> <name>`; `PULL` gets `OK <length>` and a `VACUUM INTO` snapshot, so it's consistent even while a refresh writes.
- *Pulling:* picking a peer (or typing an address) runs `INFO` on a worker, then the UI asks for confirmation with both sides' counts. Once confirmed, a worker downloads the snapshot next to the database, and `Store::replace_with` checks it (`quick_check`, schema version, tables) and copies it over the live database with the backup API. The UI thread's connection sees the new data right away; the app closes the reader and reloads its models. Refreshes and imports don't start during a pull, and a pull doesn't start during them. Results carry a generation number, so a reply that arrives after the user left the tab is dropped. A pull already confirmed always finishes.

**Add source:** the app normalises the input (adding `https://` when it's missing) and fetches it. If the response parses as a feed, that feed is used. Otherwise the page is scanned for `<link rel="alternate" type="…rss|atom|feed+json">` tags, then common paths are tried (`/feed`, `/rss.xml`, `/atom.xml`, …). The first candidate that parses wins.

**Open article (reader view):** the app loads the article's stored `body`, decodes it into blocks, and shows the reader right away. It also sets `read = 1` and updates the list row in place. The list stays alive under the reader, so its scroll position survives the round trip.
- While refreshing, each entry's fullest HTML (`content:encoded` / Atom content, else the summary) is converted to blocks and stored in `articles.body`.
- If that text is short (under ~1,200 characters, so the feed only had a teaser) or missing (articles stored before schema v2), a worker downloads the page. `reader::extract_article` picks the richest `<article>`, `<main>`, `itemprop="articleBody"` or `*-content`/`*-body` region, skipping nav, asides, forms, and share/comment/related/newsletter blocks. With no such markup, it keeps the run of substantial paragraphs. The longer result wins, and it's saved back to `body`.
- Pictures (the lead image plus inline images, at most 24) download on workers and fill in as they arrive. Broken ones are removed.
- Every async result carries the article id and is dropped if the reader has moved on. Closing the reader frees its blocks and pictures.
- Video sites (YouTube, Vimeo) aren't scraped. **Read on …** opens the original with `open::that_detached(link)` (xdg-open on Linux).

## Storage schema (`PRAGMA user_version = 2`)

```sql
feeds(id, url UNIQUE, title, site_url, etag, last_modified, last_fetched, last_error, added_at)
articles(id, feed_id → feeds ON DELETE CASCADE, guid, title, link, snippet, image_url,
         published, fetched_at, read, body, UNIQUE(feed_id, guid))
settings(key PRIMARY KEY, value)            -- theme, language, last_refresh, refresh_interval
```

Indexes cover the hot queries: `articles(published DESC)`, `articles(feed_id, published DESC)` and `articles(feed_id, read)`. For the list, articles keep a plain-text snippet of at most 280 chars. For the reader, `body` holds the article as simplified text blocks, one per line (`p `, `h `, `q `, `l `, `c `, `i ` + text, with `\` and newlines escaped), not HTML, which keeps it compact. Version 2 added `body`; it is `NULL` for older rows, which makes the reader fetch the page. Read articles older than 90 days are pruned after each refresh.

To change the schema, add an `if version < 3 { … PRAGMA user_version = 3; }` block in `Store::migrate` and bump `SCHEMA_VERSION`.

## Performance notes

- **Virtualised list.** `ListView` only instantiates the cards that are on screen, and the list is capped at the 400 newest articles.
- **Small model updates.** Marking an article read changes one row with `set_row_data`. The sidebar model is diffed with `sync_model`, so only changed rows are touched.
- **No idle animation cost.** `animation-tick()` is only evaluated while a refresh is running, because it sits behind a `refreshing ? … : …` condition. When nothing changes, Slint doesn't redraw and CPU use is zero.
- **Thumbnail memory is bounded.** Decoded thumbnails are 264×184 RGB (about 145 KB each), cached only for rows in the current list, and dropped on reload. The disk cache stores small JPEGs. A `.none` marker remembers URLs that failed, so they aren't retried every launch.
- **Lean dependency set.** Slint uses only the winit backend and the femtovg OpenGL renderer; the Qt backend is off. Aggrega uses `ureq` instead of `reqwest`/Tokio, and `image` is built with just the jpeg, png, gif and webp decoders. The OPML file dialogs come from `rfd` with only its `xdg-portal` backend: on Linux it talks to the desktop portal through `libdbus`, loaded at run time (falling back to `zenity`), so there's no GTK and no async runtime.

## Design and theming

The look is editorial, in the style of a magazine front page. It uses warm newsprint paper (`#f7f4ee`) with ink (`#141312`) in light mode and the reverse in dark mode. Hairline rules replace cards, and a single red accent (`#d2232a`) marks unread stories, selection, the primary action and hover states. The variety comes from each source's kicker colour. These colours come from a muted editorial palette in `src/text.rs` (`TINTS`), and `Theme.source-color()` brightens them in dark mode.

**Typography.** The fonts are bundled from `assets/fonts` and imported in `ui/theme.slint`:
- Source Serif 4 Black for headlines and the wordmark
- Source Serif 4 Regular and Italic for standfirsts, status lines and empty states
- Libre Franklin for small-caps kickers, labels and buttons

Every static weight has its own family name, so the `Fonts` global maps roles to family names. The files come from Bunny Fonts; licences are listed in `assets/fonts/FONTS.md`.

**Theme switching.** Every colour comes from the `Theme` global. Each token is a binding on `Theme.dark`, and surfaces declare `animate background` (and similar), so toggling the theme cross-fades the whole UI. On first run, `Theme.dark` follows `Palette.color-scheme`, which is the desktop preference. After the user toggles it, the choice is saved in `settings.theme`.

**Translations.** Every user-facing string in `ui/` is wrapped in `@tr()`; `build.rs` bundles `translations/<lang>/LC_MESSAGES/aggrega.po` (no msgctxt) into the binary. Strings built in Rust use `tr!`/`trn!` from `src/i18n.rs`, which read the same `.po`, with Slint's placeholders (`{}`, `{0}`, `{n}` for plurals). The `LanguageSwitch` left of the theme switch opens a `PopupWindow` list; picking a language calls `App::change_language`, which runs `slint::select_bundled_translation`, re-renders the Rust strings in place (view title, status line, dates) and saves `settings.language`. English is the default, whatever the desktop locale. To add a language: add its `.po`, a `Lang` variant, its date names, and an entry in `LanguageSwitch.options`.

The landing page in `public/` is translated separately: English lives in `index.html`, and `public/i18n.js` swaps in the Italian for elements tagged `data-i18n` (or `-label`, `-alt`, `-content` for attributes) when the visitor's browser prefers it. It picks the first browser language the page has, falling back to English.

**Opening sequence.** `AppWindow.intro` animates linearly from 0 to 1 over 2.8 s. Each element derives its own eased progress from that value with `Motion.seg(t, from, to)` and `Motion.ease()`:
1. The rule draws.
2. The wordmark rises out of a clipped slot.
3. The tagline tightens its letter-spacing.
4. The dateline fades in.
5. The splash lifts like a curtain while the app content settles in.

The splash element is removed once the animation finishes, so it costs nothing afterwards.

**Responsive layout.** `AppWindow` exposes `compact` (window under 1000 px), `sidebar-width` (224 or 272 px), `column-width` and `reader-column-width`, which the UI tests read. Under 640 px of content width, the side margins shrink from 72 to 40 px, the header title and the tabs get smaller, and list thumbnails narrow from 180 to 120 px. The list column is capped at 980 px and the reader at 720 px. The window's minimum width is 720 px.

**Window chrome.** The window is frameless (`no-frame: true`). The header and the sidebar masthead contain a `WindowMoveArea`, which is placed underneath the visible content so buttons keep their clicks. Edge resizing is handled by Slint (`resize-border-width`). Minimise and maximise use the built-in `minimized`/`maximized` window properties.
