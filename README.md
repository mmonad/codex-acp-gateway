# codex-acp-gateway

A protocol translation gateway that enables any
[Codex](https://github.com/openai/codex) app-server client to communicate with
any [ACP](https://agentclientprotocol.com/) (Agent Client Protocol) agent.

Codex clients speak a custom JSON-RPC dialect over stdio (NDJSON without the
`"jsonrpc"` field). ACP agents speak standard JSON-RPC 2.0 over stdio (NDJSON
with `"jsonrpc": "2.0"`). The gateway sits between them and translates
bidirectionally.

```
 Codex Client           codex-acp-gateway             ACP Agent
 (VS Code, CLI)  <--->  [translate + proxy]  <--->  (Gemini, etc.)
     stdio NDJSON            Rust / tokio            stdio NDJSON
  (no "jsonrpc")                                  ("jsonrpc":"2.0")
```

## Why

Codex has a rich ecosystem of clients (VS Code extension, CLI, web). ACP is an
emerging open protocol backed by Google, Zed, and others that many agents are
adopting. This gateway lets you use any ACP agent as the backend for any Codex
client without modifying either side.

## Installation

```bash
# Clone and build
git clone <repo-url> && cd codex-acp-gateway
cargo build --release

# Binary is at target/release/codex-acp-gateway
```

Requires Rust 2024 edition (1.85+).

## Usage

```bash
# Stdio mode (default) — pipe Codex protocol on stdin/stdout
codex-acp-gateway --agent-cmd <agent-binary> [-- agent-args...]

# WebSocket mode — accept Codex clients over WebSocket
codex-acp-gateway --listen ws://0.0.0.0:8080 --agent-cmd <agent-binary> [-- agent-args...]

# Example: use a Gemini ACP agent with Codex CLI
codex-acp-gateway --agent-cmd gemini -- --experimental-acp
```

### Transport modes

The `--listen` flag controls how Codex clients connect to the gateway:

| Value              | Behavior                                             |
|--------------------|------------------------------------------------------|
| `stdio://` (default) | Read/write Codex NDJSON on stdin/stdout. Single client, exit on EOF. |
| `ws://IP:PORT`     | Accept WebSocket connections. Each client gets its own ACP agent subprocess. |

In stdio mode, the gateway reads Codex protocol messages from stdin and writes
responses to stdout.

In WebSocket mode, the gateway listens for connections on the given address.
Each connected client gets an independent ACP agent subprocess — there is no
shared state between connections.

### Options

- `--agent-cmd` — path or name of the ACP agent binary to spawn
- `--listen <URL>` — transport endpoint (`stdio://` or `ws://IP:PORT`)
- `--cwd <PATH>` — working directory for the agent subprocess (default: `.`)
- `--log-level <LEVEL>` — `trace`, `debug`, `info`, `warn`, `error` (default: `info`)
- Everything after `--` is forwarded as arguments to the agent process

## Protocol mapping

| Codex (client -> server)             | Direction | ACP (client -> agent)           |
|--------------------------------------|-----------|---------------------------------|
| `initialize`                         |    -->    | `initialize`                    |
| `thread/start`                       |    -->    | `session/new`                   |
| `turn/start`                         |    -->    | `session/prompt`                |
| `turn/interrupt`                     |    -->    | `session/cancel`                |
| _item/* notifications_               |    <--    | `session/update` notifications  |
| `item/commandExecution/requestApproval` | <--    | `session/requestPermission`     |
| `item/fileChange/requestApproval`    |    <--    | `session/requestPermission`     |

ACP callbacks handled locally by the gateway (not forwarded to Codex):

| ACP callback             | Gateway behavior               |
|--------------------------|--------------------------------|
| `fs/readTextFile`        | Read file from local filesystem |
| `fs/writeTextFile`       | Write file to local filesystem  |
| `terminal/create`        | Spawn subprocess locally        |
| `terminal/output`        | Capture command output           |
| `terminal/release`       | Clean up subprocess              |
| `terminal/waitForExit`   | Wait for process completion      |
| `terminal/kill`          | Terminate subprocess             |

## Project structure

```
codex-acp-gateway/
  Cargo.toml                  Workspace root + gateway binary
  src/
    main.rs                   Entry point, CLI parsing, runtime setup
    config.rs                 CLI args and configuration
    error.rs                  Gateway error types
    transport/                Codex-side transport (stdio NDJSON + WebSocket)
    acp/                      ACP subprocess management + Client trait impl
    translation/              Bidirectional protocol translation engine
  crates/                     Vendored from codex, maintained locally
    app-server-protocol/      Codex wire types (JSON-RPC messages, v1/v2)
    protocol/                 codex-protocol (core domain types)
    codex-experimental-api-macros/
    execpolicy/               Sandbox execution policy
    utils/                    Transitive deps (absolute-path, cache, image, git)
  docs/
    ARCHITECTURE.md           Deep technical architecture documentation
```

## Key dependencies

- **codex-app-server-protocol** (vendored) -- all Codex JSON-RPC request/response/notification types
- **agent-client-protocol** v0.9 (crates.io) -- official ACP Rust SDK (Agent + Client traits, wire types)
- **tokio** -- async runtime (full features)
- **serde** / **serde_json** -- serialization
- **clap** -- CLI argument parsing
- **tokio-tungstenite** -- WebSocket transport
- **tracing** -- structured logging
- **uuid** v7 -- correlation IDs

## License

Apache-2.0
