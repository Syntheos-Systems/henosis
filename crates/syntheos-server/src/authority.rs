//! Authenticated public control plane, durable approvals, and guarded audit execution.

use std::{
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use axum::{
    extract::{FromRequestParts, Path, State},
    http::{request::Parts, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use henosis_approval::{
    canonical_request_hash, request_hash_hex, Approval, ApprovalDecision, ApprovalRequest,
    ApprovalStatus, ApprovalStore, RequestHash,
};
use henosis_audit::{
    AuditEventInput, AuditPhase, AuditStore, ExecutionClaim, ExecutionState, WitnessedAudit,
};
use henosis_plutus::{can, OrgStatus, Permission, PolicyBackend, Role};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use syntheos_contracts::{
    AuthorityContext, Gate, GateDecision, GateError, GateRequest, PrincipalId, RequestContext,
    TenantId, ToolInvocation,
};
use syntheos_dispatch::{
    DispatchError, DispatchOutcome, Dispatcher, ExecutionDecision, ExecutionGuard,
    ExecutionOutcome, ExecutorError,
};
use syntheos_identity::{MachineToken, SqliteDirectory};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::operator::auth;

/// Current server-owned approval policy version.
pub const APPROVAL_POLICY_VERSION: &str = "henosis-public-alpha-v1";

/// Default lifetime of a pending high-risk approval.
const APPROVAL_TTL_SECONDS: i64 = 15 * 60;
/// Longest machine-token lifetime accepted by the public control plane.
const MAX_TOKEN_TTL_SECONDS: i64 = 365 * 24 * 60 * 60;
/// Header used to resume an exact approved request.
const APPROVAL_HEADER: &str = "x-henosis-approval-id";
/// Header carrying the caller's retry identity for one public dispatch.
const IDEMPOTENCY_HEADER: &str = "x-henosis-idempotency-key";
/// Maximum byte length accepted for a public dispatch idempotency key.
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

/// Public authority API failure with a stable HTTP status and non-sensitive message.
#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    /// Authentication failed.
    #[error("authentication failed")]
    Authentication,
    /// The authenticated identity lacks the required authority.
    #[error("forbidden")]
    Forbidden,
    /// The submitted public request is invalid.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// A requested tenant-scoped record does not exist.
    #[error("record not found")]
    NotFound,
    /// A state transition conflicts with the current durable record.
    #[error("request conflicts with current state")]
    Conflict,
    /// An authority dependency could not safely complete the request.
    #[error("authority service unavailable")]
    Unavailable,
}

/// Converts authority failures into stable JSON responses without backend details.
impl IntoResponse for AuthorityError {
    /// Render the failure with its stable status and error code.
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::Authentication => (StatusCode::UNAUTHORIZED, "authentication_failed"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            Self::InvalidRequest(_) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::Conflict => (StatusCode::CONFLICT, "state_conflict"),
            Self::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "authority_unavailable"),
        };
        (status, Json(json!({"error": code}))).into_response()
    }
}

/// Audit sink selected explicitly for local or witnessed execution.
#[derive(Clone)]
pub enum AuditBoundary {
    /// Local durable audit for loopback development.
    Local(AuditStore),
    /// Local durable audit plus mandatory off-host witness receipts.
    Witnessed(WitnessedAudit),
}

/// Implements audit appends across the explicitly selected deployment boundary.
impl AuditBoundary {
    /// Append one audit event and complete every required durability step.
    async fn append(&self, input: AuditEventInput) -> Result<(), henosis_audit::AuditError> {
        match self {
            Self::Local(store) => {
                store.append(input)?;
            }
            Self::Witnessed(audit) => {
                audit.append(input).await?;
            }
        }
        Ok(())
    }

    /// Claim one execution only after every configured intent durability boundary succeeds.
    async fn claim_execution(
        &self,
        input: AuditEventInput,
    ) -> Result<ExecutionClaim, henosis_audit::AuditError> {
        match self {
            Self::Local(store) => store.claim_execution(input),
            Self::Witnessed(audit) => audit.claim_execution(input).await,
        }
    }

