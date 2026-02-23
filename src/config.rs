use std::path::PathBuf;

use clap::Parser;

/// Protocol translation gateway between Codex app-server clients and ACP agents.
#[derive(Parser, Debug)]
#[command(name = "codex-acp-gateway", about)]
pub struct GatewayConfig {
    /// Command to spawn as the ACP agent (e.g. "gemini").
    #[arg(long)]
    pub agent_cmd: String,

    /// Arguments to pass to the ACP agent command.
    /// Everything after `--` on the command line is forwarded here.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub agent_args: Vec<String>,

    /// Working directory for the ACP agent subprocess.
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,

    /// Log level (trace, debug, info, warn, error).
    #[arg(long, default_value = "info")]
    pub log_level: String,
}
