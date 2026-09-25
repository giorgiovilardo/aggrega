# Contributing to Aggrega

Thanks for your interest in Aggrega! Bug reports, ideas and pull requests are all welcome.

## Reporting bugs and suggesting features

Open an [issue](https://github.com/moebiusmania/aggrega/issues). For bugs, include:

- what you did, what you expected, and what happened instead
- your distribution and desktop (Wayland or X11)
- the Aggrega version or commit
- the feed URL, if the problem is with a specific source

Check the [roadmap](README.md#roadmap-ideas) and existing issues first; your idea may already be tracked.

## Making changes

1. Fork the repository and create a branch from `main`.
2. Set up the toolchain and native libraries as described in [docs/BUILDING.md](docs/BUILDING.md).
3. Make your change. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains how the code is organised.
4. Before pushing, make sure these pass:

   ```bash
   cargo fmt --check
   cargo clippy -- -D warnings
   cargo test
   ```

   (`make lint` and `make test` run the same commands.)
5. Open a pull request against `main` describing what changed and why. Screenshots help for UI changes, ideally in both light and dark themes.

## Guidelines

- **Keep it local and private.** Aggrega has no accounts, no cloud sync and no telemetry. Changes that add any of these won't be accepted.
- **Keep it light.** Startup time, memory use and binary size matter. Mention it in the PR if a change adds a dependency or affects performance.
- **Add tests** for parsing, storage and reader logic. UI behaviour can be tested with the headless Slint backend (see the UI tests in `src/main.rs`).
- **Small, focused PRs** are easier to review than large ones.

## Review

All pull requests need an approving review from a code owner (see [.github/CODEOWNERS](.github/CODEOWNERS)) and a passing CI run before they can be merged. Branches are deleted automatically after merge.

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
