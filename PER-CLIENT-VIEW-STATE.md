# Per-Client View State

## Problem

Today, a mux window has one global active tab:

- `Window.active` in `mux/src/window.rs`
- the server exports that single choice in `ListPanesResponse.active_tabs`
- every attached client applies that same active tab during reconcile

That means two machines attached to the same shared window cannot look at
different tabs at the same time. Whichever side becomes authoritative next
causes the other side to switch.

This is the wrong ownership boundary.

Shared mux state should include:

- windows
- tabs
- pane trees
- PTY processes
- titles
- layout geometry

Client-local view state should include:

- which tab a given client is looking at in a window
- which pane a given client has focused in a tab
- later, possibly other purely presentational state

The long-term solution is to make that split explicit in both the mux model and
the protocol.

## Goals

- Multiple attached clients can share the same tabs and panes while selecting
  different active tabs in the same window.
- The solution works for both remote clients and a GUI running directly on the
  server host.
- There is no shared active-tab fallback hidden in the mux model.
- The protocol is explicit. No inference from redraw timing or focus side
  effects.
- The design remains maintainable if we later move more state from "shared mux"
  to "client view".
- Spawn/split/activate actions use the active tab for the current client view,
  not some unrelated globally active tab.

## Non-Goals

- Duplicating tabs or windows per client
- Client-private layout trees
- Preserving per-client active tab forever with no stable client identity
- Backwards compatibility with the current fork protocol

## Current State

### Shared active tab

The current model stores active tab on the shared window object:

- `mux/src/window.rs`
- `mux/src/lib.rs:get_active_tab_for_window`

That value is treated as the active tab for every frontend.

### Server sync

The mux server includes one `active_tabs: HashMap<WindowId, TabId>` in
`ListPanesResponse`.

Current meaning:

- one active tab per window
- server-global, not client-specific

### Client reconcile

The remote client applies `active_tabs` directly to its local mirrored windows.

This is why one machine switching tabs flips the other machine too.

### Existing per-client precedent

The mux already has per-client state:

- `active_workspace`
- `focused_pane_id`

That proves the codebase already accepts "shared mux state plus client-local
state" as a valid model.

## Why A Small Patch Is Not Enough

There are two tempting shortcuts:

1. Stop syncing `active_tabs` from the server.
2. Infer active tab purely from the currently focused pane.

Both are insufficient.

### Not syncing global active tabs

That would reduce the symptom for remote mirrors, but it does not create a real
source of truth for per-client tab selection. Reconnects and reconciles would
still be ambiguous.

### Inferring from one focused pane

`focused_pane_id` is a single pane for the current client. That is not enough to
remember active tabs for multiple windows on that same client.

If a client has 3 windows open, each window needs its own remembered active tab.

## Proposed Model

Introduce a real per-client view-state layer.

### 1. Separate connection identity from view identity

Current `ClientId` is process-shaped and ephemeral:

- hostname
- username
- pid
- epoch
- in-process counter

That is useful for liveness and bookkeeping, but it is not a robust key for
client-local view state across reconnects.

Add a stable `ClientViewId`.

Properties:

- generated once per frontend instance/profile
- persisted locally by the GUI/client
- reused across reconnects
- distinct from the per-process connection id

This gives the server a stable key for "the MacBook view" versus "the desktop
view".

### 2. Add explicit per-window client view state

Add something like:

```rust
struct ClientWindowViewState {
    active_tab_id: Option<TabId>,
    active_pane_id: Option<PaneId>,
}
```

Store it per client view:

```rust
struct ClientInfo {
    connection_id: Arc<ClientId>,
    view_id: Arc<ClientViewId>,
    active_workspace: Option<String>,
    focused_pane_id: Option<PaneId>,
    window_view_state: HashMap<WindowId, ClientWindowViewState>,
}
```

Key point:

- `focused_pane_id` remains "currently focused pane overall"
- `window_view_state` becomes "what this client considers active in each window"

### 3. Remove shared window active tab completely

Delete `Window.active` from the shared mux window model.

That is the cleanest model:

- shared mux state no longer pretends there is one "real active tab"
- a tab is active only relative to a client view
- code that needs an active tab must either have a client identity or an
  explicit target

Consequences:

- `get_active_tab_for_window(window_id)` should not survive as a convenience API
- identity-less code paths must be made explicit
- shared session persistence no longer stores "window active tab" because that
  concept no longer exists globally

This is stricter than a transitional fallback model, but it avoids years of
"which active tab did this code actually mean?" bugs.

## Protocol Changes

No compatibility constraints are needed here. Change the protocol cleanly and
bump the codec version.

### Replace global `active_tabs` with explicit client view state

`ListPanesResponse` should stop pretending that `active_tabs` is global shared
window state for all frontends.

Replace it with something like:

```rust
pub struct ClientWindowViewStateSnapshot {
    pub active_tab_id: Option<TabId>,
    pub active_pane_id: Option<PaneId>,
}

pub struct ListPanesResponse {
    pub tabs: Vec<PaneNode>,
    pub tab_titles: Vec<String>,
    pub window_titles: HashMap<WindowId, String>,
    pub client_window_view_state: HashMap<WindowId, ClientWindowViewStateSnapshot>,
}
```

