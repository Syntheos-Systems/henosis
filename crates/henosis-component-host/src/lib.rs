//! Signed, deny-by-default Wasmtime Component Model host for Henosis extensions.
//!
//! This crate deliberately does not link WASI. Components receive no filesystem, process,
//! environment, clock, random, socket, or network authority from Wasmtime. External authority
//! is available only through explicitly granted mediated HTTP and opaque broker traits.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

/// Default signed ceiling for one component binary.
const DEFAULT_COMPONENT_BYTES: usize = 16 * 1024 * 1024;
/// Absolute host ceiling for one component binary.
const MAX_COMPONENT_BYTES: usize = 64 * 1024 * 1024;
/// Fixed interval used by the single process-wide Wasmtime epoch clock.
const EPOCH_TICK_MS: u64 = 10;
/// Maximum component compilations or invocations admitted process-wide.
const MAX_CONCURRENT_INVOCATIONS: usize = 8;
/// Maximum core instances allocated by one component invocation store.
const MAX_STORE_INSTANCES: usize = 1;
/// Maximum linear memories allocated by one component invocation store.
const MAX_STORE_MEMORIES: usize = 1;
/// Maximum tables allocated by one component invocation store.
const MAX_STORE_TABLES: usize = 1;
/// Maximum elements allocated by the single table in one invocation store.
const MAX_STORE_TABLE_ELEMENTS: usize = 10_000;
/// Maximum bytes accepted from one DNS resolution result.
const MAX_RESOLUTION_BYTES: usize = 16 * 16;
/// Absolute host ceiling for one mediated or component output.
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
/// Maximum stable identifier length for one trusted component signer.
const MAX_KEY_ID_BYTES: usize = 128;
/// Exact lowercase hexadecimal length of one SHA-256 digest.
const SHA256_HEX_BYTES: usize = 64;
/// Exact unpadded base64 length of one Ed25519 signature.
const ED25519_SIGNATURE_B64_BYTES: usize = 86;
/// Maximum ASCII DNS authority length accepted by mediated HTTP.
const MAX_HTTP_HOST_BYTES: usize = 253;
/// Maximum origin-form request-target length accepted by mediated HTTP.
const MAX_HTTP_PATH_BYTES: usize = 8 * 1024;
/// Maximum request body accepted by the in-process HTTP mediation contract.
const MAX_HTTP_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Generated typed bindings for the exact no-WASI extension world.
mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "extension",
        imports: { default: trappable },
    });
}

/// The exact Component Model world accepted by this public host ABI.
pub const HOST_WORLD_ID: &str = "henosis:component/extension@0.1.0";

/// The only capabilities that a signed extension may request.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    /// Allows requests through the host-controlled HTTP mediator.
    MediatedHttp,
    /// Allows opaque-handle operations through the secret broker mediator.
    Broker,
}

/// Per-invocation resource ceilings accepted by the host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Maximum signed component bytes accepted before hashing or compilation.
    pub component_bytes: usize,
    /// Maximum Wasmtime fuel units the component may consume.
    pub fuel: u64,
    /// Maximum linear-memory bytes available to the component.
    pub memory_bytes: usize,
    /// Maximum bytes emitted through host-mediated output.
    pub output_bytes: usize,
    /// Maximum wall-clock time allocated to one invocation.
    pub timeout_ms: u64,
}

/// Supplies conservative defaults when a signed manifest does not customize a limit.
impl Default for ResourceLimits {
    /// Returns conservative resource ceilings for an untrusted extension.
    fn default() -> Self {
        Self {
            component_bytes: DEFAULT_COMPONENT_BYTES,
            fuel: 10_000_000,
            memory_bytes: 64 * 1024 * 1024,
            output_bytes: 1024 * 1024,
            timeout_ms: 5_000,
        }
    }
}

/// Externally signed metadata bound to one exact component byte sequence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignedManifest {
    /// Stable identifier for the trusted signing key.
    pub key_id: String,
    /// Exact world identifier expected by the host.
    pub world: String,
    /// Lowercase hexadecimal SHA-256 digest of the component bytes.
    pub component_sha256: String,
    /// Explicitly requested host capabilities.
    pub capabilities: BTreeSet<Capability>,
    /// Resource ceilings selected for this component.
    pub limits: ResourceLimits,
    /// Base64 without padding Ed25519 signature over the canonical unsigned manifest bytes.
    pub signature: String,
}

/// Provides canonical signing bytes for a signed extension manifest.
impl SignedManifest {
    /// Returns the stable signed encoding that excludes the detached signature itself.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, SandboxError> {
        validate_manifest_fields(self)?;
        /// Canonical manifest fields excluding the detached signature.
        #[derive(Serialize)]
        struct UnsignedManifest<'a> {
            /// Stable identifier selecting the trusted verifying key.
            key_id: &'a str,
            /// Required Component Model world identifier.
            world: &'a str,
            /// SHA-256 digest bound to the exact component bytes.
            component_sha256: &'a str,
            /// Explicit capability grants requested by the component.
            capabilities: &'a BTreeSet<Capability>,
            /// Resource envelope authorized for this invocation.
            limits: &'a ResourceLimits,
        }

        serde_json::to_vec(&UnsignedManifest {
            key_id: &self.key_id,
            world: &self.world,
            component_sha256: &self.component_sha256,
            capabilities: &self.capabilities,
            limits: &self.limits,
        })
        .map_err(SandboxError::ManifestEncoding)
    }
}

/// Trusted Ed25519 verification keys indexed by their immutable key IDs.
#[derive(Clone, Debug, Default)]
pub struct TrustStore {
    /// Maps each trusted non-empty signer key ID to its verifying key.
    keys: BTreeMap<String, VerifyingKey>,
}

/// Manages immutable signer keys and validates signed component metadata.
impl TrustStore {
    /// Creates an empty trust store that accepts no extension signatures.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a key after rejecting an empty identifier or duplicate replacement.
    pub fn add_key(
        &mut self,
        key_id: impl Into<String>,
        key: VerifyingKey,
    ) -> Result<(), SandboxError> {
        let key_id = key_id.into();
        if !valid_key_id(&key_id) || self.keys.contains_key(&key_id) {
            return Err(SandboxError::InvalidTrustKeyId);
        }
        self.keys.insert(key_id, key);
        Ok(())
    }

    /// Verifies a manifest's exact world, digest, limits, and detached Ed25519 signature.
    pub fn verify(&self, manifest: &SignedManifest, component: &[u8]) -> Result<(), SandboxError> {
        validate_manifest_fields(manifest)?;
        if manifest.signature.len() != ED25519_SIGNATURE_B64_BYTES || !manifest.signature.is_ascii()
        {
            return Err(SandboxError::InvalidSignature);
        }
        if manifest.world != HOST_WORLD_ID {
            return Err(SandboxError::WorldMismatch {
                expected: HOST_WORLD_ID.to_string(),
                actual: manifest.world.clone(),
            });
        }
        validate_limits(&manifest.limits)?;
        if component.len() > manifest.limits.component_bytes {
            return Err(SandboxError::ComponentSizeExceeded);
        }
        let digest = hex::encode(Sha256::digest(component));
        if manifest.component_sha256 != digest {
            return Err(SandboxError::ComponentHashMismatch);
        }
        let key = self
            .keys
            .get(&manifest.key_id)
            .ok_or(SandboxError::UnknownSigningKey)?;
        let raw_signature = STANDARD_NO_PAD
            .decode(&manifest.signature)
            .map_err(|_| SandboxError::InvalidSignature)?;
        let signature =
            Signature::from_slice(&raw_signature).map_err(|_| SandboxError::InvalidSignature)?;
        key.verify(&manifest.signing_bytes()?, &signature)
            .map_err(|_| SandboxError::InvalidSignature)
    }
}

/// Parsed HTTP request allowed only through the host's explicit mediator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpRequest {
    /// Lowercase scheme, restricted to HTTPS by the mediator.
    pub scheme: String,
    /// DNS hostname or literal address selected by the caller.
    pub host: String,
    /// Relative path and query, without an authority component.
    pub path_and_query: String,
    /// Request body bounded by the mediator implementation.
    pub body: Vec<u8>,
}

/// Sanitized HTTP response returned by a host-controlled mediator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    /// Numeric response status returned by the remote service.
    pub status: u16,
    /// Response body after the mediator applies its own byte limit.
    pub body: Vec<u8>,
}

/// A request that the host resolved, SSRF-validated, and bound to exact connection addresses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinnedHttpRequest {
    /// Original request metadata used for HTTPS request construction and TLS server naming.
    request: HttpRequest,
    /// Public addresses the transport must use without DNS re-resolution.
    addresses: Vec<IpAddr>,
}

/// Exposes read-only access to a request that only the host may pin.
impl PinnedHttpRequest {
    /// Returns the original HTTPS request metadata.
    pub fn request(&self) -> &HttpRequest {
        &self.request
    }

