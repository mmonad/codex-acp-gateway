use std::collections::HashMap;

use agent_client_protocol::ToolKind;
use uuid::Uuid;

/// Bidirectional ID mapping between Codex and ACP identifiers.
///
/// Codex uses thread IDs, turn IDs, and item IDs while ACP uses session IDs
/// and tool call IDs. This struct maintains the mapping in both directions.
#[derive(Debug, Default)]
pub struct IdMap {
    /// Codex threadId -> ACP sessionId
    thread_to_session: HashMap<String, String>,
    /// ACP sessionId -> Codex threadId
    session_to_thread: HashMap<String, String>,
    /// ACP toolCallId -> Codex itemId
    tool_to_item: HashMap<String, String>,
    /// Codex itemId -> ACP toolCallId
    item_to_tool: HashMap<String, String>,
    /// ACP toolCallId -> ToolKind (for correct item/completed type)
    tool_kind: HashMap<String, ToolKind>,
}

impl IdMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a bidirectional mapping between a Codex thread ID and an ACP session ID.
    pub fn create_thread_session_mapping(&mut self, thread_id: String, session_id: String) {
        self.thread_to_session
            .insert(thread_id.clone(), session_id.clone());
        self.session_to_thread.insert(session_id, thread_id);
    }

    /// Look up the ACP session ID for a Codex thread ID.
    pub fn lookup_session(&self, thread_id: &str) -> Option<&String> {
        self.thread_to_session.get(thread_id)
    }

    /// Look up the Codex thread ID for an ACP session ID.
    pub fn lookup_thread(&self, session_id: &str) -> Option<&String> {
        self.session_to_thread.get(session_id)
    }

    /// Create a new Codex item ID for an ACP tool call ID and store the mapping.
    /// Returns the newly generated item ID.
    pub fn create_item_for_tool(&mut self, tool_call_id: &str) -> String {
        let item_id = new_item_id();
        self.tool_to_item
            .insert(tool_call_id.to_string(), item_id.clone());
        self.item_to_tool
            .insert(item_id.clone(), tool_call_id.to_string());
        item_id
    }

    /// Look up the Codex item ID for an ACP tool call ID.
    pub fn lookup_item(&self, tool_call_id: &str) -> Option<&String> {
        self.tool_to_item.get(tool_call_id)
    }

    /// Look up the ACP tool call ID for a Codex item ID.
    pub fn lookup_tool(&self, item_id: &str) -> Option<&String> {
        self.item_to_tool.get(item_id)
    }

    /// Store the ACP ToolKind for a tool call ID.
    pub fn set_tool_kind(&mut self, tool_call_id: &str, kind: ToolKind) {
        self.tool_kind.insert(tool_call_id.to_string(), kind);
    }

    /// Look up the ToolKind for an ACP tool call ID.
    pub fn lookup_tool_kind(&self, tool_call_id: &str) -> Option<&ToolKind> {
        self.tool_kind.get(tool_call_id)
    }
}

/// Generate a new Codex thread ID using UUID v7 (time-ordered).
pub fn new_thread_id() -> String {
    Uuid::now_v7().to_string()
}

/// Generate a new Codex turn ID using UUID v7.
pub fn new_turn_id() -> String {
    Uuid::now_v7().to_string()
}

/// Generate a new Codex item ID using UUID v7.
pub fn new_item_id() -> String {
    Uuid::now_v7().to_string()
}
