//! Public, secret-free contracts for persistent room agent control.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Maximum number of execution seats accepted in one room revision.
pub const MAX_AGENT_SEATS: usize = 32;

/// Maximum serialized settings payload accepted for one seat.
pub const MAX_SETTINGS_BYTES: usize = 16 * 1024;

/// Durable bridge-application lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyState {
    /// No managed revision has been requested.
    Idle,
    /// A desired revision is waiting for reconciliation.
    Pending,
    /// The desired revision is running and is the last known good revision.
    Active,
    /// The desired revision failed validation, preflight, or startup.
    Failed,
}

/// Safe identity details shown in the agent dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIdentitySummary {
    /// Stable Rift user identifier for the agent.
    pub id: Uuid,
    /// Unique Rift username.
    pub username: String,
    /// Optional human-readable display name.
    pub display_name: Option<String>,
    /// Human owner, or `None` for an imported unclaimed agent.
    pub owner_user_id: Option<Uuid>,
}

/// One desired execution seat supplied by the dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSeatInput {
    /// Stable seat identity that survives reordering.
    pub seat_id: Uuid,
    /// Persistent Rift agent identity occupying the seat.
    pub agent_user_id: Uuid,
    /// Catalog harness identifier.
    pub harness_id: String,
    /// Catalog model identifier under the harness.
    pub model_id: String,
    /// Typed, non-secret harness settings.
    pub settings: Value,
    /// Opaque deployment-owned credential binding identifier.
    pub credential_binding_id: Option<Uuid>,
    /// Whether this seat participates in room responses.
    pub enabled: bool,
    /// Non-negative display and execution order.
    pub position: i32,
}

/// Readiness of the execution credentials selected for a seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialReadiness {
    /// The harness will use an authenticated session already present on the host.
    HostSession,
    /// The opaque binding exists, belongs to the owner, and can be mediated.
    Ready,
    /// No usable host session or binding is available.
    Unavailable,
    /// The binding exists but needs human intervention.
    Attention,
}

/// Credential modes supported by one execution harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialMode {
    /// The harness uses only a host-authenticated session.
    HostSession,
    /// The harness can use a host session or an opaque binding.
    OptionalBinding,
    /// The harness requires an opaque binding.
    RequiredBinding,
}

/// One desired seat enriched with ownership and credential readiness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSeatView {
    /// Submitted non-secret seat configuration.
    pub seat: AgentSeatInput,
    /// Current human owner of the selected agent identity.
    pub owner_user_id: Option<Uuid>,
    /// Deployment-resolved readiness without credential contents.
    pub credential_readiness: CredentialReadiness,
}

/// Current durable room roster and reconciliation status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomAgentRoster {
    /// Rift server whose bridge the roster configures.
    pub server_id: Uuid,
    /// Latest desired immutable revision.
    pub desired_revision: Option<i64>,
    /// Revision currently running in the bridge.
    pub active_revision: Option<i64>,
    /// Most recent revision proven to start successfully.
    pub last_good_revision: Option<i64>,
    /// Current reconciliation state.
    pub apply_state: ApplyState,
    /// Stable failure code for the desired revision.
    pub apply_error_code: Option<String>,
    /// Bounded human-readable failure detail.
    pub apply_error_message: Option<String>,
    /// Desired revision seats in position order.
    pub seats: Vec<AgentSeatView>,
}

/// Optimistic whole-roster replacement submitted by the dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateRoomAgentRoster {
    /// Desired revision observed by the editor before it made changes.
    pub expected_revision: Option<i64>,
    /// Complete next roster, not a partial patch.
    pub seats: Vec<AgentSeatInput>,
}

/// One model exposed by an execution harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityModel {
    /// Stable model identifier passed to the harness.
    pub id: String,
    /// Human-readable model name.
    pub label: String,
    /// Whether the current host can use this model.
    pub available: bool,
    /// Safe explanation when the model is unavailable.
    pub unavailable_reason: Option<String>,
}