    /// Returns the exact public addresses selected by the host's validation step.
    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }
}

/// Absolute resource contract supplied to every trusted mediator operation.
#[derive(Clone, Copy, Debug)]
pub struct MediationLimits {
    /// Monotonic deadline by which the operation must return.
    pub deadline: Instant,
    /// Maximum bytes the operation may allocate for its returned value.
    pub max_response_bytes: usize,
}

/// Explicit HTTP authority supplied by the embedding application.
pub trait MediatedHttp: Send + Sync {
    /// Resolves a hostname before a request is made so SSRF policy can inspect every address.
    ///
    /// Implementations are trusted boundary code and must honor `limits` before allocating output.
    fn resolve(&self, host: &str, limits: MediationLimits) -> Result<Vec<IpAddr>, SandboxError>;

    /// Sends one pinned request without DNS re-resolution or automatic redirect following.
    ///
    /// Implementations must dial one supplied address, preserve the original host for TLS SNI,
    /// enforce `limits` before side effects, and return no more than the contracted response bytes.
    fn send_pinned(
        &self,
        request: PinnedHttpRequest,
        limits: MediationLimits,
    ) -> Result<HttpResponse, SandboxError>;
}

/// Sanitized output from an allowlisted broker-side command execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerExecOutput {
    /// Process-style status code returned by the broker's allowlisted command.
    pub status: i32,
    /// Bounded and secret-scrubbed standard output.
    pub stdout: Vec<u8>,
    /// Bounded and secret-scrubbed standard error.
    pub stderr: Vec<u8>,
}

/// Explicit opaque secret-broker authority supplied by the embedding application.
pub trait BrokerMediator: Send + Sync {
    /// Signs a payload for an opaque handle without returning its secret material.
    fn sign(
        &self,
        opaque_handle: &str,
        payload: &[u8],
        limits: MediationLimits,
    ) -> Result<Vec<u8>, SandboxError>;

    /// Verifies a signature for an opaque handle without returning its secret material.
    fn verify(
        &self,
        opaque_handle: &str,
        payload: &[u8],
        signature: &[u8],
        limits: MediationLimits,
    ) -> Result<bool, SandboxError>;

    /// Derives a fresh opaque broker handle that cannot expose derived secret bytes.
    fn derive_handle(
        &self,
        opaque_handle: &str,
        purpose: &str,
        limits: MediationLimits,
    ) -> Result<String, SandboxError>;

    /// Executes a broker-allowlisted command without exposing the selected secret.
    fn exec(
        &self,
        opaque_handle: &str,
        argv: &[String],
        input: &[u8],
        limits: MediationLimits,
    ) -> Result<BrokerExecOutput, SandboxError>;
}

/// Capability grants available during one component invocation.
pub struct MediatedCapabilities<'a> {
    /// Signed capabilities permitted for the invocation.
    pub granted: &'a BTreeSet<Capability>,
    /// Optional HTTP mediator supplied by the embedding application.
    pub http: Option<Arc<dyn MediatedHttp>>,
    /// Optional opaque secret broker supplied by the embedding application.
    pub broker: Option<Arc<dyn BrokerMediator>>,
    /// Absolute wall-clock deadline shared by every mediator call in this operation.
    pub deadline: Instant,
    /// Maximum bytes accepted from any single mediator result.
    pub max_response_bytes: usize,
}

/// Applies signed capability checks before dispatching to embedding-side mediators.
impl<'a> MediatedCapabilities<'a> {
    /// Sends an HTTPS request only when the manifest granted mediated HTTP authority.
    pub fn http_request(&self, request: HttpRequest) -> Result<HttpResponse, SandboxError> {
        let mediator = self
            .http
            .clone()
            .ok_or(SandboxError::CapabilityUnavailable)?;
        let request = self.pin_request(request)?;
        let limits = self.mediation_limits(self.max_response_bytes)?;
        let response = mediator_executor()?
            .execute(self.deadline, move || mediator.send_pinned(request, limits))?;
        validate_response_bytes(response.body.len(), limits.max_response_bytes)?;
        Ok(response)
    }

    /// Pins a redirect target before a mediator can make a separately authorized single request.
    pub fn pin_redirect(&self, redirect: HttpRequest) -> Result<PinnedHttpRequest, SandboxError> {
        self.pin_request(redirect)
    }

    /// Resolves and validates an HTTPS request before exposing it to a transport implementation.
    fn pin_request(&self, request: HttpRequest) -> Result<PinnedHttpRequest, SandboxError> {
        require_capability(self.granted, Capability::MediatedHttp)?;
        validate_http_request_shape(&request)?;
        let mediator = self
            .http
            .clone()
            .ok_or(SandboxError::CapabilityUnavailable)?;
        let host = request.host.clone();
        let limits = self.mediation_limits(MAX_RESOLUTION_BYTES.min(self.max_response_bytes))?;
        let addresses =
            mediator_executor()?.execute(self.deadline, move || mediator.resolve(&host, limits))?;
        validate_resolution_bytes(&addresses, limits.max_response_bytes)?;
        validate_http_request(&request, &addresses)?;
        Ok(PinnedHttpRequest { request, addresses })
    }

    /// Signs a payload only when the manifest granted opaque broker authority.
    pub fn sign(&self, opaque_handle: &str, payload: &[u8]) -> Result<Vec<u8>, SandboxError> {
        require_capability(self.granted, Capability::Broker)?;
        let mediator = self
            .broker
            .clone()
            .ok_or(SandboxError::CapabilityUnavailable)?;
        let opaque_handle = opaque_handle.to_owned();
        let payload = payload.to_owned();
        let limits = self.mediation_limits(self.max_response_bytes)?;
        let signature = mediator_executor()?.execute(self.deadline, move || {
            mediator.sign(&opaque_handle, &payload, limits)
        })?;
        validate_response_bytes(signature.len(), limits.max_response_bytes)?;
        Ok(signature)
    }

    /// Verifies a signature only when the manifest granted opaque broker authority.
    pub fn verify(
        &self,
        opaque_handle: &str,
        payload: &[u8],
        signature: &[u8],
    ) -> Result<bool, SandboxError> {
        require_capability(self.granted, Capability::Broker)?;
        let mediator = self
            .broker
            .clone()
            .ok_or(SandboxError::CapabilityUnavailable)?;
        let opaque_handle = opaque_handle.to_owned();
        let payload = payload.to_owned();
        let signature = signature.to_owned();
        let limits = self.mediation_limits(1)?;
        mediator_executor()?.execute(self.deadline, move || {
            mediator.verify(&opaque_handle, &payload, &signature, limits)
        })
    }

    /// Derives an opaque handle only when the manifest granted opaque broker authority.
    pub fn derive_handle(
        &self,
        opaque_handle: &str,
        purpose: &str,
    ) -> Result<String, SandboxError> {
        require_capability(self.granted, Capability::Broker)?;
        let mediator = self
            .broker
            .clone()
            .ok_or(SandboxError::CapabilityUnavailable)?;
        let opaque_handle = opaque_handle.to_owned();
        let purpose = purpose.to_owned();
        let limits = self.mediation_limits(self.max_response_bytes)?;
        let handle = mediator_executor()?.execute(self.deadline, move || {
            mediator.derive_handle(&opaque_handle, &purpose, limits)
        })?;
        validate_response_bytes(handle.len(), limits.max_response_bytes)?;
        Ok(handle)
    }

    /// Executes an allowlisted broker operation only when the manifest granted it.
    pub fn exec(
        &self,
        opaque_handle: &str,
        argv: &[String],
        input: &[u8],
    ) -> Result<BrokerExecOutput, SandboxError> {
        require_capability(self.granted, Capability::Broker)?;
        let mediator = self
            .broker
            .clone()
            .ok_or(SandboxError::CapabilityUnavailable)?;
        let opaque_handle = opaque_handle.to_owned();
        let argv = argv.to_owned();
        let input = input.to_owned();
        let limits = self.mediation_limits(self.max_response_bytes)?;
        let output = mediator_executor()?.execute(self.deadline, move || {
            mediator.exec(&opaque_handle, &argv, &input, limits)
        })?;
        validate_exec_output(&output, limits.max_response_bytes)?;
        Ok(output)
    }

    /// Builds a non-zero per-operation contract bounded by this capability wrapper.
    fn mediation_limits(&self, max_response_bytes: usize) -> Result<MediationLimits, SandboxError> {
        if Instant::now() >= self.deadline {
            return Err(SandboxError::DeadlineExceeded);
        }
        if max_response_bytes == 0 || max_response_bytes > MAX_OUTPUT_BYTES {
            return Err(SandboxError::OutputLimitExceeded);
        }
        Ok(MediationLimits {
            deadline: self.deadline,
            max_response_bytes,
        })
    }
}

