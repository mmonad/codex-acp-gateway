use clap::Parser;
use codex_acp_gateway::config::GatewayConfig;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let config = GatewayConfig::parse();

    // Initialize tracing with the configured log level
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level)),
        )
        .with_writer(std::io::stderr)
        .init();

    let result = match config.mode {
        codex_acp_gateway::config::GatewayMode::Codex => codex_acp_gateway::run(config).await,
        codex_acp_gateway::config::GatewayMode::AcpProxy => {
            codex_acp_gateway::acp_proxy::run(&config).await
        }
    };

    if let Err(e) = result {
        tracing::error!("gateway exited with error: {e:#}");
        std::process::exit(1);
    }
}
