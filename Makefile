# Convenience targets. Plain `cargo` works just as well; see docs/BUILDING.md.
PREFIX ?= $(HOME)/.local
BIN     = target/release/aggrega

.PHONY: run dev release test lint install uninstall package clean

run: release
	./$(BIN)

dev:
	cargo run

release:
	cargo build --release

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy -- -D warnings

install: release
	install -Dm755 $(BIN) $(PREFIX)/bin/aggrega
	install -Dm644 packaging/aggrega.desktop $(PREFIX)/share/applications/aggrega.desktop
	install -Dm644 assets/aggrega.svg $(PREFIX)/share/icons/hicolor/scalable/apps/aggrega.svg

uninstall:
	rm -f $(PREFIX)/bin/aggrega \
	      $(PREFIX)/share/applications/aggrega.desktop \
	      $(PREFIX)/share/icons/hicolor/scalable/apps/aggrega.svg

package:
	cd packaging/arch && makepkg -f

clean:
	cargo clean
