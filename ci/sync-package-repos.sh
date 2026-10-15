#!/bin/bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "Usage: $0 <release-tag> [--push]" >&2
  exit 1
fi

TAG_NAME="$1"
PUSH="${2:-}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
GH_BIN="${GH_BIN:-$(command -v gh || echo /home/linuxbrew/.linuxbrew/bin/gh)}"
REPO="${WAKTERM_REPO:-wakamex/wakterm}"
TAP_REPO="${HOMEBREW_TAP_REPO:-wakamex/homebrew-tap}"
BUCKET_REPO="${SCOOP_BUCKET_REPO:-wakamex/scoop-bucket}"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

"$GH_BIN" release download "$TAG_NAME" \
  --repo "$REPO" \
  --pattern "wakterm-macos-${TAG_NAME}.zip.sha256" \
  --pattern "wakterm-windows-${TAG_NAME}.zip.sha256" \
  --dir "$tmpdir"

macos_sha256="$(awk '{print $1}' "$tmpdir/wakterm-macos-${TAG_NAME}.zip.sha256")"
windows_sha256="$(awk '{print $1}' "$tmpdir/wakterm-windows-${TAG_NAME}.zip.sha256")"

test -n "$macos_sha256"
test -n "$windows_sha256"

sed -e "s/@TAG@/$TAG_NAME/g" -e "s/@SHA256@/$macos_sha256/g" \
  < "$HERE/ci/wakterm-homebrew-macos.rb.template" \
  > "$tmpdir/wakterm.rb"

sed -e "s/@TAG@/$TAG_NAME/g" -e "s/@SHA256@/$windows_sha256/g" \
  < "$HERE/ci/wakterm-scoop.json.template" \
  > "$tmpdir/wakterm.json"

echo "Generated manifests in $tmpdir:"
echo "  macOS SHA-256:   $macos_sha256"
echo "  Windows SHA-256: $windows_sha256"

if [[ "$PUSH" == "--push" ]]; then
  "$GH_BIN" repo view "$TAP_REPO" >/dev/null 2>&1 || \
    "$GH_BIN" repo create "$TAP_REPO" --public --description "Homebrew tap for wakterm"
  "$GH_BIN" repo clone "$TAP_REPO" "$tmpdir/homebrew-tap"
  mkdir -p "$tmpdir/homebrew-tap/Casks"
  cp "$tmpdir/wakterm.rb" "$tmpdir/homebrew-tap/Casks/wakterm.rb"
  git -C "$tmpdir/homebrew-tap" add Casks/wakterm.rb
  if ! git -C "$tmpdir/homebrew-tap" diff --cached --quiet; then
    git -C "$tmpdir/homebrew-tap" commit -m "Update wakterm to $TAG_NAME"
    git -C "$tmpdir/homebrew-tap" push origin HEAD
  fi

  "$GH_BIN" repo view "$BUCKET_REPO" >/dev/null 2>&1 || \
    "$GH_BIN" repo create "$BUCKET_REPO" --public --description "Scoop bucket for wakterm"
  "$GH_BIN" repo clone "$BUCKET_REPO" "$tmpdir/scoop-bucket"
  mkdir -p "$tmpdir/scoop-bucket/bucket"
  cp "$tmpdir/wakterm.json" "$tmpdir/scoop-bucket/bucket/wakterm.json"
  git -C "$tmpdir/scoop-bucket" add bucket/wakterm.json
  if ! git -C "$tmpdir/scoop-bucket" diff --cached --quiet; then
    git -C "$tmpdir/scoop-bucket" commit -m "Update wakterm to $TAG_NAME"
    git -C "$tmpdir/scoop-bucket" push origin HEAD
  fi
fi
