#!/bin/bash

SERVE=no
if [ "$1" == "serve" ] ; then
  SERVE=yes
fi

for util in gelatyx ; do
  if ! hash $util 2>/dev/null ; then
    cargo install $util --version 0.3.0 --locked
  fi
done

tracked_markdown=$(mktemp)
trap "rm ${tracked_markdown}" "EXIT"
find docs -type f | grep -E '\.(markdown|md)$' > $tracked_markdown

gelatyx --language lua --file-list $tracked_markdown --language-config ci/stylua.toml
gelatyx --language lua --file-list $tracked_markdown --language-config ci/stylua.toml --check || exit 1

set -ex

# Use the GH CLI to make an authenticated request if available,
# otherwise just do an ad-hoc curl.
# However, if we are called from within a GH actions workflow (BUILD_REASON
# is set), only use `gh` if GH_TOKEN is also set, otherwise it will refuse
# to run.
function ghapi() {
  if hash gh 2>/dev/null && test \( -n "$BUILD_REASON" -a -n "$GH_TOKEN" \) -o -z "$BUILD_REASON"; then
    gh api $1
  else
    curl https://api.github.com$1
  fi
}

[[ -f /tmp/wakterm.releases.json ]] || ghapi /repos/wakamex/wakterm/releases > /tmp/wakterm.releases.json
python3 ci/subst-release-info.py || exit 1
python3 ci/generate-docs.py || exit 1

# Adjust path to pick up pip-installed binaries
PATH="$HOME/.local/bin:$PATH"

if hash black 2>/dev/null && black --version >/dev/null 2>&1 ; then
  black ci/generate-docs.py ci/subst-release-info.py
fi

cp "assets/icon/terminal.png" docs/favicon.png
cp "assets/icon/wakterm-icon.svg" docs/favicon.svg
mkdir -p docs/fonts
cp assets/fonts/SymbolsNerdFontMono-Regular.ttf docs/fonts/

container_runtime() {
  if hash podman 2>/dev/null ; then
    echo podman
  elif hash docker 2>/dev/null ; then
    echo docker
  else
    echo "Please install either podman or docker"
    exit 1
  fi
}

runtime=$(container_runtime)
run_args=(--rm)
if [[ "$runtime" == "podman" ]]; then
  run_args+=(--security-opt label=disable)
fi

"$runtime" build -t wakterm/mkdocs-material -f ci/Dockerfile.docs .

if [ "$SERVE" == "yes" ] ; then
  "$runtime" run "${run_args[@]}" -it -p8000:8000 -v "${PWD}:/docs" wakterm/mkdocs-material serve -a 0.0.0.0:8000
else
  "$runtime" run "${run_args[@]}" -e CARDS=true -v "${PWD}:/docs" wakterm/mkdocs-material build
fi
