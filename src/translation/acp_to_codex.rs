use std::path::PathBuf;

use agent_client_protocol::{
    ContentBlock, EmbeddedResourceResource, SessionUpdate, ToolCallStatus, ToolKind,
};
use serde_json::{Value, json};

use super::id_map::IdMap;

/// Result of translating a session update.
pub struct TranslationResult {
    /// Codex notifications to emit: (method, params).
    pub notifications: Vec<(String, Value)>,
    /// File diffs collected for turn-level aggregation (path, old_text, new_text).
    pub diffs: Vec<(PathBuf, Option<String>, String)>,
}

/// Translate an ACP `SessionUpdate` into one or more Codex notification JSON values.
///
/// Each returned notification tuple is `(method, params)` for a Codex `ServerNotification`.
/// A single ACP update may produce multiple Codex notifications (e.g., a ToolCall
/// produces both an `item/started` and potentially content notifications).
///
/// The returned `diffs` field collects file diffs for turn-level aggregation.
pub fn translate_session_update(
    update: &SessionUpdate,
    thread_id: &str,
    turn_id: &str,
    id_map: &mut IdMap,
) -> TranslationResult {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            let delta = extract_text_from_content(&chunk.content);
            TranslationResult {
                notifications: vec![(
                    "item/agentMessage/delta".to_string(),
                    json!({
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "itemId": format!("{turn_id}-msg"),
                        "delta": delta,
                    }),
                )],
                diffs: vec![],
            }
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            let delta = extract_text_from_content(&chunk.content);
            TranslationResult {
                notifications: vec![(
                    "item/reasoning/summaryTextDelta".to_string(),
                    json!({
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "itemId": format!("{turn_id}-reasoning"),
                        "delta": delta,
                        "summaryIndex": 0,
                    }),
                )],
                diffs: vec![],
            }
        }
        SessionUpdate::ToolCall(tool_call) => {
            let tool_call_id = tool_call.tool_call_id.0.as_ref();
            let item_id = id_map.create_item_for_tool(tool_call_id);
            id_map.set_tool_kind(tool_call_id, tool_call.kind);

            // Determine the Codex item type based on ACP ToolKind
            let item = match tool_call.kind {
                ToolKind::Execute => {
                    json!({
                        "type": "commandExecution",
                        "id": item_id,
                        "command": tool_call.title,
                        "cwd": ".",
                        "processId": null,
                        "status": "inProgress",
                        "commandActions": [],
                        "aggregatedOutput": null,
                        "exitCode": null,
                        "durationMs": null,
                    })
                }
                ToolKind::Edit | ToolKind::Delete | ToolKind::Move => {
                    json!({
                        "type": "fileChange",
                        "id": item_id,
                        "changes": [],
                        "status": "inProgress",
                    })
                }
                _ => {
                    // Default to mcpToolCall for other kinds
                    json!({
                        "type": "mcpToolCall",
                        "id": item_id,
                        "server": "acp",
                        "tool": tool_call.title,
                        "status": "inProgress",
                        "arguments": tool_call.raw_input.clone().unwrap_or(json!({})),
                        "result": null,
                        "error": null,
                        "durationMs": null,
                    })
                }
            };

            TranslationResult {
                notifications: vec![(
                    "item/started".to_string(),
                    json!({
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "item": item,
                    }),
                )],
                diffs: vec![],
            }
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let tool_call_id = update.tool_call_id.0.as_ref();
            let item_id = id_map
                .lookup_item(tool_call_id)
                .cloned()
                .unwrap_or_else(|| format!("unknown-{tool_call_id}"));

            let mut notifications = Vec::new();
            let mut diffs = Vec::new();

            // Emit output delta if there's content
            if let Some(content) = &update.fields.content {
                for tc_content in content {
                    match tc_content {
                        agent_client_protocol::ToolCallContent::Content(c) => {
                            let text = extract_text_from_content(&c.content);
                            if !text.is_empty() {
                                // Route to correct notification based on tool kind
                                let method = match id_map.lookup_tool_kind(tool_call_id) {
                                    Some(ToolKind::Edit | ToolKind::Delete | ToolKind::Move) => {
                                        "item/fileChange/outputDelta"
                                    }
                                    _ => "item/commandExecution/outputDelta",
                                };
                                notifications.push((
                                    method.to_string(),
                                    json!({
                                        "threadId": thread_id,
                                        "turnId": turn_id,
                                        "itemId": item_id,
                                        "delta": text,
                                    }),
                                ));
                            }
                        }
                        agent_client_protocol::ToolCallContent::Diff(d) => {
                            // Compute unified diff
                            let old = d.old_text.as_deref().unwrap_or("");
                            let new_text = &d.new_text;
                            let path = d.path.display();

                            let diff = similar::TextDiff::from_lines(old, new_text);
                            let unified = diff
                                .unified_diff()
                                .context_radius(3)
                                .header(&format!("a/{path}"), &format!("b/{path}"))
                                .to_string();

                            let delta = format!("diff --git a/{path} b/{path}\n{unified}");

                            notifications.push((
                                "item/fileChange/outputDelta".to_string(),
                                json!({
                                    "threadId": thread_id,
                                    "turnId": turn_id,
                                    "itemId": item_id,
                                    "delta": delta,
                                }),
                            ));

                            // Collect for turn-level aggregation
                            diffs.push((d.path.clone(), d.old_text.clone(), d.new_text.clone()));
                        }
                        _ => {} // Terminal -- handled via ACP Client trait locally
                    }
                }
            }

            // If the tool call is completed or failed, emit item/completed
            if let Some(status) = &update.fields.status
                && matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed)
            {
                let codex_status = match status {
                    ToolCallStatus::Completed => "completed",
                    ToolCallStatus::Failed => "failed",
                    _ => "inProgress",
                };

                let title = update.fields.title.as_deref().unwrap_or("unknown");
                let kind = id_map.lookup_tool_kind(tool_call_id);

                let item = match kind {
                    Some(ToolKind::Execute) => json!({
                        "type": "commandExecution",
                        "id": item_id,
                        "command": title,
                        "cwd": ".",
                        "status": codex_status,
                        "exitCode": null,
                        "durationMs": null,
                    }),
                    Some(ToolKind::Edit | ToolKind::Delete | ToolKind::Move) => json!({
                        "type": "fileChange",
                        "id": item_id,
                        "changes": [],
                        "status": codex_status,
                    }),
                    _ => json!({
                        "type": "mcpToolCall",
                        "id": item_id,
                        "server": "acp",
                        "tool": title,
                        "status": codex_status,
                        "arguments": {},
                        "result": null,
                        "error": null,
                        "durationMs": null,
                    }),
                };

                notifications.push((
                    "item/completed".to_string(),
                    json!({
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "item": item,
                    }),
                ));
            }

            TranslationResult {
                notifications,
                diffs,
            }
        }
        SessionUpdate::Plan(plan) => {
            let steps: Vec<Value> = plan
                .entries
                .iter()
                .map(|entry| {
                    json!({
                        "step": entry.content,
                        "status": match entry.status {
                            agent_client_protocol::PlanEntryStatus::Pending => "pending",
                            agent_client_protocol::PlanEntryStatus::InProgress => "inProgress",
                            agent_client_protocol::PlanEntryStatus::Completed => "completed",
                            _ => "pending",
                        },
                    })
                })
                .collect();

            TranslationResult {
                notifications: vec![(
                    "turn/plan/updated".to_string(),
                    json!({
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "explanation": null,
                        "plan": steps,
                    }),
                )],
                diffs: vec![],
            }
        }
        SessionUpdate::UserMessageChunk(_) => {
            // Echo of user input -- the Codex client already has it, so ignore.
            TranslationResult {
                notifications: vec![],
                diffs: vec![],
            }
        }
        SessionUpdate::AvailableCommandsUpdate(update) => {
            let commands: Vec<Value> = update
                .available_commands
                .iter()
                .map(|cmd| {
                    json!({
                        "name": cmd.name,
                        "description": cmd.description,
                    })
                })
                .collect();
            TranslationResult {
                notifications: vec![(
                    "gateway/availableCommands/updated".to_string(),
                    json!({
                        "threadId": thread_id,
                        "commands": commands,
                    }),
                )],
                diffs: vec![],
            }
        }
        SessionUpdate::CurrentModeUpdate(update) => TranslationResult {
            notifications: vec![(
                "thread/status/changed".to_string(),
                json!({
                    "threadId": thread_id,
                    "mode": update.current_mode_id.0.as_ref(),
                }),
            )],
            diffs: vec![],
        },
        SessionUpdate::ConfigOptionUpdate(update) => {
            let options: Vec<Value> = update
                .config_options
                .iter()
                .map(|opt| {
                    json!({
                        "id": opt.id.0.as_ref(),
                        "name": opt.name,
                    })
                })
                .collect();
            TranslationResult {
                notifications: vec![(
                    "gateway/configOptions/updated".to_string(),
                    json!({
                        "threadId": thread_id,
                        "configOptions": options,
                    }),
                )],
                diffs: vec![],
            }
        }
        // ── Unstable variants ──────────────────────────────────────────
        SessionUpdate::SessionInfoUpdate(info) => {
            let mut notifications = Vec::new();

            // If the agent updated the session title, emit thread/name/updated.
            if let agent_client_protocol::MaybeUndefined::Value(ref title) = info.title {
                notifications.push((
                    "thread/name/updated".to_string(),
                    json!({
                        "threadId": thread_id,
                        "threadName": title,
                    }),
                ));
            }

            TranslationResult {
                notifications,
                diffs: vec![],
            }
        }
        SessionUpdate::UsageUpdate(usage) => {
            // Map ACP UsageUpdate → Codex thread/tokenUsage/updated.
            //
            // ACP provides `used` (tokens in context) and `size` (window size).
            // We approximate the Codex breakdown from the limited data available.
            TranslationResult {
                notifications: vec![(
                    "thread/tokenUsage/updated".to_string(),
                    json!({
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "tokenUsage": {
                            "total": {
                                "totalTokens": usage.used,
                                "inputTokens": usage.used,
                                "cachedInputTokens": 0,
                                "outputTokens": 0,
                                "reasoningOutputTokens": 0,
                            },
                            "last": {
                                "totalTokens": usage.used,
                                "inputTokens": usage.used,
                                "cachedInputTokens": 0,
                                "outputTokens": 0,
                                "reasoningOutputTokens": 0,
                            },
                            "modelContextWindow": usage.size,
                        },
                    }),
                )],
                diffs: vec![],
            }
        }

        // Future-proof: any new variants added upstream are silently ignored.
        _ => TranslationResult {
            notifications: vec![],
            diffs: vec![],
        },
    }
}