    /// Make a filtered result replayable only after every configured outcome boundary succeeds.
    async fn complete_execution(
        &self,
        input: AuditEventInput,
        result: Value,
    ) -> Result<(), henosis_audit::AuditError> {
        match self {
            Self::Local(store) => {
                store.complete_execution(input, result)?;
            }
            Self::Witnessed(audit) => {
                let tenant_id = input.tenant_id.clone();
                if let Err(error) = audit.complete_execution(input, result).await {
                    if let Err(block_error) = audit
                        .store()
                        .mark_stream_ambiguous(&tenant_id, "outcome_completion_failed")
                    {
                        tracing::error!(
                            %block_error,
                            "failed to persist the ambiguous witnessed audit stream state"
                        );
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Return the local store underlying either deployment mode.
    pub fn store(&self) -> &AuditStore {
        match self {
            Self::Local(store) => store,
            Self::Witnessed(audit) => audit.store(),
        }
    }

    /// Return whether this boundary requires an independent receipt.
    pub fn is_witnessed(&self) -> bool {
        matches!(self, Self::Witnessed(_))
    }
}

/// Shared state for authenticated authority routes.
#[derive(Clone)]
pub struct AuthorityState {
    /// Unified dispatcher reached only after authentication.
    pub dispatcher: Arc<Dispatcher>,
    /// Persistent identity and machine-token store.
    pub accounts: Arc<SqliteDirectory>,
    /// Current organization membership and RBAC authority.
    pub policy: Arc<dyn PolicyBackend>,
    /// Operator access-token verification key.
    pub jwt_secret: Arc<Vec<u8>>,
    /// Durable tenant-scoped approval store.
    pub approvals: Arc<ApprovalStore>,
    /// Durable audit store used for explicit verification.
    pub audit: AuditBoundary,
}

/// Authenticated identity derived exclusively from a bearer credential.
#[derive(Clone, Debug)]
pub struct AuthenticatedIdentity {
    /// Tenant established by token metadata or current operator membership.
    pub tenant: TenantId,
    /// Principal established by token metadata or verified access claims.
    pub principal: PrincipalId,
    /// Opaque token or session record identifier, never the credential itself.
    pub token_identity: String,
    /// Current operator role, absent for machine credentials.
    pub role: Option<Role>,
    /// Machine-token scopes, empty for operators.
    pub scopes: Vec<String>,
}

/// Permission checks for authenticated operator and machine identities.
impl AuthenticatedIdentity {
    /// Require tool-dispatch authority.
    fn require_dispatch(&self) -> Result<(), AuthorityError> {
        match self.role {
            Some(role) if can(role, Permission::ToolInvoke) => Ok(()),
            Some(_) => Err(AuthorityError::Forbidden),
            None if self.scopes.iter().any(|scope| scope == "dispatch") => Ok(()),
            None => Err(AuthorityError::Forbidden),
        }
    }

    /// Require an owner, administrator, or explicitly scoped machine identity.
    fn require_administrator(&self) -> Result<(), AuthorityError> {
        match self.role {
            Some(Role::Owner | Role::Admin) => Ok(()),
            Some(_) => Err(AuthorityError::Forbidden),
            None if self.scopes.iter().any(|scope| scope == "admin") => Ok(()),
            None => Err(AuthorityError::Forbidden),
        }
    }

    /// Require tenant audit-read authority.
    fn require_audit_read(&self) -> Result<(), AuthorityError> {
        match self.role {
            Some(role) if can(role, Permission::OrgRead) => Ok(()),
            Some(_) => Err(AuthorityError::Forbidden),
            None if self.scopes.iter().any(|scope| scope == "audit:read") => Ok(()),
            None => Err(AuthorityError::Forbidden),
        }
    }
}

/// Extracts machine or operator authority from the bearer credential and live policy state.
impl FromRequestParts<AuthorityState> for AuthenticatedIdentity {
    /// Stable authority rejection returned by the public API.
    type Rejection = AuthorityError;

    /// Authenticate the bearer credential without accepting tenant or principal fields.
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AuthorityState,
    ) -> Result<Self, Self::Rejection> {
        let credential = bearer_credential(&parts.headers)?;
        let now = unix_seconds();
        if credential.starts_with("hen_v1_") {
            let token = state
                .accounts
                .authenticate_machine_token(credential, now)
                .map_err(|error| {
                    tracing::error!(%error, "machine-token authentication backend failed");
                    AuthorityError::Unavailable
                })?
                .ok_or(AuthorityError::Authentication)?;
            current_member_role(state, token.tenant, token.principal).await?;
            return Ok(Self {
                tenant: token.tenant,
                principal: token.principal,
                token_identity: token.id.to_string(),
                role: None,
                scopes: token.scopes,
            });
        }

        let claims = auth::decode(credential, &state.jwt_secret)
            .map_err(|_| AuthorityError::Authentication)?;
        let principal =
            PrincipalId::from_str(&claims.sub).map_err(|_| AuthorityError::Authentication)?;
        let tenant = TenantId::from_str(&claims.org).map_err(|_| AuthorityError::Authentication)?;
        let role = current_member_role(state, tenant, principal).await?;
        Ok(Self {
            tenant,
            principal,
            token_identity: claims.sid,
            role: Some(role),
            scopes: Vec::new(),
        })
    }
}

/// Resolve one active tenant membership or reject the authenticated credential.
async fn current_member_role(
    state: &AuthorityState,
    tenant: TenantId,
    principal: PrincipalId,
) -> Result<Role, AuthorityError> {
    let status = state
        .policy
        .org_status(tenant)
        .await
        .map_err(|error| {
            tracing::error!(%error, "authenticated organization lookup failed");
            AuthorityError::Unavailable
        })?
        .ok_or(AuthorityError::Authentication)?;
    if status != OrgStatus::Active {
        return Err(AuthorityError::Authentication);
    }
    state
        .policy
        .member_role(tenant, principal)
        .await
        .map_err(|error| {
            tracing::error!(%error, "authenticated membership lookup failed");
            AuthorityError::Unavailable
        })?
        .ok_or(AuthorityError::Authentication)
}

/// Public dispatch body without caller-controlled authority fields.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchBody {
    /// Server-recognized tool name.
    pub tool: String,
    /// Server-recognized action name.
    pub action: String,
    /// Tool arguments governed by the authority chain.
    #[serde(default)]
    pub args: Value,
    /// Non-authoritative correlation context.
    #[serde(default)]
    pub context: ClientContext,
}

/// Caller-supplied correlation fields that carry no identity authority.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientContext {
    /// Active persona label, when applicable.
    pub persona: Option<String>,
    /// Client session correlation identifier.
    pub session: Option<String>,
    /// Room correlation identifier.
    pub room: Option<String>,
    /// Workflow correlation identifier.
    pub workflow: Option<String>,
}

/// Request for one server-issued machine credential.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenCreateBody {
    /// Human-readable management label.
    pub label: String,
    /// Explicit least-privilege scopes.
    pub scopes: Vec<String>,
    /// Optional lifetime from issuance in seconds.
    pub expires_in_seconds: Option<i64>,
}

/// One-time machine-token issuance response.
#[derive(Serialize, Zeroize, ZeroizeOnDrop)]
pub struct TokenIssuedResponse {
    /// Cleartext credential returned only once.
    pub token: String,
    /// Safely listable token metadata.
    #[zeroize(skip)]
    pub metadata: TokenMetadataResponse,
}

/// Safely listable machine-token metadata.
#[derive(Debug, Serialize)]
pub struct TokenMetadataResponse {
    /// Stable token record identifier.
    pub id: String,
    /// Human-readable management label.
    pub label: String,
    /// Granted least-privilege scopes.
    pub scopes: Vec<String>,
    /// Issuance timestamp.
    pub created_at: i64,
    /// Optional expiry timestamp.
    pub expires_at: Option<i64>,
    /// Optional revocation timestamp.
    pub revoked_at: Option<i64>,
    /// Optional most recent use timestamp.
    pub last_used_at: Option<i64>,
}

/// Converts internal machine-token metadata to its safe public representation.
impl From<MachineToken> for TokenMetadataResponse {
    /// Remove tenant and principal repetition while preserving management metadata.
    fn from(token: MachineToken) -> Self {
        Self {
            id: token.id.to_string(),
            label: token.label,
            scopes: token.scopes,
            created_at: token.created_at,
            expires_at: token.expires_at,
            revoked_at: token.revoked_at,
            last_used_at: token.last_used_at,
        }
    }
}

/// Human approval decision body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDecisionBody {
    /// Optional bounded human explanation.
    pub reason: Option<String>,
}

/// Safely visible approval representation.
#[derive(Debug, Serialize)]
pub struct ApprovalResponse {
    /// Stable approval identifier.
    pub id: String,
    /// Tool bound to the request.
    pub tool: String,
    /// Action bound to the request.
    pub action: String,
    /// Human-facing server-generated prompt.
    pub prompt: String,
    /// Current lifecycle state.
    pub status: &'static str,
    /// Creation timestamp.
    pub created_at: i64,
    /// Expiry timestamp.
    pub expires_at: i64,
    /// Decision timestamp when present.
    pub decided_at: Option<i64>,
    /// Stable request fingerprint for operator comparison.
    pub request_hash: String,
}

/// Converts an approval record to its metadata-only public representation.
impl From<Approval> for ApprovalResponse {
    /// Exclude authority internals and human reason text from list responses.
    fn from(approval: Approval) -> Self {
        Self {
            id: approval.id.to_string(),
            tool: approval.tool,
            action: approval.action,
            prompt: approval.prompt,
            status: approval_status_name(approval.status),
            created_at: approval.created_at,
            expires_at: approval.expires_at,
            decided_at: approval.decided_at,
            request_hash: request_hash_hex(&approval.request_hash),
        }
    }
}

/// Returns the stable public name for an approval lifecycle state.
fn approval_status_name(status: ApprovalStatus) -> &'static str {
    match status {
        ApprovalStatus::Pending => "pending",
        ApprovalStatus::Approved => "approved",
        ApprovalStatus::Denied => "denied",
        ApprovalStatus::Expired => "expired",
        ApprovalStatus::Consumed => "consumed",
    }
}

/// Durable human authority that derives escalation from server policy.
pub struct DurableHumanGate {
    approvals: Arc<ApprovalStore>,
}

/// Constructs and evaluates request-bound durable approvals.
impl DurableHumanGate {
    /// Construct the human authority over its isolated approval store.
    pub fn new(approvals: Arc<ApprovalStore>) -> Self {
        Self { approvals }
    }

    /// Resolve a supplied approval or create the single durable record for this request.
    fn approval_for_request(
        &self,
        request: &GateRequest,
        authority: &AuthorityContext,
        request_hash: RequestHash,
        now: i64,
    ) -> Result<Approval, GateError> {
        if let Some(id) = authority.approval_id.as_deref() {
            let id = Uuid::parse_str(id).map_err(|_| GateError::new("invalid approval id"))?;
            let mut approval = self
                .approvals
                .get(request.context.tenant, id)
                .map_err(|error| GateError::new(error.to_string()))?
                .ok_or_else(|| GateError::new("approval not found"))?;
            if !approval_matches(&approval, request, authority, &request_hash) {
                return Err(GateError::new("approval binding mismatch"));
            }
            if matches!(
                approval.status,
                ApprovalStatus::Pending | ApprovalStatus::Approved
            ) && approval.expires_at <= now
            {
                self.approvals
                    .expire(request.context.tenant, now)
                    .map_err(|error| GateError::new(error.to_string()))?;
                approval.status = ApprovalStatus::Expired;
            }
            return Ok(approval);
        }

        self.approvals
            .create_or_get_pending(
                ApprovalRequest {
                    tenant: request.context.tenant,
                    principal: request.context.principal,
                    token_identity: authority.token_identity.clone(),
                    tool: request.invocation.tool.clone(),
                    action: request.invocation.action.clone(),
                    request_hash,
                    prompt: approval_prompt(&request.invocation),
                    policy_version: APPROVAL_POLICY_VERSION.to_string(),
                    created_at: now,
                    expires_at: now + APPROVAL_TTL_SECONDS,
                },
                now,
            )
            .map_err(|error| GateError::new(error.to_string()))
    }
}

