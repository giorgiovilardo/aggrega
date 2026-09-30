---
name: run-app
description: Launch Aggrega locally against a throwaway profile, optionally seeded with feeds, and take screenshots (for example to refresh docs/screenshots). Use when asked to run, start, try or screenshot the app, or to check a UI change in the real window.
---

# Run Aggrega

Aggrega is a GUI app (winit + OpenGL). It needs a display: check `$WAYLAND_DISPLAY` or `$DISPLAY`. If neither is set, say so and don't try to launch it. For layout or behaviour checks without a display, use the headless UI tests in `src/app.rs` instead (see the `precheck` skill).

## Never touch the user's real data

Always point the app at a temporary profile via `AGGREGA_HOME`, which holds the database (`aggrega.db`) and thumbnails (`thumbs/`) and overrides the real locations on every platform. The real database is `~/.local/share/aggrega/aggrega.db` on Linux and `~/Library/Application Support/aggrega/aggrega.db` on macOS.

```bash
PROFILE=$(mktemp -d)   # prefer the session scratchpad directory if one exists
AGGREGA_HOME=$PROFILE cargo run
```

Use `cargo run --release` for screenshots or performance checks. The dev build is fine for everything else.

## Seeding feeds

A fresh profile has no sources. To seed some without clicking through the UI:

1. Launch once with the temp profile so it creates `$PROFILE/aggrega.db`, then quit.
2. Insert feeds. Refresh never updates `title`, so give each a real name:
   ```bash
   sqlite3 "$PROFILE/aggrega.db" "INSERT INTO feeds (url, title, added_at) VALUES
     ('https://www.theverge.com/rss/index.xml', 'The Verge', strftime('%s','now')),
     ('https://feeds.arstechnica.com/arstechnica/index', 'Ars Technica', strftime('%s','now'));"
   ```
3. Relaunch. The app refreshes on start and fetches them.

To force a theme before launch: `INSERT OR REPLACE INTO settings (key, value) VALUES ('theme', 'dark')` (or `'light'`).

## Screenshots

- The opening animation takes about 2.8 s. Wait for it to finish, plus the refresh and thumbnail fade-in, before capturing (roughly 6–8 s after launch), or press a key in the window to skip it.
- Wayland: `grim` (whole output) or `grim -g "$(slurp)"`. X11: `import -window root` (ImageMagick). The window is frameless, so window-only capture tools may crop oddly. Crop the full-screen shot instead.
- `docs/screenshots/` holds `light.png`, `dark.png`, `add-source.png` and `intro.png`. When replacing them, keep the same filenames because the README links to them, and show the user the new images before committing.

When finished, stop the app and delete the temp profile.
