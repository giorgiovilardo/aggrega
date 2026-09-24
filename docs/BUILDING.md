# Building and running Aggrega

Arch Linux is the first supported target. Other platforms will follow in later iterations. The code is portable, so what's missing for them is mostly packaging.

## 1. Prerequisites (Arch Linux)

```bash
# Toolchain + native libraries used by the windowing/rendering stack
sudo pacman -S --needed base-devel rustup pkgconf \
    fontconfig freetype2 libxkbcommon wayland libglvnd \
    libx11 libxcursor libxi libxrandr

# Install the stable Rust toolchain (Rust 1.88 or newer is required)
rustup default stable
```

> If you'd rather not use `pacman` for Rust, the official installer works too:
> `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`.
> Then run `source ~/.cargo/env` or open a new shell. In fish, use `fish_add_path ~/.cargo/bin`.

What each native library is for:

| Package | Used for |
|---|---|
| `fontconfig`, `freetype2` | Finding and rasterising system fonts |
| `libxkbcommon` | Keyboard handling (Wayland and X11) |
| `wayland` | Native Wayland windows |
| `libx11`, `libxcursor`, `libxi`, `libxrandr` | X11 / XWayland fallback |
| `libglvnd` (plus your GPU's GL driver) | OpenGL for the GPU renderer |

SQLite is compiled into the binary (the `rusqlite` `bundled` feature), and TLS uses `rustls`. You don't need system `sqlite` or `openssl` packages.

## 2. Run in development

```bash
cargo run
```

The first build downloads and compiles all dependencies, which takes a few minutes. After that, rebuilds take seconds. The dev profile optimises dependencies (`opt-level = 2`), so animations stay smooth while you iterate, but it doesn't optimise Aggrega's own code, which keeps compiles quick.

`build.rs` compiles the `.slint` files at build time, so a Slint error shows up as a normal `cargo build` error with file and line.

### Faster UI iteration (optional)

- **VS Code / VSCodium:** install the *Slint* extension to get live preview, completion and diagnostics for `ui/*.slint`.
- **Standalone viewer:** `cargo install slint-viewer`, then run `slint-viewer --auto-reload ui/app.slint`. The UI renders with default data and doesn't call into the Rust backend.

## 3. Release build

```bash
cargo build --release
./target/release/aggrega
```

The release profile uses fat LTO, a single codegen unit, `panic = "abort"`, and strips symbols. It produces one self-contained binary of about 22 MB. Measured on the first test machine: the window appears in about 250 ms, and Aggrega's own heap is roughly 15 MB. Total RSS is higher because the GPU driver's shared libraries are mapped in.

A `Makefile` wraps the common commands:

```bash
make dev        # cargo run
make release    # optimised build
make run        # build + run the release binary
make test       # unit tests
make lint       # rustfmt check + clippy (warnings are errors)
make install    # installs binary, .desktop and icon into ~/.local (PREFIX=... to change)
make uninstall
make package    # builds an Arch package with makepkg
```

## 4. Tests and linting

```bash
cargo test                  # parser, storage, HTML/text helpers
cargo fmt --check
cargo clippy -- -D warnings
```

The tests don't touch the network or your real data. The storage test uses a temporary directory.

## 5. Install on Arch Linux

### Option A: as a pacman package (recommended)

```bash
cd packaging/arch
makepkg -si
```

This builds from the current checkout. It runs `cargo fetch --locked`, a release build and the tests, then installs:

- `/usr/bin/aggrega`
- `/usr/share/applications/aggrega.desktop`, so Aggrega appears in your app launcher
- `/usr/share/icons/hicolor/scalable/apps/aggrega.svg`

To upgrade after pulling changes, bump `pkgrel` (or `pkgver`) in the PKGBUILD and run `makepkg -si` again. To remove it, run `sudo pacman -R aggrega`.

### Option B: user-local install without pacman

```bash
make install                  # to ~/.local/bin, ~/.local/share/...
make uninstall
```

Make sure `~/.local/bin` is on your `PATH`.

### Option C: prebuilt binary from CI

Pushing a version tag triggers the `Release build` GitHub Actions workflow (`.github/workflows/release.yml`):

```bash
git tag v0.3.0
git push origin v0.3.0
```

The workflow builds inside an `archlinux` container and uses the tag as the app version: `v0.3.0` becomes `0.3.0`, and the sidebar shows it under the logo. When the run finishes, download `aggrega-<version>-x86_64.tar.gz` from the run's **Artifacts** section. It contains the binary, the `.desktop` file and the icon. Local builds show the version from `Cargo.toml`.

## 6. Data and reset

| What | Location |
|---|---|
| Database | `${XDG_DATA_HOME:-~/.local/share}/aggrega/aggrega.db` |
| Thumbnails | `${XDG_CACHE_HOME:-~/.cache}/aggrega/thumbs/` |

To try Aggrega with a throwaway profile without touching your real data:

```bash
XDG_DATA_HOME=/tmp/agg/data XDG_CACHE_HOME=/tmp/agg/cache cargo run
```

## 7. Troubleshooting

**The window opens under XWayland instead of native Wayland (or the reverse).**
Aggrega uses Wayland whenever `WAYLAND_DISPLAY` is set. To force X11, run:

```bash
env -u WAYLAND_DISPLAY aggrega
```

**Black window, or crashes at startup mentioning GL/EGL.**
Make sure your GPU's OpenGL driver is installed: `mesa` for AMD/Intel, or `nvidia-utils` for NVIDIA. Check with `glxinfo -B` from `mesa-utils`.

**Text looks wrong or boxes appear instead of glyphs.**
Install a basic font set, for example `sudo pacman -S noto-fonts noto-fonts-emoji`.

**A source shows a red ⓘ icon in the sidebar.**
Its last refresh failed. The site might be down, or the feed might have moved. The error is stored in the `last_error` column of the `feeds` table. Aggrega retries automatically on the next refresh.

**Clicking an article does nothing.**
Aggrega opens links with `xdg-open`. Check that a default browser is set: `xdg-settings get default-web-browser`.

**`error: rustc 1.xx is not supported`.**
Update the toolchain with `rustup update stable`. Aggrega uses Rust 2024 edition features and needs Rust 1.88 or newer.