/// One selectable value for a typed capability setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityOption {
    /// Stable submitted value.
    pub id: String,
    /// Human-readable option name.
    pub label: String,
}

/// Dashboard control and validation contract for one harness setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CapabilitySettingControl {
    /// A finite set of string values.
    Select {
        /// Allowed submitted values.
        options: Vec<CapabilityOption>,
    },
    /// A bounded stepped integer.
    Integer {
        /// Inclusive lower bound.
        minimum: i64,
        /// Inclusive upper bound.
        maximum: i64,
        /// Positive increment measured from `minimum`.
        step: i64,
    },
    /// A boolean toggle.
    Boolean,
}

/// One typed, non-secret setting exposed by a harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitySetting {
    /// Stable settings-object key.
    pub id: String,
    /// Human-readable setting name.
    pub label: String,
    /// Whether every seat must submit the setting.
    pub required: bool,
    /// Control type and value constraints.
    pub control: CapabilitySettingControl,
}

/// One host execution harness and its selectable capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityHarness {
    /// Stable harness identifier.
    pub id: String,
    /// Human-readable harness name.
    pub label: String,
    /// Whether the harness is usable on the current host.
    pub available: bool,
    /// Safe explanation when the harness is unavailable.
    pub unavailable_reason: Option<String>,
    /// Credential selection supported by the harness.
    pub credential_mode: CredentialMode,
    /// Models allowed by this deployment.
    pub models: Vec<CapabilityModel>,
    /// Typed, non-secret harness settings.
    pub settings: Vec<CapabilitySetting>,
}

/// Generation-stamped host execution catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionCapabilityCatalog {
    /// Changes whenever the discovered host catalog is rebuilt.
    pub generation: Uuid,
    /// Stable harnesses and their deployment-owned choices.
    pub harnesses: Vec<CapabilityHarness>,
}

/// Public bridge status for a human room member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatus {
    /// Whether the room bridge is paused.
    pub paused: bool,
    /// Latest desired immutable revision.
    pub desired_revision: Option<i64>,
    /// Revision currently running.
    pub active_revision: Option<i64>,
    /// Most recent proven-good revision.
    pub last_good_revision: Option<i64>,
    /// Current reconciliation state.
    pub apply_state: ApplyState,
    /// Stable failure code for the desired revision.
    pub apply_error_code: Option<String>,
    /// Bounded safe failure detail.
    pub apply_error_message: Option<String>,
}

/// Durable status fields written by the bridge reconciler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyStatusUpdate {
    /// Revision currently running after this update.
    pub active_revision: Option<i64>,
    /// Last revision proven to start successfully.
    pub last_good_revision: Option<i64>,
    /// New lifecycle state.
    pub apply_state: ApplyState,
    /// Stable failure code, cleared on success.
    pub error_code: Option<String>,
    /// Bounded safe failure detail, cleared on success.
    pub error_message: Option<String>,
}

/// Validation failure for a roster or capability descriptor.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct AgentControlValidationError {
    /// Stable machine-readable error code.
    pub code: &'static str,
    /// Safe human-readable detail.
    pub message: String,
}

/// Validates public room roster inputs before authorization or persistence.
impl UpdateRoomAgentRoster {
    /// Reject malformed, oversized, or duplicate seat definitions.
    pub fn validate(&self) -> Result<(), AgentControlValidationError> {
        if self.seats.len() > MAX_AGENT_SEATS {
            return Err(validation_error(
                "too_many_seats",
                format!("a room may contain at most {MAX_AGENT_SEATS} agent seats"),
            ));
        }
        let mut seat_ids = HashSet::with_capacity(self.seats.len());
        let mut agent_ids = HashSet::with_capacity(self.seats.len());
        let mut positions = HashSet::with_capacity(self.seats.len());
        for seat in &self.seats {
            seat.validate()?;
            if !seat_ids.insert(seat.seat_id) {
                return Err(validation_error(
                    "duplicate_seat",
                    "seat IDs must be unique",
                ));
            }
            if !agent_ids.insert(seat.agent_user_id) {
                return Err(validation_error(
                    "duplicate_agent",
                    "an agent identity may occupy only one room seat",
                ));
            }
            if !positions.insert(seat.position) {
                return Err(validation_error(
                    "duplicate_position",
                    "seat positions must be unique",
                ));
            }
        }
        Ok(())
    }
}