/// Maximum synchronous mediator calls that may execute concurrently process-wide.
const MEDIATOR_WORKER_COUNT: usize = 4;
/// Maximum submitted mediator calls retained while fixed workers accept them.
const MEDIATOR_QUEUE_CAPACITY: usize = MEDIATOR_WORKER_COUNT;
/// Type-erased operation executed by one fixed mediator worker.
type MediatorJob = Box<dyn FnOnce() + Send + 'static>;
/// Process-wide bounded executor that isolates invoking threads from blocking mediators.
static MEDIATOR_EXECUTOR: OnceLock<Result<MediatorExecutor, String>> = OnceLock::new();

/// Fixed worker pool with a bounded submission queue for synchronous mediator operations.
struct MediatorExecutor {
    sender: mpsc::SyncSender<MediatorJob>,
    capacity: Arc<MediatorCapacity>,
}

/// Counts mediator jobs that are queued or still occupying a worker after caller timeout.
struct MediatorCapacity {
    active: AtomicUsize,
}

/// Acquires fixed mediator capacity without waiting.
impl MediatorCapacity {
    /// Reserves one worker slot or rejects work when every slot remains occupied.
    fn try_acquire(self: &Arc<Self>) -> Result<MediatorPermit, SandboxError> {
        let mut observed = self.active.load(Ordering::Acquire);
        loop {
            if observed >= MEDIATOR_WORKER_COUNT {
                return Err(SandboxError::MediatorCapacityExhausted);
            }
            match self.active.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(MediatorPermit {
                        capacity: self.clone(),
                    });
                }
                Err(actual) => observed = actual,
            }
        }
    }
}

/// Owns one mediator worker slot until the submitted operation actually exits.
struct MediatorPermit {
    capacity: Arc<MediatorCapacity>,
}

/// Releases mediator capacity after success, error, panic, or a late post-timeout return.
impl Drop for MediatorPermit {
    /// Returns one slot to the fixed mediator worker pool.
    fn drop(&mut self) {
        self.capacity.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Creates fixed workers and enforces absolute deadlines while receiving mediator results.
impl MediatorExecutor {
    /// Starts the process-wide fixed workers without creating threads per request.
    fn new() -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel::<MediatorJob>(MEDIATOR_QUEUE_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let capacity = Arc::new(MediatorCapacity {
            active: AtomicUsize::new(0),
        });
        for worker_index in 0..MEDIATOR_WORKER_COUNT {
            let receiver = receiver.clone();
            thread::Builder::new()
                .name(format!("henosis-mediator-{worker_index}"))
                .spawn(move || loop {
                    let job = {
                        let receiver = match receiver.lock() {
                            Ok(receiver) => receiver,
                            Err(_) => return,
                        };
                        receiver.recv()
                    };
                    match job {
                        Ok(job) => {
                            let _ = catch_unwind(AssertUnwindSafe(job));
                        }
                        Err(_) => return,
                    }
                })
                .map_err(|error| error.to_string())?;
        }
        Ok(Self { sender, capacity })
    }

    /// Executes one owned mediator call and stops waiting at the absolute deadline.
    fn execute<T, F>(&self, deadline: Instant, operation: F) -> Result<T, SandboxError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, SandboxError> + Send + 'static,
    {
        if Instant::now() >= deadline {
            return Err(SandboxError::DeadlineExceeded);
        }
        let permit = self.capacity.try_acquire()?;
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        self.sender
            .try_send(Box::new(move || {
                let _permit = permit;
                let result = if Instant::now() >= deadline {
                    Err(SandboxError::DeadlineExceeded)
                } else {
                    operation()
                };
                let _ = result_sender.send(result);
            }))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => SandboxError::MediatorCapacityExhausted,
                mpsc::TrySendError::Disconnected(_) => SandboxError::MediatorUnavailable,
            })?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(SandboxError::DeadlineExceeded)?;
        match result_receiver.recv_timeout(remaining) {
            Ok(result) if Instant::now() < deadline => result,
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => Err(SandboxError::DeadlineExceeded),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(SandboxError::MediatorUnavailable),
        }
    }
}

/// Returns the lazily initialized fixed mediator executor or a fail-closed startup error.
fn mediator_executor() -> Result<&'static MediatorExecutor, SandboxError> {
    MEDIATOR_EXECUTOR
        .get_or_init(MediatorExecutor::new)
        .as_ref()
        .map_err(|_| SandboxError::MediatorUnavailable)
}

/// Output and elapsed-time accounting for a single component call.
#[derive(Debug)]
pub struct InvocationBudget {
    /// Absolute monotonic deadline shared by Wasm execution and every mediator call.
    deadline: Instant,
    /// Signed limits enforced by this accounting helper.
    limits: ResourceLimits,
    /// Bytes emitted through host-mediated output so far.
    emitted: usize,
}

/// Accounts for output and elapsed time independently of Wasmtime instruction limits.
impl InvocationBudget {
    /// Starts accounting for an invocation using already-validated signed limits.
    pub fn new(limits: ResourceLimits) -> Result<Self, SandboxError> {
        validate_limits(&limits)?;
        Ok(Self {
            deadline: Instant::now() + Duration::from_millis(limits.timeout_ms),
            limits,
            emitted: 0,
        })
    }

    /// Charges mediated output and rejects any byte count above the signed ceiling.
    pub fn charge_output(&mut self, bytes: usize) -> Result<(), SandboxError> {
        self.emitted = self
            .emitted
            .checked_add(bytes)
            .ok_or(SandboxError::OutputLimitExceeded)?;
        if self.emitted > self.limits.output_bytes {
            return Err(SandboxError::OutputLimitExceeded);
        }
        Ok(())
    }

    /// Rejects work that has outlived the signed wall-clock deadline.
    pub fn check_deadline(&self) -> Result<(), SandboxError> {
        if Instant::now() >= self.deadline {
            return Err(SandboxError::DeadlineExceeded);
        }
        Ok(())
    }

    /// Returns the invocation's absolute deadline for bounded mediator dispatch.
    fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Returns the remaining signed output capacity for the next mediator result.
    fn remaining_output(&self) -> Result<usize, SandboxError> {
        self.limits
            .output_bytes
            .checked_sub(self.emitted)
            .filter(|remaining| *remaining > 0)
            .ok_or(SandboxError::OutputLimitExceeded)
    }
}

/// Process-wide admission gate that bounds component compilation and execution.
struct InvocationGate {
    /// Maximum simultaneous permits issued by this gate.
    limit: usize,
    /// Number of permits currently held by callers.
    active: AtomicUsize,
}

/// Acquires and releases fixed component-runtime capacity.
impl InvocationGate {
    /// Creates a gate with an immutable positive concurrency limit.
    const fn new(limit: usize) -> Self {
        Self {
            limit,
            active: AtomicUsize::new(0),
        }
    }

    /// Acquires one permit without waiting or fails closed when capacity is exhausted.
    fn try_acquire(&self) -> Result<InvocationPermit<'_>, SandboxError> {
        let mut observed = self.active.load(Ordering::Acquire);
        loop {
            if observed >= self.limit {
                return Err(SandboxError::InvocationCapacityExhausted);
            }
            match self.active.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(InvocationPermit { gate: self }),
                Err(actual) => observed = actual,
            }
        }
    }
}

/// RAII permit that releases process-wide component-runtime capacity on every exit path.
struct InvocationPermit<'a> {
    /// Admission gate that issued this permit.
    gate: &'a InvocationGate,
}

/// Releases one component-runtime slot after success, error, trap, or unwind.
impl Drop for InvocationPermit<'_> {
    /// Returns the held slot to the fixed process-wide capacity.
    fn drop(&mut self) {
        self.gate.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Fixed process-wide component admission capacity.
static INVOCATION_GATE: InvocationGate = InvocationGate::new(MAX_CONCURRENT_INVOCATIONS);
/// Lazily initialized shared Engine driven by one fixed epoch clock thread.
static COMPONENT_ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();

/// Creates the shared Engine and its single process-wide epoch clock.
fn component_engine() -> Result<&'static Engine, SandboxError> {
    COMPONENT_ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config.wasm_component_model(true);
            config.consume_fuel(true);
            config.epoch_interruption(true);
            let engine = Engine::new(&config).map_err(|error| error.to_string())?;
            let epoch_engine = engine.clone();
            thread::Builder::new()
                .name("henosis-component-epoch".to_string())
                .spawn(move || loop {
                    thread::park_timeout(Duration::from_millis(EPOCH_TICK_MS));
                    epoch_engine.increment_epoch();
                })
                .map_err(|error| error.to_string())?;
            Ok(engine)
        })
        .as_ref()
        .map_err(|message| SandboxError::RuntimeUnavailable(message.clone()))
}

/// Converts a signed wall-clock timeout into a per-store relative epoch deadline.
fn epoch_deadline_ticks(timeout_ms: u64) -> u64 {
    timeout_ms.saturating_add(EPOCH_TICK_MS - 1) / EPOCH_TICK_MS
}

