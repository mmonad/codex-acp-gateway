pub mod acp;
pub mod config;
pub mod error;
pub mod translation;
pub mod transport;

use std::rc::Rc;

use agent_client_protocol::{Agent, ClientSideConnection};
use anyhow::{Context, Result};
use codex_app_server_protocol::{JSONRPCMessage, JSONRPCRequest, JSONRPCResponse};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::acp::AcpEvent;
use crate::config::{GatewayConfig, ListenTransport};
use crate::error::GatewayError;
use crate::translation::acp_to_codex;
use crate::translation::approval;
use crate::translation::codex_to_acp;
use crate::translation::id_map::{self, IdMap};
use crate::translation::state::{GatewayState, SessionLifecycle};
use crate::transport::TransportEvent;
use crate::transport::outgoing::OutgoingMessageSender;
use crate::transport::stdio;
use crate::transport::websocket;

/// Run the gateway. This is the main entry point called from `main()`.
///
/// Dispatches to the appropriate transport mode based on `config.listen`:
/// - **Stdio**: single client, exit on EOF (original behavior)
/// - **WebSocket**: accept connections on a TCP port, each gets its own ACP subprocess
///
/// All work runs inside a `LocalSet` because ACP futures are `!Send`.
pub async fn run(config: GatewayConfig) -> Result<()> {
    let cwd = std::fs::canonicalize(&config.cwd).context("invalid cwd")?;
    info!(
        listen = %config.listen,
        agent_cmd = %config.agent_cmd,
        agent_args = ?config.agent_args,
        cwd = %cwd.display(),
        "starting codex-acp-gateway"
    );

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            match &config.listen {
                ListenTransport::Stdio => {
                    let (inbound_tx, inbound_rx) = mpsc::channel::<TransportEvent>(256);
                    let (outbound_tx, outbound_rx) = mpsc::channel::<String>(256);

                    stdio::spawn_reader(inbound_tx);
                    stdio::spawn_writer(outbound_rx);

                    run_session(&config, &cwd, inbound_rx, outbound_tx).await
                }
                ListenTransport::WebSocket { bind_address } => {
                    let listener = tokio::net::TcpListener::bind(bind_address)
                        .await
                        .with_context(|| {
                            format!("failed to bind WebSocket listener on {bind_address}")
                        })?;
                    info!(%bind_address, "listening for WebSocket connections");

                    loop {
                        let (tcp_stream, peer_addr) = listener.accept().await?;
                        debug!(%peer_addr, "TCP connection accepted");

                        let ws_stream =
                            match tokio_tungstenite::accept_async(tcp_stream).await {
                                Ok(ws) => ws,
                                Err(e) => {
                                    warn!(%peer_addr, "WebSocket handshake failed: {e}");
                                    continue;
                                }
                            };

                        info!(%peer_addr, "WebSocket client connected");
                        let (ws_write, ws_read) = ws_stream.split();

                        let (inbound_tx, inbound_rx) = mpsc::channel::<TransportEvent>(256);
                        let (outbound_tx, outbound_rx) = mpsc::channel::<String>(256);
                        let (pong_tx, pong_rx) = mpsc::channel::<Vec<u8>>(16);

                        websocket::spawn_reader(inbound_tx, ws_read, pong_tx);
                        websocket::spawn_writer(outbound_rx, ws_write, pong_rx);

                        let cfg = config.clone();
                        let cwd = cwd.clone();
                        tokio::task::spawn_local(async move {
                            if let Err(e) =
                                run_session(&cfg, &cwd, inbound_rx, outbound_tx).await
                            {
                                warn!(%peer_addr, "session error: {e:#}");
                            }
                            info!(%peer_addr, "session ended");
                        });
                    }
                }
            }
        })
        .await
}

