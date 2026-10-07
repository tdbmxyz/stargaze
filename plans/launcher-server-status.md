# Launcher server status

## Issues to Address

- Show a small status indicator in each saved host's launcher row.
- Report server startup, readiness, and graceful shutdown without starting a stream or interfering with an active client.
- Refresh status without blocking SDL or leaving probe tasks behind when the launcher exits.

## Important Notes

- A failed probe means stopped **or unreachable**, not proof that the remote process is absent.
- Use TCP on the configured streaming port (QUIC uses UDP). LAN users must allow both protocols through their firewall.
- The status listener starts before capture initialization, remains alive through graceful teardown, and does not control the streaming pipeline.
- Older servers have no status listener: show stopped/unreachable but still permit connecting.
- Brief starting/stopping states can be missed between polls; do not invent lifecycle transitions from connectivity changes.
- Keep the protocol small, versioned, bounded, and separate from session/control messages.

## Implementation Strategy

- Add a shared status protocol, timeout-bounded probe, and lifecycle listener.
- Wire the listener into the server lifecycle, including SIGTERM shutdown.
- Maintain asynchronous per-endpoint probe state in the launcher, reconcile it after host edits/deletions, and cancel tasks on exit.
- Draw a colored status icon with a short text label so meaning does not depend only on color.
- Document the extra TCP listener and reachability limitations.

## Tests

- Loopback status round trips for each lifecycle state, without capture or GPU dependencies.
- Refused, malformed, silent, and timed-out probes.
- Listener handles concurrent clients and stops when its handle is dropped.
- Launcher polling preserves status for unchanged endpoints, clears edited endpoints, rejects stale results, and cancels in-flight tasks.
- Run formatting, workspace check, all-target pedantic clippy, and workspace nextest.