/// Builds the complete fixed allocation envelope for one component invocation store.
fn component_store_limits(memory_bytes: usize) -> StoreLimits {
    StoreLimitsBuilder::new()
        .memory_size(memory_bytes)
        .instances(MAX_STORE_INSTANCES)
        .memories(MAX_STORE_MEMORIES)
        .tables(MAX_STORE_TABLES)
        .table_elements(MAX_STORE_TABLE_ELEMENTS)
        .build()
}

/// Wasmtime engine wrapper that has Component Model support but never links WASI.
pub struct ComponentSandbox {
    /// Shared Wasmtime engine configured with Component Model resource controls.
    engine: Engine,
}

/// Explicit mediators supplied to a single typed component invocation.
#[derive(Default)]
pub struct InvocationMediators {
    /// Optional HTTPS mediator available only to manifests that grant it.
    pub http: Option<Arc<dyn MediatedHttp>>,
    /// Optional opaque secret broker available only to manifests that grant it.
    pub broker: Option<Arc<dyn BrokerMediator>>,
}

/// Configures, verifies, links, and invokes components without linking WASI.
impl ComponentSandbox {
    /// Creates a Component Model engine configured for fuel and epoch interruption.
    pub fn new() -> Result<Self, SandboxError> {
        component_engine().cloned().map(|engine| Self { engine })
    }

    /// Verifies a signed component and compiles it before any store or linker exists.
    pub fn load_component(
        &self,
        trust: &TrustStore,
        manifest: &SignedManifest,
        bytes: &[u8],
    ) -> Result<Component, SandboxError> {
        let _permit = INVOCATION_GATE.try_acquire()?;
        self.load_component_admitted(trust, manifest, bytes)
    }

    /// Verifies and compiles a component while the caller holds runtime admission.
    fn load_component_admitted(
        &self,
        trust: &TrustStore,
        manifest: &SignedManifest,
        bytes: &[u8],
    ) -> Result<Component, SandboxError> {
        trust.verify(manifest, bytes)?;
        Component::new(&self.engine, bytes).map_err(SandboxError::Wasmtime)
    }

    /// Verifies, links, and invokes the exact typed export under signed resource limits.
    pub fn invoke(
        &self,
        trust: &TrustStore,
        manifest: &SignedManifest,
        bytes: &[u8],
        mediators: InvocationMediators,
        payload: &[u8],
    ) -> Result<Vec<u8>, SandboxError> {
        let _permit = INVOCATION_GATE.try_acquire()?;
        let component = self.load_component_admitted(trust, manifest, bytes)?;
        let limits = &manifest.limits;
        let budget = InvocationBudget::new(limits.clone())?;
        let store_limits = component_store_limits(limits.memory_bytes);
        let mut store = Store::new(
            &self.engine,
            SandboxState {
                granted: manifest.capabilities.clone(),
                http: mediators.http,
                broker: mediators.broker,
                budget,
                store_limits,
            },
        );
        store.limiter(|state| &mut state.store_limits);
        store
            .set_fuel(limits.fuel)
            .map_err(SandboxError::Wasmtime)?;
        store.set_epoch_deadline(epoch_deadline_ticks(limits.timeout_ms));
        let mut linker = Linker::new(&self.engine);
        bindings::Extension::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(SandboxError::Wasmtime)?;
        let result = (|| {
            let extension = bindings::Extension::instantiate(&mut store, &component, &linker)?;
            extension.call_invoke(&mut store, payload)
        })();
        if store.data().budget.check_deadline().is_err() {
            return Err(SandboxError::DeadlineExceeded);
        }
        let result = result.map_err(SandboxError::Wasmtime)?;
        store.data_mut().budget.charge_output(result.len())?;
        store.data().budget.check_deadline()?;
        Ok(result)
    }
}

/// Store data containing all invocation-local authority and resource accounting.
struct SandboxState {
    /// Signed capability grants enforced independently at every host import.
    granted: BTreeSet<Capability>,
    /// Optional host-owned HTTPS mediator with validated-address enforcement.
    http: Option<Arc<dyn MediatedHttp>>,
    /// Optional host-owned opaque broker with no raw-secret operation.
    broker: Option<Arc<dyn BrokerMediator>>,
    /// Output and elapsed-time budget shared by exports and host imports.
    budget: InvocationBudget,
    /// Per-store memory limiter supplied to Wasmtime before instantiation.
    store_limits: StoreLimits,
}

/// Implements the typed mediated HTTP world import for one invocation store.
impl bindings::henosis::component::http::Host for SandboxState {
    /// Validates the request, pins its public addresses, and charges its bounded response output.
    fn send(
        &mut self,
        request: bindings::henosis::component::http::Request,
    ) -> wasmtime::Result<bindings::henosis::component::http::Response> {
        self.budget.check_deadline().map_err(host_failure)?;
        require_capability(&self.granted, Capability::MediatedHttp).map_err(host_failure)?;
        let mediator = self
            .http
            .as_ref()
            .ok_or(SandboxError::CapabilityUnavailable)
            .map_err(host_failure)?;
        let request = HttpRequest {
            scheme: request.scheme,
            host: request.host,
            path_and_query: request.path_and_query,
            body: request.body,
        };
        validate_http_request_shape(&request).map_err(host_failure)?;
        let host = request.host.clone();
        let deadline = self.budget.deadline();
        let resolution_limits = MediationLimits {
            deadline,
            max_response_bytes: MAX_RESOLUTION_BYTES,
        };
        let mediator = mediator.clone();
        let addresses = mediator_executor()
            .and_then(|executor| {
                executor.execute(deadline, move || mediator.resolve(&host, resolution_limits))
            })
            .map_err(host_failure)?;
        validate_resolution_bytes(&addresses, resolution_limits.max_response_bytes)
            .map_err(host_failure)?;
        validate_http_request(&request, &addresses).map_err(host_failure)?;
        let mediator = self
            .http
            .as_ref()
            .ok_or(SandboxError::CapabilityUnavailable)
            .map_err(host_failure)?
            .clone();
        let pinned_request = PinnedHttpRequest { request, addresses };
        let response_limits = MediationLimits {
            deadline,
            max_response_bytes: self.budget.remaining_output().map_err(host_failure)?,
        };
        let response = mediator_executor()
            .and_then(|executor| {
                executor.execute(deadline, move || {
                    mediator.send_pinned(pinned_request, response_limits)
                })
            })
            .map_err(host_failure)?;
        validate_response_bytes(response.body.len(), response_limits.max_response_bytes)
            .map_err(host_failure)?;
        self.budget
            .charge_output(response.body.len())
            .map_err(host_failure)?;
        Ok(bindings::henosis::component::http::Response {
            status: response.status,
            body: response.body,
        })
    }
}

/// Implements typed non-exportable secret-broker operations for one invocation store.
impl bindings::henosis::component::broker::Host for SandboxState {
    /// Signs with an opaque handle and charges the non-secret signature bytes.
    fn sign(&mut self, handle: String, payload: Vec<u8>) -> wasmtime::Result<Vec<u8>> {
        self.budget.check_deadline().map_err(host_failure)?;
        require_capability(&self.granted, Capability::Broker).map_err(host_failure)?;
        let mediator = self
            .broker
            .as_ref()
            .ok_or(SandboxError::CapabilityUnavailable)
            .map_err(host_failure)?
            .clone();
        let limits = MediationLimits {
            deadline: self.budget.deadline(),
            max_response_bytes: self.budget.remaining_output().map_err(host_failure)?,
        };
        let signature = mediator_executor()
            .and_then(|executor| {
                executor.execute(limits.deadline, move || {
                    mediator.sign(&handle, &payload, limits)
                })
            })
            .map_err(host_failure)?;
        validate_response_bytes(signature.len(), limits.max_response_bytes)
            .map_err(host_failure)?;
        self.budget
            .charge_output(signature.len())
            .map_err(host_failure)?;
        Ok(signature)
    }

    /// Verifies with an opaque handle and returns only a boolean result.
    fn verify(
        &mut self,
        handle: String,
        payload: Vec<u8>,
        signature: Vec<u8>,
    ) -> wasmtime::Result<bool> {
        self.budget.check_deadline().map_err(host_failure)?;
        require_capability(&self.granted, Capability::Broker).map_err(host_failure)?;
        let mediator = self
            .broker
            .as_ref()
            .ok_or(SandboxError::CapabilityUnavailable)
            .map_err(host_failure)?
            .clone();
        let limits = MediationLimits {
            deadline: self.budget.deadline(),
            max_response_bytes: 1,
        };
        mediator_executor()
            .and_then(|executor| {
                executor.execute(limits.deadline, move || {
                    mediator.verify(&handle, &payload, &signature, limits)
                })
            })
            .map_err(host_failure)
    }

