# Wakterm development guidance

## Documentation scope

Describe the documented subject's own observable behavior. Do not explain it by
listing unrelated work it does not perform, and do not expose implementation
details owned by another Wakterm component. Include a negative guarantee or a
cross-component mechanism only when readers need it to use or implement the
documented contract correctly.

## Multi-client mux synchronization

Treat the server mux as the authority for shared window, tab, pane, and layout state. A GUI client has a local mirror of that state. Local active-tab and active-pane selection may remain client-specific unless a feature explicitly makes them shared.

A `ClientDomain` does not own a local GUI window. It owns the mappings between local mirror IDs and the IDs in one remote domain. A local window can contain tabs from more than one domain, so domain-specific requests must extract only tabs that map to the target remote window and translate every local ID to its remote ID. Never send local IDs to the server and never fall back to treating an unmapped local ID as a remote ID.

Use one atomic request for an aggregate state change. Resizing a split layout and reordering tabs must send the complete intended state in one request, not a series of per-pane or per-tab mutations. Route every interaction that performs the same operation, including divider drags and keyboard or mouse tab moves, through the same canonical request path. Remove or disable redundant legacy sends so they cannot interleave with the atomic update.

Separate local user intent from application of remote state. The user-intent path may send a request. The remote-notification and reconciliation paths must apply state through a no-send or no-notify path so they cannot create feedback loops.

Track notification origin explicitly. Do not infer self-echoes from timing. Do not reintroduce the historical two-second `recent_resizes` heuristic because it could suppress a legitimate resize from another client, especially during continuous resizing. Attach the initiating client ID to the internal mux mutation or notification, suppress the echo only for that client, and still forward the update to every other client. The wire notification does not need to expose the origin if dispatch handles it server-side.

Coalescing must retain the newest desired state. If an update or reconciliation is already in flight, record that another pass is pending and run it after the current pass completes. Do not simply drop overlapping work. For rapidly changing aggregate state, use a single-flight latest-state sender so older asynchronous requests cannot arrive after and overwrite a newer user action.

Validate aggregate requests before mutation. Reject duplicate IDs, unknown IDs, IDs from another window, and lists that do not exactly match the current object set. Apply a valid change under one mutation boundary, preserve active objects by ID rather than position, release locks, then emit one notification for the final state. On a validation or mapping mismatch, leave state unchanged and reconcile from the server.

For concurrent edits, prefer deterministic last-accepted-request-wins behavior until evidence justifies revisions or conflict detection. Make the full-state request idempotent so retries and duplicate delivery are harmless.

Test synchronization with at least two clients and the server view. Cover the originating client, a second connected client, disconnect and reconnect, rapid repeated edits, edits from both clients close together, missing or stale mappings, and persistence across server restart where applicable. Verify both the visible result and the authoritative server order or layout. A single-client test cannot establish that echo suppression and cross-client convergence both work.

When diagnosing flicker or stale state, identify the mutation, ID-translation, notification, dispatch, and reconciliation layers separately. Change one causal layer at a time. First test the cheapest counterfactual that could disprove the proposed cause, and do not turn a timing workaround into a protocol contract.

## Agent lifecycle and recovery

A tab may contain zero, one, or multiple agent panes. Never model a tab as an
agent identity or assume that its active pane is the tab's only agent. Tab
titles and badges are presentation metadata. An effective title may resolve a
human-facing route label to a current candidate, but watching, delivery,
lifecycle, and recovery must use exact pane and provider identity. Effective
titles are recomputed from current state and do not imply that a tab or pane
survived recreation.

Follow the lifecycle and protocol boundaries in [docs/agent-lifecycle.md](docs/agent-lifecycle.md). Detection, confirmed adoption, restorability, and managed mode are distinct guarantees. Do not persist weak process or title detection as session identity, and do not treat a PID as a recovery handle.

When restoring an expected harness, resume the exact confirmed provider session or surface a visible failure. Never silently substitute a fresh shell or a new provider session. Keep PTY restoration separate from future managed backends, and do not automatically promote a live PTY into managed mode.

For future managed backends, use ACP as the default integration boundary when its implementation passes the required lifecycle tests. Keep native provider protocols available behind the same internal interface when a concrete capability, reliability, recovery, or diagnostic gap justifies them.