/// Implements the canonical human gate without trusting invocation arguments.
#[async_trait]
impl Gate for DurableHumanGate {
    /// Return the canonical human authority name.
    fn name(&self) -> &str {
        "human"
    }

    /// Require a durable human decision for every non-read-only action.
    async fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if !requires_human_approval(&request.invocation) {
            return Ok(GateDecision::Allow);
        }
        let authority = request
            .context
            .authority
            .as_ref()
            .ok_or_else(|| GateError::new("authenticated authority context is required"))?;
        let envelope = canonical_request_envelope(request);
        let request_hash =
            canonical_request_hash(&envelope).map_err(|error| GateError::new(error.to_string()))?;
        let now = unix_seconds();
        let approval = self.approval_for_request(request, authority, request_hash, now)?;
        let supplied_approval = authority.approval_id.as_deref();
        let stored_approval = approval.id.to_string();
        match approval.status {
            ApprovalStatus::Approved if supplied_approval == Some(stored_approval.as_str()) => {
                Ok(GateDecision::Allow)
            }
            ApprovalStatus::Pending | ApprovalStatus::Approved => {
                Ok(GateDecision::RequireApproval {
                    prompt: approval.prompt,
                    approval_id: Some(approval.id.to_string()),
                })
            }
            ApprovalStatus::Denied => Ok(GateDecision::Deny {
                reason: "approval was denied".to_string(),
            }),
            ApprovalStatus::Expired => Ok(GateDecision::Deny {
                reason: "approval expired".to_string(),
            }),
            ApprovalStatus::Consumed if supplied_approval == Some(stored_approval.as_str()) => {
                Ok(GateDecision::Allow)
            }
            ApprovalStatus::Consumed => Ok(GateDecision::Deny {
                reason: "approval was already consumed".to_string(),
            }),
        }
    }
}

/// Structural credential gate for the external phylaxd broker boundary.
pub struct PhylaxdGate;

/// Constructs the credential broker authority.
impl PhylaxdGate {
    /// Construct the stateless structural authority.
    pub fn new() -> Self {
        Self
    }
}

/// Supplies the default structural credential authority.
impl Default for PhylaxdGate {
    /// Construct the default stateless gate.
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
/// Validates broker operations before the executor can contact phylaxd.
impl Gate for PhylaxdGate {
    /// Return the canonical credential authority name.
    fn name(&self) -> &str {
        "phylaxd"
    }

    /// Allow non-credential tools and fail closed on malformed broker operations.
    async fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.invocation.tool != "phylaxd" {
            return Ok(GateDecision::Allow);
        }
        if !matches!(
            request.invocation.action.as_str(),
            "sign" | "verify" | "derive" | "exec"
        ) {
            return Ok(GateDecision::Deny {
                reason: "unsupported credential broker operation".to_string(),
            });
        }
        let category = request
            .invocation
            .args
            .get("category")
            .and_then(Value::as_str);
        let name = request.invocation.args.get("name").and_then(Value::as_str);
        if !category.is_some_and(valid_name) || !name.is_some_and(valid_name) {
            return Ok(GateDecision::Deny {
                reason: "credential slot identifiers are invalid".to_string(),
            });
        }
        let tenant_slot = request.context.tenant.to_string();
        if name != Some(tenant_slot.as_str()) {
            return Ok(GateDecision::Deny {
                reason: "credential slot does not belong to the authenticated tenant".to_string(),
            });
        }
        Ok(GateDecision::Allow)
    }
}

/// Final execution guard that binds approvals to witnessed audit intent and outcome records.
pub struct AuditExecutionGuard {
    approvals: Arc<ApprovalStore>,
    audit: AuditBoundary,
}

/// Constructs the guarded execution boundary.
impl AuditExecutionGuard {
    /// Construct the guard over the same stores exposed by the authority API.
    pub fn new(approvals: Arc<ApprovalStore>, audit: AuditBoundary) -> Self {
        Self { approvals, audit }
    }

    /// Build one metadata-only audit event bound to the caller's tenant-scoped retry key.
    fn event_input(
        &self,
        request: &GateRequest,
        phase: AuditPhase,
        payload: Value,
    ) -> Result<AuditEventInput, henosis_audit::AuditError> {
        let envelope = canonical_request_envelope(request);
        let authority = request.context.authority.as_ref().ok_or_else(|| {
            henosis_audit::AuditError::InvalidInput(
                "authenticated authority context is required".to_string(),
            )
        })?;
        Ok(AuditEventInput {
            tenant_id: request.context.tenant.to_string(),
            principal_id: request.context.principal.to_string(),
            action: format!("{}.{}", request.invocation.tool, request.invocation.action),
            phase,
            request: envelope,
            payload,
            idempotency_key: Some(authority.idempotency_key.clone()),
        })
    }

    /// Append and, when configured, witness one audit event before returning.
    async fn append_event(
        &self,
        request: &GateRequest,
        phase: AuditPhase,
        payload: Value,
    ) -> Result<(), henosis_audit::AuditError> {
        let input = self.event_input(request, phase, payload)?;
        self.audit.append(input).await
    }
}

/// Enforces intent durability and at-most-once approval consumption around the executor.
#[async_trait]
impl ExecutionGuard for AuditExecutionGuard {
    /// Persist and witness intent, then atomically consume any exact approval.
    async fn before_execute(
        &self,
        request: &GateRequest,
        allowed_gates: &[String],
    ) -> Result<ExecutionDecision, ExecutorError> {
        let authority = request
            .context
            .authority
            .as_ref()
            .ok_or_else(|| ExecutorError::new("authenticated authority context is required"))?;
        let intent = self
            .event_input(
                request,
                AuditPhase::Intent,
                json!({
                    "tool": request.invocation.tool,
                    "operation": request.invocation.action,
                    "gates": allowed_gates,
                    "approval_id": authority.approval_id,
                    "token_identity": authority.token_identity,
                    "witness_mode": if self.audit.is_witnessed() { "required" } else { "local" },
                }),
            )
            .map_err(audit_executor_error)?;
        let claim = self
            .audit
            .claim_execution(intent)
            .await
            .map_err(audit_executor_error)?;
        match claim {
            ExecutionClaim::Existing(record) => match record.state {
                ExecutionState::Completed => {
                    let result = record.sanitized_result.ok_or_else(|| {
                        ExecutorError::new("completed execution has no replayable result")
                    })?;
                    return Ok(ExecutionDecision::Cached(result));
                }
                ExecutionState::Claimed | ExecutionState::Indeterminate => {
                    return Err(ExecutorError::new(
                        "idempotent execution cannot be replayed safely",
                    ));
                }
            },
            ExecutionClaim::Acquired(_) => {}
        }

        if let Some(id) = authority.approval_id.as_deref() {
            let id = Uuid::parse_str(id).map_err(|_| ExecutorError::new("invalid approval id"))?;
            let hash = canonical_request_hash(&canonical_request_envelope(request))
                .map_err(|_| ExecutorError::new("request hash failed"))?;
            let consumed = self
                .approvals
                .consume_approved(
                    request.context.tenant,
                    id,
                    request.context.principal,
                    &authority.token_identity,
                    &hash,
                    APPROVAL_POLICY_VERSION,
                    unix_seconds(),
                )
                .map_err(|error| {
                    tracing::error!(%error, "approval consumption failed");
                    ExecutorError::new("approval authority unavailable")
                })?;
            if consumed.is_none() {
                if let Err(error) = self.audit.store().mark_execution_indeterminate(
                    &request.context.tenant.to_string(),
                    &request.context.principal.to_string(),
                    &authority.idempotency_key,
                    &canonical_request_envelope(request),
                ) {
                    tracing::error!(%error, "failed to close an unconsumed execution claim");
                }
                return Err(ExecutorError::new(
                    "approval is not active or does not match this request",
                ));
            }
        }
        Ok(ExecutionDecision::Execute)
    }

