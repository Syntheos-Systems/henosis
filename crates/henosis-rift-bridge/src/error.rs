/// Bridge-level errors.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// Error communicating with the Rift REST API.
    #[error("Rift API error: {0}")]
    RiftApi(String),

    /// The managed-room leadership capability was rejected or could not be verified.
    #[error("managed-room leadership is no longer authorized")]
    StaleLeadership,

    /// WebSocket connection or protocol error.
    #[error("WebSocket error: {0}")]
    WebSocket(String),

    /// Agent executor failed to produce a response.
    #[error("Executor error: {0}")]
    Executor(String),

    /// Configuration file is missing or malformed.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Kleos integration failed or returned an unexpected payload.
    #[error("Kleos integration error: {0}")]
    Kleos(String),

    /// Execution-mode orchestration failed (approval, supervision, or writeback).
    #[error("Execution error: {0}")]
    Execution(String),

    /// Sandbox (git worktree) creation or teardown failed.
    #[error("Sandbox error: {0}")]
    Sandbox(String),

    /// JWT issuance or validation failed.
    #[error("Authentication error: {0}")]
    Auth(String),

    /// Embeddings endpoint failed or returned an unusable payload.
    #[error("Embedding error: {0}")]
    Embedding(String),

    /// Durable Rift event admission state could not safely authorize dispatch.
    #[error("Rift event state error: {0}")]
    EventState(String),

    /// Underlying HTTP transport error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// File system I/O error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
