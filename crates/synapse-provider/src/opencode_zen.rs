//! OpenCode Zen provider preset.
//!
//! OpenCode Zen is an OpenAI-compatible aggregator that fronts many models
//! (Anthropic, Qwen, Grok, OpenAI, etc.) behind a single API key. Wire-format
//! identical to the proxy provider; this module pins the canonical base URL,
//! a curated list of useful model IDs for `/settings` quick-pick UX, and a
//! loader for the subscription token written by the OpenCode CLI's auth flow.

/// OpenCode Go subscription tier base URL. The `/zen/v1` path is the
/// pay-per-token aggregator and bills against a separate workspace balance --
/// don't confuse the two. `synapse doctor` will surface "Insufficient balance"
/// if a request lands on the wrong tier.
pub const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

/// Models offered by the GO subscription as of writing. Source: opencode CLI
/// binary's bundled provider config. `synapse doctor` hits `/v1/models` for the
/// live list which is authoritative.
pub const MODEL_PRESETS: &[&str] = &[
    "kimi-k2.5",
    "minimax-m2.7",
    "minimax-m2.5",
    "glm-5.1",
    "glm-5",
    "mimo-v2-pro",
    "mimo-v2-omni",
];

pub const DEFAULT_MODEL: &str = "kimi-k2.5";

/// Auth.json key the OpenCode CLI writes after `opencode providers` login
/// for the Zen subscription. Shape: `{ "opencode-go": { "type": "api", "key": "<token>" } }`.
const AUTH_JSON_PROVIDER_KEY: &str = "opencode-go";

/// Read the OpenCode Zen subscription token from the OpenCode CLI's auth.json.
/// Returns the token string, or None if no auth.json contains an `opencode-go` entry.
///
/// Reuses the same path-search list as the Anthropic OAuth loader so this works
/// across Linux, macOS, WSL, and Windows without further configuration.
pub fn load_subscription_token() -> Option<String> {
    let paths = crate::anthropic::opencode_auth_paths();
    for path in &paths {
        let Ok(data) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        let Some(entry) = v.get(AUTH_JSON_PROVIDER_KEY) else {
            continue;
        };
        if let Some(key) = entry.get("key").and_then(|k| k.as_str())
            && !key.is_empty()
        {
            log::info!(
                "loaded OpenCode Zen subscription token from {}",
                path.display()
            );
            return Some(key.to_string());
        }
    }
    None
}
