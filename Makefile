# hostd: user-space install, no root required (except `make lowports`, which says so).

PREFIX    ?= $(HOME)/.local
BINDIR    ?= $(PREFIX)/bin
CONFDIR   ?= $(or $(XDG_CONFIG_HOME),$(HOME)/.config)
STATEDIR  ?= $(or $(XDG_STATE_HOME),$(HOME)/.local/state)/hostd
DINITDIR  ?= $(CONFDIR)/dinit.d
SUDO      ?= doas

CARGO     ?= cargo

.PHONY: build install uninstall lint fmt test lowports clean help

help:
	@echo "targets: build install uninstall lint fmt test lowports clean"

build:
	$(CARGO) build --release --locked

# Installs binaries, a starter config (never overwrites one you already have) and the
# user-level dinit service (rendered with the real install paths).
install: build
	install -Dm755 target/release/hostd    $(DESTDIR)$(BINDIR)/hostd
	install -Dm755 target/release/hostctl  $(DESTDIR)$(BINDIR)/hostctl
	install -d $(DESTDIR)$(STATEDIR)
	@if [ ! -e "$(DESTDIR)$(CONFDIR)/hostd/config.toml" ]; then \
	  install -Dm644 config.example.toml "$(DESTDIR)$(CONFDIR)/hostd/config.toml"; \
	  echo "wrote $(CONFDIR)/hostd/config.toml"; \
	else \
	  echo "kept existing $(CONFDIR)/hostd/config.toml"; \
	fi
	install -d $(DESTDIR)$(DINITDIR)
	sed -e 's|@BINDIR@|$(BINDIR)|g' -e 's|@STATEDIR@|$(STATEDIR)|g' \
	    dist/hostd.dinit.in > $(DESTDIR)$(DINITDIR)/hostd
	@echo
	@echo "Installed. Next:"
	@echo "  1. allow port 80 once:   make lowports      (or use a high port, see README)"
	@echo "  2. start it:             dinitctl --user enable hostd"

# Leaves your config and logs alone.
uninstall:
	-dinitctl --user disable hostd 2>/dev/null
	rm -f $(DESTDIR)$(BINDIR)/hostd $(DESTDIR)$(BINDIR)/hostctl
	rm -f $(DESTDIR)$(DINITDIR)/hostd
	@echo "left $(CONFDIR)/hostd and $(STATEDIR) in place; delete them by hand if unwanted"

lint:
	$(CARGO) clippy --all-targets --locked -- -D warnings
	$(CARGO) fmt --check

fmt:
	$(CARGO) fmt

test:
	$(CARGO) test --locked

# The one privileged step: let unprivileged users bind port 80 and up.
lowports:
	$(SUDO) install -Dm644 dist/99-hostd-lowports.conf /etc/sysctl.d/99-hostd-lowports.conf
	$(SUDO) sysctl --system >/dev/null
	@echo "unprivileged processes may now bind ports >= 80"

clean:
	$(CARGO) clean
