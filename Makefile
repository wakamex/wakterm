.PHONY: all fmt build check test docs servedocs update-derived check-derived

DERIVED_PROFILE ?= debug
ifeq ($(DERIVED_PROFILE),release)
DERIVED_BUILD_OPTS := --release
else ifeq ($(DERIVED_PROFILE),debug)
DERIVED_BUILD_OPTS :=
else
$(error DERIVED_PROFILE must be debug or release)
endif

DERIVED_TARGET_DIR ?= $(or $(CARGO_TARGET_DIR),$(CURDIR)/target)
DERIVED_BIN_DIR := $(DERIVED_TARGET_DIR)/$(DERIVED_PROFILE)

all: build

test:
	cargo nextest run
	cargo nextest run -p wakterm-escape-parser # no_std by default

check:
	cargo check
	cargo check -p wakterm-escape-parser
	cargo check -p wakterm-cell
	cargo check -p wakterm-surface
	cargo check -p wakterm-ssh

build:
	cargo build $(BUILD_OPTS) -p wakterm
	cargo build $(BUILD_OPTS) -p wakterm-gui
	cargo build $(BUILD_OPTS) -p wakterm-mux-server
	cargo build $(BUILD_OPTS) -p strip-ansi-escapes

fmt:
	cargo +nightly fmt

docs:
	ci/build-docs.sh

servedocs:
	ci/build-docs.sh serve

update-derived:
	cargo build $(DERIVED_BUILD_OPTS) -p wakterm -p wakterm-gui -p strip-ansi-escapes
	cargo build $(DERIVED_BUILD_OPTS) --example narrow
	WAKTERM_BIN="$(DERIVED_BIN_DIR)/wakterm" \
		STRIP_BIN="$(DERIVED_BIN_DIR)/strip-ansi-escapes" \
		NARROW_BIN="$(DERIVED_BIN_DIR)/examples/narrow" \
		ci/update-derived-files.sh

check-derived: update-derived
	git diff --check -- assets/shell-completion docs/generated/cli-help docs/generated/key-tables
	@test -z "$$(git status --short -- assets/shell-completion docs/generated/cli-help docs/generated/key-tables)" || { \
		git status --short -- assets/shell-completion docs/generated/cli-help docs/generated/key-tables; \
		echo "Derived files are stale. Run 'make update-derived'." >&2; \
		exit 1; \
	}
