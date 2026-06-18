//! Control command parsing for execution approvals.

use crate::execution::ProposalId;

/// A control command parsed from a room message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCommand {
    /// Approve the proposal with this id.
    Approve(ProposalId),
    /// Reject the proposal with this id.
    Reject(ProposalId),
}

/// Parse a room message into a control command, if it is one.
///
/// Recognizes `!approve <id>` and `!reject <id>` (surrounding whitespace
/// ignored). Returns `None` for anything else, including malformed ids.
pub fn parse_control_command(text: &str) -> Option<ControlCommand> {
    let trimmed = text.trim();
    let mut parts = trimmed.split_whitespace();
    let verb = parts.next()?;
    let id_str = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let id = id_str.parse::<u64>().ok().map(ProposalId)?;
    match verb {
        "!approve" => Some(ControlCommand::Approve(id)),
        "!reject" => Some(ControlCommand::Reject(id)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_control_command, ControlCommand};
    use crate::execution::ProposalId;

    /// Verifies `!approve 7` parses to an approve command.
    #[test]
    fn test_parse_approve() {
        assert_eq!(
            parse_control_command("!approve 7"),
            Some(ControlCommand::Approve(ProposalId(7)))
        );
    }

    /// Verifies `!reject 12` parses to a reject command.
    #[test]
    fn test_parse_reject() {
        assert_eq!(
            parse_control_command("  !reject 12  "),
            Some(ControlCommand::Reject(ProposalId(12)))
        );
    }

    /// Verifies non-command text returns None.
    #[test]
    fn test_parse_non_command() {
        assert_eq!(
            parse_control_command("I think we should approve this"),
            None
        );
    }

    /// Verifies a malformed id returns None rather than panicking.
    #[test]
    fn test_parse_bad_id() {
        assert_eq!(parse_control_command("!approve seven"), None);
        assert_eq!(parse_control_command("!approve"), None);
    }
}
