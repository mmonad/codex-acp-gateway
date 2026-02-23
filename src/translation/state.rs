use std::collections::HashMap;

/// Tracks the lifecycle state of a single Codex thread / ACP session pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLifecycle {
    /// The thread has not yet been mapped to an ACP session.
    Uninitialized,
    /// A `session/new` request is in-flight.
    Creating,
    /// The ACP session exists but no turn is active.
    Idle { session_id: String },
    /// A prompt turn is in progress.
    InTurn {
        session_id: String,
        turn_id: String,
    },
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
}

impl GatewayState {
    pub fn new() -> Self {
        Self {
            initialized: false,
            client_name: None,
            client_version: None,
            threads: HashMap::new(),
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