    /// Derives only another opaque handle, never derived key material.
    fn derive(&mut self, handle: String, purpose: String) -> wasmtime::Result<String> {
        self.budget.check_deadline().map_err(host_failure)?;
        require_capability(&self.granted, Capability::Broker).map_err(host_failure)?;
        let mediator = self
            .broker
            .as_ref()
            .ok_or(SandboxError::CapabilityUnavailable)
            .map_err(host_failure)?
            .clone();
        let limits = MediationLimits {
            deadline: self.budget.deadline(),
            max_response_bytes: self.budget.remaining_output().map_err(host_failure)?,
        };
        let derived_handle = mediator_executor()
            .and_then(|executor| {
                executor.execute(limits.deadline, move || {
                    mediator.derive_handle(&handle, &purpose, limits)
                })
            })
            .map_err(host_failure)?;
        validate_response_bytes(derived_handle.len(), limits.max_response_bytes)
            .map_err(host_failure)?;
        self.budget
            .charge_output(derived_handle.len())
            .map_err(host_failure)?;
        Ok(derived_handle)
    }

    /// Executes only through the broker's allowlist and charges scrubbed output bytes.
    fn exec(
        &mut self,
        handle: String,
        argv: Vec<String>,
        input: Vec<u8>,
    ) -> wasmtime::Result<bindings::henosis::component::broker::ExecResult> {
        self.budget.check_deadline().map_err(host_failure)?;
        require_capability(&self.granted, Capability::Broker).map_err(host_failure)?;
        let mediator = self
            .broker
            .as_ref()
            .ok_or(SandboxError::CapabilityUnavailable)
            .map_err(host_failure)?
            .clone();
        let limits = MediationLimits {
            deadline: self.budget.deadline(),
            max_response_bytes: self.budget.remaining_output().map_err(host_failure)?,
        };
        let output = mediator_executor()
            .and_then(|executor| {
                executor.execute(limits.deadline, move || {
                    mediator.exec(&handle, &argv, &input, limits)
                })
            })
            .map_err(host_failure)?;
        validate_exec_output(&output, limits.max_response_bytes).map_err(host_failure)?;
        self.budget
            .charge_output(
                output
                    .stdout
                    .len()
                    .checked_add(output.stderr.len())
                    .ok_or(SandboxError::OutputLimitExceeded)
                    .map_err(host_failure)?,
            )
            .map_err(host_failure)?;
        Ok(bindings::henosis::component::broker::ExecResult {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// Converts a security-boundary error into a Component Model trap.
fn host_failure(error: SandboxError) -> wasmtime::Error {
    wasmtime::Error::msg(error.to_string())
}

/// All failures intentionally exposed by the component-host security boundary.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// The signer key ID is empty or attempts to replace an existing trust anchor.
    #[error("invalid trust key identifier")]
    InvalidTrustKeyId,
    /// The manifest requested a world other than the exact accepted ABI.
    #[error("component world mismatch: expected {expected}, got {actual}")]
    WorldMismatch { expected: String, actual: String },
    /// The component bytes do not match the signed manifest digest.
    #[error("component hash does not match signed manifest")]
    ComponentHashMismatch,
    /// Component bytes exceeded the signed pre-compilation byte ceiling.
    #[error("component byte size exceeds signed limit")]
    ComponentSizeExceeded,
    /// The manifest key ID is not trusted by this host.
    #[error("manifest signing key is not trusted")]
    UnknownSigningKey,
    /// The detached signature has an invalid encoding or does not verify.
    #[error("manifest signature is invalid")]
    InvalidSignature,
    /// Manifest identifiers or digest fields are oversized or noncanonical.
    #[error("manifest fields are invalid")]
    InvalidManifestFields,
    /// Canonical manifest encoding failed before signature verification.
    #[error("manifest encoding failed: {0}")]
    ManifestEncoding(serde_json::Error),
    /// A signed resource value is zero, excessive, or internally inconsistent.
    #[error("invalid resource limits")]
    InvalidResourceLimits,
    /// A caller attempted a capability absent from the signed manifest.
    #[error("capability is not granted")]
    CapabilityDenied,
    /// A granted capability has no embedding-side mediator implementation.
    #[error("capability mediator is unavailable")]
    CapabilityUnavailable,
    /// An HTTP request violates the strict mediated-network policy.
    #[error("HTTP request is blocked by SSRF policy")]
    SsrfBlocked,
    /// Mediated output exceeded its signed byte ceiling.
    #[error("component output limit exceeded")]
    OutputLimitExceeded,
    /// Component work exceeded its signed wall-clock deadline.
    #[error("component deadline exceeded")]
    DeadlineExceeded,
    /// Every bounded mediator worker or queue slot is already occupied.
    #[error("mediator capacity is exhausted")]
    MediatorCapacityExhausted,
    /// Every fixed component compilation or invocation slot is already occupied.
    #[error("component invocation capacity is exhausted")]
    InvocationCapacityExhausted,
    /// The fixed mediator worker pool could not start or is unavailable.
    #[error("mediator executor is unavailable")]
    MediatorUnavailable,
    /// The fixed component runtime or epoch clock could not start.
    #[error("component runtime is unavailable: {0}")]
    RuntimeUnavailable(String),
    /// Wasmtime rejected configuration, compilation, fuel assignment, or instantiation.
    #[error("wasmtime sandbox error: {0}")]
    Wasmtime(wasmtime::Error),
}

/// Enforces conservative ceilings before values reach Wasmtime.
fn validate_limits(limits: &ResourceLimits) -> Result<(), SandboxError> {
    const MAX_MEMORY_BYTES: usize = 512 * 1024 * 1024;
    const MAX_TIMEOUT_MS: u64 = 60_000;
    if limits.component_bytes == 0
        || limits.component_bytes > MAX_COMPONENT_BYTES
        || limits.fuel == 0
        || limits.memory_bytes == 0
        || limits.memory_bytes > MAX_MEMORY_BYTES
        || limits.output_bytes == 0
        || limits.output_bytes > MAX_OUTPUT_BYTES
        || limits.timeout_ms == 0
        || limits.timeout_ms > MAX_TIMEOUT_MS
    {
        return Err(SandboxError::InvalidResourceLimits);
    }
    Ok(())
}

/// Accepts only bounded canonical identifiers before manifest encoding or verification.
fn validate_manifest_fields(manifest: &SignedManifest) -> Result<(), SandboxError> {
    let digest_is_lower_hex = manifest.component_sha256.len() == SHA256_HEX_BYTES
        && manifest
            .component_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !valid_key_id(&manifest.key_id)
        || manifest.world.len() > HOST_WORLD_ID.len()
        || !manifest.world.is_ascii()
        || !digest_is_lower_hex
    {
        return Err(SandboxError::InvalidManifestFields);
    }
    Ok(())
}

/// Accepts stable ASCII signer identifiers without whitespace or path syntax.
fn valid_key_id(key_id: &str) -> bool {
    !key_id.is_empty()
        && key_id.len() <= MAX_KEY_ID_BYTES
        && key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Rejects one mediator byte result that violated its pre-dispatch allocation contract.
fn validate_response_bytes(actual: usize, ceiling: usize) -> Result<(), SandboxError> {
    if actual > ceiling {
        Err(SandboxError::OutputLimitExceeded)
    } else {
        Ok(())
    }
}

/// Rejects an address collection that exceeded the mediator's resolution byte contract.
fn validate_resolution_bytes(addresses: &[IpAddr], ceiling: usize) -> Result<(), SandboxError> {
    let bytes = addresses
        .len()
        .checked_mul(16)
        .ok_or(SandboxError::OutputLimitExceeded)?;
    validate_response_bytes(bytes, ceiling)
}

/// Rejects combined broker output that exceeded its pre-dispatch allocation contract.
fn validate_exec_output(output: &BrokerExecOutput, ceiling: usize) -> Result<(), SandboxError> {
    let bytes = output
        .stdout
        .len()
        .checked_add(output.stderr.len())
        .ok_or(SandboxError::OutputLimitExceeded)?;
    validate_response_bytes(bytes, ceiling)
}

/// Requires exactly one capability from the signed manifest before invoking a mediator.
fn require_capability(
    granted: &BTreeSet<Capability>,
    required: Capability,
) -> Result<(), SandboxError> {
    if granted.contains(&required) {
        Ok(())
    } else {
        Err(SandboxError::CapabilityDenied)
    }
}

/// Rejects non-HTTPS, malformed, or private-address HTTP requests before connection.
fn validate_http_request(request: &HttpRequest, resolved: &[IpAddr]) -> Result<(), SandboxError> {
    validate_http_request_shape(request)?;
    if resolved.is_empty() || resolved.iter().any(is_restricted_address) {
        return Err(SandboxError::SsrfBlocked);
    }
    Ok(())
}

/// Rejects ambiguous or oversized HTTP metadata before calling a resolver or transport.
fn validate_http_request_shape(request: &HttpRequest) -> Result<(), SandboxError> {
    let valid_path = !request.path_and_query.is_empty()
        && request.path_and_query.len() <= MAX_HTTP_PATH_BYTES
        && request.path_and_query.starts_with('/')
        && !request.path_and_query.starts_with("//")
        && request.path_and_query.is_ascii()
        && !request
            .path_and_query
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b'\\' || byte == b'#');
    if request.scheme != "https"
        || !valid_http_host(&request.host)
        || !valid_path
        || request.body.len() > MAX_HTTP_REQUEST_BODY_BYTES
    {
        return Err(SandboxError::SsrfBlocked);
    }
    Ok(())
}

/// Accepts an IP literal or a fully qualified conservative ASCII DNS hostname.
fn valid_http_host(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    if host.is_empty()
        || host.len() > MAX_HTTP_HOST_BYTES
        || !host.is_ascii()
        || !host.contains('.')
        || host.starts_with('.')
        || host.ends_with('.')
    {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

/// Rejects every address that is not explicitly classified as globally routable.
fn is_restricted_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => !is_globally_routable_ipv4(*value),
        IpAddr::V6(value) => {
            if let Some(mapped) = value.to_ipv4_mapped() {
                !is_globally_routable_ipv4(mapped)
            } else {
                !is_globally_routable_ipv6(*value)
            }
        }
    }
}

/// Allows IPv4 unicast only after excluding every fixed special-use allocation.
fn is_globally_routable_ipv4(address: Ipv4Addr) -> bool {
    let first_octet = address.octets()[0];
    (1..=223).contains(&first_octet)
        && !ipv4_in_prefix(address, Ipv4Addr::new(10, 0, 0, 0), 8)
        && !ipv4_in_prefix(address, Ipv4Addr::new(100, 64, 0, 0), 10)
        && !ipv4_in_prefix(address, Ipv4Addr::new(127, 0, 0, 0), 8)
        && !ipv4_in_prefix(address, Ipv4Addr::new(169, 254, 0, 0), 16)
        && !ipv4_in_prefix(address, Ipv4Addr::new(172, 16, 0, 0), 12)
        && !ipv4_in_prefix(address, Ipv4Addr::new(192, 0, 0, 0), 24)
        && !ipv4_in_prefix(address, Ipv4Addr::new(192, 0, 2, 0), 24)
        && !ipv4_in_prefix(address, Ipv4Addr::new(192, 31, 196, 0), 24)
        && !ipv4_in_prefix(address, Ipv4Addr::new(192, 52, 193, 0), 24)
        && !ipv4_in_prefix(address, Ipv4Addr::new(192, 88, 99, 0), 24)
        && !ipv4_in_prefix(address, Ipv4Addr::new(192, 168, 0, 0), 16)
        && !ipv4_in_prefix(address, Ipv4Addr::new(192, 175, 48, 0), 24)
        && !ipv4_in_prefix(address, Ipv4Addr::new(198, 18, 0, 0), 15)
        && !ipv4_in_prefix(address, Ipv4Addr::new(198, 51, 100, 0), 24)
        && !ipv4_in_prefix(address, Ipv4Addr::new(203, 0, 113, 0), 24)
}

/// Allows IPv6 only inside global unicast space and outside fixed special allocations.
fn is_globally_routable_ipv6(address: Ipv6Addr) -> bool {
    ipv6_in_prefix(address, Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3)
        && !ipv6_in_prefix(address, Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23)
        && !ipv6_in_prefix(address, Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0), 32)
        && !ipv6_in_prefix(address, Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16)
        && !ipv6_in_prefix(address, Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20)
        && !ipv6_in_prefix(
            address,
            Ipv6Addr::new(0x2620, 0x004f, 0x8000, 0, 0, 0, 0, 0),
            48,
        )
}

/// Tests whether an IPv4 address belongs to one exact CIDR prefix.
fn ipv4_in_prefix(address: Ipv4Addr, network: Ipv4Addr, prefix_bits: u32) -> bool {
    let mask = u32::MAX << (u32::BITS - prefix_bits);
    u32::from(address) & mask == u32::from(network) & mask
}

/// Tests whether an IPv6 address belongs to one exact CIDR prefix.
fn ipv6_in_prefix(address: Ipv6Addr, network: Ipv6Addr, prefix_bits: u32) -> bool {
    let mask = u128::MAX << (u128::BITS - prefix_bits);
    u128::from(address) & mask == u128::from(network) & mask
}

/// Exercises signed-boundary, capability, SSRF, and resource-limit failures.
#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::str::FromStr;
    use std::sync::Mutex;
    use wasmtime::ResourceLimiter;

    /// Records the addresses handed from resolution to the connection mediator.
    struct RecordingHttp {
        /// Captured addresses that the mediator must pin for its connection.
        received_addresses: Mutex<Vec<IpAddr>>,
        /// Captured contracts that the host supplies before mediator allocation.
        received_limits: Mutex<Vec<MediationLimits>>,
    }

    /// Supplies a deterministic public resolver and captures the pin passed to transport.
    impl MediatedHttp for RecordingHttp {
        /// Returns one public address used by the request-pinning test.
        fn resolve(
            &self,
            _host: &str,
            limits: MediationLimits,
        ) -> Result<Vec<IpAddr>, SandboxError> {
            self.received_limits.lock().unwrap().push(limits);
            Ok(vec![IpAddr::from_str("8.8.8.8").unwrap()])
        }

        /// Captures the host-pinned addresses instead of re-resolving the hostname.
        fn send_pinned(
            &self,
            request: PinnedHttpRequest,
            limits: MediationLimits,
        ) -> Result<HttpResponse, SandboxError> {
            *self.received_addresses.lock().unwrap() = request.addresses().to_vec();
            self.received_limits.lock().unwrap().push(limits);
            Ok(HttpResponse {
                status: 200,
                body: b"ok".to_vec(),
            })
        }
    }

    /// Resolves a malicious redirect target to prove redirect pinning repeats SSRF validation.
    struct RedirectResolver;

    /// Supplies separate public and private resolution paths without performing network I/O.
    impl MediatedHttp for RedirectResolver {
        /// Resolves only the redirect target to a private address for this policy test.
        fn resolve(
            &self,
            host: &str,
            _limits: MediationLimits,
        ) -> Result<Vec<IpAddr>, SandboxError> {
            let address = if host == "internal.example" {
                "10.0.0.1"
            } else {
                "8.8.8.8"
            };
            Ok(vec![IpAddr::from_str(address).unwrap()])
        }

        /// Fails if the redirect test reaches transport instead of stopping at validation.
        fn send_pinned(
            &self,
            _request: PinnedHttpRequest,
            _limits: MediationLimits,
        ) -> Result<HttpResponse, SandboxError> {
            unreachable!("redirect validation must fail before transport")
        }
    }

    /// Implements only non-exportable broker operations for capability-boundary tests.
    struct OpaqueBroker;

    /// Keeps every broker response non-secret, including the derived value.
    impl BrokerMediator for OpaqueBroker {
        /// Returns a deterministic signature rather than secret material.
        fn sign(
            &self,
            _opaque_handle: &str,
            _payload: &[u8],
            _limits: MediationLimits,
        ) -> Result<Vec<u8>, SandboxError> {
            Ok(vec![1, 2, 3])
        }

        /// Returns a verification bit rather than secret material.
        fn verify(
            &self,
            _opaque_handle: &str,
            _payload: &[u8],
            _signature: &[u8],
            _limits: MediationLimits,
        ) -> Result<bool, SandboxError> {
            Ok(true)
        }

        /// Returns an opaque derived-handle identifier rather than derived bytes.
        fn derive_handle(
            &self,
            _opaque_handle: &str,
            _purpose: &str,
            _limits: MediationLimits,
        ) -> Result<String, SandboxError> {
            Ok("derived-handle-1".to_string())
        }

        /// Returns sanitized command output without exposing a selected secret.
        fn exec(
            &self,
            _opaque_handle: &str,
            _argv: &[String],
            _input: &[u8],
            _limits: MediationLimits,
        ) -> Result<BrokerExecOutput, SandboxError> {
            Ok(BrokerExecOutput {
                status: 0,
                stdout: b"ok".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    /// Returns one byte beyond every supplied limit to exercise host-side validation.
    struct OversizedBroker;

    /// Violates allocation contracts so the host must reject every oversized response.
    impl BrokerMediator for OversizedBroker {
        /// Returns a signature one byte larger than its explicit result ceiling.
        fn sign(
            &self,
            _opaque_handle: &str,
            _payload: &[u8],
            limits: MediationLimits,
        ) -> Result<Vec<u8>, SandboxError> {
            Ok(vec![1; limits.max_response_bytes + 1])
        }

        /// Returns a deterministic verification result.
        fn verify(
            &self,
            _opaque_handle: &str,
            _payload: &[u8],
            _signature: &[u8],
            _limits: MediationLimits,
        ) -> Result<bool, SandboxError> {
            Ok(true)
        }

        /// Returns a deterministic oversized opaque handle.
        fn derive_handle(
            &self,
            _opaque_handle: &str,
            _purpose: &str,
            limits: MediationLimits,
        ) -> Result<String, SandboxError> {
            Ok("x".repeat(limits.max_response_bytes + 1))
        }

        /// Returns deterministic oversized sanitized output.
        fn exec(
            &self,
            _opaque_handle: &str,
            _argv: &[String],
            _input: &[u8],
            limits: MediationLimits,
        ) -> Result<BrokerExecOutput, SandboxError> {
            Ok(BrokerExecOutput {
                status: 0,
                stdout: vec![1; limits.max_response_bytes + 1],
                stderr: Vec::new(),
            })
        }
    }

    /// Builds a trusted manifest and its signing key for boundary tests.
    fn signed_fixture(component: &[u8]) -> (TrustStore, SignedManifest, SigningKey) {
        let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
        let mut trust = TrustStore::new();
        trust
            .add_key("extension-test", signing_key.verifying_key())
            .unwrap();
        let mut manifest = SignedManifest {
            key_id: "extension-test".to_string(),
            world: HOST_WORLD_ID.to_string(),
            component_sha256: hex::encode(Sha256::digest(component)),
            capabilities: BTreeSet::new(),
            limits: ResourceLimits::default(),
            signature: String::new(),
        };
        manifest.signature = STANDARD_NO_PAD.encode(
            signing_key
                .sign(&manifest.signing_bytes().unwrap())
                .to_bytes(),
        );
        (trust, manifest, signing_key)
    }

    /// Rejects unsigned manifests even when every other field is valid.
    #[test]
    fn unsigned_manifest_is_rejected() {
        let (trust, mut manifest, _) = signed_fixture(b"component");
        manifest.signature.clear();
        assert!(matches!(
            trust.verify(&manifest, b"component"),
            Err(SandboxError::InvalidSignature)
        ));
    }

    /// Rejects oversized or noncanonical manifest text before signature decoding or hashing.
    #[test]
    fn malformed_manifest_fields_are_rejected_early() {
        let (trust, mut manifest, _) = signed_fixture(b"component");
        manifest.signature = "A".repeat(ED25519_SIGNATURE_B64_BYTES + 1);
        assert!(matches!(
            trust.verify(&manifest, b"component"),
            Err(SandboxError::InvalidSignature)
        ));

        let (_, mut manifest, _) = signed_fixture(b"component");
        manifest.key_id = "invalid/key".to_string();
        assert!(matches!(
            manifest.signing_bytes(),
            Err(SandboxError::InvalidManifestFields)
        ));

        let (_, mut manifest, _) = signed_fixture(b"component");
        manifest.component_sha256.make_ascii_uppercase();
        assert!(matches!(
            manifest.signing_bytes(),
            Err(SandboxError::InvalidManifestFields)
        ));
    }

    /// Preserves an existing trust anchor when a duplicate key identifier is rejected.
    #[test]
    fn duplicate_key_does_not_replace_the_trusted_anchor() {
        let original = SigningKey::from_bytes(&[1_u8; 32]);
        let replacement = SigningKey::from_bytes(&[2_u8; 32]);
        let mut trust = TrustStore::new();
        trust
            .add_key("extension-test", original.verifying_key())
            .unwrap();
        assert!(matches!(
            trust.add_key("extension-test", replacement.verifying_key()),
            Err(SandboxError::InvalidTrustKeyId)
        ));
        let mut manifest = SignedManifest {
            key_id: "extension-test".to_string(),
            world: HOST_WORLD_ID.to_string(),
            component_sha256: hex::encode(Sha256::digest(b"component")),
            capabilities: BTreeSet::new(),
            limits: ResourceLimits::default(),
            signature: String::new(),
        };
        manifest.signature =
            STANDARD_NO_PAD.encode(original.sign(&manifest.signing_bytes().unwrap()).to_bytes());
        assert!(trust.verify(&manifest, b"component").is_ok());
    }

    /// Rejects component bytes that differ from the signed hash.
    #[test]
    fn mismatched_component_hash_is_rejected() {
        let (trust, manifest, _) = signed_fixture(b"component");
        assert!(matches!(
            trust.verify(&manifest, b"other"),
            Err(SandboxError::ComponentHashMismatch)
        ));
    }

    /// Rejects signed component bytes above their ceiling before digest comparison.
    #[test]
    fn oversized_component_is_rejected_before_hashing() {
        let component = b"component";
        let (trust, mut manifest, signing_key) = signed_fixture(component);
        manifest.limits.component_bytes = component.len() - 1;
        manifest.signature = STANDARD_NO_PAD.encode(
            signing_key
                .sign(&manifest.signing_bytes().unwrap())
                .to_bytes(),
        );

        assert!(matches!(
            trust.verify(&manifest, component),
            Err(SandboxError::ComponentSizeExceeded)
        ));
    }

    /// Rejects manifests modified after the detached signature was issued.
    #[test]
    fn manifest_tamper_is_rejected() {
        let (trust, mut manifest, _) = signed_fixture(b"component");
        manifest.capabilities.insert(Capability::MediatedHttp);
        assert!(matches!(
            trust.verify(&manifest, b"component"),
            Err(SandboxError::InvalidSignature)
        ));
    }

    /// Rejects any capability use absent from the signed manifest.
    #[test]
    fn forbidden_capability_is_rejected() {
        assert!(matches!(
            require_capability(&BTreeSet::new(), Capability::Broker),
            Err(SandboxError::CapabilityDenied)
        ));
    }

    /// Rejects private and loopback destinations before a mediator can connect.
    #[test]
    fn private_http_destination_is_rejected() {
        let request = HttpRequest {
            scheme: "https".into(),
            host: "service.example".into(),
            path_and_query: "/".into(),
            body: Vec::new(),
        };
        let addresses = [IpAddr::from_str("127.0.0.1").unwrap()];
        assert!(matches!(
            validate_http_request(&request, &addresses),
            Err(SandboxError::SsrfBlocked)
        ));
    }

    /// Rejects IPv4-mapped IPv6 loopback values before a mediator can connect.
    #[test]
    fn mapped_ipv6_private_destination_is_rejected() {
        let request = HttpRequest {
            scheme: "https".into(),
            host: "service.example".into(),
            path_and_query: "/".into(),
            body: Vec::new(),
        };
        let addresses = [IpAddr::from_str("::ffff:127.0.0.1").unwrap()];
        assert!(matches!(
            validate_http_request(&request, &addresses),
            Err(SandboxError::SsrfBlocked)
        ));
    }

    /// Rejects representative IPv4 special-use ranges omitted by common private predicates.
    #[test]
    fn ipv4_special_use_destinations_are_rejected() {
        for address in [
            "0.1.2.3",
            "100.64.0.1",
            "192.0.0.9",
            "192.0.2.1",
            "192.31.196.1",
            "192.52.193.1",
            "192.88.99.1",
            "192.175.48.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(
                is_restricted_address(&IpAddr::from_str(address).unwrap()),
                "{address} must be blocked"
            );
        }
        assert!(!is_restricted_address(
            &IpAddr::from_str("8.8.8.8").unwrap()
        ));
    }

    /// Rejects IPv6 local, transition, benchmarking, documentation, and reserved allocations.
    #[test]
    fn ipv6_special_use_destinations_are_rejected() {
        for address in [
            "::",
            "::1",
            "::10.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::8.8.8.8",
            "100::1",
            "2001::1",
            "2001:2::1",
            "2001:db8::1",
            "2002:0808:0808::1",
            "2620:4f:8000::1",
            "3fff::1",
            "5f00::1",
            "fc00::1",
            "fe80::1",
            "ff00::1",
        ] {
            assert!(
                is_restricted_address(&IpAddr::from_str(address).unwrap()),
                "{address} must be blocked"
            );
        }
        assert!(!is_restricted_address(
            &IpAddr::from_str("2606:4700:4700::1111").unwrap()
        ));
    }

    /// Passes the resolver's validated addresses into transport to prevent DNS re-resolution.
    #[test]
    fn mediated_http_pins_validated_addresses_for_transport() {
        let grants = BTreeSet::from([Capability::MediatedHttp]);
        let mediator = Arc::new(RecordingHttp {
            received_addresses: Mutex::new(Vec::new()),
            received_limits: Mutex::new(Vec::new()),
        });
        let capabilities = MediatedCapabilities {
            granted: &grants,
            http: Some(mediator.clone()),
            broker: None,
            deadline: Instant::now() + Duration::from_secs(1),
            max_response_bytes: 1024,
        };
        capabilities
            .http_request(HttpRequest {
                scheme: "https".into(),
                host: "service.example".into(),
                path_and_query: "/".into(),
                body: Vec::new(),
            })
            .unwrap();
        assert_eq!(
            *mediator.received_addresses.lock().unwrap(),
            vec![IpAddr::from_str("8.8.8.8").unwrap()]
        );
        let received_limits = mediator.received_limits.lock().unwrap();
        assert_eq!(received_limits.len(), 2);
        assert_eq!(received_limits[0].max_response_bytes, MAX_RESOLUTION_BYTES);
        assert_eq!(received_limits[1].max_response_bytes, 1024);
        assert_eq!(received_limits[0].deadline, capabilities.deadline);
        assert_eq!(received_limits[1].deadline, capabilities.deadline);
    }

    /// Rejects ambiguous authority and request-target syntax before invoking the resolver.
    #[test]
    fn malformed_http_metadata_is_rejected_before_resolution() {
        let grants = BTreeSet::from([Capability::MediatedHttp]);
        let mediator = Arc::new(RecordingHttp {
            received_addresses: Mutex::new(Vec::new()),
            received_limits: Mutex::new(Vec::new()),
        });
        let capabilities = MediatedCapabilities {
            granted: &grants,
            http: Some(mediator.clone()),
            broker: None,
            deadline: Instant::now() + Duration::from_secs(1),
            max_response_bytes: 1024,
        };

        for (host, path) in [
            ("service.example@evil.example", "/"),
            ("service.example", "//evil.example/path"),
            ("service", "/path"),
            ("service.example", "/path#fragment"),
        ] {
            assert!(matches!(
                capabilities.http_request(HttpRequest {
                    scheme: "https".into(),
                    host: host.into(),
                    path_and_query: path.into(),
                    body: Vec::new(),
                }),
                Err(SandboxError::SsrfBlocked)
            ));
        }
        assert!(mediator.received_limits.lock().unwrap().is_empty());
    }

    /// Blocks a redirect that resolves to private space before any transport can follow it.
    #[test]
    fn redirect_target_is_pinned_and_revalidated() {
        let grants = BTreeSet::from([Capability::MediatedHttp]);
        let mediator = RedirectResolver;
        let capabilities = MediatedCapabilities {
            granted: &grants,
            http: Some(Arc::new(mediator)),
            broker: None,
            deadline: Instant::now() + Duration::from_secs(1),
            max_response_bytes: 1024,
        };
        assert!(matches!(
            capabilities.pin_redirect(HttpRequest {
                scheme: "https".into(),
                host: "internal.example".into(),
                path_and_query: "/next".into(),
                body: Vec::new(),
            }),
            Err(SandboxError::SsrfBlocked)
        ));
    }

    /// Exposes only an opaque derived handle when the broker capability is granted.
    #[test]
    fn broker_derive_never_returns_raw_secret_bytes() {
        let grants = BTreeSet::from([Capability::Broker]);
        let broker = OpaqueBroker;
        let capabilities = MediatedCapabilities {
            granted: &grants,
            http: None,
            broker: Some(Arc::new(broker)),
            deadline: Instant::now() + Duration::from_secs(1),
            max_response_bytes: 1024,
        };
        assert_eq!(
            capabilities
                .derive_handle("source-handle", "request-signing")
                .unwrap(),
            "derived-handle-1"
        );
    }

    /// Rejects malformed resource limits before an invocation can allocate work.
    #[test]
    fn invalid_resource_limits_are_rejected() {
        let limits = ResourceLimits {
            fuel: 0,
            ..ResourceLimits::default()
        };
        assert!(matches!(
            InvocationBudget::new(limits),
            Err(SandboxError::InvalidResourceLimits)
        ));
    }

    /// Rejects mediated output that exceeds the signed byte ceiling.
    #[test]
    fn output_resource_failure_is_rejected() {
        let mut budget = InvocationBudget::new(ResourceLimits {
            output_bytes: 2,
            ..ResourceLimits::default()
        })
        .unwrap();
        assert!(matches!(
            budget.charge_output(3),
            Err(SandboxError::OutputLimitExceeded)
        ));
    }

    /// Converts each timeout to an independent relative store deadline.
    #[test]
    fn epoch_deadlines_are_relative_and_rounded_up() {
        assert_eq!(epoch_deadline_ticks(1), 1);
        assert_eq!(epoch_deadline_ticks(EPOCH_TICK_MS), 1);
        assert_eq!(epoch_deadline_ticks(EPOCH_TICK_MS + 1), 2);
        assert_eq!(epoch_deadline_ticks(60_000), 6_000);
    }

    /// Bounds every store allocation count and the single table's element capacity.
    #[test]
    fn component_store_counts_and_table_elements_are_bounded() {
        let mut store_limits = component_store_limits(ResourceLimits::default().memory_bytes);

        assert_eq!(store_limits.instances(), MAX_STORE_INSTANCES);
        assert_eq!(store_limits.memories(), MAX_STORE_MEMORIES);
        assert_eq!(store_limits.tables(), MAX_STORE_TABLES);
        assert!(store_limits
            .table_growing(0, MAX_STORE_TABLE_ELEMENTS, None)
            .unwrap());
        assert!(!store_limits
            .table_growing(0, MAX_STORE_TABLE_ELEMENTS + 1, None)
            .unwrap());
    }

    /// Allows one memory through its signed ceiling and rejects growth beyond it.
    #[test]
    fn component_store_supports_one_bounded_memory() {
        const WASM_PAGE_BYTES: usize = 64 * 1024;
        let memory_bytes = ResourceLimits::default().memory_bytes;
        let mut store_limits = component_store_limits(memory_bytes);

        assert_eq!(store_limits.memories(), 1);
        assert!(store_limits.memory_growing(0, memory_bytes, None).unwrap());
        assert!(!store_limits
            .memory_growing(0, memory_bytes + WASM_PAGE_BYTES, None)
            .unwrap());
    }

    /// Releases a fixed invocation slot after every permit is dropped.
    #[test]
    fn invocation_capacity_is_bounded_and_released() {
        let gate = InvocationGate::new(1);
        let permit = gate.try_acquire().unwrap();
        assert!(matches!(
            gate.try_acquire(),
            Err(SandboxError::InvocationCapacityExhausted)
        ));
        drop(permit);
        assert!(gate.try_acquire().is_ok());

        let panic_result = catch_unwind(AssertUnwindSafe(|| {
            let _permit = gate.try_acquire().unwrap();
            panic!("exercise permit unwind");
        }));
        assert!(panic_result.is_err());
        assert!(gate.try_acquire().is_ok());
    }

    /// Rejects a trusted mediator result that exceeds its pre-dispatch byte ceiling.
    #[test]
    fn oversized_mediator_output_is_rejected() {
        let grants = BTreeSet::from([Capability::Broker]);
        let capabilities = MediatedCapabilities {
            granted: &grants,
            http: None,
            broker: Some(Arc::new(OversizedBroker)),
            deadline: Instant::now() + Duration::from_secs(1),
            max_response_bytes: 8,
        };
        let result = capabilities.sign("opaque-handle", b"payload");

        assert!(matches!(result, Err(SandboxError::OutputLimitExceeded)));
    }

    /// Preserves unaffected workers after one trusted operation violates its deadline.
    #[test]
    fn deadline_ignoring_mediator_preserves_unaffected_workers() {
        let executor = MediatorExecutor::new().unwrap();
        let deadline = Instant::now() + Duration::from_millis(10);
        let result = executor.execute(deadline, || {
            thread::sleep(Duration::from_millis(100));
            Ok(())
        });
        assert!(matches!(result, Err(SandboxError::DeadlineExceeded)));
        assert!(executor
            .execute(Instant::now() + Duration::from_secs(1), || Ok(()))
            .is_ok());
    }

    /// Retains slots for stalled calls and recovers them only after those calls exit.
    #[test]
    fn stalled_mediator_calls_exhaust_then_recover_capacity() {
        let executor = Arc::new(MediatorExecutor::new().unwrap());
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut callers = Vec::new();
        for _ in 0..MEDIATOR_WORKER_COUNT {
            let executor = executor.clone();
            let started = started.clone();
            let release = release.clone();
            callers.push(thread::spawn(move || {
                executor.execute(Instant::now() + Duration::from_millis(250), move || {
                    started.fetch_add(1, Ordering::AcqRel);
                    while !release.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Ok(())
                })
            }));
        }

        let start_deadline = Instant::now() + Duration::from_secs(1);
        while started.load(Ordering::Acquire) != MEDIATOR_WORKER_COUNT
            && Instant::now() < start_deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(started.load(Ordering::Acquire), MEDIATOR_WORKER_COUNT);
        for caller in callers {
            assert!(matches!(
                caller.join().unwrap(),
                Err(SandboxError::DeadlineExceeded)
            ));
        }
        assert!(matches!(
            executor.execute(Instant::now() + Duration::from_secs(1), || Ok(())),
            Err(SandboxError::MediatorCapacityExhausted)
        ));

        release.store(true, Ordering::Release);
        let recovery_deadline = Instant::now() + Duration::from_secs(1);
        while executor.capacity.active.load(Ordering::Acquire) != 0
            && Instant::now() < recovery_deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(executor.capacity.active.load(Ordering::Acquire), 0);
        assert!(executor
            .execute(Instant::now() + Duration::from_secs(1), || Ok(()))
            .is_ok());
    }
}
