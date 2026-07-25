//! Standalone adapter for the reusable Rift server lifecycle.

use henosis_rift_server::{config::Config, runtime};

/// Initialize process-level diagnostics and run the Rift server.
#[tokio::main]
async fn main() -> Result<(), runtime::RuntimeError> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "henosis_rift_server=debug,tower_http=info".into()),
        )
        .init();

    runtime::serve(Config::from_env()).await
}
