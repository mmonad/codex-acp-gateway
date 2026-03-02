# AI Agent Instructions for codex-acp-gateway

## Quick reference

```bash
cargo check -p codex-acp-gateway   # Build the gateway binary
cargo check --workspace            # Build entire workspace (includes vendored crates)
cargo test --workspace             # Run tests
cargo fmt --all                    # Format
cargo clippy --workspace           # Lint
```

## Project overview

A Rust protocol translation gateway and WebSocket proxy for ACP (Agent Client
Protocol) agents. Two operating modes:

- **`codex` mode** — Translate between Codex app-server protocol and ACP
- **`acp-proxy` mode** — Expose stdio-based ACP agents over WebSocket (no translation)

Gateway source lives in `src/`. Vendored Codex crates (Apache-2.0, from
[openai/codex](https://github.com/openai/codex)) live in `crates/`.

For protocol details, data flow diagrams, and module-level architecture, see
`docs/ARCHITECTURE.md`. For protocol coverage status and known gaps, see
`docs/GAP_ANALYSIS.md`.

## Implementation rules

### ACP uses !Send futures

The ACP SDK uses `async_trait(?Send)` — all futures are `!Send`. This means:
- The top-level `run()` wraps everything in a `tokio::task::LocalSet`
- Do NOT spawn ACP futures with `tokio::spawn` (requires Send); use `tokio::task::spawn_local`
- In WebSocket mode, per-connection sessions use `spawn_local` (not `tokio::spawn`)

### JSON-RPC framing differences

- **Codex**: NDJSON lines, no `"jsonrpc"` field
- **ACP**: NDJSON lines, includes `"jsonrpc": "2.0"` (standard JSON-RPC 2.0)

When translating, the gateway must add/strip the `"jsonrpc"` field.

### Request ID management

Both protocols use independent ID spaces. Always translate IDs through the
bidirectional correlation map (`src/translation/id_map.rs`). Never pass a
Codex ID directly to ACP or vice versa.

### Target Codex v2 API only

The vendored `codex-app-server-protocol` crate has both v1 (legacy) and v2
(current) APIs. The gateway targets v2 methods exclusively:
- `thread/start` (not `newConversation`)
- `turn/start` (not `sendUserTurn`)
- `item/*` notifications (not legacy event notifications)

### Reusing Codex app-server implementations

When implementing gateway features that are **outside ACP translation** (i.e.,
Codex-specific behavior with no ACP equivalent), reuse the official Codex
app-server implementation from the [codex repo](https://github.com/openai/codex)
as much as possible. Prefer adapting Codex's existing code over writing new
implementations from scratch to stay consistent with Codex client expectations.

### What NOT to implement

Do not implement Codex app-server methods that are **agent-triggered** but have
**no ACP equivalent**. If there is no ACP event that can trigger a Codex
notification or server request, the gateway has no source event to translate —
the implementation would be dead code. Example:
`item/commandExecution/terminalInteraction`.

The exception is **client-initiated queries** that the gateway can answer
locally without an ACP trigger (e.g., `experimentalFeature/list` returns a
static empty list).

## Common pitfalls

1. **!Send futures**: ACP SDK requires LocalSet. See above.
2. **Missing "jsonrpc" field**: Codex messages omit it. Don't accidentally require it when parsing Codex input.
3. **Notification vs request**: `session/cancel` (ACP) is a notification, not a request. `turn/interrupt` (Codex) is a request that expects a response.
4. **ID correlation**: Both protocols use independent ID spaces. Always translate IDs through the correlation map.
5. **Streaming backpressure**: ACP agent may send many `session/update` notifications rapidly. Buffer and translate them without blocking the agent.
