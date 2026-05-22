//! syntheos-memory-gateway: a standalone HTTP service that exposes the
//! FrameShift `frameshift-memory-http` wire contract and proxies each call to a
//! Kleos instance, letting FrameShift use Kleos as its memory backend without
//! modifying either system's source.

mod config;
mod dto;
mod error;
mod kleos;
mod routes;
mod signing;

use config::Config;
use kleos::KleosClient;
use signing::RequestSigner;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

/// Bootstrap the gateway: init tracing, load config, load signing key, build
/// the router, and start serving requests.
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config = Config::from_env();

    // Warn when binding on a non-loopback address -- this exposes the gateway
    // to the network without transport-level auth on the Kleos side.
    if !config.bind_addr.starts_with("127.") && !config.bind_addr.starts_with("localhost") {
        tracing::warn!(
            bind_addr = %config.bind_addr,
            "gateway is binding on a non-loopback address; \
             ensure this is intentional and the network is trusted"
        );
    }

    // Load the Ed25519 signing key.
    let signer = match RequestSigner::from_env_or_file(
        &config.signing_host,
        &config.signing_agent,
        &config.signing_model,
    ) {
        Ok(Some(s)) => {
            tracing::info!(
                fingerprint = %s.fingerprint(),
                identity = %s.identity_hash(),
                "Ed25519 signing key loaded"
            );
            Some(Arc::new(s))
        }
        Ok(None) => {
            tracing::warn!(
                "no signing key found; requests to Kleos will be unauthenticated and may be rejected"
            );
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to load signing key; starting without auth");
            None
        }
    };

    let client = KleosClient::new(&config, signer);
    let app = routes::router(client);

    let listener = tokio::net::TcpListener::bind(config.bind_addr.as_str())
        .await
        .unwrap_or_else(|e| panic!("failed to bind {}: {e}", config.bind_addr));
    tracing::info!(
        "syntheos-memory-gateway listening on {} -> {}",
        config.bind_addr,
        config.kleos_base_url
    );
    axum::serve(listener, app)
        .await
        .expect("gateway server error");
}
