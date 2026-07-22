//! Capability resolution for execution mode.
//!
//! Defines the `CapabilityOracle` trust boundary. The static allowlist
//! implementation ships now; a `PistisOracle` implementation can be swapped in
//! without changing the room or supervisor.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use henosis_pistis::authority::{CapabilityCheckRequest, CapabilityRequirement};
use henosis_pistis::model::ActionKind;
use serde::{Deserialize, Serialize};

use crate::error::BridgeError;
use crate::executor::Capability;

/// Outcome of a capability check for a proposed task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityDecision {
    /// Agent holds every required capability; carries the granted set.
    Granted(Vec<Capability>),
    /// Agent is missing one or more capabilities; carries the missing set.
    Denied(Vec<Capability>),
}

/// Resolves whether an agent may perform work requiring a capability set.
#[async_trait]
pub trait CapabilityOracle: Send + Sync {
    /// Check an agent's required capabilities and return a decision.
    async fn check(
        &self,
        agent: &str,
        required: &[Capability],
    ) -> Result<CapabilityDecision, BridgeError>;
}

/// Config-backed oracle: each agent holds a fixed allowlist of capabilities.
pub struct StaticAllowlistOracle {
    /// Agent username -> set of held capability names.
    allowlist: HashMap<String, HashSet<String>>,
}

/// Construction and lookup for the static allowlist oracle.
impl StaticAllowlistOracle {
    /// Build from a config map of agent username -> capability name list.
    pub fn new(allowlist: HashMap<String, Vec<String>>) -> Self {
        let allowlist = allowlist
            .into_iter()
            .map(|(agent, caps)| (agent, caps.into_iter().collect()))
            .collect();
        Self { allowlist }
    }
}

/// Static allowlist capability resolution.
#[async_trait]
impl CapabilityOracle for StaticAllowlistOracle {
    /// Grant only when every required capability is in the agent's allowlist.
    async fn check(
        &self,
        agent: &str,
        required: &[Capability],
    ) -> Result<CapabilityDecision, BridgeError> {
        let held = self.allowlist.get(agent);
        let missing: Vec<Capability> = required
            .iter()
            .filter(|cap| match held {
                Some(set) => !set.contains(cap.as_str()),
                None => true,
            })
            .cloned()
            .collect();

        if missing.is_empty() {
            Ok(CapabilityDecision::Granted(required.to_vec()))
        } else {
            Ok(CapabilityDecision::Denied(missing))
        }
    }
}

/// HTTP-backed capability oracle that delegates allow and deny decisions to Pistis.
pub struct PistisOracle {
    /// Shared HTTP client for Pistis capability-check requests.
    client: reqwest::Client,
    /// Base URL for the Pistis orchestrator.
    orchestrator_url: String,
    /// Bearer token used for orchestrator authorization.
    auth_token: String,
    /// Matrix room identifier forwarded to the Pistis route.
    room: String,
}

/// Wire response body returned by the Pistis capability-check endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct PistisDecisionBody {
    /// Whether every requested capability is allowed.
    allowed: bool,
    /// Requirements denied by Pistis.
    missing: Vec<CapabilityRequirement>,
    /// Trust score included for observability.
    trust_score: f64,
    /// Optional denial reason.
    reason: Option<String>,
}

/// Constructors and helpers for the Pistis-backed oracle.
impl PistisOracle {
    /// Construct a Pistis-backed oracle from its HTTP configuration.
    pub fn new(orchestrator_url: String, auth_token: String, room: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            orchestrator_url,
            auth_token,
            room,
        }
    }

    /// Convert a Rift capability into the Pistis action kind used for authorization.
    fn capability_to_action_kind(capability: &Capability) -> ActionKind {
        match capability.as_str() {
            Capability::FS_WRITE | Capability::BASH => ActionKind::Commit,
            Capability::FS_READ | Capability::NETWORK => ActionKind::Message,
            _ => ActionKind::Message,
        }
    }

    /// Convert one Rift capability into a Pistis capability requirement.
    fn requirement_from_capability(capability: &Capability) -> CapabilityRequirement {
        CapabilityRequirement {
            name: capability.as_str().to_owned(),
            action_kind: Self::capability_to_action_kind(capability),
        }
    }

    /// Convert a Pistis denied requirement back into the bridge capability name.
    fn capability_from_requirement(requirement: &CapabilityRequirement) -> Capability {
        Capability::new(requirement.name.clone())
    }
}

/// Pistis-backed capability resolution.
#[async_trait]
impl CapabilityOracle for PistisOracle {
    /// Forward the capability check to the Pistis orchestrator over HTTP.
    async fn check(
        &self,
        agent: &str,
        required: &[Capability],
    ) -> Result<CapabilityDecision, BridgeError> {
        let request = CapabilityCheckRequest {
            principal: crate::identity::principal_for_agent(agent),
            required: required
                .iter()
                .map(Self::requirement_from_capability)
                .collect(),
        };
        let room = urlencoding::encode(&self.room);
        let url = format!(
            "{}/api/v1/rooms/{room}/capabilities/check",
            self.orchestrator_url.trim_end_matches('/')
        );
        let response = self
            .client
            .post(url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.auth_token),
            )
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(BridgeError::Execution(format!(
                "Pistis check rejected with status {}",
                response.status()
            )));
        }

        let decision: PistisDecisionBody = response.json().await?;
        if decision.allowed {
            Ok(CapabilityDecision::Granted(required.to_vec()))
        } else {
            Ok(CapabilityDecision::Denied(
                decision
                    .missing
                    .iter()
                    .map(Self::capability_from_requirement)
                    .collect(),
            ))
        }
    }
}

#[cfg(test)]
/// Unit tests for the static allowlist oracle behavior.
mod tests {
    use super::{CapabilityDecision, CapabilityOracle, StaticAllowlistOracle};
    use crate::executor::Capability;
    use std::collections::HashMap;

    /// Builds an allowlist oracle with one agent holding fs_read + bash.
    fn oracle() -> StaticAllowlistOracle {
        let mut map = HashMap::new();
        map.insert(
            "architect".to_string(),
            vec!["fs_read".to_string(), "bash".to_string()],
        );
        StaticAllowlistOracle::new(map)
    }

    /// Verifies a fully-covered request is granted with the intersection set.
    #[tokio::test]
    async fn test_granted_when_all_required_present() {
        let decision = oracle()
            .check(
                "architect",
                &[
                    Capability::new(Capability::FS_READ),
                    Capability::new(Capability::BASH),
                ],
            )
            .await
            .unwrap();
        match decision {
            CapabilityDecision::Granted(caps) => assert_eq!(caps.len(), 2),
            CapabilityDecision::Denied(_) => panic!("should be granted"),
        }
    }

    /// Verifies a missing capability produces a denial listing what is missing.
    #[tokio::test]
    async fn test_denied_lists_missing_capabilities() {
        let decision = oracle()
            .check("architect", &[Capability::new(Capability::FS_WRITE)])
            .await
            .unwrap();
        match decision {
            CapabilityDecision::Denied(missing) => {
                assert_eq!(missing, vec![Capability::new(Capability::FS_WRITE)]);
            }
            CapabilityDecision::Granted(_) => panic!("should be denied"),
        }
    }

    /// Verifies an unknown agent is denied everything it asks for.
    #[tokio::test]
    async fn test_unknown_agent_denied() {
        let decision = oracle()
            .check("ghost", &[Capability::new(Capability::FS_READ)])
            .await
            .unwrap();
        assert!(matches!(decision, CapabilityDecision::Denied(_)));
    }
}
