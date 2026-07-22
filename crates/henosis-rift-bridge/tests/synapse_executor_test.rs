use henosis_rift_bridge::executors::build_synapse_executor;

/// Verifies Foundry Anthropic configuration rejects a missing host.
#[test]
fn test_rejects_missing_host_for_foundry() {
    let result = build_synapse_executor(
        "foundry-anthropic",
        Some("claude-sonnet-4-6".into()),
        None, // no host
        Some("token".into()),
        None,
        None,
        None,
        None,
    );
    assert!(result.is_err(), "foundry provider requires host");
}

/// Verifies Foundry Anthropic configuration rejects a missing token.
#[test]
fn test_rejects_missing_token_for_foundry() {
    let result = build_synapse_executor(
        "foundry-anthropic",
        Some("claude-sonnet-4-6".into()),
        Some("your-foundry-host.example.com".into()),
        None, // no token
        None,
        None,
        None,
        None,
    );
    assert!(result.is_err(), "foundry provider requires token");
}

/// Verifies text-only Claude Max configuration can be constructed without explicit credentials.
#[test]
fn test_claude_max_needs_no_credentials() {
    let result = build_synapse_executor(
        "claude-max",
        Some("claude-sonnet-4-6".into()),
        None,
        None,
        None,
        None,
        None,
        None,
    );
    assert!(
        result.is_ok(),
        "text-only claude-max should work with no explicit credentials"
    );
}

/// Verifies the generic OpenAI-compatible proxy arm builds with a base URL
/// and explicit key (the DeepSeek cheap-agent path).
#[test]
fn test_proxy_provider_builds_with_host_and_key() {
    let result = build_synapse_executor(
        "proxy",
        Some("deepseek-v4-flash".into()),
        Some("https://api.example.com".into()),
        None,
        Some("test-key".into()),
        None,
        None,
        None,
    );
    assert!(
        result.is_ok(),
        "proxy provider should build: {:?}",
        result.err().map(|e| e.to_string())
    );
}

/// Verifies the proxy arm rejects a missing base URL.
#[test]
fn test_proxy_provider_rejects_missing_host() {
    let result = build_synapse_executor(
        "proxy",
        None,
        None, // no host
        None,
        Some("test-key".into()),
        None,
        None,
        None,
    );
    assert!(result.is_err(), "proxy provider requires host");
}

/// Verifies unknown provider identifiers are rejected.
#[test]
fn test_rejects_unknown_provider() {
    let result = build_synapse_executor(
        "nonexistent-provider",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    assert!(result.is_err(), "unknown provider should fail");
}
