# Protocol Gap Analysis

> Audit of codex-acp-gateway coverage against both the Codex app-server
> protocol and the Agent Client Protocol (ACP). Last updated: 2026-02-23.

## Summary

| Area | Implemented | Gap | Notes |
|------|-------------|-----|-------|
| Codex client requests (v2) | 28 of 36 | 8 | Account, remote skills, steer/review |
| Codex server requests | 2 of 5 | 3 | Tool input, dynamic tool call, token refresh |
| Codex server notifications | 19 of ~35 | ~16 | Account, config, usage, terminal notifications |
| ACP Agent trait (stable) | 8 of 8 | 0 | Full coverage |
| ACP Agent trait (unstable) | 0 of 4 | 4 | Feature-gated session methods |
| ACP Client trait | 11 of 11 | 0 | Full coverage |
| ACP SessionUpdate (stable) | 9 of 9 | 0 | Full coverage |
| ACP SessionUpdate (unstable) | 0 of 2 | 2 | SessionInfoUpdate, UsageUpdate |

---

## 1. Codex Client Requests — Not Implemented

| Wire method | Priority | Notes |
|---|---|---|
| `turn/steer` | Medium | No ACP equivalent currently |
| `review/start` | Low | Codex-specific feature |
| `skills/remote/list` | Low | No ACP equivalent |
| `skills/remote/export` | Low | No ACP equivalent |
| `skills/config/write` | Low | No ACP equivalent |
| `app/list` | Low | No ACP equivalent |
| `account/read` | Low | Codex auth, not applicable |
| `account/login/start` | Low | Codex-specific |
| `account/login/cancel` | Low | Codex-specific |
| `account/logout` | Low | Codex-specific |
| `account/rateLimits/read` | Low | Codex-specific |
| `feedback/upload` | Low | Codex-specific |
| `mcpServer/oauth/login` | Low | Codex-specific |
| `windowsSandbox/setupStart` | Low | Windows-only |

### Experimental

| Wire method | Notes |
|---|---|
| `collaborationMode/list` | Experimental collaboration modes |
| `fuzzyFileSearch/session*` | Experimental incremental file search |

---

## 2. Codex Server Requests — Not Implemented

| Wire method | Priority | Notes |
|---|---|---|
| `item/tool/requestUserInput` | Medium | Agent requests free-form user input (not just approve/reject) |
| `item/tool/call` | Medium | Dynamic tool call — gateway invokes a client-side tool |
| `account/chatgptAuthTokens/refresh` | Low | OpenAI-specific auth token refresh |

---

## 3. Codex Server Notifications — Not Implemented

| Wire method | Priority | Notes |
|---|---|---|
| `thread/tokenUsage/updated` | Medium | Blocked on ACP `UsageUpdate` (RFD in Draft as of Jan 2026) |
| `thread/compacted` | Low | Context compaction notification |
| `item/plan/delta` | Low | Incremental plan text delta (we use `turn/plan/updated` instead) |
| `item/commandExecution/terminalInteraction` | Low | Terminal interaction events |
| `item/mcpToolCall/progress` | Low | MCP tool call progress |
| `item/reasoning/summaryPartAdded` | Low | Reasoning summary part boundaries |
| `item/reasoning/textDelta` | Low | Raw reasoning text (vs summary) |
| `rawResponseItem/completed` | Low | Raw model response items |
| `model/rerouted` | Low | Model reroute notification |
| `account/updated` | Low | Account state changes |
| `account/rateLimits/updated` | Low | Rate limit changes |
| `account/login/completed` | Low | Login completion |
| `app/list/updated` | Low | App list changes |
| `mcpServer/oauthLogin/completed` | Low | MCP OAuth completion |
| `deprecationNotice` | Low | Deprecation warnings |
| `configWarning` | Low | Config warnings |
| `fuzzyFileSearch/session*` | Low | File search results |
| `windowsSandbox/setupCompleted` | Low | Windows sandbox |
| `windows/worldWritableWarning` | Low | Windows security |

---

## 4. ACP Unstable Methods — Not Implemented

| ACP method | Feature flag | Notes |
|---|---|---|
| `session/set_model` | `unstable_session_model` | Change model mid-session |
| `session/list` | `unstable_session_list` | List agent's sessions |
| `session/fork` | `unstable_session_fork` | Fork session (maps to `thread/fork`) |
| `session/resume` | `unstable_session_resume` | Resume without replay |

---

## 5. ACP Unstable SessionUpdate Variants — Not Implemented

| SessionUpdate variant | Feature flag | Notes |
|---|---|---|
| `SessionInfoUpdate` | `unstable_session_info_update` | Session title/timestamp updates |
| `UsageUpdate` | `unstable_session_usage` | Token usage / cost updates (RFD in Draft) |

---

## 6. Translation Quality Issues

### Permission ToolKind bucketing is coarse

The approval translation maps everything that isn't Edit/Delete/Move to
`item/commandExecution/requestApproval`. ToolKind variants like `Read`,
`Browser`, `Notebook`, `Other` all get the same command-execution treatment.
Codex only has two approval types today, so this may not be fixable without
upstream changes.

### Hardcoded response fields

`handle_thread_start` hardcodes `approvalPolicy: "on-request"` and
`sandbox: { type: "dangerFullAccess" }`. These should reflect the agent's
actual capabilities or be configurable.

---

## 7. Recommended Next Steps

1. **`thread/tokenUsage/updated`** — Blocked on ACP `UsageUpdate` RFD (Draft)
2. **`turn/steer`** — Mid-turn steering; no ACP equivalent yet
3. **`item/tool/requestUserInput`** — Agent requests free-form user input
4. **`item/tool/call`** — Dynamic client-side tool calls
