.PHONY: all fmt build check test docs servedocs

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