Semantics:

- this snapshot is for the requesting client view only
- there is no "global server active tab"

### Add explicit client-view update PDUs

Do not rely on focus side effects alone.

Add explicit RPCs such as:

```rust
SetClientActiveTab {
    window_id: WindowId,
    tab_id: TabId,
}

SetClientActivePane {
    window_id: WindowId,
    tab_id: TabId,
    pane_id: PaneId,
}
```

`SetFocusedPane` can remain for liveness/input semantics, but it should not be
the only mechanism defining tab selection.

Explicit protocol is easier to reason about and test.

### Make client view identity explicit in handshake

The connection handshake should include a stable `ClientViewId`.

That is the server key for:

- which tab this client is looking at in each window
- which pane this client considers active in each window
- restoring per-client view state on reconnect

Do not infer view identity from:

- process id
- host/user pair
- focused pane
- SSH session lifetime

## Mux API Changes

### Identity-aware read path

Add new APIs and migrate GUI/client code to them:

- `get_active_tab_for_window_for_client(view_id, window_id)`
- `get_active_tab_for_window_for_current_identity(window_id)`
- `get_active_pane_for_window_for_current_identity(window_id)` if needed

Delete `get_active_tab_for_window(window_id)`.

Callers must choose one of:

- identity-aware resolution
- explicit `window_id + tab_id`
- erroring out when no identity/target is available

### Identity-aware write path

Add:

- `set_active_tab_for_client_view(view_id, window_id, tab_id)`
- `set_active_tab_for_current_identity(window_id, tab_id)`

These should update per-client view state and emit a notification scoped to
frontends that need to repaint.

There should be no shared `window.set_active_*` equivalent left in the steady
state design.

## Frontend Changes

### Remote client domain

Reconcile should apply `client_window_view_state` from the server, not global
active tabs.

That lets the MacBook and desktop mirrors stay different even while sharing the
same remote panes.

### GUI on the server host

This is the part that prevents the solution from being "just a remote-client
hack".

The local GUI must also participate as a first-class client view:

- it needs a stable `ClientViewId`
- tab switching must update client view state
- active-tab reads in `TermWindow`, spawn logic, pane selection, tab bar, and
  commands must be identity-aware

If the server-host GUI bypasses client view state, the feature is broken.

## Notifications

The current notification surface is centered around shared mux mutation.

For per-client view state, add an explicit notification such as:

```rust
MuxNotification::ClientWindowViewStateChanged {
    view_id: Arc<ClientViewId>,
    window_id: WindowId,
}
```

A frontend should ignore view-state notifications for other clients.

That keeps repaint traffic scoped and avoids cross-client churn.

## Session Persistence

Shared mux session persistence should continue to save:

- windows
- tabs
- panes
- shared titles
- shared geometry

Per-client view state should be persisted separately, if at all.

Recommended approach:

- do not mix per-client view state into shared `session.json`
- if persistence is desired, store it in a separate client-view-state file keyed
  by `ClientViewId`

That prevents the shared session file from becoming polluted by per-machine UI
preferences.

Also:

- do not try to reconstruct active tabs from shared pane/tree state
- restoring shared state and restoring client view state should be two separate
  steps

## Implementation Plan

### Phase 1: Data model and protocol

- Add `ClientViewId`
- Extend client registration/handshake to include it
- Add per-client `window_view_state`
- Add explicit `SetClientActiveTab` PDU
- Replace `ListPanesResponse.active_tabs` with client-specific view-state
  snapshot
- Bump codec version

### Phase 2: Identity-aware reads and writes

- Add identity-aware mux getters/setters
- Delete shared active-tab getters/setters from the public mux API
- Convert GUI tab switching to write per-client active tab
- Convert GUI active-tab reads to use per-client resolution
- Convert spawn/split context lookup to use per-client active tab

### Phase 3: Reconcile and notifications

- Apply client-specific view state in remote reconcile
- Add client-scoped notifications
- Make repaint/update paths ignore other clients' view notifications

### Phase 4: Cleanup

- Remove `Window.active` from the window model
- Remove call sites that assume one global active tab for GUI behavior
- Update tests and docs

## Test Plan

The implementation should not proceed without a broad test matrix. This feature
changes a core ownership boundary and has many subtle failure modes.

## Failure-Point Test Matrix

### 1. Data model and identity

- creating a new client view with no prior state yields no active tab until one
  is explicitly chosen
- per-client window view state stores separate active tabs for multiple windows
  on the same client
- two different `ClientViewId`s can store different active tabs for the same
  window
- reconnect with the same `ClientViewId` restores prior per-window active tabs
- reconnect with a different `ClientViewId` does not inherit another client's
  active tabs
- unregistering a connection does not delete persistent client-view state unless
  explicitly configured to do so
- stale view-state entries for removed windows/tabs are cleaned up safely

### 2. Protocol

