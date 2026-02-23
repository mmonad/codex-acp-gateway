# AI Agent Instructions for codex-acp-gateway

## Quick reference

```bash
# Build the gateway binary
cargo check -p codex-acp-gateway

# Build entire workspace (includes vendored crates)
cargo check --workspace

# Run tests
cargo test --workspace

# Format
cargo fmt --all

# Lint
cargo clippy --workspace
```

## Project overview

This is a Rust protocol translation gateway between Codex app-server clients and
ACP (Agent Client Protocol) agents. The gateway:

1. Listens on stdio or WebSocket for Codex NDJSON messages (JSON-RPC without `"jsonrpc"` field)
2. Spawns an ACP agent subprocess, connects via stdio NDJSON (JSON-RPC 2.0 with `"jsonrpc": "2.0"`)
3. Translates messages bidirectionally between the two protocols
4. Handles ACP filesystem/terminal callbacks locally

## Workspace structure

```
Cargo.toml                    Workspace root, defines path deps + external deps
src/                          Gateway binary source
  main.rs                     Entry point: CLI parsing, tokio runtime, orchestration
  config.rs                   CLI argument definitions (clap), --listen transport modes
  error.rs                    Gateway-specific error types (thiserror)
  transport/                  Codex-side transport (stdio NDJSON + WebSocket)
  acp/                        ACP-side: subprocess lifecycle, Client trait impl
  translation/                Core translation engine between protocols
crates/
  app-server-protocol/        All Codex wire types (vendored from codex repo)
    src/protocol/common.rs    Client/Server request/notification enum definitions (via macros)
    src/protocol/v1.rs        Legacy Codex API types
    src/protocol/v2.rs        Current Codex API types (thread/*, turn/*, item/*)
    src/jsonrpc_lite.rs       JSON-RPC message types (no "jsonrpc" field)
  protocol/                   codex-protocol: core domain types (ThreadId, ResponseItem, etc.)
  codex-experimental-api-macros/  Proc macros for experimental field gating
  execpolicy/                 Sandbox execution policy definitions
  utils/                      Transitive utility crates
```

## Protocol translation patterns

### Core mapping

| Codex method           | ACP method         | Notes                                    |
|------------------------|--------------------|------------------------------------------|
| `initialize`           | `initialize`       | Map ClientInfo <-> Implementation         |
| `thread/start`         | `session/new`      | Map thread params to session config        |
| `turn/start`           | `session/prompt`   | Map InputItem[] to ContentBlock[]          |
| `turn/interrupt`       | `session/cancel`   | Notification (no response expected)        |

### Streaming (ACP -> Codex)

ACP sends `session/update` notifications with `SessionUpdate` variants:
- `AgentMessageChunk` -> `item/agentMessage/delta`
- `ToolCall` / `ToolCallUpdate` -> `item/started` + `item/completed` + delta notifications
- `Plan` -> `item/plan/delta`

### Approval flows (ACP -> Codex -> ACP)

ACP `session/requestPermission` -> Codex `item/commandExecution/requestApproval`
or `item/fileChange/requestApproval` -> user decision -> ACP response

### ACP callbacks handled locally

The gateway implements the ACP `Client` trait to handle:
- `fs/readTextFile`, `fs/writeTextFile` -- direct filesystem I/O
- `terminal/create`, `terminal/output`, `terminal/release`, etc. -- subprocess management

These never reach the Codex client.

## Key types

### Codex side (codex-app-server-protocol)
- `JSONRPCMessage` -- top-level wire envelope (no `"jsonrpc"` field)
- `ClientRequest` -- all client-to-server requests (tagged by `method`)
- `ServerNotification` -- all server-to-client notifications
- `ServerRequest` -- server-to-client requests (approval flows)
- `v2::ThreadStartParams`, `v2::TurnStartParams` -- current API params
- `v2::ItemStartedNotification`, `v2::AgentMessageDeltaNotification` -- streaming

