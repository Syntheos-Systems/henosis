//! RFC3339 timestamp wrapper. The wire form is the RFC3339 string, so timestamps
//! are stable and human-readable across every service.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A UTC-anchored instant serialized as an RFC3339 string (e.g. `2026-06-02T13:40:07Z`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timestamp(#[serde(with = "time::serde::rfc3339")] pub OffsetDateTime);

impl Timestamp {
    /// Capture the current instant in UTC.
    pub fn now() -> Self {
        Self(OffsetDateTime::now_utc())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn serializes_to_rfc3339_string() {
        let ts = Timestamp(datetime!(2026-06-02 13:40:07 UTC));
        let json = serde_json::to_string(&ts).expect("serialize");
        assert_eq!(json, "\"2026-06-02T13:40:07Z\"");
    }

    #[test]
    fn rfc3339_roundtrip() {
        let ts = Timestamp(datetime!(2026-06-02 13:40:07 UTC));
        let json = serde_json::to_string(&ts).expect("serialize");
        let back: Timestamp = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ts, back);
    }

    #[test]
    fn now_is_utc() {
        let ts = Timestamp::now();
        assert_eq!(ts.0.offset(), time::UtcOffset::UTC);
    }
}
