#!/usr/bin/env bash
# Regenerate the platform icons from the square source PNG.
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
icon_dir="$repo_root/assets/icon"
source_png="$icon_dir/wakterm-icon.png"
temporary_dir=$(mktemp -d)
trap 'rm -rf "$temporary_dir"' EXIT

for command in magick png2icns; do
  command -v "$command" >/dev/null || {
    echo "error: $command is required to regenerate the icons" >&2
    exit 1
  }
done

magick "$source_png" -resize '!128x128' -strip \
  "$icon_dir/terminal.png"

icns_inputs=()
for dimension in 16 32 128 256 512 1024; do
  output="$temporary_dir/icon_${dimension}px.png"
  magick "$source_png" -resize "!${dimension}x${dimension}" -strip "$output"
  icns_inputs+=("$output")
done

# Fedora's png2icns emits non-actionable JasPer deprecation diagnostics on
# successful conversions. Keep successful runs quiet, but preserve its full
# output when conversion fails.
icns_log="$temporary_dir/png2icns.log"
if ! png2icns \
  "$repo_root/assets/macos/wakterm.app/Contents/Resources/terminal.icns" \
  "${icns_inputs[@]}" >"$icns_log" 2>&1; then
  cat "$icns_log" >&2
  exit 1
fi

magick "$source_png" \
  -define icon:auto-resize=256,128,96,64,48,32,16 \
  -units Undefined -density 0 -strip \
  "$repo_root/assets/windows/terminal.ico"
