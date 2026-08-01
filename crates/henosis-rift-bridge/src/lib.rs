//! Rift agent bridge library.

/// Approval dispatch decoupled from room state (immediate execution).
pub mod approval_dispatch;
/// Agent JWT issuance for Rift API calls.
pub mod auth;
/// Capability oracles gating execution proposals.
pub mod capability;
/// Host execution capability discovery.
pub mod catalog;
/// Bridge, roster, and execution configuration.
pub mod config;
/// Discussion context assembly for executors.
pub mod context;
/// HTTP control server (approval endpoint).
pub mod control;
/// Governed scheduling adapter for managed-room cron jobs.
pub(crate) mod cron;
/// Cross-agent echo suppression (token-overlap and embedding tiers).
pub mod echo;
/// Optional text-embedding capability (semantic echo/loop detection).
pub mod embedding;
/// Probabilistic engagement engine.
pub mod engagement;
/// Bridge error types.
pub mod error;
/// Execution mode: proposals, approvals, sandboxes, supervision.
pub mod execution;
/// AgentExecutor trait and discussion types.
pub mod executor;
/// Executor implementations (Claude Code, Synapse).
pub mod executors;
/// Per-agent growth file store.
pub mod growth;
/// Bridge tenant and principal identity helpers.
pub mod identity;
/// Kleos client seam (HTTP and in-process backends).
pub mod kleos;
/// Turn budgets, consensus signals, thread ceiling.
pub mod loop_prevention;
/// Managed revision validation and bridge configuration materialization.
pub mod materialize;
/// Frameshift persona allocation across the roster.
pub mod persona_alloc;
/// Message-to-persona relevance scoring.
pub mod relevance;
/// Rift REST and WebSocket clients.
pub mod rift_client;
/// Room state machine driving the conversation cascade.
pub mod room;
/// Agent roster provisioning and runtime state.
pub mod roster;
/// Reusable Rift bridge and Synapse room lifecycle.
pub mod runtime;
/// Stimulus injection (reflection, task, and git signals).
pub mod stimulus;
/// Turn interleaving, compose slots, and the compose floor.
pub mod turn_manager;
/// Shared bridge types (agents, messages, states).
pub mod types;
