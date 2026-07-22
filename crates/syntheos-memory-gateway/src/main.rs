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
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env();
    let bind_addr = config::validated_bind_addr(
        &config.bind_addr,
        std::env::var("SYNTHEOS_GATEWAY_ALLOW_INSECURE_REMOTE")
            .ok()
            .as_deref(),
    )
    .unwrap_or_else(|error| panic!("{error}"));

    // Load the signing key (PIV YubiKey attempted first, Ed25519 software key as fallback).
    let signer = match RequestSigner::from_env_or_file(
        &config.signing_host,
        &config.signing_agent,
        &config.signing_model,
    ) {
        Ok(Some(s)) => {
            tracing::info!(
                fingerprint = %s.fingerprint(),
                identity = %s.identity_hash(),
                algo = %s.algo(),
                "signing key loaded"
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
            // Fail closed: a key-load error is a misconfiguration, not a reason
            // to silently drop outbound authentication. Refuse to start rather
            // than proxy to Kleos unsigned. (Ok(None) above is the explicit
            // no-key-configured path and stays a warning.)
            tracing::error!(error = %e, "failed to load signing key; refusing to start unsigned");
            std::process::exit(1);
        }
    };

    let client = KleosClient::new(&config, signer);
    let app = routes::router(client);

    let listener = tokio::net::TcpListener::bind(bind_addr)
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
