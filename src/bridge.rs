//! Reuse the WebSocket transport for an app-server NDJSON router subprocess.
use crate::{config::GatewayConfig, transport::TransportEvent};
use anyhow::{Context, Result};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};
pub async fn run(
    config: &GatewayConfig,
    cwd: &std::path::Path,
    mut inbound: mpsc::Receiver<TransportEvent>,
    outbound: mpsc::Sender<String>,
) -> Result<()> {
    let mut child =
        crate::acp::spawn::spawn_subprocess(&config.agent_cmd, &config.agent_args, cwd)?;
    let mut stdin = child.stdin.take().context("router stdin")?;
    let mut lines = BufReader::new(child.stdout.take().context("router stdout")?).lines();
    loop {
        tokio::select! {
            event = inbound.recv() => match event {
                Some(TransportEvent::MessageReceived{message}) => {
                    stdin.write_all(serde_json::to_string(&message)?.as_bytes()).await?;
                    stdin.write_all(b"\n").await?; stdin.flush().await?;
                }
                _ => break,
            },
            line = lines.next_line() => match line? {Some(line) => { if outbound.send(line).await.is_err(){break;} }, None=>break}
        }
    }
    drop(stdin);
    if tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    Ok(())
}
