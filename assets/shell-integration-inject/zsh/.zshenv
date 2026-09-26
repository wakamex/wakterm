# Wakterm starts zsh with ZDOTDIR pointing at this directory so that it can
# load its shell integration without changes to the user's startup files.
# Restore the user's ZDOTDIR first: zsh reads every later startup file from
# it, and programs started by the shell inherit it.
if [[ -n "${WAKTERM_ORIG_ZDOTDIR+x}" ]]; then
  export ZDOTDIR="$WAKTERM_ORIG_ZDOTDIR"
  unset WAKTERM_ORIG_ZDOTDIR
else
  unset ZDOTDIR
fi

if [[ -f "${ZDOTDIR:-$HOME}/.zshenv" ]]; then
  builtin source "${ZDOTDIR:-$HOME}/.zshenv"
fi

if [[ -o interactive && -n "${WAKTERM_SHELL_INTEGRATION_DIR-}" ]]; then
  builtin source "$WAKTERM_SHELL_INTEGRATION_DIR/wakterm.sh"
  # Wakterm's agent restore wrapper runs `zsh -l -i -c '<agent>; ...'` and
  # then evaluates this to replace itself with an integrated login shell.
  typeset -g __wakterm_reexec='
    if [[ -n "${ZDOTDIR+x}" ]]; then export WAKTERM_ORIG_ZDOTDIR="$ZDOTDIR"; fi
    export ZDOTDIR="$WAKTERM_SHELL_INTEGRATION_DIR/zsh"
    exec "$0" -l'
fi
