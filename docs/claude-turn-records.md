# What Claude Code records about its turns

Wakterm follows interactive Claude Code sessions through two records that Claude Code writes itself: the session transcript, a JSON Lines file under `~/.claude/projects/`, and [hooks](https://code.claude.com/docs/en/hooks), commands Claude Code runs on events, configured in its settings. This page lists what each records about a turn's start, the input taken into it, waits on the user and its end, as observed with Claude Code 2.1.332 running with `--dangerously-skip-permissions`, the way Wakterm starts Claude agents. Each fact below was seen in a session whose project settings registered a hook that appended every event to a journal, next to that session's transcript.

## Turn boundaries in the transcript

Every record of a turn carries the turn's `promptId`: the user record that starts it, tool results, interruption markers and the local command records of that turn. Assistant records do not.

| Moment | Transcript records |
| --- | --- |
| A prompt starts a turn | A user record with a new `promptId`. |
| A notice starts a turn, such as a background task finishing | `queue-operation` `enqueue` and `dequeue`, then a user record with a new `promptId` whose text begins `<task-notification>` and names the task and the tool call that started it. |
| Input sent while the turn runs is taken into it | `queue-operation` `enqueue` when it arrives; after the next tool result, `queue-operation` `remove` and an `attachment` record of type `queued_command` holding the input's text, written before the next model request. No user record is written for it. |
| Input sent while the turn runs waits for the next turn | `queue-operation` `enqueue` when it arrives; after the turn's `turn_duration`, `queue-operation` `dequeue` and a user record with a new `promptId`. |
| The turn ends | `system` `turn_duration`, after a `system` `stop_hook_summary` when hooks are configured. |
| The user interrupts | A tool result saying the user did not want to proceed, when a tool was running, then a user record whose text is `[Request interrupted by user for tool use]` or `[Request interrupted by user]`, with the turn's `promptId`. No `turn_duration` follows. |
| `/compact` | The `<local-command-caveat>` note, the `<command-name>/compact</command-name>` record, `system` `compact_boundary`, the summary as a user record marked `isCompactSummary`, and the command's `<local-command-stdout>`. No `turn_duration`: a local command is not a model turn. |
| `/clear` | The command's records in a new transcript file, with `system` `local_command`. |

Records are not always written in timestamp order; an attachment can be written after a later user record, so a reader follows the order of the file.

## Hook events

Every hook event carries `session_id`, `prompt_id`, `transcript_path` and `hook_event_name`.

| Event | When it fired in the session |
| --- | --- |
| `UserPromptSubmit` | When a prompt or a notice arrived, including input sent while a turn ran, at the moment it was queued, with the running turn's `prompt_id`. Input that then waited for the next turn did not fire it again when that turn started under its own `prompt_id`, so this event does not say which turn an input joins. |
| `PermissionRequest` | When a question form (`AskUserQuestion`) was shown, with `tool_name`, even with permission checks skipped. The transcript held no record of the question until it was answered. |
| `Notification` | 6 seconds after the question form, with `notification_type` `permission_prompt`; also when Claude went idle. |
| `Stop` | Once at each turn's end, with the turn's `prompt_id`, `last_assistant_message` holding the final reply, and `background_tasks` listing tasks still running, each with its ID, type, status and description. It did not fire on an interruption. |
| `SubagentStop` | After most turns' `Stop`, for an internal agent with an empty `agent_type`; it is not a turn of the session. |
| `PreCompact`, `PostCompact` | Around `/compact`, with trigger `manual`, together with `SessionStart` with source `compact`. |
| `SessionEnd`, `SessionStart` | For `/clear`: `SessionEnd` with reason `clear`, then `SessionStart` with source `clear`, a new `session_id` and the new `transcript_path`. |

A hook that fails, such as one whose command is missing or exits with status 1, does not block the turn: Claude records a `hook_non_blocking_error` attachment, lists the error in `stop_hook_summary`'s `hookErrors` with `preventedContinuation` false, and shows "Stop hook failed" after the turn, which then ends as usual. A hook that runs past its timeout is cancelled (`hook_cancelled`) and delays the turn's end by the timeout. Neither stops a question form from appearing or decides it. A non-empty `hookErrors` therefore does not mean a stop was blocked; the "Stop hook feedback" record does. Hooks passed with `--settings` run under `claude --resume`, where `SessionStart` has source `resume`.

Hooks from the user's own settings run alongside a project's, and a `UserPromptSubmit` hook's output is added to Claude's context, so a recording hook must print nothing.

## What only one of the two records

- Which turn an input joins: only the transcript, through `remove` with a `queued_command` attachment, or `dequeue` with a new `promptId`.
- An interruption: only the transcript's marker.
- A question form waiting for its answer: only the `PermissionRequest` hook, since the transcript writes the question with its answer.
- A turn's final reply at the moment it ends: `Stop`'s `last_assistant_message`; the hooks documentation notes the transcript is written asynchronously and may lag.
- Background tasks still running when a turn ends: `Stop`'s `background_tasks`.
- A new transcript file after `/clear`: `SessionStart` names it.

## Fields that name a turn's cause and an input's fate

From Claude Code 2.1.330, a user record that starts a turn also carries `turnPromptId`, equal to its `promptId` in all 2,136 records seen, `turnOrigin` (`human`, `task_notification`, `peer` for another session's message, `scheduled`, `auto_continuation` or `system`), `promptSource` (`typed`, `queued`, `system` or `suggestion_accepted`) and `turnPosition` (`promptIndex` and `turnIndex`). A `queue-operation` `remove` carries a `reason`: `absorbed_mid_turn` (2,868 seen) for input taken into the running turn, or `delivered_to_agent` (45).

## Turns across a restart, a resume and compaction

Interactive transcripts from six days (Claude Code 2.1.28x to 2.1.332), with each `turn_duration` attributed to a turn by following `parentUuid` to the nearest record that has a `promptId`:

- 24,407 turns started by a prompt end exactly once, by `turn_duration` or an interruption marker.
- No `promptId` appears in more than one file. A resumed Claude Code process goes on writing the same file under new `promptId`s.
- A process killed during a tool call leaves its turn without an end; the resumed process writes that tool's result under a new `promptId`. Each of three such changes examined fell within a minute of a mux restart, as did 89 of 156 turns that had replies but no end before the next prompt.
- Automatic compaction during a turn keeps the turn's `promptId` (133 of 200 automatic compactions); in the other 67 the next record is a new prompt.
- A local command typed while no turn runs opens a `promptId` of its own, holding the caveat, the command and its output, which never gets a `turn_duration` (34 seen).
- A prompt typed again before Claude replies to the first is a sibling of it, with the same `parentUuid`, which abandons the first (26 seen).

Not yet explained: 145 prompts that got no reply before a later prompt that is neither a sibling nor a child of it, 58 that got none before a child prompt, and 67 turns with replies but no end before the next prompt and no restart nearby. Most come from releases before 2.1.330.

## Rules in Claude Code's source

A reading of the Claude Code 2.1.332 source gives the rules behind these records:

- `turn_duration` is written only for a turn that queried the model, was not aborted, and ended completed or waiting on background work. A turn ended by an interruption, an API error or another failure, or by the process exiting, gets none, and neither does a local command. While subagents, team tasks or detached tool calls still run, it is deferred until they finish, so it can be written after the next prompt, or never if the process exits first. A turn's end is therefore read from `turn_duration`, an interruption marker, an API error (an assistant record marked `isApiErrorMessage` with a `system` `api_error` record) or the process ending, and `turn_duration` is attributed to its turn through `parentUuid`, never by position.
- A new `promptId` is made for input that is not a tool result (a typed prompt, queued input that starts a turn, a task notification), for a prompt command such as a skill, for a local command, and for a session resumed with a permission request left open, which is why a tool result written after a resume carries a new one. Tool results and every other record written during a turn, including an automatic compaction summary, keep the turn's. `turnPromptId` is written only on the record that opens a turn.
- `queue-operation` `remove` reasons are `absorbed_mid_turn`, `delivered_to_agent`, `delivered_as_tool_result`, `stored_at_idle`, `carried_to_background`, `not_carried_to_background`, `written_at_exit`, `dropped_by_hook` and `cleared_on_cancel`; `popAll` and `popOne` mark queued input pulled back into the input box.
- No hook fires when a queued input starts a turn; the transcript's `dequeue` and the user record with a new `promptId` are the only record of it. `UserPromptSubmit` runs when input is submitted.
- `Stop` fires once at the end of each turn, with `stop_hook_active` set when a blocking stop hook made the turn continue, and `StopFailure` instead after an API error. `PermissionRequest` fires whenever a tool's permission decision is to ask, which tools that need the person, such as `AskUserQuestion`, keep even with permission checks skipped. Hooks passed with `--settings` load on every launch, including `claude --resume`; `disableAllHooks`, and `allowManagedHooksOnly` in managed settings, silence them.
- A stop hook that blocks makes the turn continue under the same `promptId`. Each run of stop hooks in which at least one hook ran writes one `system` `stop_hook_summary` record as soon as the hooks finish, with no deferral: a block shows as a non-empty `hookErrors` with `preventedContinuation` false, while `preventedContinuation` true, with a `stopReason`, means a hook ended the turn. Before the summary, a block writes a user record marked `isMeta` and `hookText` whose content begins "Stop hook feedback:", which the model reads and continues from. `Stop` fires at the turn's end even when `turn_duration` is then deferred, and again with `stop_hook_active` true when the continued turn ends.
- `turnOrigin` takes `human`, `sdk`, `scheduled`, `task_notification`, `auto_continuation` (for automatic resumes after sleep, quit, a remote restart or a usage limit, and for CI monitoring), `peer`, `host_synthetic`, `system` and `unknown`.

Not yet observed live: a forked session, a subagent's turn, a turn ended by an API error (`StopFailure`), and a refusal.
