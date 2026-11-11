# Local Codex agent gateway

This local jj checkout adapts mmonad/codex-acp-gateway for Codex CLI 0.167.1.
Changes have not been published upstream.

## Launch

Run `codex-acp` from your project. Its shared `/model` menu includes:

- Antigravity: Gemini Carbon, Argon, and Flash 3.8 only; low/medium/high.
- Muse: all five advertised models, via the native MSP Rust backend. Efforts
  are minimal/low/medium/high/xhigh/max, except Spark 1.2 has no max.
- Claude: models and per-model efforts advertised by claude-agent-acp.
- Grok: models and per-model efforts advertised by `grok agent stdio`, including
  custom models already configured in Grok.

Catalogs are discovered at launch. Availability in a catalog does not guarantee
account access or quota for every model. Discovery failures are logged in
`~/.codex/acp/all/gateway.log`; other available agents remain listed.

Model identifiers are namespaced, for example `muse/muse-wonder-full-1.0`.
Default selection remains Antigravity Flash/high until you save another choice
in the gateway. Normal `codex` and `~/.codex/config.toml` are unchanged.

```sh
codex-acp
codex-acp all -- --no-alt-screen
codex-acp all -- resume THREAD_ID
codex-acp muse
codex-acp antigravity
codex-acp claude
codex-acp grok
codex-acp --list
```

Pass additional Codex options after an explicit agent (or `all`) and `--`.
The launcher uses a temporary loopback WebSocket port and stops the gateway
on exit. Resume through this launcher, not the temporary URL printed by Codex.

## Switching agents

Within one thread, `/model` can switch agents. Each agent keeps its own durable
session. On its next turn it receives user/assistant text it has not seen from
other agents. This shares conversation text with the selected provider.
Tool execution state and hidden reasoning remain with their original agent;
commands are never replayed. All agents use the same project directory.
The router saves the combined visible history and bindings under
`~/.codex/acp/all/threads/`, with backend data below `all/AGENT/`.
Named launches use `~/.codex/acp/AGENT/` separately. Resume old single-agent
threads through their named launch. Switch agents only between turns.

## Add an agent

Edit `~/.codex/acp/agents.json`. No router code changes are needed for another
ACP agent. Each entry takes `command`, optional `args`, `env`, `models`
(allowlist), `model` (initial selection), `effort`, `enabled`, and `backend`
(`acp` by default, or `muse-msp`). Use absolute executable paths and keep
credentials in the environment. The shared default agent is `default_agent`.

```json
{
  "default_agent": "another-agent",
  "agents": {
    "another-agent": {
      "command": "/absolute/path/to/acp-server",
      "args": [],
      "model": "advertised-model-id"
    }
  }
}
```

The local Claude adapter is pinned to `@agentclientprotocol/claude-agent-acp`
0.106.0 under `~/.local/share/codex-acp/adapters`. Existing agent logins are used;
listing auth methods does not initiate OAuth.

Muse runs `muse serve`, using muse-code-sdk 1.4.14 with installed host 1.4.15.
Their schema fingerprints differ, so the gateway logs a warning. The supported
methods have been tested live. MSP snapshots omit reasoning effort; the gateway
persists acknowledged selections and reapplies them through MSP on resume.
MSP streaming loss fails the turn visibly instead of claiming success.

## Build and verify

```sh
cargo build --release --locked -p codex-acp-gateway
cargo test --locked -p codex-acp-gateway --lib
python3 scripts/test-router.py
uv run --with jsonschema scripts/smoke.py
uv run --with jsonschema scripts/smoke-shared.py --config ~/.codex/acp/agents.json --gateway target/release/codex-acp-gateway --live
```

The shared smoke test validates responses and notifications against schemas
emitted by the installed Codex binary. It checks catalogs, model-specific
limits, Muse max effort, file-tool execution, conversation continuity across
all four agents, returning to Muse, and restart/resume. `--live` uses account
quota. Temporary test artifacts and agent-side session records remain for
inspection. Install the release executable, `scripts/codex-acp`, and
`scripts/codex-acp-router` into `~/.local/bin`.

## Current limits

This integrates coding agents, not raw model APIs. Each agent runs its own
tools and permission policy; approval requests are forwarded to Codex. Native
Muse approvals offer once-only choices, without persisting policy grants.
OpenAI service-tier settings carried by Codex model selections are ignored;
standalone tier changes are rejected. Codex sandbox, plugins, remote apps, collaboration and review settings do not
automatically apply to these agents. Upstream Linux bwrap support provides no
macOS sandbox isolation. The shared router rejects fork and rollback.
Muse and the shared cross-agent handoff currently support text conversations;
multimodal continuity and advanced agent features are not verified.

## Verified locally (2026-11-11)

The release executable is installed at `~/.local/bin/codex-acp-gateway`
(SHA-256 `e3c7726959312d57bf4b48f6e1b7d89aefb6a3aeebf66ca971fc61ebb4a7dece`).
Checks passed: 18 Rust tests; seven router tests; current Codex protocol schemas;
38 catalog entries; live replies across Muse, Antigravity, Claude and Grok;
Muse file reading; shared text handoffs and restart/resume; all six Muse effort
settings and immediate cancellation. The real Codex terminal menu selected
Muse Wonder Full/low, completed a live reply, and restored that model and
visible history through the installed launcher after restart. No screenshot-based visual
review was performed. Model catalog entries beyond those live checks remain
advertised, not individually generation-tested.

Pre-upgrade launcher, binary and registry backups are in
`~/.codex/acp/backups/pre-shared-msp/`.