/// Validates the intrinsic shape of one desired execution seat.
impl AgentSeatInput {
    /// Reject invalid identifiers, positions, and settings envelopes.
    pub fn validate(&self) -> Result<(), AgentControlValidationError> {
        validate_identifier("harness_id", &self.harness_id, 64)?;
        validate_identifier("model_id", &self.model_id, 128)?;
        if self.position < 0 {
            return Err(validation_error(
                "invalid_position",
                "seat position must be non-negative",
            ));
        }
        if !self.settings.is_object() {
            return Err(validation_error(
                "invalid_settings",
                "seat settings must be a JSON object",
            ));
        }
        let bytes = serde_json::to_vec(&self.settings).map_err(|error| {
            validation_error(
                "invalid_settings",
                format!("seat settings could not be serialized: {error}"),
            )
        })?;
        if bytes.len() > MAX_SETTINGS_BYTES {
            return Err(validation_error(
                "settings_too_large",
                format!("seat settings may contain at most {MAX_SETTINGS_BYTES} bytes"),
            ));
        }
        Ok(())
    }
}

/// Validates host capability descriptors and submitted setting values.
impl CapabilityHarness {
    /// Reject duplicate or malformed model and setting descriptors.
    pub fn validate(&self) -> Result<(), AgentControlValidationError> {
        validate_identifier("harness_id", &self.id, 64)?;
        let mut model_ids = HashSet::with_capacity(self.models.len());
        for model in &self.models {
            validate_identifier("model_id", &model.id, 128)?;
            if !model_ids.insert(model.id.as_str()) {
                return Err(validation_error(
                    "duplicate_model",
                    "model IDs must be unique within a harness",
                ));
            }
        }
        let mut setting_ids = HashSet::with_capacity(self.settings.len());
        for setting in &self.settings {
            setting.validate_descriptor()?;
            if !setting_ids.insert(setting.id.as_str()) {
                return Err(validation_error(
                    "duplicate_setting",
                    "setting IDs must be unique within a harness",
                ));
            }
        }
        Ok(())
    }

    /// Validate one submitted settings object against this harness.
    pub fn validate_settings(&self, submitted: &Value) -> Result<(), AgentControlValidationError> {
        let object = submitted.as_object().ok_or_else(|| {
            validation_error("invalid_settings", "seat settings must be a JSON object")
        })?;
        for key in object.keys() {
            if !self.settings.iter().any(|setting| setting.id == *key) {
                return Err(validation_error(
                    "unsupported_setting",
                    format!("setting {key:?} is not supported by harness {:?}", self.id),
                ));
            }
        }
        for setting in &self.settings {
            match object.get(&setting.id) {
                Some(value) => setting.validate_value(value)?,
                None if setting.required => {
                    return Err(validation_error(
                        "missing_setting",
                        format!("required setting {:?} is missing", setting.id),
                    ));
                }
                None => {}
            }
        }
        Ok(())
    }
}

