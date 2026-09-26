# Wakterm adds this directory's parent to XDG_DATA_DIRS so that fish loads
# this file without changes to the user's configuration. Restore the user's
# XDG_DATA_DIRS first so programs started by the shell inherit their own.
if set -q WAKTERM_FISH_ORIG_XDG_DATA_DIRS
    set -gx XDG_DATA_DIRS $WAKTERM_FISH_ORIG_XDG_DATA_DIRS
    set -e WAKTERM_FISH_ORIG_XDG_DATA_DIRS
else
    set -e XDG_DATA_DIRS
end

# Pane-local history. Wakterm gives each pane a WAKTERM_PANE_TOKEN that
# survives mux restarts. The outermost interactive fish in a pane uses a
# history session of its own, seeded from the shared history when the pane
# first uses it, so Up recalls this pane's commands first and then older
# shared ones. Commands entered in the pane are appended to the shared
# history when the shell exits, or by the next shell in the pane if this one
# is killed.
function __wakterm_pane_history_merge
    # Append the pane history written since the last merge to the shared
    # history. Fish entries are independent, so appending them is valid.
    set -l size (command wc -c <$__wakterm_pane_histfile 2>/dev/null; or echo 0)
    set -l merged (command cat $__wakterm_pane_merged 2>/dev/null; or echo $size)
    if test "$size" -gt "$merged"
        # Leave the offset unchanged if the append fails, so it is retried.
        command tail -c +(math $merged + 1) $__wakterm_pane_histfile >>$__wakterm_shared_histfile
        or return 1
    end
    echo $size >$__wakterm_pane_merged
end

function __wakterm_pane_history_exit --on-event fish_exit
    builtin history save
    __wakterm_pane_history_merge
end

# Write each command to the pane history as it finishes, as the bash and zsh
# integration do, so a killed shell's commands are not lost.
function __wakterm_pane_history_save --on-event fish_postexec
    builtin history save
end

if status is-interactive
    and not set -q WAKTERM_SHELL_SKIP_PANE_HISTORY
    and not set -q WSH_NATIVE_PANE_HISTORY
    and string match -qr '^[0-9a-f-]{36}$' -- "$WAKTERM_PANE_TOKEN"
    and begin
        not set -q WAKTERM_PANE_HISTORY_OWNER
        or test "$WAKTERM_PANE_HISTORY_OWNER" = "$fish_pid"
    end
    set -gx WAKTERM_PANE_HISTORY_OWNER $fish_pid
    set -l data_dir (set -q XDG_DATA_HOME; and echo $XDG_DATA_HOME; or echo $HOME/.local/share)/fish
    set -l state_dir (set -q XDG_STATE_HOME; and echo $XDG_STATE_HOME; or echo $HOME/.local/state)/wakterm/pane-history
    set -l shared_session fish
    set -q fish_history; and set shared_session $fish_history
    set -l pane_session wakterm_(string replace -a - _ -- $WAKTERM_PANE_TOKEN)
    set -g __wakterm_shared_histfile $data_dir/{$shared_session}_history
    set -g __wakterm_pane_histfile $data_dir/{$pane_session}_history
    set -g __wakterm_pane_merged $state_dir/$WAKTERM_PANE_TOKEN.fish.merged
    command mkdir -p -m 700 $data_dir $state_dir
    if not test -e $__wakterm_pane_histfile
        if test -f $__wakterm_shared_histfile
            command cp $__wakterm_shared_histfile $__wakterm_pane_histfile
        else
            command touch $__wakterm_pane_histfile
        end
        command wc -c <$__wakterm_pane_histfile >$__wakterm_pane_merged
    else
        __wakterm_pane_history_merge
    end
    set -g fish_history $pane_session
else
    functions -e __wakterm_pane_history_exit __wakterm_pane_history_save
end
