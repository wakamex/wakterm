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

gui_bin="$(dirname "$WAKTERM_BIN")/wakterm-gui"
if [ ! -x "$gui_bin" ]; then
  echo "Error: companion GUI binary $gui_bin not found. wakterm show-keys requires wakterm-gui in the same directory as wakterm." >&2
  exit 1
fi

trim_file() {
  perl -pe 's/[ \t]+$//' | perl -0777 -pe 's/^\n+|\n\K\n+$//g'
}

cleanup_tmp() {
  rm -f docs/examples/*.tmp.* assets/shell-completion/*.tmp.*
}
trap cleanup_tmp EXIT

generate_file() {
  local target="$1"
  shift
  local tmp
  tmp=$(mktemp "${target}.tmp.XXXXXX")
  if "$@" > "$tmp"; then
    mv -f "$tmp" "$target"
  else
    local status=$?
    rm -f "$tmp"
    return "$status"
  fi
}

render_shell_completion() {
  local shell="$1"
  "$WAKTERM_BIN" shell-completion --shell "$shell"
}

render_key_table() {
  local mode="$1"
  echo '```lua'
  "$WAKTERM_BIN" -n show-keys --lua --key-table "$mode" || return $?
  echo '```'
}

render_synopsis() {
  cargo run --example narrow "$WAKTERM_BIN" "$@" | "$STRIP_BIN" | trim_file
}

for shell in bash zsh fish ; do
  generate_file "assets/shell-completion/$shell" render_shell_completion "$shell"
done

for mode in copy_mode search_mode ; do
  fname="docs/examples/default-$(echo $mode | tr _ -)-key-table.markdown"
  generate_file "$fname" render_key_table "$mode"
done

generate_file docs/examples/cmd-synopsis-wakterm--help.txt render_synopsis --help

for cmd in start ssh serial connect ls-fonts show-keys agent imgcat set-working-directory record replay ; do
  generate_file "docs/examples/cmd-synopsis-wakterm-${cmd}--help.txt" render_synopsis "$cmd" --help
done

for cmd in \
    activate-pane \
    activate-pane-direction \
    adjust-pane-size \
    activate-tab \
    get-pane-direction \
    get-text \
    kill-pane \
    list \
    list-clients \
    move-pane-to-new-tab \
    rename-workspace \
    restore-layout \
    save-layout \
    send-text \
    set-tab-title \
    set-window-title \
    spawn \
    split-pane \
    zoom-pane \
    ; do
  generate_file "docs/examples/cmd-synopsis-wakterm-cli-${cmd}--help.txt" render_synopsis cli "$cmd" --help
done

for cmd in \
    start \
    launch \
    adopt \
    adopt-detected \
    list \
    watch \
    inspect \
    output \
    events \
    capabilities \
    catalog \
    admit \
    send \
    request \
    interrupt \
    set \
    clear \
    ; do
  generate_file "docs/examples/cmd-synopsis-wakterm-agent-${cmd}--help.txt" render_synopsis agent "$cmd" --help
done

generate_file "docs/examples/cmd-synopsis-wakterm-agent-launch-codex--help.txt" render_synopsis agent launch codex --help

for sub in get watch cancel ; do
  generate_file "docs/examples/cmd-synopsis-wakterm-agent-request-${sub}--help.txt" render_synopsis agent request "$sub" --help
done
