//! Rift agent bridge library.

/// Agent JWT issuance for Rift API calls.
pub mod auth;
/// Capability oracles gating execution proposals.
pub mod capability;
/// Bridge, roster, and execution configuration.
pub mod config;
/// Discussion context assembly for executors.
pub mod context;
/// HTTP control server (approval endpoint).
pub mod control;
/// Cross-agent echo suppression (token-overlap similarity).
pub mod echo;
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
/// Kleos client seam (HTTP and in-process backends).
pub mod kleos;
/// Turn budgets, consensus signals, thread ceiling.
pub mod loop_prevention;
/// Rift REST and WebSocket clients.
pub mod rift_client;
/// Room state machine driving the conversation cascade.
pub mod room;
/// Per-agent growth file store.
pub mod growth;
/// Bridge tenant and principal identity helpers.
pub mod identity;
/// Frameshift persona allocation across the roster.
pub mod persona_alloc;
/// Message-to-persona relevance scoring.
pub mod relevance;
/// Agent roster provisioning and runtime state.
pub mod roster;
/// Turn interleaving, compose slots, and the compose floor.
pub mod turn_manager;
/// Shared bridge types (agents, messages, states).
pub mod types;
