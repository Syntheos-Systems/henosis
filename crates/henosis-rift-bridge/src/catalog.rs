//! Secret-free discovery of execution harnesses available on the current host.

use std::collections::BTreeSet;
use std::path::Path;

use henosis_rift_server::models::agent_control::{
    CapabilityHarness, CapabilityModel, CapabilityOption, CapabilitySetting,
    CapabilitySettingControl, CredentialMode, ExecutionCapabilityCatalog,
};
use uuid::Uuid;

use crate::config::{BridgeConfig, ExecutorConfig};

/// Stable Claude Code model aliases exposed by a default Henosis deployment.
const CLAUDE_MODEL_ALIASES: &[&str] = &["sonnet", "opus", "haiku"];

/// Stable Codex models explicitly supported by this Henosis release.
const CODEX_MODELS: &[&str] = &["gpt-5.6-sol"];

/// Discover the current host catalog without probing credentials or starting executors.
pub fn discover_catalog(base: &BridgeConfig, generation: Uuid) -> ExecutionCapabilityCatalog {
    ExecutionCapabilityCatalog {
        generation,
        harnesses: vec![
            claude_harness(base),
            codex_harness(base),
            synapse_harness(base),
        ],
    }
}

/// Build the Claude Code descriptor from deployment templates and host binaries.
fn claude_harness(base: &BridgeConfig) -> CapabilityHarness {
    let templates = base
        .agents
        .iter()
        .filter_map(|agent| match &agent.executor {
            ExecutorConfig::ClaudeCode { binary, model, .. } => Some((binary, model.as_deref())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let available = templates
        .iter()
        .any(|(binary, _)| command_available(binary));
    let mut models = CLAUDE_MODEL_ALIASES
        .iter()
        .map(|model| (*model).to_string())
        .collect::<BTreeSet<_>>();
    for (_, model) in &templates {
        if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
            models.insert(model.to_string());
        }
    }
    CapabilityHarness {
        id: "claude-code".to_string(),
        label: "Claude Code".to_string(),
        available,
        unavailable_reason: unavailable_reason(templates.is_empty(), available),
        credential_mode: CredentialMode::OptionalBinding,
        models: capability_models(models, available),
        settings: Vec::new(),
    }
}

/// Build the Codex descriptor from deployment templates and host binaries.
fn codex_harness(base: &BridgeConfig) -> CapabilityHarness {
    let templates = base
        .agents
        .iter()
        .filter_map(|agent| match &agent.executor {
            ExecutorConfig::Codex { binary, model, .. } => Some((binary, model.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let available = templates
        .iter()
        .any(|(binary, _)| command_available(binary));
    let mut models = CODEX_MODELS
        .iter()
        .map(|model| (*model).to_string())
        .collect::<BTreeSet<_>>();
    for (_, model) in &templates {
        if !model.trim().is_empty() {
            models.insert((*model).to_string());
        }
    }
    CapabilityHarness {
        id: "codex".to_string(),
        label: "Codex".to_string(),
        available,
        unavailable_reason: unavailable_reason(templates.is_empty(), available),
        credential_mode: CredentialMode::OptionalBinding,
        models: capability_models(models, available),
        settings: vec![CapabilitySetting {
            id: "reasoning_effort".to_string(),
            label: "Reasoning effort".to_string(),
            required: false,
            control: CapabilitySettingControl::Select {
                options: ["low", "medium", "high", "xhigh", "max", "ultra"]
                    .into_iter()
                    .map(|value| CapabilityOption {
                        id: value.to_string(),
                        label: title_case(value),
                    })
                    .collect(),
            },
        }],
    }
}

/// Build the in-process Synapse descriptor from configured provider templates.
fn synapse_harness(base: &BridgeConfig) -> CapabilityHarness {
    let mut templates = base
        .agents
        .iter()
        .filter_map(|agent| match &agent.executor {
            ExecutorConfig::Synapse { model, .. } => model.as_deref(),
            _ => None,
        })
        .filter(|model| !model.trim().is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let configured = base
        .agents
        .iter()
        .any(|agent| matches!(agent.executor, ExecutorConfig::Synapse { .. }));
    if configured && templates.is_empty() {
        templates.insert("default".to_string());
    }
    CapabilityHarness {
        id: "synapse".to_string(),
        label: "Synapse".to_string(),
        available: configured,
        unavailable_reason: (!configured).then(|| "harness is not configured".to_string()),
        credential_mode: CredentialMode::HostSession,
        models: capability_models(templates, configured),
        settings: vec![
            integer_setting("max_tokens", "Maximum tokens", 1, 1_000_000),
            integer_setting("max_turns", "Maximum turns", 1, 100),
        ],
    }
}

/// Convert stable model identifiers into dashboard descriptors.
fn capability_models(
    models: impl IntoIterator<Item = String>,
    available: bool,
) -> Vec<CapabilityModel> {
    models
        .into_iter()
        .map(|id| CapabilityModel {
            label: model_label(&id),
            id,
            available,
            unavailable_reason: (!available).then(|| "harness is unavailable".to_string()),
        })
        .collect()
}

/// Construct one bounded integer setting descriptor.
fn integer_setting(id: &str, label: &str, minimum: i64, maximum: i64) -> CapabilitySetting {
    CapabilitySetting {
        id: id.to_string(),
        label: label.to_string(),
        required: false,
        control: CapabilitySettingControl::Integer {
            minimum,
            maximum,
            step: 1,
        },
    }
}

/// Return a stable unavailable explanation without exposing local paths.
fn unavailable_reason(unconfigured: bool, available: bool) -> Option<String> {
    if available {
        None
    } else if unconfigured {
        Some("harness is not configured".to_string())
    } else {
        Some("configured command is unavailable".to_string())
    }
}

/// Convert a stable model identifier into a compact display label.
fn model_label(model: &str) -> String {
    match model {
        "gpt-5.6-sol" => "GPT-5.6 Sol".to_string(),
        "sonnet" => "Claude Sonnet".to_string(),
        "opus" => "Claude Opus".to_string(),
        "haiku" => "Claude Haiku".to_string(),
        other => other.to_string(),
    }
}

/// Capitalize a lower-case setting option for display.
fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

/// Report whether an explicit path or PATH-resolved command names an executable file.
pub(crate) fn command_available(command: &Path) -> bool {
    if command.is_absolute() || command.components().count() > 1 {
        return executable_file(command);
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(command))
        .any(|candidate| executable_file(&candidate))
}

/// Check whether a candidate is a regular file with platform-appropriate execute access.
fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
