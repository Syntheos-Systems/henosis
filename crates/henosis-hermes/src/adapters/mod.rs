//! Tool adapter registry and all per-provider adapter modules.
//!
//! The `register_all` function populates a `ToolRegistry` with every
//! adapter in this tree. New adapters must be added here.

// Test-only support; declared first so its `test_adapter!` macro is in scope
// for the adapter modules below.
#[cfg(test)]
#[macro_use]
mod test_support;

/// Shared HTTP retry helpers, client builder, and phylaxd error mapping.
pub mod common;
/// Google Calendar adapters (list_events, create_event, update_event, delete_event).
pub mod gcal;
/// Google Drive adapters (list, upload, download, get_metadata).
pub mod gdrive;
/// GitHub REST API adapters (issues, PRs, search, webhooks).
pub mod github;
/// Gmail adapters (send, read, search, list_labels).
pub mod gmail;
/// Henosis-owned side-effect-free adapters used to verify governed execution.
pub mod henosis;
/// Linear GraphQL adapters (issues, search, webhooks).
pub mod linear;
/// Notion REST API adapters (search, get_page, create_page, append_blocks).
pub mod notion;
/// Slack adapters (send_message).
pub mod slack;

use crate::registry::ToolRegistry;

/// Register every bundled adapter tool into `registry`.
pub fn register_all(registry: &mut ToolRegistry) {
    // Henosis local diagnostics
    registry.register(henosis::HenosisProbeTool);
    // Gmail
    registry.register(gmail::GmailSendTool);
    registry.register(gmail::GmailReadTool);
    registry.register(gmail::GmailSearchTool);
    registry.register(gmail::GmailListLabelsTool);
    // Google Drive
    registry.register(gdrive::GDriveListTool);
    registry.register(gdrive::GDriveUploadTool);
    registry.register(gdrive::GDriveDownloadTool);
    registry.register(gdrive::GDriveGetMetadataTool);
    // Google Calendar
    registry.register(gcal::GCalListEventsTool);
    registry.register(gcal::GCalCreateEventTool);
    registry.register(gcal::GCalUpdateEventTool);
    registry.register(gcal::GCalDeleteEventTool);
    // GitHub
    registry.register(github::GitHubCreateIssueTool);
    registry.register(github::GitHubListIssuesTool);
    registry.register(github::GitHubGetIssueTool);
    registry.register(github::GitHubCreatePrTool);
    registry.register(github::GitHubListPrsTool);
    registry.register(github::GitHubMergePrTool);
    registry.register(github::GitHubSearchCodeTool);
    registry.register(github::GitHubListReposTool);
    registry.register(github::GitHubCreateWebhookTool);
    // Slack
    registry.register(slack::SlackSendMessageTool);
    // Linear
    registry.register(linear::LinearCreateIssueTool);
    registry.register(linear::LinearListIssuesTool);
    registry.register(linear::LinearUpdateIssueTool);
    registry.register(linear::LinearSearchTool);
    registry.register(linear::LinearCreateWebhookTool);
    // Notion
    registry.register(notion::NotionSearchTool);
    registry.register(notion::NotionGetPageTool);
    registry.register(notion::NotionCreatePageTool);
    registry.register(notion::NotionAppendBlocksTool);
}