    /// Persist and witness terminal outcome before the dispatcher reports it.
    async fn after_execute(
        &self,
        request: &GateRequest,
        outcome: ExecutionOutcome,
    ) -> Result<(), ExecutorError> {
        match outcome {
            ExecutionOutcome::Succeeded { result } => {
                let event = self
                    .event_input(
                        request,
                        AuditPhase::Outcome,
                        execution_outcome_payload(request, "succeeded"),
                    )
                    .map_err(audit_executor_error)?;
                self.audit
                    .complete_execution(event, result)
                    .await
                    .map_err(audit_executor_error)?;
            }
            ExecutionOutcome::Failed => {
                self.append_event(
                    request,
                    AuditPhase::Outcome,
                    execution_outcome_payload(request, "failed"),
                )
                .await
                .map_err(audit_executor_error)?;
                self.mark_indeterminate(request).await?;
            }
        }
        Ok(())
    }

    /// Persist a non-replayable marker whenever completion cannot be established.
    async fn mark_indeterminate(&self, request: &GateRequest) -> Result<(), ExecutorError> {
        let authority = request
            .context
            .authority
            .as_ref()
            .ok_or_else(|| ExecutorError::new("authenticated authority context is required"))?;
        self.audit
            .store()
            .mark_execution_indeterminate(
                &request.context.tenant.to_string(),
                &request.context.principal.to_string(),
                &authority.idempotency_key,
                &canonical_request_envelope(request),
            )
            .map_err(audit_executor_error)?;
        Ok(())
    }
}

/// Construct the authenticated public authority router.
pub fn authority_router(state: AuthorityState) -> Router {
    Router::new()
        .route("/api/v1/dispatch", post(dispatch))
        .route("/api/v1/tokens", post(create_token).get(list_tokens))
        .route("/api/v1/tokens/{id}/revoke", post(revoke_token))
        .route("/api/v1/approvals", get(list_approvals))
        .route("/api/v1/approvals/{id}", get(get_approval))
        .route("/api/v1/approvals/{id}/approve", post(approve))
        .route("/api/v1/approvals/{id}/deny", post(deny))
        .route("/api/v1/audit/verify", get(verify_audit))
        .with_state(state)
}

/// Dispatch one server-authorized request and return approval escalation as HTTP 202.
async fn dispatch(
    State(state): State<AuthorityState>,
    identity: AuthenticatedIdentity,
    headers: HeaderMap,
    Json(body): Json<DispatchBody>,
) -> Result<Response, AuthorityError> {
    identity.require_dispatch()?;
    validate_dispatch_body(&body)?;
    let idempotency_key = dispatch_idempotency_key(&headers)?;
    let approval_id = dispatch_approval_id(&headers)?;
    let request = GateRequest {
        context: RequestContext {
            tenant: identity.tenant,
            principal: identity.principal,
            persona: body.context.persona,
            session: body.context.session,
            room: body.context.room,
            task: None,
            workflow: body.context.workflow,
            authority: Some(AuthorityContext {
                token_identity: identity.token_identity,
                idempotency_key,
                approval_id,
            }),
        },
        invocation: ToolInvocation {
            tool: body.tool,
            action: body.action,
            args: body.args,
        },
    };
    let outcome = state.dispatcher.dispatch(request).await.map_err(|error| {
        let conflict = matches!(
            &error,
            DispatchError::Execution(executor_error) if executor_error.is_conflict()
        );
        tracing::error!(%error, "guarded dispatch failed");
        if conflict {
            AuthorityError::Conflict
        } else {
            AuthorityError::Unavailable
        }
    })?;
    match outcome {
        DispatchOutcome::Executed { result } => Ok(Json(json!({"result": result})).into_response()),
        DispatchOutcome::Denied { gate, reason } => Ok((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "denied", "gate": gate, "reason": reason})),
        )
            .into_response()),
        DispatchOutcome::RequiresApproval {
            gate,
            prompt,
            approval_id,
        } => {
            let approval_id = approval_id.ok_or(AuthorityError::Unavailable)?;
            let mut response = (
                StatusCode::ACCEPTED,
                Json(json!({
                    "status": "approval_required",
                    "gate": gate,
                    "prompt": prompt,
                    "approval_id": approval_id.clone(),
                })),
            )
                .into_response();
            response.headers_mut().insert(
                APPROVAL_HEADER,
                approval_id
                    .parse()
                    .map_err(|_| AuthorityError::Unavailable)?,
            );
            Ok(response)
        }
        _ => Err(AuthorityError::Unavailable),
    }
}

/// Issue one least-privilege machine token for the authenticated tenant.
async fn create_token(
    State(state): State<AuthorityState>,
    identity: AuthenticatedIdentity,
    Json(body): Json<TokenCreateBody>,
) -> Result<Json<TokenIssuedResponse>, AuthorityError> {
    identity.require_administrator()?;
    validate_scopes(&body.scopes)?;
    let now = unix_seconds();
    let expires_at = body
        .expires_in_seconds
        .map(|ttl| {
            if !(60..=MAX_TOKEN_TTL_SECONDS).contains(&ttl) {
                return Err(AuthorityError::InvalidRequest(
                    "token lifetime is outside the supported range".to_string(),
                ));
            }
            now.checked_add(ttl).ok_or_else(|| {
                AuthorityError::InvalidRequest("token lifetime overflow".to_string())
            })
        })
        .transpose()?;
    let mut issued = state
        .accounts
        .create_machine_token(
            identity.tenant,
            identity.principal,
            &body.label,
            body.scopes,
            expires_at,
            now,
        )
        .map_err(|error| {
            tracing::error!(%error, "machine-token issuance failed");
            AuthorityError::Unavailable
        })?;
    let metadata = issued.metadata.clone().into();
    Ok(Json(TokenIssuedResponse {
        token: std::mem::take(&mut issued.token),
        metadata,
    }))
}

/// List safely visible machine-token metadata for the authenticated tenant.
async fn list_tokens(
    State(state): State<AuthorityState>,
    identity: AuthenticatedIdentity,
) -> Result<Json<Vec<TokenMetadataResponse>>, AuthorityError> {
    identity.require_administrator()?;
    let tokens = state
        .accounts
        .list_machine_tokens(identity.tenant)
        .map_err(|error| {
            tracing::error!(%error, "machine-token listing failed");
            AuthorityError::Unavailable
        })?;
    Ok(Json(tokens.into_iter().map(Into::into).collect()))
}

/// Revoke one tenant-scoped machine token.
async fn revoke_token(
    State(state): State<AuthorityState>,
    identity: AuthenticatedIdentity,
    Path(id): Path<String>,
) -> Result<StatusCode, AuthorityError> {
    identity.require_administrator()?;
    let id = Uuid::parse_str(&id)
        .map_err(|_| AuthorityError::InvalidRequest("invalid token id".to_string()))?;
    let revoked = state
        .accounts
        .revoke_machine_token(identity.tenant, id, unix_seconds())
        .map_err(|error| {
            tracing::error!(%error, "machine-token revocation failed");
            AuthorityError::Unavailable
        })?;
    if revoked {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AuthorityError::NotFound)
    }
}