/// Run one gateway session for a single Codex client connection.
///
/// Spawns an ACP agent subprocess and runs the event loop that translates
/// between the Codex transport channels and the ACP connection.
///
/// This function is transport-agnostic — it only uses `mpsc` channels. Must be
/// called from within a `LocalSet` (ACP futures are `!Send`).
async fn run_session(
    config: &GatewayConfig,
    cwd: &std::path::Path,
    mut codex_inbound_rx: mpsc::Receiver<TransportEvent>,
    codex_outbound_tx: mpsc::Sender<String>,
) -> Result<()> {
    let outgoing = OutgoingMessageSender::new(codex_outbound_tx);

    let (acp_conn, mut acp_events) =
        acp::connect(&config.agent_cmd, &config.agent_args, cwd)
            .await
            .context("failed to connect to ACP agent")?;

    let mut state = GatewayState::new();
    let mut id_map = IdMap::new();
    let agent = Rc::new(acp_conn.agent);

    info!("gateway event loop started");

    loop {
        tokio::select! {
            event = codex_inbound_rx.recv() => {
                match event {
                    Some(TransportEvent::MessageReceived { message }) => {
                        handle_codex_message(
                            message,
                            &agent,
                            &outgoing,
                            &mut state,
                            &mut id_map,
                            cwd,
                        ).await;
                    }
                    Some(TransportEvent::Disconnected) | None => {
                        info!("codex client disconnected, shutting down");
                        break;
                    }
                }
            }

            event = acp_events.recv() => {
                match event {
                    Some(acp_event) => {
                        handle_acp_event(
                            acp_event,
                            &outgoing,
                            &mut state,
                            &mut id_map,
                        ).await;
                    }
                    None => {
                        info!("ACP event channel closed, agent likely exited");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Handle an incoming JSON-RPC message from the Codex client.
async fn handle_codex_message(
    message: JSONRPCMessage,
    agent: &Rc<ClientSideConnection>,
    outgoing: &OutgoingMessageSender,
    state: &mut GatewayState,
    id_map: &mut IdMap,
    cwd: &std::path::Path,
) {
    match message {
        JSONRPCMessage::Request(req) => {
            handle_codex_request(req, agent, outgoing, state, id_map, cwd).await;
        }
        JSONRPCMessage::Response(resp) => {
            // This is a response to a server-initiated request (e.g., approval)
            outgoing.resolve_pending(resp).await;
        }
        JSONRPCMessage::Notification(notif) => {
            debug!(method = %notif.method, "codex notification (ignored for now)");
        }
        JSONRPCMessage::Error(err) => {
            warn!(id = ?err.id, error = %err.error.message, "codex client returned error");
            let resp = JSONRPCResponse {
                id: err.id,
                result: json!({ "error": err.error.message }),
            };
            outgoing.resolve_pending(resp).await;
        }
    }
}

/// Dispatch a Codex client request to the appropriate handler.
async fn handle_codex_request(
    req: JSONRPCRequest,
    agent: &Rc<ClientSideConnection>,
    outgoing: &OutgoingMessageSender,
    state: &mut GatewayState,
    id_map: &mut IdMap,
    cwd: &std::path::Path,
) {
    let id = req.id.clone();
    let params = req.params.clone().unwrap_or(json!({}));

    let result = match req.method.as_str() {
        "initialize" => handle_initialize(&params, agent.as_ref(), state).await,
        "initialized" => {
            // Notification-style acknowledgment from client, no response needed
            return;
        }
        "thread/start" => handle_thread_start(&params, agent.as_ref(), state, id_map, cwd).await,
        "thread/resume" => {
            handle_thread_resume(&params, agent.as_ref(), state, id_map, cwd).await
        }
        "turn/start" => {
            handle_turn_start(&params, Rc::clone(agent), outgoing, state, id_map).await
        }
        "turn/interrupt" => {
            handle_turn_interrupt(&params, agent.as_ref(), state, id_map).await
        }
        "session/setMode" => {
            handle_set_session_mode(&params, agent.as_ref(), state, id_map).await
        }
        "session/setConfigOption" => {
            handle_set_session_config_option(&params, agent.as_ref(), state, id_map).await
        }
        "command/exec" => handle_command_exec(&params, cwd).await,
        method => {
            debug!(method, "unhandled codex request method");
            Err(GatewayError::Translation(format!(
                "unsupported method: {method}"
            )))
        }
    };

    // After successful thread/start or thread/resume, emit a thread/started notification.
    let emit_thread_started = matches!(req.method.as_str(), "thread/start" | "thread/resume")
        && result.is_ok();

    match result {
        Ok(value) => {
            if let Err(e) = outgoing.send_response(id, value).await {
                error!("failed to send response: {e}");
            }
            if emit_thread_started
                && let Some(thread_id) = params.get("threadId").and_then(|v| v.as_str())
            {
                let _ = outgoing
                    .send_notification(
                        "thread/started",
                        Some(json!({ "threadId": thread_id })),
                    )
                    .await;
            }
        }
        Err(e) => {
            warn!(error = %e, "request failed");
            if let Err(send_err) = outgoing.send_error(id, e.to_jsonrpc_error()).await {
                error!("failed to send error response: {send_err}");
            }
        }
    }
}

/// Handle `initialize` - translate to ACP initialize, auto-authenticate if
/// needed, and return a rich response.
async fn handle_initialize(
    params: &Value,
    agent: &(impl Agent + ?Sized),
    state: &mut GatewayState,
) -> Result<Value, GatewayError> {
    let acp_req = codex_to_acp::translate_initialize(params);

    let acp_resp = agent
        .initialize(acp_req)
        .await
        .map_err(|e| GatewayError::Acp(format!("{e:?}")))?;

    // Store agent capabilities and auth methods.
    state.agent_capabilities = Some(acp_resp.agent_capabilities.clone());
    state.auth_methods = acp_resp.auth_methods.clone();

    // Auto-authenticate if the agent advertises auth methods.
    if let Some(first_method) = acp_resp.auth_methods.first() {
        info!(method_id = %first_method.id.0, "auto-authenticating with agent");
        let auth_req =
            agent_client_protocol::AuthenticateRequest::new(first_method.id.clone());
        agent
            .authenticate(auth_req)
            .await
            .map_err(|e| GatewayError::Acp(format!("authenticate failed: {e:?}")))?;
    }

    state.initialized = true;
    if let Some(info) = params.get("clientInfo") {
        state.client_name = info.get("name").and_then(|v| v.as_str()).map(String::from);
        state.client_version = info
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);
    }

    let protocol_version = format!("{}", acp_resp.protocol_version);
    info!(protocol_version = %protocol_version, "ACP initialize complete");

    let agent_version = acp_resp
        .agent_info
        .as_ref()
        .map(|i| i.version.as_str())
        .unwrap_or("unknown");

    Ok(json!({
        "userAgent": format!("codex-acp-gateway/0.1.0 (acp-agent/{agent_version})"),
        "protocolVersion": protocol_version,
        "capabilities": {
            "experimental": {},
        },
    }))
}

/// Handle `thread/start` - create a new ACP session.
async fn handle_thread_start(
    params: &Value,
    agent: &(impl Agent + ?Sized),
    state: &mut GatewayState,
    id_map: &mut IdMap,
    cwd: &std::path::Path,
) -> Result<Value, GatewayError> {
    if !state.initialized {
        return Err(GatewayError::NotInitialized);
    }

    let thread_id = id_map::new_thread_id();
    state.set_lifecycle(thread_id.clone(), SessionLifecycle::Creating);

    let acp_req = codex_to_acp::translate_thread_start(params, cwd);
    let acp_resp = agent
        .new_session(acp_req)
        .await
        .map_err(|e| GatewayError::Acp(format!("{e:?}")))?;

    let session_id = acp_resp.session_id.to_string();
    id_map.create_thread_session_mapping(thread_id.clone(), session_id.clone());
    state.set_lifecycle(
        thread_id.clone(),
        SessionLifecycle::Idle {
            session_id: session_id.clone(),
        },
    );

    info!(%thread_id, %session_id, "thread/session created");

    Ok(json!({
        "thread": {
            "id": thread_id,
            "cwd": cwd.to_string_lossy(),
            "cliVersion": "0.1.0",
            "createdAt": chrono_now_secs(),
            "updatedAt": chrono_now_secs(),
            "modelProvider": "acp",
            "preview": "",
            "source": "appServer",
            "status": { "type": "idle" },
            "turns": [],
        },
        "model": "acp-agent",
        "modelProvider": "acp",
        "cwd": cwd.to_string_lossy(),
        "approvalPolicy": "on-request",
        "sandbox": { "type": "dangerFullAccess" },
    }))
}

/// Handle `turn/start` - send a prompt to the ACP session.
///
/// Takes an `Rc<ClientSideConnection>` (not a reference) so we can move a clone
/// into the `spawn_local` task that drives the prompt to completion.
async fn handle_turn_start(
    params: &Value,
    agent: Rc<ClientSideConnection>,
    outgoing: &OutgoingMessageSender,
    state: &mut GatewayState,
    id_map: &mut IdMap,
) -> Result<Value, GatewayError> {
    let thread_id = params
        .get("threadId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GatewayError::Translation("missing threadId".into()))?;

    let session_id = id_map
        .lookup_session(thread_id)
        .ok_or_else(|| GatewayError::SessionNotFound {
            thread_id: thread_id.into(),
        })?
        .clone();

    let turn_id = id_map::new_turn_id();

    state.set_lifecycle(
        thread_id.to_string(),
        SessionLifecycle::InTurn {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        },
    );

    // Send turn/started notification to Codex client
    let _ = outgoing
        .send_notification(
            "turn/started",
            Some(json!({
                "threadId": thread_id,
                "turn": {
                    "id": turn_id,
                    "items": [],
                    "status": "inProgress",
                    "error": null,
                },
            })),
        )
        .await;

    // Send item/started for the agent message (so deltas have an item to target)
    let _ = outgoing
        .send_notification(
            "item/started",
            Some(json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "item": {
                    "type": "agentMessage",
                    "id": format!("{turn_id}-msg"),
                    "text": "",
                },
            })),
        )
        .await;

    // Translate and send the prompt to ACP
    let acp_req = codex_to_acp::translate_turn_start(params, &session_id);

    // Spawn the prompt call on the LocalSet. It blocks until the ACP agent
    // finishes streaming, while notifications flow through the AcpEvent channel.
    let outgoing_clone = outgoing.clone();
    let thread_id_owned = thread_id.to_string();
    let turn_id_clone = turn_id.clone();

    tokio::task::spawn_local(async move {
        match agent.prompt(acp_req).await {
            Ok(resp) => {
                let stop_reason = format!("{:?}", resp.stop_reason);
                let (method, params) = acp_to_codex::translate_prompt_response(
                    &stop_reason,
                    &thread_id_owned,
                    &turn_id_clone,
                );

                // Send item/completed for the agent message
                let _ = outgoing_clone
                    .send_notification(
                        "item/completed",
                        Some(json!({
                            "threadId": thread_id_owned,
                            "turnId": turn_id_clone,
                            "item": {
                                "type": "agentMessage",
                                "id": format!("{turn_id_clone}-msg"),
                                "text": "",
                            },
                        })),
                    )
                    .await;

                // Send turn/completed
                let _ = outgoing_clone
                    .send_notification(method, Some(params))
                    .await;
            }
            Err(e) => {
                error!("ACP prompt failed: {e:?}");
                let _ = outgoing_clone
                    .send_notification(
                        "turn/completed",
                        Some(json!({
                            "threadId": thread_id_owned,
                            "turn": {
                                "id": turn_id_clone,
                                "items": [],
                                "status": "failed",
                                "error": {
                                    "message": format!("{e:?}"),
                                },
                            },
                        })),
                    )
                    .await;
            }
        }
    });

    // Return the turn/start response immediately (streaming happens via notifications)
    Ok(json!({
        "turn": {
            "id": turn_id,
            "items": [],
            "status": "inProgress",
            "error": null,
        },
    }))
}

/// Handle `turn/interrupt` - cancel the active ACP prompt.
async fn handle_turn_interrupt(
    params: &Value,
    agent: &(impl Agent + ?Sized),
    state: &mut GatewayState,
    id_map: &mut IdMap,
) -> Result<Value, GatewayError> {
    let thread_id = params
        .get("threadId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GatewayError::Translation("missing threadId".into()))?;

    let session_id = id_map
        .lookup_session(thread_id)
        .ok_or_else(|| GatewayError::SessionNotFound {
            thread_id: thread_id.into(),
        })?
        .clone();

    agent
        .cancel(agent_client_protocol::CancelNotification::new(
            session_id.clone(),
        ))
        .await
        .map_err(|e| GatewayError::Acp(format!("{e:?}")))?;

    state.set_lifecycle(
        thread_id.to_string(),
        SessionLifecycle::Idle { session_id },
    );

    Ok(json!({}))
}

/// Handle `thread/resume` — load an existing ACP session.
async fn handle_thread_resume(
    params: &Value,
    agent: &(impl Agent + ?Sized),
    state: &mut GatewayState,
    id_map: &mut IdMap,
    cwd: &std::path::Path,
) -> Result<Value, GatewayError> {
    if !state.initialized {
        return Err(GatewayError::NotInitialized);
    }

    // The Codex client sends threadId for the thread to resume.
    let thread_id = params
        .get("threadId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GatewayError::Translation("missing threadId".into()))?;

    // Look up the existing session for this thread, or use threadId as session_id
    // if this is a session the agent knows about from a previous run.
    let session_id = id_map
        .lookup_session(thread_id)
        .cloned()
        .unwrap_or_else(|| thread_id.to_string());

    state.set_lifecycle(thread_id.to_string(), SessionLifecycle::Loading);

    let acp_req = codex_to_acp::translate_thread_resume(params, &session_id, cwd);
    let acp_resp = agent
        .load_session(acp_req)
        .await
        .map_err(|e| GatewayError::Acp(format!("{e:?}")))?;

    // Store modes and config from the response.
    if let Some(modes) = acp_resp.modes {
        state.session_modes.insert(session_id.clone(), modes);
    }
    if let Some(config) = acp_resp.config_options {
        state.session_config.insert(session_id.clone(), config);
    }

    // Ensure the mapping exists.
    if id_map.lookup_session(thread_id).is_none() {
        id_map.create_thread_session_mapping(thread_id.to_string(), session_id.clone());
    }

    state.set_lifecycle(
        thread_id.to_string(),
        SessionLifecycle::Idle {
            session_id: session_id.clone(),
        },
    );

    info!(%thread_id, %session_id, "thread/session resumed");

    Ok(json!({
        "thread": {
            "id": thread_id,
            "cwd": cwd.to_string_lossy(),
            "cliVersion": "0.1.0",
            "createdAt": chrono_now_secs(),
            "updatedAt": chrono_now_secs(),
            "modelProvider": "acp",
            "preview": "",
            "source": "appServer",
            "status": { "type": "idle" },
            "turns": [],
        },
        "model": "acp-agent",
        "modelProvider": "acp",
        "cwd": cwd.to_string_lossy(),
        "approvalPolicy": "on-request",
        "sandbox": { "type": "dangerFullAccess" },
    }))
}

/// Handle `session/setMode` — change the agent's session mode.
async fn handle_set_session_mode(
    params: &Value,
    agent: &(impl Agent + ?Sized),
    _state: &mut GatewayState,
    id_map: &mut IdMap,
) -> Result<Value, GatewayError> {
    let thread_id = params
        .get("threadId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GatewayError::Translation("missing threadId".into()))?;
    let mode_id = params
        .get("modeId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GatewayError::Translation("missing modeId".into()))?;

    let session_id = id_map
        .lookup_session(thread_id)
        .ok_or_else(|| GatewayError::SessionNotFound {
            thread_id: thread_id.into(),
        })?
        .clone();

    let acp_req = codex_to_acp::translate_set_session_mode(&session_id, mode_id);
    agent
        .set_session_mode(acp_req)
        .await
        .map_err(|e| GatewayError::Acp(format!("{e:?}")))?;

    info!(%thread_id, %mode_id, "session mode changed");

    // The agent will send a CurrentModeUpdate notification confirming the change.
    Ok(json!({}))
}

/// Handle `session/setConfigOption` — change an agent config option.
async fn handle_set_session_config_option(
    params: &Value,
    agent: &(impl Agent + ?Sized),
    state: &mut GatewayState,
    id_map: &mut IdMap,
) -> Result<Value, GatewayError> {
    let thread_id = params
        .get("threadId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GatewayError::Translation("missing threadId".into()))?;
    let config_id = params
        .get("configId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GatewayError::Translation("missing configId".into()))?;
    let value = params
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GatewayError::Translation("missing value".into()))?;

    let session_id = id_map
        .lookup_session(thread_id)
        .ok_or_else(|| GatewayError::SessionNotFound {
            thread_id: thread_id.into(),
        })?
        .clone();

    let acp_req =
        codex_to_acp::translate_set_session_config_option(&session_id, config_id, value);
    let acp_resp = agent
        .set_session_config_option(acp_req)
        .await
        .map_err(|e| GatewayError::Acp(format!("{e:?}")))?;

    // Store updated config in state.
    state
        .session_config
        .insert(session_id.clone(), acp_resp.config_options.clone());

    info!(%thread_id, %config_id, %value, "session config option changed");

    // Return the full updated config options so the client can refresh its UI.
    let options: Vec<Value> = acp_resp
        .config_options
        .iter()
        .map(|opt| {
            json!({
                "id": opt.id.0.as_ref(),
                "name": opt.name,
            })
        })
        .collect();

    Ok(json!({ "configOptions": options }))
}

/// Handle `command/exec` — execute a command locally and return its output.
///
/// This is a Codex client request for one-off command execution (e.g. running
/// a build tool). The gateway handles it directly without involving the ACP
/// agent.
async fn handle_command_exec(
    params: &Value,
    cwd: &std::path::Path,
) -> Result<Value, GatewayError> {
    let command: Vec<String> = params
        .get("command")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| GatewayError::Translation("command/exec: missing 'command' array".into()))?;

    if command.is_empty() {
        return Err(GatewayError::Translation(
            "command/exec: empty command array".into(),
        ));
    }

    let timeout_ms = params
        .get("timeoutMs")
        .and_then(|v| v.as_i64())
        .unwrap_or(30_000);

    let exec_cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| cwd.to_path_buf());

    debug!(cmd = ?command, cwd = %exec_cwd.display(), "command/exec");

    let child = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .current_dir(&exec_cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| GatewayError::Translation(format!("command/exec spawn failed: {e}")))?;

    let output = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms as u64),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| {
        GatewayError::Translation(format!("command/exec timed out after {timeout_ms}ms"))
    })?
    .map_err(|e| GatewayError::Translation(format!("command/exec failed: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code().unwrap_or(-1);
    Ok(json!({
        "exitCode": exit_code,
        "stdout": stdout,
        "stderr": stderr,
    }))
}

/// Handle an ACP event (notification or permission request from the agent).
async fn handle_acp_event(
    event: AcpEvent,
    outgoing: &OutgoingMessageSender,
    state: &mut GatewayState,
    id_map: &mut IdMap,
) {
    match event {
        AcpEvent::SessionNotification(notif) => {
            let session_id = notif.session_id.to_string();
            let thread_id = id_map
                .lookup_thread(&session_id)
                .cloned()
                .unwrap_or_else(|| format!("unknown-{session_id}"));

            // Determine turn_id from state
            let turn_id = match state.lifecycle(&thread_id) {
                SessionLifecycle::InTurn { turn_id, .. } => turn_id.clone(),
                _ => "unknown-turn".to_string(),
            };

            let notifications =
                acp_to_codex::translate_session_update(&notif.update, &thread_id, &turn_id, id_map);

            for (method, params) in notifications {
                if let Err(e) = outgoing.send_notification(&method, Some(params)).await {
                    warn!(method, "failed to send notification: {e}");
                }
            }
        }
        AcpEvent::PermissionRequest {
            request,
            response_tx,
        } => {
            let session_id = request.session_id.to_string();
            let thread_id = id_map
                .lookup_thread(&session_id)
                .cloned()
                .unwrap_or_else(|| format!("unknown-{session_id}"));

            let turn_id = match state.lifecycle(&thread_id) {
                SessionLifecycle::InTurn { turn_id, .. } => turn_id.clone(),
                _ => "unknown-turn".to_string(),
            };

            // Get or create item ID for this tool call
            let tool_call_id = request.tool_call.tool_call_id.0.as_ref();
            let item_id = id_map
                .lookup_item(tool_call_id)
                .cloned()
                .unwrap_or_else(|| id_map.create_item_for_tool(tool_call_id));

            let (method, params) =
                approval::translate_permission_to_codex(&request, &thread_id, &turn_id, &item_id);

            // Send request to Codex client and await response
            match outgoing.send_request(method, Some(params)).await {
                Ok(rx) => {
                    let options = request.options.clone();
                    tokio::task::spawn_local(async move {
                        match rx.await {
                            Ok(resp) => {
                                let acp_resp =
                                    approval::translate_codex_approval_to_acp(&resp.result, &options);
                                let _ = response_tx.send(acp_resp);
                            }
                            Err(_) => {
                                let _ = response_tx.send(
                                    agent_client_protocol::RequestPermissionResponse::new(
                                        agent_client_protocol::RequestPermissionOutcome::Cancelled,
                                    ),
                                );
                            }
                        }
                    });
                }
                Err(e) => {
                    error!("failed to send approval request to codex client: {e}");
                    let _ = response_tx.send(
                        agent_client_protocol::RequestPermissionResponse::new(
                            agent_client_protocol::RequestPermissionOutcome::Cancelled,
                        ),
                    );
                }
            }
        }
    }
}

/// Return current unix timestamp in seconds.
fn chrono_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
