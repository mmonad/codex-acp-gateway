use std::collections::HashMap;
use std::path::PathBuf;

use agent_client_protocol::{
    AgentCapabilities, AuthMethod, SessionConfigOption, SessionModeState, SessionModelState,
};

use crate::rollout::recorder::RolloutRecorder;

/// Tracks the lifecycle state of a single Codex thread / ACP session pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLifecycle {
    /// The thread has not yet been mapped to an ACP session.
    Uninitialized,
    /// A `session/new` request is in-flight.
    Creating,
    /// A `session/load` request is in-flight.
    Loading,
    /// The ACP session exists but no turn is active.
    Idle { session_id: String },
    /// A prompt turn is in progress.
    InTurn { session_id: String, turn_id: String },
    /// The session has been closed or the thread archived.
    Closed,
}

/// Shared gateway state tracking initialization and per-thread lifecycles.
#[derive(Debug)]
pub struct GatewayState {
    /// Whether the Codex `initialize` handshake has completed.
    pub initialized: bool,
    /// Client info extracted from the Codex `initialize` request.
    pub client_name: Option<String>,
    pub client_version: Option<String>,
    /// Per-thread lifecycle state keyed by Codex thread ID.
    pub threads: HashMap<String, SessionLifecycle>,
    /// Agent capabilities from the ACP initialize response.
    pub agent_capabilities: Option<AgentCapabilities>,
    /// Auth methods advertised by the ACP agent.
    pub auth_methods: Vec<AuthMethod>,
    /// Per-session mode state (keyed by ACP session ID).
    pub session_modes: HashMap<String, SessionModeState>,
    /// Per-session config options (keyed by ACP session ID).
    pub session_config: HashMap<String, Vec<SessionConfigOption>>,
    /// Accumulated file diffs per turn (keyed by turn ID).
    /// Each entry is `(path, old_text, new_text)`.
    pub turn_diffs: HashMap<String, Vec<(PathBuf, Option<String>, String)>>,
    /// Agent's model selection state, populated from session creation/load
    /// responses when the agent advertises `unstable_session_model` support.
    pub model_state: Option<SessionModelState>,
    pub catalog_cache: Option<serde_json::Value>,
    /// Rollout recorder for persisting thread history to disk.
    /// Uses Codex-compatible `RolloutLine`/`RolloutItem` NDJSON format.
    pub rollout: RolloutRecorder,
}

impl GatewayState {
    pub fn new() -> Self {
        Self {
            initialized: false,
            client_name: None,
            client_version: None,
            threads: HashMap::new(),
            agent_capabilities: None,
            auth_methods: Vec::new(),
            session_modes: HashMap::new(),
            session_config: HashMap::new(),
            turn_diffs: HashMap::new(),
            model_state: None,
            catalog_cache: None,
            rollout: RolloutRecorder::default(),
        }
    }

    /// Get the lifecycle for a thread, defaulting to `Uninitialized`.
    pub fn lifecycle(&self, thread_id: &str) -> &SessionLifecycle {
        self.threads
            .get(thread_id)
            .unwrap_or(&SessionLifecycle::Uninitialized)
    }

    /// Set the lifecycle for a thread.
    pub fn set_lifecycle(&mut self, thread_id: String, state: SessionLifecycle) {
        self.threads.insert(thread_id, state);
    }
}

impl Default for GatewayState {
    fn default() -> Self {
        Self::new()
    }
}
