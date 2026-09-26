# Wakterm starts bash as `bash --posix` with ENV pointing at this file, the
# one startup hook bash offers without changes to the user's files. Return to
# normal mode, run the startup files bash would have run, then load the
# integration.
builtin set +o posix

if [[ -n "${WAKTERM_BASH_ORIG_ENV+x}" ]]; then
  export ENV="$WAKTERM_BASH_ORIG_ENV"
  unset WAKTERM_BASH_ORIG_ENV
else
  unset ENV
fi

# POSIX mode defaults HISTFILE to ~/.sh_history, so Wakterm passes bash's
# normal default in the environment; keep it a plain shell variable.
if [[ -n "${WAKTERM_BASH_UNEXPORT_HISTFILE-}" ]]; then
  export -n HISTFILE
  unset WAKTERM_BASH_UNEXPORT_HISTFILE
fi

if [[ -n "${WAKTERM_BASH_LOGIN-}" ]]; then
  unset WAKTERM_BASH_LOGIN
  if [[ -r /etc/profile ]]; then
    builtin source /etc/profile
  fi
  for __wakterm_profile in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
    if [[ -r "$__wakterm_profile" ]]; then
      builtin source "$__wakterm_profile"
      break
    fi
  done
  unset __wakterm_profile
else
  # Debian and derivatives build bash to read this for interactive shells.
  if [[ -r /etc/bash.bashrc ]]; then
    builtin source /etc/bash.bashrc
  fi
  if [[ -r "$HOME/.bashrc" ]]; then
    builtin source "$HOME/.bashrc"
  fi
fi

if [[ -n "${WAKTERM_SHELL_INTEGRATION_DIR-}" ]]; then
  builtin source "$WAKTERM_SHELL_INTEGRATION_DIR/wakterm.sh"
  # Wakterm's agent restore wrapper runs `bash -l -i -c '<agent>; ...'` and
  # then evaluates this to replace itself with an integrated login shell.
  __wakterm_reexec='
    if [[ -n "${ENV+x}" ]]; then export WAKTERM_BASH_ORIG_ENV="$ENV"; fi
    if [[ -n "${HISTFILE+x}" && "${HISTFILE@a}" != *x* ]]; then
      export HISTFILE WAKTERM_BASH_UNEXPORT_HISTFILE=1
    fi
    export ENV="$WAKTERM_SHELL_INTEGRATION_DIR/bash/inject.bash" WAKTERM_BASH_LOGIN=1
    exec "$0" --posix'
fi