### ACP side (agent-client-protocol)
- `Agent` trait -- methods the agent implements (initialize, new_session, prompt, cancel)
- `Client` trait -- methods the client implements (request_permission, session_notification, fs/terminal ops)
- `SessionUpdate` -- streaming notification variants (AgentMessageChunk, ToolCall, etc.)
- `ContentBlock` -- prompt content (TextContent, ImageContent, EmbeddedResource)

## Important implementation notes

### Transport modes

The gateway supports two transport modes for the Codex client side:
- **Stdio** (`--listen stdio://`, default): one client via stdin/stdout, exit on EOF
- **WebSocket** (`--listen ws://IP:PORT`): each WS client gets its own ACP subprocess

Both modes use the same `mpsc` channel interface internally. The event loop
(`run_session()`) is completely transport-agnostic.

In WebSocket mode, each connection is handled via `spawn_local` inside the
top-level `LocalSet`. The WS reader/writer tasks use `tokio::spawn` (they are
`Send`), but session logic runs on the `LocalSet` because of ACP's `!Send`
futures.

### ACP uses !Send futures

The ACP SDK uses `async_trait(?Send)` -- all futures are `!Send`. This means:
- The top-level `run()` wraps everything in a `tokio::task::LocalSet`
- The ACP subprocess connection must live on a single-threaded runtime or LocalSet
- Do NOT spawn ACP futures with `tokio::spawn` (requires Send); use `tokio::task::spawn_local`
- In WebSocket mode, per-connection sessions use `spawn_local` (not `tokio::spawn`)

### JSON-RPC framing differences

- **Codex**: NDJSON lines, no `"jsonrpc"` field. Messages have `id` + `method` + `params`.
- **ACP**: NDJSON lines, includes `"jsonrpc": "2.0"`. Standard JSON-RPC 2.0.

When translating, the gateway must add/strip the `"jsonrpc"` field.

### Request ID management

The gateway must maintain a mapping between Codex request IDs and ACP request
IDs since both sides generate their own IDs independently. Use a bidirectional
map to correlate requests with their responses.

### Codex protocol versioning

The vendored `codex-app-server-protocol` crate has both v1 (legacy) and v2
(current) APIs. The gateway should target v2 methods:
- `thread/start` (not `newConversation`)
- `turn/start` (not `sendUserTurn`)
- `item/*` notifications (not legacy event notifications)

## Testing

- Unit tests for translation functions: test each direction independently
- Integration tests: spawn a mock ACP agent, send Codex messages, verify output
- The vendored crates have their own test suites: `cargo test -p codex-app-server-protocol`

## Reusing Codex app-server implementations

When implementing gateway features that are **outside ACP translation** (i.e.,
Codex-specific behavior with no ACP equivalent), reuse the official Codex
app-server implementation from `../codex/` as much as possible. This includes:

- Handlers for Codex-specific requests (e.g., `experimentalFeature/list`)
- Local feature logic (e.g., turn diff tracking, token usage formatting)
- Self-contained utility modules that can be adapted without pulling in the
  full Codex agent (e.g., `turn_diff_tracker.rs`)

Prefer adapting Codex's existing code over writing new implementations from
scratch to stay consistent with Codex client expectations and reduce drift.

## What NOT to implement

Do not implement Codex app-server methods that are **agent-triggered** but have
**no ACP equivalent**. If there is no ACP event that can trigger a Codex
notification or server request, the gateway has no source event to translate —
the implementation would be dead code. Examples: `item/tool/requestUserInput`,
`item/commandExecution/terminalInteraction`.

The exception is **client-initiated queries** that the gateway can answer
locally without an ACP trigger (e.g., `experimentalFeature/list` returns a
static empty list).

## Common pitfalls

1. **!Send futures**: ACP SDK requires LocalSet. See note above.
2. **Missing "jsonrpc" field**: Codex messages omit it. Don't accidentally require it when parsing Codex input.
3. **Notification vs request**: `session/cancel` (ACP) is a notification, not a request. `turn/interrupt` (Codex) is a request that expects a response.
4. **ID correlation**: Both protocols use independent ID spaces. Always translate IDs through a correlation map.
5. **Streaming backpressure**: ACP agent may send many `session/update` notifications rapidly. Buffer and translate them without blocking the agent.