/// Translate a completed ACP prompt response into a Codex `turn/completed` notification.
pub fn translate_prompt_response(
    stop_reason: &str,
    thread_id: &str,
    turn_id: &str,
) -> (String, Value) {
    let status = match stop_reason {
        "end_turn" => "completed",
        "cancelled" => "interrupted",
        _ => "completed",
    };

    (
        "turn/completed".to_string(),
        json!({
            "threadId": thread_id,
            "turn": {
                "id": turn_id,
                "items": [],
                "status": status,
                "error": null,
            },
        }),
    )
}

/// Translate an ACP `Usage` (from `PromptResponse`) into a Codex
/// `thread/tokenUsage/updated` notification payload.
///
/// This provides more detailed token breakdown than the streaming `UsageUpdate`
/// because `PromptResponse.usage` has per-category counts.
pub fn translate_prompt_usage(
    usage: &agent_client_protocol::Usage,
    thread_id: &str,
    turn_id: &str,
) -> (String, Value) {
    (
        "thread/tokenUsage/updated".to_string(),
        json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "tokenUsage": {
                "total": {
                    "totalTokens": usage.total_tokens,
                    "inputTokens": usage.input_tokens,
                    "cachedInputTokens": usage.cached_read_tokens.unwrap_or(0),
                    "outputTokens": usage.output_tokens,
                    "reasoningOutputTokens": usage.thought_tokens.unwrap_or(0),
                },
                "last": {
                    "totalTokens": usage.total_tokens,
                    "inputTokens": usage.input_tokens,
                    "cachedInputTokens": usage.cached_read_tokens.unwrap_or(0),
                    "outputTokens": usage.output_tokens,
                    "reasoningOutputTokens": usage.thought_tokens.unwrap_or(0),
                },
                "modelContextWindow": null,
            },
        }),
    )
}

/// Extract a text representation from a ContentBlock.
///
/// Returns the text directly for text content, or a descriptive placeholder for
/// non-text content (images, audio, resources).
fn extract_text_from_content(content: &ContentBlock) -> String {
    match content {
        ContentBlock::Text(t) => t.text.clone(),
        ContentBlock::Image(img) => {
            if let Some(uri) = &img.uri {
                format!("[image: {uri}]")
            } else {
                format!("[image: {}]", img.mime_type)
            }
        }
        ContentBlock::Audio(_) => "[audio content]".to_string(),
        ContentBlock::Resource(res) => match &res.resource {
            EmbeddedResourceResource::TextResourceContents(t) => t.text.clone(),
            EmbeddedResourceResource::BlobResourceContents(b) => {
                format!("[blob resource: {}]", b.uri)
            }
            _ => "[resource]".to_string(),
        },
        ContentBlock::ResourceLink(link) => {
            format!("[resource: {} ({})]", link.name, link.uri)
        }
        _ => String::new(),
    }
}
