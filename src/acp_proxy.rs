//! Transparent ACP WebSocket-to-stdio proxy.
//!
//! Accepts ACP client connections over WebSocket and bridges each one to an
//! ACP agent subprocess over stdio.  Messages pass through unmodified — no
//! protocol translation, just framing conversion:
//!
//!   WS text message  ⟷  NDJSON line on stdin/stdout
//!
//! Each WebSocket connection gets its own agent subprocess.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::acp::spawn::spawn_subprocess;
use crate::config::{GatewayConfig, ListenTransport};

/// Run the ACP proxy.  Listens for WebSocket connections and bridges each
/// to an agent subprocess.
pub async fn run(config: &GatewayConfig) -> Result<()> {
    let bind_address = match &config.listen {
        ListenTransport::WebSocket { bind_address } => *bind_address,
        ListenTransport::Stdio => {
            bail!("acp-proxy mode requires --listen ws://IP:PORT (stdio not supported)");
        }
    };

    let cwd = std::fs::canonicalize(&config.cwd).context("invalid cwd")?;

    info!(
        %bind_address,
        agent_cmd = %config.agent_cmd,
        agent_args = ?config.agent_args,
        "starting ACP WebSocket proxy"
    );

    let listener = TcpListener::bind(bind_address)
        .await
        .with_context(|| format!("failed to bind {bind_address}"))?;

    info!(%bind_address, "listening for ACP WebSocket connections");

    loop {
        let (tcp_stream, peer) = listener.accept().await?;
        let agent_cmd = config.agent_cmd.clone();
        let agent_args = config.agent_args.clone();
        let cwd = cwd.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(tcp_stream, peer, &agent_cmd, &agent_args, &cwd).await
            {
                error!(%peer, "connection error: {e:#}");
            }
        });
    }
}

/// Handle a single ACP client WebSocket connection by bridging it to an
/// agent subprocess.
async fn handle_connection(
    tcp_stream: tokio::net::TcpStream,
    peer: SocketAddr,
    agent_cmd: &str,
    agent_args: &[String],
    cwd: &Path,
) -> Result<()> {
    info!(%peer, "new ACP client connection");

    let ws_stream = tokio_tungstenite::accept_async(tcp_stream)
        .await
        .context("WebSocket handshake failed")?;

    let (mut ws_write, mut ws_read) = ws_stream.split();

    // Spawn the ACP agent subprocess.
    let mut child = spawn_subprocess(agent_cmd, agent_args, cwd)?;
    let child_stdin = child.stdin.take().context("agent has no stdin")?;
    let child_stdout = child.stdout.take().context("agent has no stdout")?;

    let mut stdin_writer = child_stdin;
    let mut stdout_reader = BufReader::new(child_stdout).lines();

    info!(%peer, pid = child.id().unwrap_or(0), "agent subprocess spawned");

    // Task: agent stdout → WS (each NDJSON line becomes a WS text message)
    let stdout_to_ws = async {
        while let Ok(Some(line)) = stdout_reader.next_line().await {
            debug!(%peer, direction = "agent→ws", len = line.len(), "forwarding");
            if ws_write.send(Message::Text(line.into())).await.is_err() {
                debug!(%peer, "WS write failed, stopping stdout→ws");
                break;
            }
        }
        // Agent closed stdout — send WS close frame.
        let _ = ws_write.send(Message::Close(None)).await;
        debug!(%peer, "agent stdout closed");
    };

    // Task: WS → agent stdin (each WS text message becomes an NDJSON line)
    let ws_to_stdin = async {
        while let Some(frame) = ws_read.next().await {
            match frame {
                Ok(Message::Text(text)) => {
                    let text: &str = &text;
                    debug!(%peer, direction = "ws→agent", len = text.len(), "forwarding");
                    let mut line = text.as_bytes().to_vec();
                    if !text.ends_with('\n') {
                        line.push(b'\n');
                    }
                    if stdin_writer.write_all(&line).await.is_err() {
                        debug!(%peer, "stdin write failed, stopping ws→agent");
                        break;
                    }
                    if stdin_writer.flush().await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) => {
                    debug!(%peer, "WS close received");
                    break;
                }
                Ok(
                    Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_),
                ) => {}
                Err(e) => {
                    warn!(%peer, error = %e, "WS read error");
                    break;
                }
            }
        }
        // Client disconnected — close agent stdin to signal EOF.
        drop(stdin_writer);
        debug!(%peer, "WS client disconnected, closed agent stdin");
    };

    // Run both directions concurrently; when either finishes, clean up.
    tokio::select! {
        _ = stdout_to_ws => {}
        _ = ws_to_stdin => {}
    }

    // Kill the agent if it's still running.
    let _ = child.kill().await;
    let _ = child.wait().await;
    info!(%peer, "connection closed, agent terminated");

    Ok(())
}