- handshake without a `ClientViewId` fails cleanly
- `ListPanesResponse` returns only the requesting client's view state
- `SetClientActiveTab` updates only the targeted client view
- `SetClientActivePane` updates only the targeted client view
- out-of-date `tab_id` or `window_id` in client-view PDUs fails loudly and does
  not mutate unrelated state
- reconnect after codec bump resyncs correctly with the new view-state snapshot

### 3. Mux resolution semantics

- identity-aware active-tab lookup resolves the correct tab for the current
  client view
- active-tab lookup without identity fails instead of silently picking a random
  or first tab
- explicit `window_id + tab_id` paths bypass client-view lookup correctly
- deleting `Window.active` does not regress code that targets panes explicitly
- active-pane resolution within a tab stays client-local and consistent with the
  selected tab

### 4. Remote reconcile

- desktop and laptop attached to the same window select different tabs and stay
  different after repeated reconciles
- reconcile from client A does not overwrite client B's active tab
- reconcile after server-side tab creation/removal remaps client-local active
  tab correctly if the active tab still exists
- reconcile after active tab deletion clears or reassigns the client's active
  tab deterministically
- out-of-order notifications followed by full reconcile converge correctly

### 5. Local GUI behavior

- server-host GUI can keep a different active tab from a remote client
- tab bar highlight uses per-client active tab
- window title formatting uses the current client's active tab and active pane
- pane selection overlay operates on the current client's active tab only
- keyboard tab activation changes only the current client view
- mouse tab clicks change only the current client view

### 6. Spawn, split, and pane targeting

- spawn uses the current client's active tab in the selected window
- split uses the current client's active tab in the selected window
- two clients can spawn/split in different tabs of the same shared window
  without switching each other
- CLI commands with explicit pane IDs still behave identically
- CLI commands that previously depended on implicit active tab now require
  explicit target or client identity and fail cleanly otherwise

### 7. Focus interactions

- changing tabs updates per-client active pane/view state coherently
- focusing a pane in a non-active tab either activates that tab for that client
  or fails consistently, depending on chosen semantics
- `SetFocusedPane` does not accidentally become the authoritative active-tab
  source again
- focus notifications from one client do not move another client's active tab

### 8. Multi-window behavior

- one client can have different active tabs in window A and window B
- switching tabs in window A does not affect that client's window B
- switching tabs in window A does not affect another client's window A
- creating a new window initializes a fresh per-client active tab state

### 9. Removal and cleanup cases

- removing the active tab for one client leaves other clients' selections alone
  where possible
- closing the last tab in a window clears per-client view state for that window
- removing a window deletes per-client view state for that window
- deleting a pane that is active for one client reassigns active pane within the
  same tab without touching other clients

### 10. Persistence and restore

- shared `session.json` restore rebuilds windows/tabs/panes correctly without
  any client-view data mixed in
- optional per-client view-state restore reapplies per-window active tabs after
  shared session restore
- restoring shared session state without any client-view snapshot leaves active
  tab unset until the client chooses one
- restoring two client snapshots does not cross-apply tabs between machines

### 11. Notifications and repaint scoping

- `ClientWindowViewStateChanged` only repaints the intended client
- unrelated clients ignore another client's view-state notifications
- shared mux mutations still notify all interested clients when required
- repaint storms do not occur when two clients switch tabs rapidly

### 12. Race and stress cases

- two clients switching tabs rapidly in the same window never converge to a
  shared state unless they explicitly choose the same tab
- rapid switch/split/close cycles do not leave dangling client-view references
- reconnect during active-tab churn converges correctly after full resync
- suspend/resume or temporary disconnect does not cause another client's active
  tab to overwrite the returning client's view state

### 13. Error handling

- corrupted persisted client-view snapshot is rejected without damaging shared
  mux state
- unknown `ClientViewId` in incoming updates is rejected cleanly
- identity-less local code paths panic only in tests or return clear errors in
  production, but never silently guess
- stale tab ids in view-state snapshots are dropped with structured logging

### 14. Regression coverage

- single-client behavior remains unchanged from the user's perspective
- session restore still restores shared windows/tabs/panes correctly
- layout, spawn sizing, and resize tests still pass
- active workspace behavior remains per-client and unaffected
- existing focused-pane behavior remains correct under the new active-tab model

## Risks

### Hidden assumption: active pane is also shared today

Active tab and active pane are related. A future cleanup may want to move more
of "selection/focus inside a tab" into per-client view state as well.

This design leaves room for that by creating a `ClientWindowViewState` struct
instead of adding yet another standalone map.

### Identity-less code paths

Some current APIs assume there is always a meaningful implicit active tab for a
window. After removing `Window.active`, that assumption is invalid.

Those paths must be rewritten to:

- require current client identity
- require explicit `tab_id` or `pane_id`
- or fail explicitly

That is good for correctness, but it will flush out a lot of lazy assumptions.

## Recommendation

Implement this as a real "per-client view state" feature, not as a narrower
"desynchronize active tab" tweak.

That means:

- stable client view identity
- explicit protocol
- identity-aware mux APIs
- local GUI and remote clients using the same model
- no shared `Window.active` fallback anywhere in the steady-state design

Anything smaller will either break on reconnect, fail for the server-host GUI,
or turn into sync heuristics again.