/// List durable approvals for the authenticated tenant.
async fn list_approvals(
    State(state): State<AuthorityState>,
    identity: AuthenticatedIdentity,
) -> Result<Json<Vec<ApprovalResponse>>, AuthorityError> {
    identity.require_administrator()?;
    state
        .approvals
        .expire(identity.tenant, unix_seconds())
        .map_err(|_| AuthorityError::Unavailable)?;
    let approvals = state
        .approvals
        .list(identity.tenant)
        .map_err(|_| AuthorityError::Unavailable)?;
    Ok(Json(approvals.into_iter().map(Into::into).collect()))
}

/// Get one durable tenant-scoped approval.
async fn get_approval(
    State(state): State<AuthorityState>,
    identity: AuthenticatedIdentity,
    Path(id): Path<String>,
) -> Result<Json<ApprovalResponse>, AuthorityError> {
    identity.require_administrator()?;
    let id = parse_public_id(&id)?;
    let approval = state
        .approvals
        .get(identity.tenant, id)
        .map_err(|_| AuthorityError::Unavailable)?
        .ok_or(AuthorityError::NotFound)?;
    Ok(Json(approval.into()))
}

/// Approve one pending request for the authenticated tenant.
async fn approve(
    State(state): State<AuthorityState>,
    identity: AuthenticatedIdentity,
    Path(id): Path<String>,
    Json(body): Json<ApprovalDecisionBody>,
) -> Result<Json<ApprovalResponse>, AuthorityError> {
    decide_approval(state, identity, id, body, ApprovalDecision::Approve).await
}

/// Deny one pending request for the authenticated tenant.
async fn deny(
    State(state): State<AuthorityState>,
    identity: AuthenticatedIdentity,
    Path(id): Path<String>,
    Json(body): Json<ApprovalDecisionBody>,
) -> Result<Json<ApprovalResponse>, AuthorityError> {
    decide_approval(state, identity, id, body, ApprovalDecision::Deny).await
}

/// Apply one human decision through the tenant-scoped approval store.
async fn decide_approval(
    state: AuthorityState,
    identity: AuthenticatedIdentity,
    id: String,
    body: ApprovalDecisionBody,
    decision: ApprovalDecision,
) -> Result<Json<ApprovalResponse>, AuthorityError> {
    identity.require_administrator()?;
    let id = parse_public_id(&id)?;
    if decision == ApprovalDecision::Approve && state.audit.is_witnessed() {
        let approval = state
            .approvals
            .get(identity.tenant, id)
            .map_err(|_| AuthorityError::Unavailable)?
            .ok_or(AuthorityError::NotFound)?;
        if approval.principal == identity.principal {
            return Err(AuthorityError::Forbidden);
        }
    }
    let approval = state
        .approvals
        .decide(
            identity.tenant,
            id,
            decision,
            identity.principal.to_string(),
            body.reason,
            unix_seconds(),
        )
        .map_err(|_| AuthorityError::Unavailable)?
        .ok_or(AuthorityError::Conflict)?;
    Ok(Json(approval.into()))
}

/// Verify the authenticated tenant's complete local audit hash chain.
async fn verify_audit(
    State(state): State<AuthorityState>,
    identity: AuthenticatedIdentity,
) -> Result<Json<Value>, AuthorityError> {
    identity.require_audit_read()?;
    let count = state
        .audit
        .store()
        .verify_tenant(&identity.tenant.to_string())
        .map_err(|error| {
            tracing::error!(%error, "audit verification failed");
            AuthorityError::Conflict
        })?;
    let stream = state
        .audit
        .store()
        .stream_state(&identity.tenant.to_string())
        .map_err(|_| AuthorityError::Unavailable)?;
    Ok(Json(json!({
        "verified_records": count,
        "blocked": stream.as_ref().is_some_and(|state| state.blocked),
        "witnessed": state.audit.is_witnessed(),
    })))
}

/// Extract a case-sensitive bearer credential from an HTTP header map.
fn bearer_credential(headers: &HeaderMap) -> Result<&str, AuthorityError> {
    let mut values = headers.get_all(axum::http::header::AUTHORIZATION).iter();
    let value = values.next().ok_or(AuthorityError::Authentication)?;
    if values.next().is_some() {
        return Err(AuthorityError::Authentication);
    }
    value
        .to_str()
        .ok()
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or(AuthorityError::Authentication)
}

/// Validate bounded public dispatch identifiers and correlation values.
fn validate_dispatch_body(body: &DispatchBody) -> Result<(), AuthorityError> {
    if !valid_name(&body.tool) || !valid_name(&body.action) {
        return Err(AuthorityError::InvalidRequest(
            "tool and action must be bounded identifiers".to_string(),
        ));
    }
    if body.tool == "phylaxd" {
        return Err(AuthorityError::Forbidden);
    }
    for value in [
        body.context.persona.as_deref(),
        body.context.session.as_deref(),
        body.context.room.as_deref(),
        body.context.workflow.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if value.len() > 256 || value.chars().any(char::is_control) {
            return Err(AuthorityError::InvalidRequest(
                "correlation context is invalid".to_string(),
            ));
        }
    }
    Ok(())
}

/// Extract one unambiguous bounded idempotency key from public dispatch headers.
fn dispatch_idempotency_key(headers: &HeaderMap) -> Result<String, AuthorityError> {
    let all_values = headers.get_all(IDEMPOTENCY_HEADER);
    let mut values = all_values.iter();
    let value = values.next().ok_or_else(|| {
        AuthorityError::InvalidRequest("idempotency key header is required".to_string())
    })?;
    if values.next().is_some() {
        return Err(AuthorityError::InvalidRequest(
            "idempotency key header must appear exactly once".to_string(),
        ));
    }
    let value = value.to_str().map_err(|_| {
        AuthorityError::InvalidRequest("idempotency key header is invalid".to_string())
    })?;
    if !valid_idempotency_key(value) {
        return Err(AuthorityError::InvalidRequest(
            "idempotency key must be a bounded opaque identifier".to_string(),
        ));
    }
    Ok(value.to_string())
}

/// Extract zero or one canonical approval identifier from public dispatch headers.
fn dispatch_approval_id(headers: &HeaderMap) -> Result<Option<String>, AuthorityError> {
    let all_values = headers.get_all(APPROVAL_HEADER);
    let mut values = all_values.iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(AuthorityError::InvalidRequest(
            "approval header must appear at most once".to_string(),
        ));
    }
    let value = value
        .to_str()
        .map_err(|_| AuthorityError::InvalidRequest("invalid approval header".to_string()))?;
    let id = Uuid::parse_str(value)
        .map_err(|_| AuthorityError::InvalidRequest("invalid approval header".to_string()))?;
    if id.to_string() != value {
        return Err(AuthorityError::InvalidRequest(
            "invalid approval header".to_string(),
        ));
    }
    Ok(Some(value.to_string()))
}

/// Return whether a public dispatch idempotency key is safe to bind and persist.
fn valid_idempotency_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDEMPOTENCY_KEY_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

/// Return whether a tool or action is a safe bounded identifier.
fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

/// Validate the fixed machine-token scope catalog.
fn validate_scopes(scopes: &[String]) -> Result<(), AuthorityError> {
    if scopes.is_empty()
        || scopes.len() > 16
        || scopes
            .iter()
            .any(|scope| !matches!(scope.as_str(), "admin" | "dispatch" | "audit:read"))
    {
        return Err(AuthorityError::InvalidRequest(
            "machine-token scopes must use the supported least-privilege catalog".to_string(),
        ));
    }
    Ok(())
}

