#!/bin/bash
set -euo pipefail

# Update files that are derived from things baked into the executable

if [ -z "${WAKTERM_BIN:-}" ]; then
  if [ -x "${PWD}/target/debug/wakterm" ]; then
    WAKTERM_BIN="${PWD}/target/debug/wakterm"
  elif [ -x "${PWD}/target/release/wakterm" ]; then
    WAKTERM_BIN="${PWD}/target/release/wakterm"
  else
    echo "Error: wakterm binary not found in target/debug or target/release. Please build it first or set WAKTERM_BIN." >&2
    exit 1
  fi
fi

if [ -z "${STRIP_BIN:-}" ]; then
  if [ -x "${PWD}/target/debug/strip-ansi-escapes" ]; then
    STRIP_BIN="${PWD}/target/debug/strip-ansi-escapes"
  elif [ -x "${PWD}/target/release/strip-ansi-escapes" ]; then
    STRIP_BIN="${PWD}/target/release/strip-ansi-escapes"
  else
    echo "Error: strip-ansi-escapes binary not found in target/debug or target/release. Please build it first or set STRIP_BIN." >&2
    exit 1
  fi
fi

if [ -z "${NARROW_BIN:-}" ]; then
  NARROW_BIN="$(dirname "$WAKTERM_BIN")/examples/narrow"
fi
if [ ! -x "$NARROW_BIN" ]; then
  echo "Error: narrow helper $NARROW_BIN not found. Build it with 'cargo build --example narrow' or set NARROW_BIN." >&2
  exit 1
fi

gui_bin="$(dirname "$WAKTERM_BIN")/wakterm-gui"
if [ ! -x "$gui_bin" ]; then
  echo "Error: companion GUI binary $gui_bin not found. wakterm show-keys requires wakterm-gui in the same directory as wakterm." >&2
  exit 1
fi

GELATYX_BIN="${GELATYX_BIN:-gelatyx}"
if ! command -v "$GELATYX_BIN" >/dev/null 2>&1; then
  echo "Error: gelatyx not found. Derived Lua key tables require gelatyx formatting." >&2
  exit 1
fi

trim_file() {
  perl -pe 's/[ \t]+$//' | perl -0777 -pe 's/^\n+|\n\K\n+$//g'
}

active_tmp=""
cleanup_tmp() {
  if [ -n "$active_tmp" ]; then
    rm -f -- "$active_tmp"
  fi
}
trap cleanup_tmp EXIT

generate_file() {
  local target="$1"
  shift
  active_tmp=$(mktemp "${target}.tmp.XXXXXX")
  if "$@" > "$active_tmp"; then
    mv -f "$active_tmp" "$target"
    active_tmp=""
  else
    local status=$?
    rm -f -- "$active_tmp"
    active_tmp=""
    return "$status"
  fi
}

render_shell_completion() {
  local shell="$1"
  "$WAKTERM_BIN" shell-completion --shell "$shell"
}

render_key_table() {
  local mode="$1"
  local fmt_tmp
  fmt_tmp=$(mktemp "${TMPDIR:-/tmp}/key_table.XXXXXX.md")
  {
    echo '```lua'
    "$WAKTERM_BIN" -n show-keys --lua --key-table "$mode" || {
      local status=$?
      rm -f -- "$fmt_tmp"
      return "$status"
    }
    echo '```'
  } > "$fmt_tmp"
  if ! "$GELATYX_BIN" --language lua --language-config ci/stylua.toml "$fmt_tmp" >/dev/null; then
    rm -f -- "$fmt_tmp"
    return 1
  fi
  if ! cat "$fmt_tmp"; then
    rm -f -- "$fmt_tmp"
    return 1
  fi
  rm -f -- "$fmt_tmp"
}

render_synopsis() {
  "$NARROW_BIN" "$WAKTERM_BIN" "$@" | "$STRIP_BIN" | trim_file
}

for shell in bash zsh fish ; do
  generate_file "assets/shell-completion/$shell" render_shell_completion "$shell"
done

for mode in copy_mode search_mode ; do
  fname="docs/examples/default-$(echo $mode | tr _ -)-key-table.markdown"
  generate_file "$fname" render_key_table "$mode"
done

synopsis_commands=(
  ""
  "start"
  "ssh"
  "serial"
  "connect"
  "ls-fonts"
  "show-keys"
  "agent"
  "imgcat"
  "set-working-directory"
  "record"
  "replay"
  "cli activate-pane"
  "cli activate-pane-direction"
  "cli adjust-pane-size"
  "cli activate-tab"
  "cli get-pane-direction"
  "cli get-text"
  "cli kill-pane"
  "cli list"
  "cli list-clients"
  "cli move-pane-to-new-tab"
  "cli rename-workspace"
  "cli restore-layout"
  "cli save-layout"
  "cli send-text"
  "cli set-tab-title"
  "cli set-window-title"
  "cli spawn"
  "cli split-pane"
  "cli zoom-pane"
  "agent start"
  "agent launch"
  "agent launch codex"
  "agent adopt"
  "agent adopt-detected"
  "agent list"
  "agent watch"
  "agent inspect"
  "agent output"
  "agent events"
  "agent capabilities"
  "agent catalog"
  "agent admit"
  "agent send"
  "agent request"
  "agent request get"
  "agent request watch"
  "agent request cancel"
  "agent interrupt"
  "agent set"
  "agent clear"
)

generated_synopses=()
for command_path in "${synopsis_commands[@]}"; do
  command_suffix=${command_path// /-}
  target="docs/examples/cmd-synopsis-wakterm${command_suffix:+-${command_suffix}}--help.txt"
  command_args=()
  if [ -n "$command_path" ]; then
    read -r -a command_args <<< "$command_path"
  fi
  generate_file "$target" render_synopsis "${command_args[@]}" --help
  generated_synopses+=("$target")
done

was_generated() {
  local expected="$1"
  local generated
  for generated in "${generated_synopses[@]}"; do
    if [ "$generated" = "$expected" ]; then
      return 0
    fi
  done
  return 1
}

while IFS= read -r synopsis; do
  if ! was_generated "docs/examples/$synopsis"; then
    echo "Error: documentation includes a synopsis that is not in synopsis_commands: $synopsis" >&2
    exit 1
  fi
done < <(
  find docs -type f \( -name '*.md' -o -name '*.markdown' \) -exec \
    grep -hoE 'cmd-synopsis-[^" ]+\.txt' {} + | sort -u
)
