# Architecture

Aggrega is a single native binary with no runtime services. The UI is declared in Slint markup, compiled to Rust at build time, and driven by a small amount of Rust state on the UI thread. All I/O runs on short-lived worker threads.

```
                ┌──────────────────────────── UI thread ─────────────────────────────┐
  user input ──▶│ Slint UI (ui/*.slint)  ──callbacks──▶  App (src/main.rs)            │
                │        ▲                                 │  owns Store (SQLite conn) │
                │        └──── VecModel<ArticleItem/FeedItem> ◀┘  + models, caches     │
                └──────────────────────────────▲────────────────────────────────────┘
                                               │ slint::invoke_from_event_loop
          ┌────────────────────────────────────┴───────────────────────────────────┐
          │ worker threads (std::thread, scoped pools of 6–8)                       │
          │  • refresh: fetch::fetch_all → Store::apply_refresh (own SQLite conn)  │
          │  • add source: fetch::subscribe (discovery) → Store::add_feed          │
          │  • thumbnails: thumbs::load (disk cache or download+resize)            │
          └────────────────────────────────────────────────────────────────────────┘
```

## Modules

| File | Responsibility |
|---|---|
| `src/main.rs` | Creates the window, wires Slint callbacks, owns `App` (models, thumbnail cache, refresh state), and schedules background work |
| `src/fetch.rs` | HTTP via `ureq` (blocking, rustls, gzip). Parses feeds with `feed-rs`, discovers feeds in HTML, and extracts article images. Contains `par_for_each`, a tiny scoped thread pool |
| `src/db.rs` | `Store`: schema and migrations, queries, and transactional refresh writes |
| `src/thumbs.rs` | Downloads images, `resize_to_fill` to 264×184, keeps a JPEG disk cache and negative cache, and returns a `SharedPixelBuffer` |
| `src/text.rs` | HTML to plain text, truncation, relative times ("5m ago"), avatar letter and colour |
| `ui/theme.slint` | `Theme` global with every colour token, switched by `Theme.dark`, plus the `Icons` global |
| `ui/app.slint` | `AppWindow`: layout, header, list, toast, keyboard shortcuts, and the public API used from Rust |

## Threading model

- **Slint objects stay on the UI thread.** `App` lives in a `thread_local!`. A worker never touches the UI directly. It posts a closure with `slint::invoke_from_event_loop`, and that closure calls `with_app(|app| …)` on the UI thread.
- **Only `Send` data crosses threads:** plain Rust structs, `String`s, and `SharedPixelBuffer`s. Thumbnails are decoded and resized on workers, so the UI thread only wraps the finished pixel buffer in a `slint::Image`.
- **SQLite:** the UI thread keeps one connection for reads and small writes, such as marking an article read. Workers open their own connection. WAL mode plus a 5 s busy timeout let reads and writes run concurrently. A refresh writes all results in a single transaction.
- **No async runtime.** Blocking I/O on a handful of threads is simpler and lighter than pulling in Tokio for a desktop app that fetches a few dozen URLs at a time.

## Data flow

**Startup:** the app opens the DB, applies the saved theme, and loads cached feeds and articles, so content shows immediately. Then it starts a background refresh.

**Refresh:** the app reads a `FeedJob` (id, url, etag, last_modified) for each feed. Up to 8 threads fetch them in parallel with `If-None-Match` / `If-Modified-Since` headers. A `304` response is recorded without parsing. New entries are inserted with `ON CONFLICT(feed_id, guid) DO NOTHING`, so an existing article never shows up as new again. Per-feed errors go into `feeds.last_error`, and the sidebar shows them as a red icon. When the refresh ends, the app reloads the models and shows a toast.