/// Validates one typed capability setting descriptor and its values.
impl CapabilitySetting {
    /// Reject internally inconsistent control definitions.
    pub fn validate_descriptor(&self) -> Result<(), AgentControlValidationError> {
        validate_identifier("setting_id", &self.id, 64)?;
        match &self.control {
            CapabilitySettingControl::Select { options } => {
                if options.is_empty() {
                    return Err(validation_error(
                        "empty_setting_options",
                        format!("select setting {:?} must declare options", self.id),
                    ));
                }
                let mut ids = HashSet::with_capacity(options.len());
                for option in options {
                    validate_identifier("option_id", &option.id, 64)?;
                    if !ids.insert(option.id.as_str()) {
                        return Err(validation_error(
                            "duplicate_setting_option",
                            format!("select setting {:?} has duplicate options", self.id),
                        ));
                    }
                }
            }
            CapabilitySettingControl::Integer {
                minimum,
                maximum,
                step,
            } if minimum > maximum || *step <= 0 => {
                return Err(validation_error(
                    "invalid_integer_range",
                    format!("integer setting {:?} has an invalid range", self.id),
                ));
            }
            CapabilitySettingControl::Integer { .. } | CapabilitySettingControl::Boolean => {}
        }
        Ok(())
    }

    /// Reject a submitted JSON value outside this setting's control contract.
    pub fn validate_value(&self, value: &Value) -> Result<(), AgentControlValidationError> {
        let valid = match &self.control {
            CapabilitySettingControl::Select { options } => value
                .as_str()
                .is_some_and(|selected| options.iter().any(|option| option.id == selected)),
            CapabilitySettingControl::Integer {
                minimum,
                maximum,
                step,
            } => value.as_i64().is_some_and(|integer| {
                (*minimum..=*maximum).contains(&integer) && (integer - *minimum) % *step == 0
            }),
            CapabilitySettingControl::Boolean => value.is_boolean(),
        };
        if valid {
            Ok(())
        } else {
            Err(validation_error(
                "unsupported_setting_value",
                format!("setting {:?} contains an unsupported value", self.id),
            ))
        }
    }
}

/// Validate an ASCII identifier accepted across API, database, and CLI boundaries.
fn validate_identifier(
    field: &str,
    value: &str,
    maximum: usize,
) -> Result<(), AgentControlValidationError> {
    if value.is_empty()
        || value.len() > maximum
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(validation_error(
            "invalid_identifier",
            format!(
                "{field} must contain 1 to {maximum} ASCII letters, digits, dots, underscores, or hyphens"
            ),
        ));
    }
    Ok(())
}

