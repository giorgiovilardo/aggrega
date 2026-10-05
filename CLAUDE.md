# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Aggrega is a local-first desktop RSS/Atom/JSON Feed reader: a single Rust binary with a Slint UI, SQLite storage and no async runtime. `docs/ARCHITECTURE.md` (threading, data flow, schema, design system) and `docs/BUILDING.md` (prerequisites, packaging, troubleshooting) are the detailed references. Keep them and the README in sync when behaviour changes.

## Commands

```bash
cargo run                         # dev build (deps optimised, own crate not)
cargo build --release             # fat LTO, stripped, panic=abort
cargo test                        # all tests, incl. headless UI tests
cargo test reader::               # one module's tests
cargo test layout_adapts_to_window_width   # a single test by name
cargo fmt --check && cargo clippy -- -D warnings   # lint (= make lint)
AGGREGA_HOME=/tmp/agg cargo run   # throwaway profile (db + thumbs in one dir, any OS)
```

CI (`.github/workflows/ci.yml`) runs `cargo test --locked` and `make lint`'s checks as parallel jobs in an `archlinux` container on pushes to main and on PRs. It pins the Rust toolchain (`RUST_TOOLCHAIN`) so the cache survives new stable releases, and only main saves the cache. Bump the pin on purpose. Pushing a `vX.Y.Z` tag triggers `release.yml`, which rewrites the `Cargo.toml` version from the tag and uploads a tarball artifact.

## Architecture

- **UI is compiled at build time.** `build.rs` compiles `ui/app.slint` (which imports the other `.slint` files) into Rust, pulled in via `slint::include_modules!()` in `src/main.rs`. Slint errors surface as `cargo build` errors. Debug info for elements is only emitted in the `debug` profile, and the UI tests need it to find elements.
- **`src/app.rs` owns everything UI-side.** `App` (models, caches, refresh state) lives in a `thread_local!` and is reached with `with_app(|app| …)`. `src/main.rs` only wires each `AppWindow` callback to a `pub` `App` method. `AppWindow`'s properties and callbacks (declared in `ui/app.slint`) are the Rust↔UI contract; adding a UI feature usually means touching `ui/app.slint`, `app.rs` and the wiring in `main.rs`.
- **Threading rule:** Slint objects never leave the UI thread. I/O runs on short-lived `std::thread` workers (`pool::par_for_each`, a scoped pool of 4–8), which send only `Send` data back through `slint::invoke_from_event_loop(move || with_app(...))`. Async results carry the article/feed id and are dropped if the UI has moved on.
- **SQLite:** the UI thread keeps one `Store` connection, and each worker opens its own (WAL + 5 s busy timeout). Refresh results are written in one transaction (`Store::apply_refresh`). Schema changes go in `Store::migrate` as a new `if version < N` block, bumping `SCHEMA_VERSION`.
- **Article bodies aren't HTML.** `reader.rs` converts feed/page HTML into `Block`s, stored in `articles.body` in a compact line format (`reader::encode`/`decode`). Short bodies trigger a page fetch plus `reader::extract_article`.
- **Module split:** `html.rs` (the only HTML tokenizer, `attr`, `to_text`), `feed.rs` (feed data types, feed-rs parsing, thumbnails, feed discovery; no network), `fetch.rs` (ureq HTTP only), `pool.rs` (scoped thread pool), `db.rs` (`Store`), `reader.rs` (tokens → blocks, content extraction), `thumbs.rs` (image download/resize/disk cache), `text.rs` (truncation, relative dates, source colours). New HTML scanning goes through `html::tokenize`/`open_tags`, not substring searches.
- **Theming:** every colour is a token in the `Theme` global (`ui/theme.slint`) bound to `Theme.dark`, so don't hardcode colours in components. Fonts are bundled in `assets/fonts` and mapped by role in the `Fonts` global.

## Tests

- Unit tests sit in `#[cfg(test)] mod tests` at the bottom of each module. Nothing touches the network or real user data; storage tests use a temp directory.
- Sample feeds (RSS 0.91–2.0, Atom, JSON Feed) live in `tests/fixtures/feeds/` (copied from feed-rs) for `feed`'s tests, compiled in via its `FIXTURES` list. `parses_every_fixture` runs them all through `parse`.
- UI tests in `src/app.rs` drive the real `AppWindow` via `i-slint-backend-testing` (no display needed). `start(name)` builds a window over a temp DB, then tests use `mock_elapsed_time` to advance animations and `dispatch_event` for keys. Links point at `127.0.0.1:9` so page fetches fail fast.
- `i-slint-backend-testing` is pinned to the exact `slint` version (`=1.18.1`). Bump both together.
- CI installs `ttf-dejavu` because headless font lookup needs at least one system font.

## Constraints

- Keep the dependency set lean: no Tokio/reqwest, Slint only with `backend-winit` + `renderer-femtovg`, and `image` limited to jpeg/png/gif/webp decoders.
- No telemetry or network traffic beyond fetching the user's feeds, thumbnails and articles (a stated product guarantee).
- Idle CPU must stay at zero, so continuous animations must sit behind a condition (see `animation-tick()` gated on `refreshing`).

## Project skills

`.claude/skills/` has `precheck` (fmt + clippy + tests before pushing), `run-app` (launch against a throwaway profile, seed feeds, take screenshots) and `release` (tag, watch the Release build; the tag alone sets the version).