**Add source:** the app normalises the input (adding `https://` when it's missing) and fetches it. If the response parses as a feed, that feed is used. Otherwise the page is scanned for `<link rel="alternate" type="…rss|atom|feed+json">` tags, then common paths are tried (`/feed`, `/rss.xml`, `/atom.xml`, …). The first candidate that parses wins.

**Open article:** the app calls `open::that_detached(link)` (xdg-open on Linux), sets `read = 1`, and updates the row in place. Updating in place keeps the scroll position and plays the "read" animation.

## Storage schema (`PRAGMA user_version = 1`)

```sql
feeds(id, url UNIQUE, title, site_url, etag, last_modified, last_fetched, last_error, added_at)
articles(id, feed_id → feeds ON DELETE CASCADE, guid, title, link, snippet, image_url,
         published, fetched_at, read, UNIQUE(feed_id, guid))
settings(key PRIMARY KEY, value)            -- theme, last_refresh
```

Indexes cover the hot queries: `articles(published DESC)`, `articles(feed_id, published DESC)` and `articles(feed_id, read)`. Articles store a plain-text snippet of at most 280 chars rather than full HTML, which keeps the DB small. Read articles older than 90 days are pruned after each refresh.

To change the schema, add an `if version < 2 { … PRAGMA user_version = 2; }` block in `Store::migrate` and bump `SCHEMA_VERSION`.

## Performance notes

- **Virtualised list.** `ListView` only instantiates the cards that are on screen, and the list is capped at the 400 newest articles.
- **Small model updates.** Marking an article read changes one row with `set_row_data`. The sidebar model is diffed with `sync_model`, so only changed rows are touched.
- **No idle animation cost.** `animation-tick()` is only evaluated while a refresh is running, because it sits behind a `refreshing ? … : …` condition. When nothing changes, Slint doesn't redraw and CPU use is zero.
- **Thumbnail memory is bounded.** Decoded thumbnails are 264×184 RGB (about 145 KB each), cached only for rows in the current list, and dropped on reload. The disk cache stores small JPEGs. A `.none` marker remembers URLs that failed, so they aren't retried every launch.
- **Lean dependency set.** Slint uses only the winit backend and the femtovg OpenGL renderer; the Qt backend is off. Aggrega uses `ureq` instead of `reqwest`/Tokio, and `image` is built with just the jpeg, png, gif and webp decoders.

## Design and theming

The look is editorial, in the style of a magazine front page. It uses warm newsprint paper (`#f7f4ee`) with ink (`#141312`) in light mode and the reverse in dark mode. Hairline rules replace cards, and a single red accent (`#d2232a`) marks unread stories, selection, the primary action and hover states. The variety comes from each source's kicker colour. These colours come from a muted editorial palette in `src/text.rs` (`TINTS`), and `Theme.source-color()` brightens them in dark mode.

**Typography.** The fonts are bundled from `assets/fonts` and imported in `ui/theme.slint`:
- Source Serif 4 Black for headlines and the wordmark
- Source Serif 4 Regular and Italic for standfirsts, status lines and empty states
- Libre Franklin for small-caps kickers, labels and buttons

Every static weight has its own family name, so the `Fonts` global maps roles to family names. The files come from Bunny Fonts; licences are listed in `assets/fonts/FONTS.md`.

**Theme switching.** Every colour comes from the `Theme` global. Each token is a binding on `Theme.dark`, and surfaces declare `animate background` (and similar), so toggling the theme cross-fades the whole UI. On first run, `Theme.dark` follows `Palette.color-scheme`, which is the desktop preference. After the user toggles it, the choice is saved in `settings.theme`.

**Opening sequence.** `AppWindow.intro` animates linearly from 0 to 1 over 2.8 s. Each element derives its own eased progress from that value with `Motion.seg(t, from, to)` and `Motion.ease()`:
1. The rule draws.
2. The wordmark rises out of a clipped slot.
3. The tagline tightens its letter-spacing.
4. The dateline fades in.
5. The splash lifts like a curtain while the app content settles in.

The splash element is removed once the animation finishes, so it costs nothing afterwards.

**Window chrome.** The window is frameless (`no-frame: true`). The header and the sidebar masthead contain a `WindowMoveArea`, which is placed underneath the visible content so buttons keep their clicks. Edge resizing is handled by Slint (`resize-border-width`). Minimise and maximise use the built-in `minimized`/`maximized` window properties.
