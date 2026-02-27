//! Shared `command/exec` handler used by both Codex and ACP-proxy modes.
//!
//! Executes a subprocess with an explicit argument list (no shell invocation)
//! and returns structured `{exitCode, stdout, stderr}` output.

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tracing::debug;

/// Execute a command described by JSON-RPC `params` and return the result.
///
/// Expected params shape:
/// ```json
/// {
///   "command": ["ls", "-la"],
///   "cwd": "/optional/override",
///   "timeoutMs": 30000
/// }
/// ```
pub async fn run(params: &Value, default_cwd: &Path) -> Result<Value> {
    let command: Vec<String> = params
        .get("command")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    if command.is_empty() {
        bail!("command/exec: missing or empty 'command' array");
    }

    let timeout_ms = params
        .get("timeoutMs")
        .and_then(|v| v.as_i64())
        .unwrap_or(30_000);

    let exec_cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| default_cwd.to_path_buf());

    debug!(cmd = ?command, cwd = %exec_cwd.display(), "command/exec");

    let child = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .current_dir(&exec_cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let output = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms as u64),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("command/exec timed out after {timeout_ms}ms"))??;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code().unwrap_or(-1);

    Ok(json!({
        "exitCode": exit_code,
        "stdout": stdout,
        "stderr": stderr,
    }))
}
