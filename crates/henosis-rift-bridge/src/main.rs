//! Standalone adapter for the reusable Rift bridge lifecycle.

use std::path::PathBuf;

use henosis_rift_bridge::{config::BridgeConfig, runtime};

/// Initialize process diagnostics, load bridge configuration, and run the room.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "henosis_rift_bridge=info".into()),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    runtime::run(BridgeConfig::load(&config_path)?).await
}
