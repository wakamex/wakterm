#!/bin/bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "Usage: $0 <release-tag> [--push]" >&2
  exit 1
fi

TAG_NAME="$1"
VERSION_NO_V="${TAG_NAME#v}"
PUSH="${2:-}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
GH_BIN="${GH_BIN:-$(command -v gh || echo /home/linuxbrew/.linuxbrew/bin/gh)}"
REPO="${WAKTERM_REPO:-wakamex/wakterm}"
TAP_REPO="${HOMEBREW_TAP_REPO:-wakamex/homebrew-tap}"
BUCKET_REPO="${SCOOP_BUCKET_REPO:-wakamex/scoop-bucket}"

actual_tag="$(git -C "$HERE" describe --exact-match --tags)"
if [[ "$actual_tag" != "$TAG_NAME" ]]; then
  echo "Error: current checkout is at '$actual_tag', expected tag '$TAG_NAME'" >&2
  exit 1
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

"$GH_BIN" release download "$TAG_NAME" \
  --repo "$REPO" \
  --pattern "SHA256SUMS" \
  --dir "$tmpdir"

macos_sha256="$(awk -v f="wakterm-macos-${TAG_NAME}.zip" '$2 == f || $2 == "*"f {print $1}' "$tmpdir/SHA256SUMS")"
windows_sha256="$(awk -v f="wakterm-windows-${TAG_NAME}.zip" '$2 == f || $2 == "*"f {print $1}' "$tmpdir/SHA256SUMS")"

test -n "$macos_sha256"
test -n "$windows_sha256"

sed -e "s/@TAG@/$TAG_NAME/g" -e "s/@VERSION@/$VERSION_NO_V/g" -e "s/@SHA256@/$macos_sha256/g" \
  < "$HERE/ci/wakterm-homebrew-macos.rb.template" \
  > "$tmpdir/wakterm.rb"

sed -e "s/@TAG@/$TAG_NAME/g" -e "s/@VERSION@/$VERSION_NO_V/g" -e "s/@SHA256@/$windows_sha256/g" \
  < "$HERE/ci/wakterm-scoop.json.template" \
  > "$tmpdir/wakterm.json"

echo "Generated manifests in $tmpdir:"
echo "  Version:        $VERSION_NO_V ($TAG_NAME)"
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
