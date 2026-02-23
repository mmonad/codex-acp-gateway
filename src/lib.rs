pub mod acp;
pub mod config;
pub mod error;
pub mod translation;
pub mod transport;

use std::rc::Rc;

use agent_client_protocol::{Agent, ClientSideConnection};
use anyhow::{Context, Result};
use codex_app_server_protocol::{JSONRPCMessage, JSONRPCRequest, JSONRPCResponse};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::acp::AcpEvent;
use crate::config::GatewayConfig;
use crate::error::GatewayError;
use crate::translation::acp_to_codex;
use crate::translation::approval;
use crate::translation::codex_to_acp;
use crate::translation::id_map::{self, IdMap};
use crate::translation::state::{GatewayState, SessionLifecycle};
use crate::transport::TransportEvent;
use crate::transport::outgoing::OutgoingMessageSender;
use crate::transport::stdio;

/// Run the gateway. This is the main entry point called from `main()`.
///
/// Sets up the Codex transport, spawns the ACP agent subprocess, and runs the
/// event loop that translates between the two protocols.
pub async fn run(config: GatewayConfig) -> Result<()> {
    let cwd = std::fs::canonicalize(&config.cwd).context("invalid cwd")?;
    info!(
        agent_cmd = %config.agent_cmd,
        agent_args = ?config.agent_args,
        cwd = %cwd.display(),
        "starting codex-acp-gateway"
    );

    // -- Codex transport channels --
    let (codex_inbound_tx, mut codex_inbound_rx) = mpsc::channel::<TransportEvent>(256);
    let (codex_outbound_tx, codex_outbound_rx) = mpsc::channel::<String>(256);

    // Spawn the Codex stdio reader/writer (Send tasks, normal tokio::spawn)
    stdio::spawn_reader(codex_inbound_tx);
    stdio::spawn_writer(codex_outbound_rx);

    let outgoing = OutgoingMessageSender::new(codex_outbound_tx);

    // -- ACP connection (must run on LocalSet because of !Send futures) --
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let (acp_conn, mut acp_events) =
                acp::connect(&config.agent_cmd, &config.agent_args, &cwd)
                    .await
                    .context("failed to connect to ACP agent")?;

            let mut state = GatewayState::new();
            let mut id_map = IdMap::new();
            // Wrap in Rc so we can clone into spawn_local tasks
            let agent = Rc::new(acp_conn.agent);

            info!("gateway event loop started");

            // -- Main event loop: multiplex Codex inbound + ACP events --
            loop {
                tokio::select! {
                    // Codex client sent us a message
                    event = codex_inbound_rx.recv() => {
                        match event {
                            Some(TransportEvent::MessageReceived { message }) => {
                                handle_codex_message(
                                    message,
                                    &agent,
                                    &outgoing,
                                    &mut state,
                                    &mut id_map,
                                    &cwd,
                                ).await;
                            }
                            Some(TransportEvent::Disconnected) | None => {
                                info!("codex client disconnected, shutting down");
                                break;
                            }
                        }
                    }

                    // ACP agent sent us a notification or permission request
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
        })
        .await
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
        "turn/start" => {
            handle_turn_start(&params, Rc::clone(agent), outgoing, state, id_map).await
        }
        "turn/interrupt" => {
            handle_turn_interrupt(&params, agent.as_ref(), state, id_map).await
        }
        method => {
            debug!(method, "unhandled codex request method");
            Err(GatewayError::Translation(format!(
                "unsupported method: {method}"
            )))
        }
    };

    match result {
        Ok(value) => {
            if let Err(e) = outgoing.send_response(id, value).await {
                error!("failed to send response: {e}");
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

/// Handle `initialize` - translate to ACP initialize and return response.
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

    state.initialized = true;
    if let Some(info) = params.get("clientInfo") {
        state.client_name = info.get("name").and_then(|v| v.as_str()).map(String::from);
        state.client_version = info
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);
    }

    let version = format!("{}", acp_resp.protocol_version);
    info!(protocol_version = %version, "ACP initialize complete");

    Ok(json!({
        "userAgent": format!(
            "codex-acp-gateway/0.1.0 (acp-agent/{})",
            acp_resp.agent_info
                .as_ref()
                .map(|i| i.version.as_str())
                .unwrap_or("unknown")
        ),
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