/// Parse one public UUID record identifier.
fn parse_public_id(value: &str) -> Result<Uuid, AuthorityError> {
    Uuid::parse_str(value)
        .map_err(|_| AuthorityError::InvalidRequest("invalid record id".to_string()))
}

/// Require approval unless the exact registered adapter operation is reviewed as side-effect-free.
fn requires_human_approval(invocation: &ToolInvocation) -> bool {
    !matches!(
        (invocation.tool.as_str(), invocation.action.as_str()),
        ("henosis", "probe")
            | ("gcal", "list_events")
            | ("gdrive", "list" | "download" | "get_metadata")
            | (
                "github",
                "get_issue" | "list_issues" | "list_prs" | "search_code" | "list_repos"
            )
            | ("gmail", "read" | "search" | "list_labels")
            | ("linear", "list_issues" | "search")
            | ("notion", "get_page" | "search")
    )
}

/// Build a human-readable prompt only from bounded server-recognized identifiers.
fn approval_prompt(invocation: &ToolInvocation) -> String {
    format!(
        "Approve {}.{} for this authenticated principal?",
        invocation.tool, invocation.action
    )
}

/// Build the canonical request envelope shared by approval and audit authorities.
fn canonical_request_envelope(request: &GateRequest) -> Value {
    let authority = request.context.authority.as_ref();
    json!({
        "tenant": request.context.tenant.to_string(),
        "principal": request.context.principal.to_string(),
        "token_identity": authority.map(|value| value.token_identity.as_str()),
        "idempotency_key": authority.map(|value| value.idempotency_key.as_str()),
        "tool": request.invocation.tool,
        "action": request.invocation.action,
        "args": request.invocation.args,
        "persona": request.context.persona,
        "session": request.context.session,
        "room": request.context.room,
        "workflow": request.context.workflow,
    })
}

/// Verify that a durable approval matches every authoritative request binding.
fn approval_matches(
    approval: &Approval,
    request: &GateRequest,
    authority: &AuthorityContext,
    request_hash: &RequestHash,
) -> bool {
    approval.tenant == request.context.tenant
        && approval.principal == request.context.principal
        && approval.token_identity == authority.token_identity
        && approval.tool == request.invocation.tool
        && approval.action == request.invocation.action
        && approval.request_hash == *request_hash
        && approval.policy_version == APPROVAL_POLICY_VERSION
}

/// Build the stable metadata-only terminal payload used for persistence and replay witnessing.
fn execution_outcome_payload(request: &GateRequest, outcome: &'static str) -> Value {
    json!({
        "tool": request.invocation.tool,
        "operation": request.invocation.action,
        "outcome": outcome,
    })
}

/// Convert a detailed audit failure to a sanitized executor failure.
fn audit_executor_error(error: henosis_audit::AuditError) -> ExecutorError {
    if matches!(&error, henosis_audit::AuditError::IdempotencyConflict) {
        return ExecutorError::conflict("idempotency key conflicts with an earlier request");
    }
    tracing::error!(%error, "audit execution boundary failed");
    ExecutorError::new("audit execution boundary unavailable")
}

/// Return the current Unix timestamp in whole seconds.
fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64
}

