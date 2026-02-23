use std::path::Path;

use agent_client_protocol::{
    ClientCapabilities, ContentBlock, FileSystemCapability, InitializeRequest,
    LoadSessionRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
    SetSessionConfigOptionRequest, SetSessionModeRequest, TextContent,
};
use serde_json::Value;

use super::content::codex_input_to_acp;

/// Translate a Codex `initialize` request (as JSON params) into an ACP `InitializeRequest`.
///
/// The Codex initialize carries `clientInfo { name, version }` and capabilities.
/// We map these to the ACP equivalent, advertising that the gateway supports
/// filesystem and terminal callbacks.
pub fn translate_initialize(params: &Value) -> InitializeRequest {
    let mut req = InitializeRequest::new(ProtocolVersion::LATEST);

    if let Some(client_info) = params.get("clientInfo") {
        let name = client_info
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("codex");
        let version = client_info
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0");
        req = req.client_info(agent_client_protocol::Implementation::new(name, version));
    }

    // Advertise the callbacks the gateway handles locally.
    req = req.client_capabilities(
        ClientCapabilities::new()
            .fs(
                FileSystemCapability::new()
                    .read_text_file(true)
                    .write_text_file(true),
            )
            .terminal(true),
    );

    req
}

/// Translate a Codex `thread/start` request (as JSON params) into an ACP `NewSessionRequest`.
///
/// The main field we need is `cwd` (working directory). If not provided, use the
/// given fallback.
pub fn translate_thread_start(params: &Value, fallback_cwd: &Path) -> NewSessionRequest {
    let cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| fallback_cwd.to_path_buf());

    NewSessionRequest::new(cwd)
}

/// Translate a Codex `turn/start` request (as JSON params) into an ACP `PromptRequest`.
///
/// Extracts the user input array and converts it to ACP ContentBlocks.
pub fn translate_turn_start(params: &Value, session_id: &str) -> PromptRequest {
    let prompt = if let Some(inputs) = params.get("input").and_then(|v| v.as_array()) {
        codex_input_to_acp(inputs)
    } else {
        // Fallback: if there's no structured input, check for a plain text field
        vec![ContentBlock::Text(TextContent::new(
            "[empty turn input]",
        ))]
    };

    PromptRequest::new(session_id.to_string(), prompt)
}

/// Translate a Codex `thread/resume` request into an ACP `LoadSessionRequest`.
pub fn translate_thread_resume(
    params: &Value,
    session_id: &str,
    fallback_cwd: &Path,
) -> LoadSessionRequest {
    let cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| fallback_cwd.to_path_buf());

    LoadSessionRequest::new(session_id.to_string(), cwd)
}

/// Translate into an ACP `SetSessionModeRequest`.
pub fn translate_set_session_mode(
    session_id: &str,
    mode_id: &str,
) -> SetSessionModeRequest {
    SetSessionModeRequest::new(session_id.to_string(), mode_id.to_string())
}

/// Translate into an ACP `SetSessionConfigOptionRequest`.
pub fn translate_set_session_config_option(
    session_id: &str,
    config_id: &str,
    value: &str,
) -> SetSessionConfigOptionRequest {
    SetSessionConfigOptionRequest::new(
        session_id.to_string(),
        config_id.to_string(),
        value.to_string(),
    )
}
