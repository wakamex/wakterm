#!/bin/bash

set -euo pipefail

project_dir=$(cd "$(dirname "$0")/.." && pwd)
cd "$project_dir"

if ! command -v gelatyx >/dev/null 2>&1; then
  cargo install gelatyx --version 0.3.0 --locked
fi

tracked_markdown=$(mktemp)
trap 'rm -f "$tracked_markdown"' EXIT
git ls-files 'docs/**/*.md' 'docs/*.md' > "$tracked_markdown"
gelatyx --language lua --file-list "$tracked_markdown" --language-config ci/stylua.toml
gelatyx --language lua --file-list "$tracked_markdown" --language-config ci/stylua.toml --check

if [[ "${DOCS_OFFLINE:-0}" != "1" && ! -f /tmp/wakterm.releases.json ]]; then
  if command -v gh >/dev/null 2>&1 && [[ -n "${GH_TOKEN:-}" ]]; then
    gh api /repos/wakamex/wakterm/releases > /tmp/wakterm.releases.json || rm -f /tmp/wakterm.releases.json
  elif command -v curl >/dev/null 2>&1; then
    curl --fail --silent --show-error https://api.github.com/repos/wakamex/wakterm/releases \
      --output /tmp/wakterm.releases.json || rm -f /tmp/wakterm.releases.json
  fi
fi

zola_bin=${ZOLA_BIN:-$project_dir/.cache/zola/0.23.4/zola}
if [[ ! -x "$zola_bin" ]]; then
  ci/install-zola.sh "$zola_bin"
fi

if [[ "${1:-}" == "serve" ]]; then
  python3 ci/build-zola-docs.py --zola "$zola_bin" --prepare-only
  exec "$zola_bin" --root docs-site serve \
    --interface 0.0.0.0 \
    --port "${DOCS_PORT:-8000}" \
    --extra-watch-path ../docs \
    --extra-watch-path ../ci
fi

python3 ci/build-zola-docs.py --zola "$zola_bin"
