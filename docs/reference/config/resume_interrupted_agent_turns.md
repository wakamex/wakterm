# `resume_interrupted_agent_turns = false`

When the mux server restarts, as after a reboot or an update, it restores each agent's session, but an agent that was working on a turn comes back waiting for input with that turn unfinished. Set this to `true` to have the restarted server send each such agent the prompt "Wakterm's mux server restarted, which interrupted your turn. Continue where you left off." once the agent is restored and ready for input.

The prompt goes through prompt admission, so it is typed only into an agent that is idle with an empty input box. The server retries for up to 10 minutes while the agent is not ready, then gives up and logs why. The server's log records each agent it resumed.

An agent counts as interrupted when the last session save before the restart saw it working on a turn. A turn that ended before that save is not resumed.

```lua
config.resume_interrupted_agent_turns = true
```
