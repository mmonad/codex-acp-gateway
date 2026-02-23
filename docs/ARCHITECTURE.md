# Architecture

## Overview

codex-acp-gateway is a protocol translation gateway that bridges the Codex
app-server protocol with the Agent Client Protocol (ACP). It enables any Codex
client (VS Code extension, CLI, web) to use any ACP-compliant agent as its
backend.

## Protocol comparison

| Aspect                | Codex app-server protocol                    | ACP (Agent Client Protocol)               |
|-----------------------|----------------------------------------------|--------------------------------------------|
| **Transport**         | stdio NDJSON                                 | stdio NDJSON                               |
| **Wire format**       | JSON-RPC (no `"jsonrpc"` field)              | JSON-RPC 2.0 (`"jsonrpc": "2.0"`)         |
| **Session concept**   | Thread (thread/start, thread/resume)         | Session (session/new, session/load)        |
| **Prompt concept**    | Turn (turn/start)                            | Prompt (session/prompt)                    |
| **Streaming**         | Server notifications (item/* family)         | session/update notifications               |
| **Approval flow**     | Server requests (item/*/requestApproval)     | Client request (session/requestPermission) |
| **Cancellation**      | Client request (turn/interrupt) with response| Client notification (session/cancel)       |
| **File operations**   | Handled by codex-core internally             | Client callbacks (fs/readTextFile, etc.)   |
| **Terminal ops**      | Handled by codex-core internally             | Client callbacks (terminal/* family)       |
| **Request IDs**       | String or Integer, independent space         | Integer, independent space                 |

## Data flow

```
                          codex-acp-gateway
                    +--------------------------+
                    |                          |
  Codex Client      |   +------------------+   |      ACP Agent
  (stdin/stdout)    |   |                  |   |   (stdin/stdout)
       |            |   |   Translation    |   |        |
       |   Codex    |   |     Engine       |   |  ACP   |
       |  NDJSON    |   |                  |   | NDJSON  |
       v            |   +--------+---------+   |         v
  +---------+       |            |             |   +---------+
  |         | read  |   +--------v---------+   |   |         |
  | Codex   |------>|   |  Request ID      |   |   |  ACP    |
  |Transport|       |   |  Correlation Map |   |   | Process |
  |  Layer  |<------|   +------------------+   |   | Manager |
  |         | write |            |             |   |         |
  +---------+       |   +--------v---------+   |   +---------+
       ^            |   | Local Handlers   |   |        ^
       |            |   | - fs read/write  |   |        |
       |            |   | - terminal mgmt  |   |        |
       |            |   +------------------+   |        |
       |            |                          |        |
       |            +--------------------------+        |
       |                                                |
       +--- stdin/stdout (Codex client) ----------------+
                                         stdin/stdout (ACP agent subprocess)
```

## Request lifecycle

### Initialization

```
Codex Client                Gateway                    ACP Agent
    |                          |                          |
    |-- initialize ----------->|                          |
    |   {id, method, params}   |-- initialize ----------->|
    |   (no "jsonrpc")         |   {jsonrpc, id, method}  |
    |                          |                          |
    |                          |<- initialize response ---|
    |<- initialize response ---|   {jsonrpc, id, result}  |
    |   {id, result}           |                          |
```

### Thread/Session creation

```
Codex Client                Gateway                    ACP Agent
    |                          |                          |
    |-- thread/start --------->|                          |
    |   ThreadStartParams      |-- session/new ---------->|
    |                          |   NewSessionRequest      |
    |                          |                          |
    |                          |<- session/new response --|
    |<- thread/start response -|   NewSessionResponse     |
    |   ThreadStartResponse    |                          |
    |                          |                          |
    |<- thread/started --------|  (gateway synthesizes)   |
    |   notification           |                          |
```

### Turn/Prompt with streaming

```
Codex Client                Gateway                    ACP Agent
    |                          |                          |
    |-- turn/start ----------->|                          |
    |   TurnStartParams        |-- session/prompt ------->|
    |                          |   PromptRequest          |
    |                          |                          |
    |                          |<- session/update --------|
    |                          |   AgentMessageChunk      |
    |<- item/agentMessage/delta|                          |
    |                          |                          |
    |                          |<- session/update --------|
    |                          |   ToolCall               |
    |<- item/started -----------|                          |
    |<- item/completed ---------|                          |
    |                          |                          |
    |                          |<-- session/requestPermission --|
    |<- item/commandExecution/ |                          |
    |   requestApproval ------>|                          |
    |   (user approves)        |-- permission response -->|
    |                          |                          |
    |                          |<- session/prompt resp ---|
    |<- turn/completed --------|   PromptResponse         |
    |   notification           |                          |
```

### ACP callbacks handled locally

```
                               Gateway                    ACP Agent
                                  |                          |
                                  |<- fs/readTextFile -------|
                                  |   ReadTextFileRequest    |
            (read from disk) ---->|                          |
                                  |-- fs/readTextFile resp ->|
                                  |                          |
                                  |<- terminal/create -------|
                                  |   CreateTerminalRequest  |
          (spawn subprocess) ---->|                          |
                                  |-- terminal/create resp ->|
                                  |                          |
                                  |<- terminal/output -------|
                                  |   (capture output)       |
                                  |-- terminal/output resp ->|
```

## Module responsibilities

### `src/main.rs`
Entry point. Parses CLI arguments, initializes tracing, sets up the tokio
runtime with a `LocalSet` (required for ACP's !Send futures), and orchestrates
the gateway lifecycle.

### `src/config.rs`
Defines the CLI interface using clap. Key options:
- `--agent-cmd`: path/name of the ACP agent binary
- Remaining args after `--`: forwarded to the agent process

### `src/error.rs`
Gateway-specific error types using thiserror. Covers transport errors, protocol
translation errors, and subprocess management failures.

### `src/transport/`
Codex-side NDJSON transport layer:
- **Reader**: Reads NDJSON lines from stdin, deserializes into Codex `JSONRPCMessage` variants
- **Writer**: Serializes Codex messages and writes them as NDJSON to stdout
- Handles the Codex-specific framing (no `"jsonrpc"` field)

### `src/acp/`
ACP subprocess management and Client trait implementation:
- **Process manager**: Spawns the ACP agent binary, manages its stdin/stdout/stderr
- **Client impl**: Implements the ACP `Client` trait to handle callbacks:
  - `request_permission` -> forwards to Codex client as approval request
  - `session_notification` -> translates and forwards to Codex as item/* notifications
  - `fs/readTextFile`, `fs/writeTextFile` -> handles locally via std::fs
  - `terminal/*` -> manages subprocesses locally via tokio::process

### `src/translation/`
Bidirectional protocol translation engine:
- **Codex-to-ACP**: Translates incoming Codex requests to ACP method calls
  - `initialize` params mapping (ClientInfo <-> Implementation)
  - `thread/start` -> `session/new` (ThreadStartParams -> NewSessionRequest)
  - `turn/start` -> `session/prompt` (TurnStartParams + InputItem[] -> PromptRequest + ContentBlock[])
  - `turn/interrupt` -> `session/cancel`
- **ACP-to-Codex**: Translates ACP events to Codex notifications
  - `SessionUpdate::AgentMessageChunk` -> `item/agentMessage/delta`
  - `SessionUpdate::ToolCall` -> `item/started` + tool-specific notifications
  - `SessionUpdate::ToolCallUpdate` -> delta notifications + `item/completed`
  - `SessionUpdate::Plan` -> `item/plan/delta`
  - `PromptResponse` (stop_reason) -> `turn/completed`
- **Approval translation**: Maps between ACP `RequestPermissionRequest` and Codex
  `CommandExecutionRequestApprovalParams` / `FileChangeRequestApprovalParams`
- **ID correlation**: Maintains a bidirectional map of Codex request IDs <-> ACP request IDs

## Key types from each protocol

### Codex (codex-app-server-protocol)

```
JSONRPCMessage (untagged)
  |-- JSONRPCRequest    { id, method, params }
  |-- JSONRPCNotification { method, params }
  |-- JSONRPCResponse   { id, result }
  |-- JSONRPCError      { id, error: { code, message, data } }

ClientRequest (tagged by "method")
  |-- Initialize        { params: InitializeParams }
  |-- ThreadStart       { params: ThreadStartParams }        "thread/start"
  |-- TurnStart         { params: TurnStartParams }          "turn/start"
  |-- TurnInterrupt     { params: TurnInterruptParams }      "turn/interrupt"
  ...

ServerNotification (tagged by "method")
  |-- ThreadStarted     (ThreadStartedNotification)          "thread/started"
  |-- TurnStarted       (TurnStartedNotification)            "turn/started"
  |-- TurnCompleted     (TurnCompletedNotification)          "turn/completed"
  |-- ItemStarted       (ItemStartedNotification)            "item/started"
  |-- ItemCompleted     (ItemCompletedNotification)          "item/completed"
  |-- AgentMessageDelta (AgentMessageDeltaNotification)      "item/agentMessage/delta"
  ...

ServerRequest (tagged by "method")
  |-- CommandExecutionRequestApproval  "item/commandExecution/requestApproval"
  |-- FileChangeRequestApproval        "item/fileChange/requestApproval"
  ...
```

### ACP (agent-client-protocol)

```
Agent trait (methods the agent implements)
  |-- initialize(InitializeRequest) -> InitializeResponse
  |-- new_session(NewSessionRequest) -> NewSessionResponse
  |-- prompt(PromptRequest) -> PromptResponse
  |-- cancel(CancelNotification)
  |-- load_session(LoadSessionRequest) -> LoadSessionResponse
  |-- set_session_mode(SetSessionModeRequest) -> SetSessionModeResponse
  ...

Client trait (methods the client/gateway implements)
  |-- request_permission(RequestPermissionRequest) -> RequestPermissionResponse
  |-- session_notification(SessionNotification)      [session/update]
  |-- read_text_file(ReadTextFileRequest) -> ReadTextFileResponse
  |-- write_text_file(WriteTextFileRequest) -> WriteTextFileResponse
  |-- create_terminal(CreateTerminalRequest) -> CreateTerminalResponse
  |-- terminal_output(TerminalOutputRequest) -> TerminalOutputResponse
  |-- release_terminal(ReleaseTerminalRequest) -> ReleaseTerminalResponse
  |-- wait_for_terminal_exit(...) -> WaitForTerminalExitResponse
  |-- kill_terminal_command(...) -> KillTerminalCommandResponse

SessionUpdate (notification variants)
  |-- UserMessageChunk
  |-- AgentMessageChunk
  |-- AgentThoughtChunk
  |-- ToolCall
  |-- ToolCallUpdate
  |-- Plan
  |-- AvailableCommandsUpdate
  |-- CurrentModeUpdate
  |-- ConfigOptionUpdate
```

## Task/channel topology

```
tokio::task::LocalSet
  |
  |-- Codex Reader Task
  |     Reads stdin line-by-line, parses NDJSON
  |     Sends parsed messages to translation engine via mpsc channel
  |
  |-- Codex Writer Task
  |     Receives translated messages from mpsc channel
  |     Serializes and writes to stdout
  |
  |-- ACP Connection Task
  |     Manages the RpcConnection to the ACP subprocess
  |     Routes incoming ACP messages to the Client trait handler
  |     Sends outgoing ACP requests via the connection
  |
  |-- Translation Engine
  |     Receives Codex requests, translates, sends to ACP connection
  |     Receives ACP events (via Client trait), translates, sends to Codex writer
  |     Manages request ID correlation map
  |     Manages session/thread state
  |
  |-- Local Handler Tasks (as needed)
        fs read/write operations
        terminal subprocess management
```

## Translation state machine

```
                    +------------------+
                    |   Disconnected   |
                    +--------+---------+
                             |
                     initialize request
                             |
                    +--------v---------+
                    |   Initializing   |
                    | (waiting for ACP |
                    |  init response)  |
                    +--------+---------+
                             |
                     init response received
                             |
                    +--------v---------+
                    |     Ready        |
                    | (no active       |
                    |  thread/session) |
                    +--------+---------+
                             |
                     thread/start -> session/new
                             |
                    +--------v---------+
                    |  Session Active  |
                    | (thread started, |
                    |  no active turn) |
                    +--------+---------+
                             |
                     turn/start -> session/prompt
                             |
                    +--------v---------+
              +---->|  Turn Active     |<----+
              |     | (streaming item  |     |
              |     |  notifications)  |     |
              |     +--------+---------+     |
              |              |               |
     session/update    prompt response    turn/interrupt
     notifications     (stop_reason)     -> session/cancel
              |              |               |
              |     +--------v---------+     |
              +-----|  Session Active  |-----+
                    | (turn complete,  |
                    |  ready for next) |
                    +------------------+
```

## Error handling strategy

- **Transport errors** (broken pipe, malformed JSON): Log and terminate the gateway
- **Translation errors** (unknown method, missing field): Return JSON-RPC error to the originating side
- **ACP subprocess crash**: Detect via process exit, send error notification to Codex client, terminate
- **Codex client disconnect**: Detect via stdin EOF, send cancel to ACP agent, clean up, exit
