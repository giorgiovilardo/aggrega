---
name: precheck
description: Run Aggrega's pre-push checks (rustfmt, clippy with warnings as errors, and the full test suite). Use before committing, pushing or opening a PR, or when asked to "check", "lint" or "make sure CI passes".
---

# Precheck

CI (`.github/workflows/ci.yml`) only runs `cargo test --locked`, so formatting and clippy problems are not caught remotely. Run all three locally, in this order, from the repo root:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

- `cargo fmt --check` fails: run `cargo fmt`, then show the user the diff it produced.
- `clippy` fails: fix the warnings in the code. Don't add `#[allow(...)]` unless the lint is a genuine false positive, and say so if you do.
- `--locked` fails because `Cargo.lock` is out of date: the change edited `Cargo.toml` without updating the lock file. Run `cargo update -p <crate>` for the crate that changed rather than a blanket `cargo update`.
- UI test failures: the tests in `src/app.rs` use Slint's headless backend. They need no display but do need one system font. If every UI test fails with a font or backend error, the machine is missing fonts (Arch: `ttf-dejavu`), not the code.
- If `slint` was upgraded, `i-slint-backend-testing` in `[dev-dependencies]` must be pinned to the exact same version.

Report the result of each step. Don't claim the checks pass unless all three exited successfully.
