#!/bin/bash

set -euo pipefail

version=0.23.4
archive_sha256=54d1a347781b2f32330914fcc02def81c7e3ddb6111b36d1cc89c06557aed1de
archive_name=zola-v${version}-x86_64-unknown-linux-gnu.tar.gz
archive_url=https://github.com/getzola/zola/releases/download/v${version}/${archive_name}
destination=${1:-.cache/zola/${version}/zola}

if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  echo "The pinned docs installer currently supports Linux x86_64; set ZOLA_BIN for another platform." >&2
  exit 1
fi

destination_dir=$(dirname "$destination")
mkdir -p "$destination_dir"
temporary_dir=$(mktemp -d)
trap 'rm -rf "$temporary_dir"' EXIT

curl --fail --location --silent --show-error "$archive_url" --output "$temporary_dir/$archive_name"
printf '%s  %s\n' "$archive_sha256" "$temporary_dir/$archive_name" | sha256sum --check --status
tar -xzf "$temporary_dir/$archive_name" -C "$temporary_dir" zola
install -m 0755 "$temporary_dir/zola" "$destination"
"$destination" --version
