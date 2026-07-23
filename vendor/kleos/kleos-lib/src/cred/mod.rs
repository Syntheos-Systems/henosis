//! Credential placeholder resolution, broker access, and outbound proxy support.

pub mod bootstrap;
pub mod client;
pub mod pattern;
pub mod proxy;
pub mod types;

pub use client::PhylaxdClient;
pub use pattern::{find_secret_patterns, has_secret_patterns};
pub use types::{ProxyRequest, ProxyResponse, SecretAccessMode, SecretPattern, SecretPatternKind};

#[cfg(test)]
mod tests;
