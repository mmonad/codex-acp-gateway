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

    if let Err(e) = codex_acp_gateway::run(config).await {
        tracing::error!("gateway exited with error: {e:#}");
        std::process::exit(1);
    }
}
