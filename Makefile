# Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
#
# SPDX-License-Identifier: Apache-2.0
#
# Swedish Embedded AB implements solutions for reproducible Rust build,
# test and release pipelines for its clients. If your team needs expertise
# in release engineering then you can procure our services by sending an
# email to info@swedishembedded.com.

CARGO   ?= cargo
PREFIX  ?= $(HOME)/.local
DESTDIR ?=
BINDIR  := $(DESTDIR)$(PREFIX)/bin
MANDIR  := $(DESTDIR)$(PREFIX)/share/man/man1

NPROC := $(shell nproc 2>/dev/null || echo 4)
# Builds and tests take at most 80% of the CPUs (at least one).
ifndef JOBS
JOBS := $(shell n=$$(( $(NPROC) * 4 / 5 )); [ $$n -ge 1 ] && echo $$n || echo 1)
endif
CARGO_FLAGS ?= --jobs $(JOBS)
export CARGO_BUILD_JOBS ?= $(JOBS)

VERSION := $(shell sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
TAG     := v$(VERSION)
ARCH    := $(shell uname -m)
DIST_NAME := rtop-$(VERSION)-$(ARCH)-linux
# Release bumps run these first; `make release/patch RELEASE_TESTS=` skips.
RELEASE_TESTS ?= check test

# A target is `make <concept>/<subconcept>/<action>`; `make help` prints
# every target from the `## ` doc lines below.

.DEFAULT_GOAL := build
.PHONY: all build build/debug build/release install uninstall run run/stats \
	test test/unit test/e2e test/performance test/soak test/fuzz check check/fmt check/clippy check/doc check/privacy check/audit \
	fmt shots package/dist release release/patch release/minor release/major \
	clean help _require-cargo-release

##@ Build

## all - same as build
all: build

## build - default: build/debug
build: build/debug

## build/debug - debug build of every crate and the rtop binary
build/debug:
	$(CARGO) build $(CARGO_FLAGS)

## build/release - optimised release build (target/release/rtop)
build/release:
	$(CARGO) build --release --locked $(CARGO_FLAGS) -p rtop

## install - build/release, then install rtop into PREFIX/bin (default ~/.local/bin; honours DESTDIR)
install: build/release
	install -d "$(BINDIR)"
	install -m 0755 target/release/rtop "$(BINDIR)/rtop"
	@if [ -f docs/rtop.1 ]; then install -d "$(MANDIR)" && install -m 0644 docs/rtop.1 "$(MANDIR)/rtop.1"; fi
	@echo "installed $(BINDIR)/rtop $(VERSION)"

## uninstall - remove rtop from PREFIX/bin
uninstall:
	rm -f "$(BINDIR)/rtop" "$(MANDIR)/rtop.1"

## run - run the interactive TUI from this checkout (ARGS="..." passes flags)
run:
	$(CARGO) run $(CARGO_FLAGS) -p rtop -- $(ARGS)

## run/stats - print the telemetry graph as YAML
run/stats:
	$(CARGO) run $(CARGO_FLAGS) -p rtop -- --stats $(ARGS)

##@ Test

## test - default: test/unit and test/e2e
test: test/unit test/e2e

## test/unit - unit, property and UI snapshot tests of every crate
test/unit:
	$(CARGO) test $(CARGO_FLAGS) --workspace --lib --bins --examples

## test/e2e - CLI and PTY end-to-end tests (signals only test-owned child processes)
test/e2e:
	$(CARGO) test $(CARGO_FLAGS) --workspace --test '*'

## test/performance - performance and scale budgets (release build; see crates/rtop-tui/tests/performance.rs)
test/performance:
	$(CARGO) test $(CARGO_FLAGS) --release -p rtop-tui --test performance -- --nocapture --test-threads=1
	$(CARGO) test $(CARGO_FLAGS) --release -p rtop --test pty -- --nocapture --test-threads=1 budget

SOAK_SECONDS ?= 300
## test/soak - drive the real UI for SOAK_SECONDS; memory and CPU must stay flat (release build)
test/soak:
	SOAK_SECONDS=$(SOAK_SECONDS) $(CARGO) test $(CARGO_FLAGS) --release -p rtop --test pty -- --ignored --nocapture soak

FUZZ_SECONDS ?= 60
## test/fuzz - run every fuzz target for FUZZ_SECONDS each (needs nightly and cargo-fuzz)
test/fuzz:
	cd fuzz && for t in procfs process netlink filter export; do \
		cargo +nightly fuzz run $$t -- -max_total_time=$(FUZZ_SECONDS) -max_len=4096 || exit 1; \
	done

##@ Check

## check - every static gate: fmt, clippy, rustdoc, privacy
check: check/fmt check/clippy check/doc check/privacy

## check/fmt - verify formatting
check/fmt:
	$(CARGO) fmt --all --check

## check/clippy - clippy with warnings as errors
check/clippy:
	$(CARGO) clippy $(CARGO_FLAGS) --workspace --all-targets -- -D warnings

## check/doc - rustdoc with warnings as errors
check/doc:
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc $(CARGO_FLAGS) --workspace --no-deps

## check/privacy - fail if tracked files contain this machine's kernel release, host name, ids or login
check/privacy:
	scripts/check-privacy.sh

## check/audit - RustSec advisory scan of Cargo.lock (needs cargo-audit)
check/audit:
	$(CARGO) audit

## fmt - format all code
fmt:
	$(CARGO) fmt --all

##@ Screenshots

## shots - render every screen at several sizes from a redacted demo system into target/shots/*.webp
shots:
	$(CARGO) run $(CARGO_FLAGS) -p rtop-tui --example screenshots -- target/shots
	scripts/shots.sh target/shots

##@ Release

## package/dist - release binary plus README, LICENSE and CHANGELOG as dist/<name>.tar.gz with .sha256
package/dist: build/release
	install -d target/dist-staging/$(DIST_NAME) dist
	install -m 0755 target/release/rtop target/dist-staging/$(DIST_NAME)/rtop
	install -m 0644 README.md LICENSE CHANGELOG.md target/dist-staging/$(DIST_NAME)/
	tar -C target/dist-staging -czf dist/$(DIST_NAME).tar.gz $(DIST_NAME)
	cd dist && sha256sum $(DIST_NAME).tar.gz > $(DIST_NAME).tar.gz.sha256
	@echo "dist/$(DIST_NAME).tar.gz"

## release - lists the release targets; does nothing
release:
	@grep -E '^## release/' $(MAKEFILE_LIST) | sed 's/^## /  /'

## release/patch - run RELEASE_TESTS, bump x.y.Z, date the CHANGELOG, commit and tag (local; push yourself)
release/patch: _require-cargo-release $(RELEASE_TESTS)
	@$(CARGO) release patch --execute --no-confirm

## release/minor - as release/patch for x.Y.0
release/minor: _require-cargo-release $(RELEASE_TESTS)
	@$(CARGO) release minor --execute --no-confirm

## release/major - as release/patch for X.0.0
release/major: _require-cargo-release $(RELEASE_TESTS)
	@$(CARGO) release major --execute --no-confirm

_require-cargo-release:
	@$(CARGO) release --version >/dev/null 2>&1 || $(CARGO) install cargo-release --locked

##@ Housekeeping

## clean - remove build artefacts (target/ and dist/)
clean:
	$(CARGO) clean
	rm -f dist/*.tar.gz dist/*.sha256

## help - list every target
help:
	@awk '/^##@/ { printf "\n%s\n", substr($$0, 5); next } /^## / { sub(/^## /, "  "); print }' $(MAKEFILE_LIST)
