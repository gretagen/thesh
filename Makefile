CARGO ?= cargo

DESTDIR ?=
PREFIX  ?= $(HOME)/.local

all: debug

debug:
	$(CARGO) build --manifest-path rust/Cargo.toml

release:
	$(CARGO) build --release --manifest-path rust/Cargo.toml

# Unit + integration suites (golden vectors, interactive pty sessions).
test:
	$(CARGO) test --manifest-path rust/Cargo.toml

# Same, with per-case output for the parity/interactive runners.
test-verbose:
	$(CARGO) test --manifest-path rust/Cargo.toml -- --nocapture

install: release
	install -Dm755 rust/target/release/thesh $(DESTDIR)$(PREFIX)/bin/thesh
	@echo "installed thesh -> $(DESTDIR)$(PREFIX)/bin/thesh"
	@echo "config: cp theshrc.sample ~/.theshrc   (system-wide: /etc/theshrc)"

clean:
	$(CARGO) clean --manifest-path rust/Cargo.toml

.PHONY: all debug release test test-verbose install clean