/// Construct one stable validation failure.
fn validation_error(code: &'static str, message: impl Into<String>) -> AgentControlValidationError {
    AgentControlValidationError {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
/// Exercises secret-free agent-control wire and validation contracts.
mod tests {
    use super::*;
    use serde_json::json;

    /// Build one intrinsically valid seat for mutation-based tests.
    fn seat(position: i32) -> AgentSeatInput {
        AgentSeatInput {
            seat_id: Uuid::new_v4(),
            agent_user_id: Uuid::new_v4(),
            harness_id: "codex".to_string(),
            model_id: "gpt-5.6-sol".to_string(),
            settings: json!({"reasoning_effort": "medium"}),
            credential_binding_id: None,
            enabled: true,
            position,
        }
    }

    /// Apply states use stable snake-case values on the wire.
    #[test]
    fn apply_states_serialize_stably() {
        assert_eq!(
            serde_json::to_string(&ApplyState::Idle).unwrap(),
            "\"idle\""
        );
        assert_eq!(
            serde_json::to_string(&ApplyState::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&ApplyState::Active).unwrap(),
            "\"active\""
        );
        assert_eq!(
            serde_json::to_string(&ApplyState::Failed).unwrap(),
            "\"failed\""
        );
    }

    /// Whole-roster validation rejects excessive size and duplicate identities.
    #[test]
    fn roster_bounds_and_uniqueness_are_enforced() {
        let too_many = UpdateRoomAgentRoster {
            expected_revision: None,
            seats: (0..=MAX_AGENT_SEATS)
                .map(|index| seat(index as i32))
                .collect(),
        };
        assert_eq!(too_many.validate().unwrap_err().code, "too_many_seats");

        let first = seat(0);
        let mut duplicate_seat = seat(1);
        duplicate_seat.seat_id = first.seat_id;
        let update = UpdateRoomAgentRoster {
            expected_revision: None,
            seats: vec![first.clone(), duplicate_seat],
        };
        assert_eq!(update.validate().unwrap_err().code, "duplicate_seat");

        let mut duplicate_agent = seat(1);
        duplicate_agent.agent_user_id = first.agent_user_id;
        let update = UpdateRoomAgentRoster {
            expected_revision: None,
            seats: vec![first, duplicate_agent],
        };
        assert_eq!(update.validate().unwrap_err().code, "duplicate_agent");
    }

    /// Settings must be bounded objects and positions must be unique and non-negative.
    #[test]
    fn settings_and_positions_are_bounded() {
        let mut oversized = seat(0);
        oversized.settings = json!({"value": "x".repeat(MAX_SETTINGS_BYTES)});
        assert_eq!(oversized.validate().unwrap_err().code, "settings_too_large");

        let mut negative = seat(-1);
        assert_eq!(negative.validate().unwrap_err().code, "invalid_position");
        negative.position = 0;
        negative.settings = json!([]);
        assert_eq!(negative.validate().unwrap_err().code, "invalid_settings");

        let update = UpdateRoomAgentRoster {
            expected_revision: None,
            seats: vec![seat(0), seat(0)],
        };
        assert_eq!(update.validate().unwrap_err().code, "duplicate_position");
    }

    /// Harness and model identifiers reject shell metacharacters and length overflow.
    #[test]
    fn execution_identifiers_are_strict() {
        let mut invalid = seat(0);
        invalid.harness_id = "codex;rm".to_string();
        assert_eq!(invalid.validate().unwrap_err().code, "invalid_identifier");
        invalid.harness_id = "x".repeat(65);
        assert_eq!(invalid.validate().unwrap_err().code, "invalid_identifier");
        invalid.harness_id = "codex".to_string();
        invalid.model_id = "x".repeat(129);
        assert_eq!(invalid.validate().unwrap_err().code, "invalid_identifier");
    }

    /// Typed setting descriptors and submitted values reject ambiguous definitions.
    #[test]
    fn capability_settings_validate_descriptors_and_values() {
        let empty = CapabilitySetting {
            id: "reasoning_effort".to_string(),
            label: "Reasoning effort".to_string(),
            required: true,
            control: CapabilitySettingControl::Select { options: vec![] },
        };
        assert_eq!(
            empty.validate_descriptor().unwrap_err().code,
            "empty_setting_options"
        );

        let invalid_range = CapabilitySetting {
            id: "turns".to_string(),
            label: "Turns".to_string(),
            required: false,
            control: CapabilitySettingControl::Integer {
                minimum: 10,
                maximum: 1,
                step: 0,
            },
        };
        assert_eq!(
            invalid_range.validate_descriptor().unwrap_err().code,
            "invalid_integer_range"
        );

        let select = CapabilitySetting {
            id: "reasoning_effort".to_string(),
            label: "Reasoning effort".to_string(),
            required: true,
            control: CapabilitySettingControl::Select {
                options: vec![
                    CapabilityOption {
                        id: "medium".to_string(),
                        label: "Medium".to_string(),
                    },
                    CapabilityOption {
                        id: "medium".to_string(),
                        label: "Medium duplicate".to_string(),
                    },
                ],
            },
        };
        assert_eq!(
            select.validate_descriptor().unwrap_err().code,
            "duplicate_setting_option"
        );

        let boolean = CapabilitySetting {
            id: "fast".to_string(),
            label: "Fast".to_string(),
            required: false,
            control: CapabilitySettingControl::Boolean,
        };
        assert!(boolean.validate_value(&json!(true)).is_ok());
        assert_eq!(
            boolean.validate_value(&json!("true")).unwrap_err().code,
            "unsupported_setting_value"
        );
    }
}
