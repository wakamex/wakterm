# Security

Wakterm is a same-user terminal and multiplexer. Any interface that can write terminal input, admit an agent prompt, launch a process, or retrieve a control credential is equivalent to same-user command execution. A process must not gain that authority merely because it can reach a Wakterm socket or claim a trusted client identity.

## Current boundary

On Linux, Wakterm classifies each direct local mux connection before accepting client-supplied metadata. It reads Unix-socket peer credentials, binds namespace inspection to the peer process with a pidfd, and compares the peer's user, mount, PID, and IPC namespaces with the mux server.

A peer in different namespaces receives restricted local authority. Restricted peers may use an explicit metadata-only PDU allowlist, while unlisted and newly added operations are denied by default. They can inspect pane layout, titles, agent identity, harness, lifecycle, and resource status. They cannot read terminal contents, scrollback, images, agent prompts, output, requests, or events, and they cannot send terminal input, admit prompts, launch processes, mutate panes, or retrieve TLS credentials. Wakterm also replaces their claimed client identity with a server-generated identity, removes SSH-agent socket information, omits SSH-agent socket paths from client-list responses, and removes output-derived content from agent status responses.

This is an immediate Linux integrity defense against the demonstrated case where a filesystem-sandboxed process used a host Wakterm pane as a command-execution proxy. It is not a complete sandbox boundary.

Metadata access still exposes pane titles, working directories, process and harness identity, lifecycle state, layout, and resource status. A caller that must not read that metadata must not receive restricted mux access.

Namespace equality also does not prove that a process is unrestricted. Landlock, SELinux, AppArmor, seccomp, and other confinement can apply without distinct namespaces. Other operating systems currently rely on their existing local socket access controls rather than Linux namespace classification.

## Remaining paths around the local check

The direct Unix-socket check does not independently contain:

- A mux descriptor opened by a trusted process and then inherited or transferred into a sandbox.
- Authenticated TLS access using readable cached credentials.
- A loopback TCP, SSH, Unix-socket, or other host proxy that reconnects with greater authority.
- SSH keys, SSH-agent sockets, TLS certificates, or other bearer credentials reachable by the restricted process.
- A host broker such as Panetone accepting a restricted request and forwarding it with the broker's own authority.
- Same-namespace or unsupported-platform confinement that Wakterm cannot currently classify.

Blocking one request such as credential retrieval is insufficient when the same credential is readable from storage or reachable through another service. The sandbox and credential owner must close those paths.

## Target capability model

The long-term boundary should use explicit capabilities rather than executable names, process allowlists, or a single trusted flag. Useful scopes include:

| Capability | Authority |
| --- | --- |
| Observe metadata | List panes, routes, harness types, and lifecycle state without terminal contents. |
| Read terminal data | Read pane contents, scrollback, images, prompts, and agent output. |
| Admit prompts | Submit a prompt to one exact agent and incarnation. |
| Control panes | Send keys or paste, change layouts, resize, focus, park, or close panes. |
| Launch harnesses | Start a reviewed harness profile for an exact project. |
| Retrieve credentials | Obtain TLS, SSH-agent, or other reusable control credentials. |

Every transport entry point must supply authority explicitly. No helper should silently assign full control. New operations must remain denied until their required capability is reviewed.

Capabilities must be preserved or reduced across intermediaries. A restricted caller must not acquire Panetone's, an SSH proxy's, or another broker's host authority. Sending a prompt to a more privileged agent is itself privilege delegation because that agent can act on the prompt.

An operation that intentionally crosses the caller's boundary should require a user-backed grant bound to the exact operation, target, and authority profile. Grants should be one-use or short-lived, revocable, auditable, and unusable as arbitrary shell input. For example, a launcher may accept a reviewed project and harness profile, but a restricted request to launch `danger-full-access` must not be translated into an unrestricted command line without explicit user authority.

## Ownership

The complete boundary spans several components:

- Codex or another sandbox owner prevents restricted processes from freely reaching pre-existing host IPC, inherited descriptors, loopback proxies, and bearer credentials. Explicit grants should expose structured operations rather than an entire control socket.
- Wakterm enforces the connection's capabilities on every mux and Agent API operation. Client-supplied PID, name, route, and view metadata never establish authority.
- Panetone and other brokers classify their own peers and propagate or attenuate caller authority. Read-only inspection may be granted separately from sending, route mutation, prompt admission, and privileged launch.
- The user-facing authorization layer creates narrow elevation grants when a restricted caller intentionally needs a more privileged operation.

`danger-full-access` processes have no sandbox boundary for Wakterm to preserve. Their ability to control same-user Wakterm and Panetone services is expected, subject to ordinary authentication and authorization.

## Verification

Security regression coverage should include:

- Direct host and sandboxed Unix-socket clients.
- Client identity spoofing and restricted proxy registration.
- Panetone or another broker relaying a restricted request.
- Cached TLS credentials and localhost TLS or SSH access.
- Inherited or transferred connected descriptors.
- Same-namespace confinement.
- Passive metadata access versus terminal-content access.
- Windows and macOS behavior once those platforms have an explicit authority source.

Tests should prove that permitted observation still works, denied operations have no side effect, a broker cannot amplify authority, and an explicit user grant authorizes only its bound operation.

## Reporting a vulnerability

Do not publish exploit details or credentials in a public issue. If this repository does not offer private vulnerability reporting, open a minimal issue requesting a private contact channel without including the sensitive details.