#[cfg(test)]
/// Exercises authority validation and approval escalation policy.
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use henosis_audit::{OriginSigner, WitnessClient};
    use henosis_plutus::MockPolicyBackend;
    use syntheos_axon::AxonBus;
    use syntheos_dispatch::{deny_gate_chain, DenyExecutor};

    /// Construct a witnessed authority state without contacting the inert witness endpoint.
    fn witnessed_state(approvals: Arc<ApprovalStore>) -> AuthorityState {
        let origin_signer = OriginSigner::new("origin-test", SigningKey::from_bytes(&[7_u8; 32]))
            .expect("origin signer");
        let witness_key = SigningKey::from_bytes(&[8_u8; 32]).verifying_key();
        let witness_client = WitnessClient::new(
            "https://127.0.0.1:1",
            "witness-test",
            witness_key,
            Duration::from_secs(1),
        )
        .expect("witness client");
        let audit = AuditBoundary::Witnessed(WitnessedAudit::new(
            AuditStore::open_in_memory().expect("audit store"),
            origin_signer,
            witness_client,
        ));
        let dispatcher = Dispatcher::new(
            deny_gate_chain(),
            Box::new(DenyExecutor),
            Arc::new(AxonBus::new()),
        )
        .expect("dispatcher");
        AuthorityState {
            dispatcher: Arc::new(dispatcher),
            accounts: Arc::new(SqliteDirectory::open_in_memory().expect("accounts")),
            policy: Arc::new(MockPolicyBackend::with_role(Role::Admin)),
            jwt_secret: Arc::new(vec![9_u8; 32]),
            approvals,
            audit,
        }
    }

    /// Construct an authenticated administrator identity for one tenant and principal.
    fn administrator_identity(tenant: TenantId, principal: PrincipalId) -> AuthenticatedIdentity {
        AuthenticatedIdentity {
            tenant,
            principal,
            token_identity: Uuid::new_v4().to_string(),
            role: Some(Role::Admin),
            scopes: Vec::new(),
        }
    }

    /// Insert one unexpired approval request for the supplied originating principal.
    fn pending_approval(
        approvals: &ApprovalStore,
        tenant: TenantId,
        principal: PrincipalId,
    ) -> Approval {
        let now = unix_seconds();
        approvals
            .create_or_get_pending(
                ApprovalRequest {
                    tenant,
                    principal,
                    token_identity: Uuid::new_v4().to_string(),
                    tool: "demo".to_string(),
                    action: "create".to_string(),
                    request_hash: [4_u8; 32],
                    prompt: "Approve demo.create?".to_string(),
                    policy_version: APPROVAL_POLICY_VERSION.to_string(),
                    created_at: now,
                    expires_at: now + 60,
                },
                now,
            )
            .expect("pending approval")
    }

    /// Only exact reviewed read-only operations bypass escalation.
    #[test]
    fn approval_policy_is_fail_closed_for_unknown_actions() {
        let invocation = |tool: &str, action: &str| ToolInvocation {
            tool: tool.to_string(),
            action: action.to_string(),
            args: json!({}),
        };
        assert!(!requires_human_approval(&invocation(
            "github",
            "list_issues"
        )));
        assert!(!requires_human_approval(&invocation("henosis", "probe")));
        assert!(requires_human_approval(&invocation(
            "github",
            "get_and_delete"
        )));
        assert!(requires_human_approval(&invocation("demo", "list_issues")));
        assert!(requires_human_approval(&invocation("demo", "unrecognized")));
    }

    /// Public dispatch rejects direct requests for caller-selected phylaxd secret slots.
    #[test]
    fn public_dispatch_rejects_raw_phylaxd_target() {
        let body = DispatchBody {
            tool: "phylaxd".to_string(),
            action: "sign".to_string(),
            args: json!({"category": "production", "name": "deploy"}),
            context: ClientContext::default(),
        };
        assert!(matches!(
            validate_dispatch_body(&body),
            Err(AuthorityError::Forbidden)
        ));
    }

    /// The trusted internal phylaxd gate still accepts a structurally valid broker request.
    #[tokio::test]
    async fn internal_phylaxd_gate_remains_available_to_trusted_adapters() {
        let tenant = TenantId::new();
        let request = GateRequest {
            context: RequestContext {
                tenant,
                principal: PrincipalId::new(),
                persona: None,
                session: None,
                room: None,
                task: None,
                workflow: None,
                authority: Some(AuthorityContext {
                    token_identity: Uuid::new_v4().to_string(),
                    idempotency_key: "trusted-adapter-call".to_string(),
                    approval_id: None,
                }),
            },
            invocation: ToolInvocation {
                tool: "phylaxd".to_string(),
                action: "sign".to_string(),
                args: json!({"category": "production", "name": tenant.to_string()}),
            },
        };
        assert_eq!(
            PhylaxdGate::new().check(&request).await.expect("gate"),
            GateDecision::Allow
        );
    }

    /// The internal broker gate rejects a valid slot owned by another tenant.
    #[tokio::test]
    async fn internal_phylaxd_gate_rejects_cross_tenant_slot() {
        let tenant = TenantId::new();
        let request = GateRequest {
            context: RequestContext {
                tenant,
                principal: PrincipalId::new(),
                persona: None,
                session: None,
                room: None,
                task: None,
                workflow: None,
                authority: Some(AuthorityContext {
                    token_identity: Uuid::new_v4().to_string(),
                    idempotency_key: "cross-tenant-broker-call".to_string(),
                    approval_id: None,
                }),
            },
            invocation: ToolInvocation {
                tool: "phylaxd".to_string(),
                action: "derive".to_string(),
                args: json!({
                    "category": "production",
                    "name": TenantId::new().to_string()
                }),
            },
        };
        assert!(matches!(
            PhylaxdGate::new().check(&request).await.expect("gate"),
            GateDecision::Deny { reason }
                if reason == "credential slot does not belong to the authenticated tenant"
        ));
    }

    /// Dispatch idempotency headers accept one bounded opaque identifier only.
    #[test]
    fn dispatch_idempotency_header_is_required_unique_and_bounded() {
        let mut headers = HeaderMap::new();
        assert!(matches!(
            dispatch_idempotency_key(&headers),
            Err(AuthorityError::InvalidRequest(_))
        ));

        headers.insert(
            IDEMPOTENCY_HEADER,
            "retry.2026-07-23:0001".parse().expect("header"),
        );
        assert_eq!(
            dispatch_idempotency_key(&headers).expect("idempotency key"),
            "retry.2026-07-23:0001"
        );

        headers.append(IDEMPOTENCY_HEADER, "duplicate".parse().expect("header"));
        assert!(matches!(
            dispatch_idempotency_key(&headers),
            Err(AuthorityError::InvalidRequest(_))
        ));

        let mut invalid = HeaderMap::new();
        invalid.insert(IDEMPOTENCY_HEADER, "".parse().expect("header"));
        assert!(matches!(
            dispatch_idempotency_key(&invalid),
            Err(AuthorityError::InvalidRequest(_))
        ));

        invalid.insert(
            IDEMPOTENCY_HEADER,
            axum::http::HeaderValue::from_bytes(b"opaque\xfa").expect("header"),
        );
        assert!(matches!(
            dispatch_idempotency_key(&invalid),
            Err(AuthorityError::InvalidRequest(_))
        ));

        invalid.insert(
            IDEMPOTENCY_HEADER,
            "contains space".parse().expect("header"),
        );
        assert!(matches!(
            dispatch_idempotency_key(&invalid),
            Err(AuthorityError::InvalidRequest(_))
        ));

        invalid.insert(
            IDEMPOTENCY_HEADER,
            "x".repeat(MAX_IDEMPOTENCY_KEY_BYTES + 1)
                .parse()
                .expect("header"),
        );
        assert!(matches!(
            dispatch_idempotency_key(&invalid),
            Err(AuthorityError::InvalidRequest(_))
        ));
    }

    /// Approval headers accept zero or one canonical UUID and reject duplicates.
    #[test]
    fn dispatch_approval_header_is_optional_unique_and_canonical() {
        let mut headers = HeaderMap::new();
        assert_eq!(dispatch_approval_id(&headers).expect("absent header"), None);

        let id = Uuid::new_v4().to_string();
        headers.insert(APPROVAL_HEADER, id.parse().expect("header"));
        assert_eq!(
            dispatch_approval_id(&headers).expect("approval id"),
            Some(id)
        );

        headers.append(
            APPROVAL_HEADER,
            Uuid::new_v4().to_string().parse().expect("header"),
        );
        assert!(matches!(
            dispatch_approval_id(&headers),
            Err(AuthorityError::InvalidRequest(_))
        ));

        let mut noncanonical = HeaderMap::new();
        noncanonical.insert(
            APPROVAL_HEADER,
            "550E8400-E29B-41D4-A716-446655440000"
                .parse()
                .expect("header"),
        );
        assert!(matches!(
            dispatch_approval_id(&noncanonical),
            Err(AuthorityError::InvalidRequest(_))
        ));
    }

    /// Authority authentication accepts exactly one non-empty bearer field.
    #[test]
    fn bearer_credential_rejects_ambiguous_headers() {
        let mut headers = HeaderMap::new();
        assert!(matches!(
            bearer_credential(&headers),
            Err(AuthorityError::Authentication)
        ));
        headers.append(
            axum::http::header::AUTHORIZATION,
            "Bearer machine-token".parse().expect("header"),
        );
        assert_eq!(
            bearer_credential(&headers).expect("bearer"),
            "machine-token"
        );
        headers.append(
            axum::http::header::AUTHORIZATION,
            "Bearer second-token".parse().expect("header"),
        );
        assert!(matches!(
            bearer_credential(&headers),
            Err(AuthorityError::Authentication)
        ));
    }

    /// The dispatch handler rejects a missing idempotency key before invoking any gate.
    #[tokio::test]
    async fn dispatch_handler_requires_idempotency_header() {
        let approvals = Arc::new(ApprovalStore::open_in_memory().expect("approvals"));
        let state = witnessed_state(approvals);
        let identity = administrator_identity(TenantId::new(), PrincipalId::new());
        let result = dispatch(
            State(state),
            identity,
            HeaderMap::new(),
            Json(DispatchBody {
                tool: "demo".to_string(),
                action: "list".to_string(),
                args: json!({}),
                context: ClientContext::default(),
            }),
        )
        .await;
        assert!(matches!(result, Err(AuthorityError::InvalidRequest(_))));
    }

    /// Witnessed production approval rejects the principal that originated the request.
    #[tokio::test]
    async fn witnessed_approval_rejects_same_principal() {
        let approvals = Arc::new(ApprovalStore::open_in_memory().expect("approvals"));
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let approval = pending_approval(&approvals, tenant, principal);
        let state = witnessed_state(Arc::clone(&approvals));

        let result = decide_approval(
            state,
            administrator_identity(tenant, principal),
            approval.id.to_string(),
            ApprovalDecisionBody {
                reason: Some("self approval must fail".to_string()),
            },
            ApprovalDecision::Approve,
        )
        .await;

        assert!(matches!(result, Err(AuthorityError::Forbidden)));
        let stored = approvals
            .get(tenant, approval.id)
            .expect("approval lookup")
            .expect("approval record");
        assert_eq!(stored.status, ApprovalStatus::Pending);
        assert!(stored.decision_actor.is_none());
    }

    /// Witnessed production approval permits a distinct authorized administrator principal.
    #[tokio::test]
    async fn witnessed_approval_allows_distinct_authorized_principal() {
        let approvals = Arc::new(ApprovalStore::open_in_memory().expect("approvals"));
        let tenant = TenantId::new();
        let requester = PrincipalId::new();
        let approver = PrincipalId::new();
        let approval = pending_approval(&approvals, tenant, requester);
        let state = witnessed_state(Arc::clone(&approvals));

        let response = decide_approval(
            state,
            administrator_identity(tenant, approver),
            approval.id.to_string(),
            ApprovalDecisionBody {
                reason: Some("reviewed by a separate administrator".to_string()),
            },
            ApprovalDecision::Approve,
        )
        .await
        .expect("distinct approval");

        assert_eq!(response.0.status, "approved");
        let stored = approvals
            .get(tenant, approval.id)
            .expect("approval lookup")
            .expect("approval record");
        let expected_actor = approver.to_string();
        assert_eq!(stored.status, ApprovalStatus::Approved);
        assert_eq!(
            stored.decision_actor.as_deref(),
            Some(expected_actor.as_str())
        );
    }

    /// Canonical request hashing excludes approval retries while binding idempotency identity.
    #[test]
    fn request_hash_is_stable_across_approval_retry_and_binds_idempotency() {
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let mut request = GateRequest {
            context: RequestContext {
                tenant,
                principal,
                persona: None,
                session: Some("session-a".to_string()),
                room: None,
                task: None,
                workflow: None,
                authority: Some(AuthorityContext {
                    token_identity: Uuid::new_v4().to_string(),
                    idempotency_key: "retry-1".to_string(),
                    approval_id: None,
                }),
            },
            invocation: ToolInvocation {
                tool: "demo".to_string(),
                action: "create".to_string(),
                args: json!({"name": "bounded"}),
            },
        };
        let first = canonical_request_hash(&canonical_request_envelope(&request)).unwrap();
        request.context.authority.as_mut().unwrap().approval_id = Some(Uuid::new_v4().to_string());
        let retry = canonical_request_hash(&canonical_request_envelope(&request)).unwrap();
        assert_eq!(first, retry);
        request.context.authority.as_mut().unwrap().idempotency_key = "retry-2".to_string();
        let distinct_retry = canonical_request_hash(&canonical_request_envelope(&request)).unwrap();
        assert_ne!(first, distinct_retry);
    }

    /// An expired explicit approval is denied before execution and can be renewed without it.
    #[tokio::test]
    async fn expired_explicit_approval_can_start_a_fresh_lifecycle() {
        let approvals = Arc::new(ApprovalStore::open_in_memory().expect("approvals"));
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let token_identity = Uuid::new_v4().to_string();
        let mut request = GateRequest {
            context: RequestContext {
                tenant,
                principal,
                persona: None,
                session: None,
                room: None,
                task: None,
                workflow: None,
                authority: Some(AuthorityContext {
                    token_identity: token_identity.clone(),
                    idempotency_key: "renew-expired-approval".to_string(),
                    approval_id: None,
                }),
            },
            invocation: ToolInvocation {
                tool: "demo".to_string(),
                action: "create".to_string(),
                args: json!({"name": "bounded"}),
            },
        };
        let request_hash =
            canonical_request_hash(&canonical_request_envelope(&request)).expect("request hash");
        let now = unix_seconds();
        let approval = approvals
            .create_or_get_pending(
                ApprovalRequest {
                    tenant,
                    principal,
                    token_identity,
                    tool: request.invocation.tool.clone(),
                    action: request.invocation.action.clone(),
                    request_hash,
                    prompt: "Approve demo.create?".to_string(),
                    policy_version: APPROVAL_POLICY_VERSION.to_string(),
                    created_at: now - 10,
                    expires_at: now - 1,
                },
                now - 10,
            )
            .expect("pending approval");
        approvals
            .decide(
                tenant,
                approval.id,
                ApprovalDecision::Approve,
                "principal:reviewer",
                None,
                now - 9,
            )
            .expect("approval decision")
            .expect("approved record");

        let gate = DurableHumanGate::new(Arc::clone(&approvals));
        request.context.authority.as_mut().unwrap().approval_id = Some(approval.id.to_string());
        assert!(matches!(
            gate.check(&request).await.expect("expired decision"),
            GateDecision::Deny { .. }
        ));
        assert_eq!(
            approvals
                .get(tenant, approval.id)
                .expect("approval lookup")
                .expect("expired record")
                .status,
            ApprovalStatus::Expired
        );

        request.context.authority.as_mut().unwrap().approval_id = None;
        let GateDecision::RequireApproval {
            approval_id: Some(renewed),
            ..
        } = gate.check(&request).await.expect("renewal decision")
        else {
            panic!("expired approval must produce a fresh pending lifecycle");
        };
        assert_ne!(renewed, approval.id.to_string());
    }

    /// An exact consumed approval reaches the ledger for cached replay but cannot stand alone.
    #[tokio::test]
    async fn consumed_approval_allows_only_exact_explicit_replay() {
        let approvals = Arc::new(ApprovalStore::open_in_memory().expect("approvals"));
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let token_identity = Uuid::new_v4().to_string();
        let mut request = GateRequest {
            context: RequestContext {
                tenant,
                principal,
                persona: None,
                session: None,
                room: None,
                task: None,
                workflow: None,
                authority: Some(AuthorityContext {
                    token_identity: token_identity.clone(),
                    idempotency_key: "replay-after-completion".to_string(),
                    approval_id: None,
                }),
            },
            invocation: ToolInvocation {
                tool: "demo".to_string(),
                action: "create".to_string(),
                args: json!({"name": "bounded"}),
            },
        };
        let request_hash =
            canonical_request_hash(&canonical_request_envelope(&request)).expect("request hash");
        let now = unix_seconds();
        let approval = approvals
            .create_or_get_pending(
                ApprovalRequest {
                    tenant,
                    principal,
                    token_identity: token_identity.clone(),
                    tool: request.invocation.tool.clone(),
                    action: request.invocation.action.clone(),
                    request_hash,
                    prompt: "Approve demo.create?".to_string(),
                    policy_version: APPROVAL_POLICY_VERSION.to_string(),
                    created_at: now,
                    expires_at: now + 60,
                },
                now,
            )
            .expect("pending approval");
        approvals
            .decide(
                tenant,
                approval.id,
                ApprovalDecision::Approve,
                "principal:reviewer",
                None,
                now,
            )
            .expect("approval decision")
            .expect("approved record");
        approvals
            .consume_approved(
                tenant,
                approval.id,
                principal,
                &token_identity,
                &request_hash,
                APPROVAL_POLICY_VERSION,
                now,
            )
            .expect("approval consumption")
            .expect("consumed record");

        let gate = DurableHumanGate::new(Arc::clone(&approvals));
        assert!(matches!(
            gate.check(&request).await.expect("gate decision"),
            GateDecision::Deny { .. }
        ));
        request.context.authority.as_mut().unwrap().approval_id = Some(approval.id.to_string());
        assert_eq!(
            gate.check(&request).await.expect("gate decision"),
            GateDecision::Allow
        );
    }

    /// A failed production outcome witness persistently blocks later tenant audit writes.
    #[tokio::test]
    async fn witnessed_completion_failure_blocks_stream() {
        let state = witnessed_state(Arc::new(
            ApprovalStore::open_in_memory().expect("approvals"),
        ));
        let tenant = TenantId::new().to_string();
        let principal = PrincipalId::new().to_string();
        let request = json!({"tool": "demo", "action": "create"});
        let idempotency_key = "witness-outcome-failure".to_string();
        state
            .audit
            .store()
            .claim_execution(AuditEventInput {
                tenant_id: tenant.clone(),
                principal_id: principal.clone(),
                action: "demo.create".to_string(),
                phase: AuditPhase::Intent,
                request: request.clone(),
                payload: json!({"tool": "demo"}),
                idempotency_key: Some(idempotency_key.clone()),
            })
            .expect("local execution claim");

        let result = state
            .audit
            .complete_execution(
                AuditEventInput {
                    tenant_id: tenant.clone(),
                    principal_id: principal,
                    action: "demo.create".to_string(),
                    phase: AuditPhase::Outcome,
                    request,
                    payload: json!({"tool": "demo", "outcome": "succeeded"}),
                    idempotency_key: Some(idempotency_key),
                },
                json!({"safe": true}),
            )
            .await;

        assert!(result.is_err());
        let stream = state
            .audit
            .store()
            .stream_state(&tenant)
            .expect("stream state")
            .expect("blocked stream");
        assert!(stream.blocked);
        assert_eq!(
            stream.reason_code.as_deref(),
            Some("outcome_completion_failed")
        );
    }
}
