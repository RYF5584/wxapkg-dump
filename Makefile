# Developer shortcuts. Everything here is a thin wrapper around cargo.

CARGO ?= cargo
PROFILE ?= release
PREFIX ?= /usr/local

.PHONY: help build test lint fmt doc clean dist release verify man man-view

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## Build every workspace member (debug)
	$(CARGO) build --workspace

release: ## Build every workspace member (release)
	$(CARGO) build --release --workspace

test: ## Run the full test suite
	$(CARGO) test --workspace

lint: ## Run clippy with warnings denied
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt: ## Format the workspace
	$(CARGO) fmt --all

doc: ## Build the API documentation
	$(CARGO) doc --workspace --no-deps --open

dist: ## Assemble the runnable bundle in dist/ (CLI and desktop shell)
	$(CARGO) build --release -p wxapkg-cli
	$(CARGO) build --release --manifest-path crates/wxapkg-gui/Cargo.toml
	@rm -rf dist
	@mkdir -p dist
	@cp "target/release/wxapkg-dump" dist/wxapkg-dump
	@cp "crates/wxapkg-gui/target/release/wxapkg-dump-gui" dist/wxapkg-dump-gui
	@echo "dist/: wxapkg-dump + wxapkg-dump-gui"

man: ## Install the manual page so `man wxapkg-dump` works
	@mkdir -p "$(PREFIX)/share/man/man1"
	@install -m 644 docs/wxapkg-dump.1 "$(PREFIX)/share/man/man1/wxapkg-dump.1"
	@echo "installed $(PREFIX)/share/man/man1/wxapkg-dump.1"

man-view: ## Render the manual page without installing it
	@stage=$$(mktemp -d); mkdir -p "$$stage/man1"; \
	 cp docs/wxapkg-dump.1 "$$stage/man1/wxapkg-dump.1"; \
	 MANPAGER=cat man -M "$$stage" wxapkg-dump; \
	 rm -rf "$$stage"

verify: ## Validate a reconstructed project (DIR=... [NODE=1])
	$(CARGO) run -q -p wxapkg-cli -- verify "$(DIR)" $(if $(NODE),--node,)

clean: ## Remove build artefacts
	$(CARGO) clean
