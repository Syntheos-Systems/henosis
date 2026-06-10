//! RFC3339 timestamp wrapper. The wire form is the RFC3339 string, so timestamps
//! are stable and human-readable across every service.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::{OffsetDateTime, UtcOffset};

/// A UTC-anchored instant serialized as an RFC3339 string (e.g. `2026-06-02T13:40:07Z`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp(OffsetDateTime);

/// Constructors and accessors for canonical UTC timestamps.
impl Timestamp {
    /// Capture the current instant in UTC.
    pub fn now() -> Self {
        Self(OffsetDateTime::now_utc())
    }

    /// Build a timestamp, normalizing the provided instant to UTC.
    pub fn from_utc(value: OffsetDateTime) -> Self {
        Self(value.to_offset(UtcOffset::UTC))
    }

    /// Borrow the normalized UTC instant.
    pub fn as_offset_date_time(&self) -> OffsetDateTime {
        self.0
    }
}

/// Serialize timestamps as canonical RFC3339 UTC strings ending in `Z`.
impl Serialize for Timestamp {
    /// Emit this timestamp as an RFC3339 UTC string.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let utc = self.0.to_offset(UtcOffset::UTC);
        time::serde::rfc3339::serialize(&utc, serializer)
    }
}

/// Deserialize RFC3339 timestamps and normalize equivalent instants to UTC.
impl<'de> Deserialize<'de> for Timestamp {
    /// Parse an RFC3339 timestamp string and normalize it to UTC.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let parsed = time::serde::rfc3339::deserialize(deserializer)?;
        Ok(Self::from_utc(parsed))
    }
}

/// Tests for timestamp UTC normalization and RFC3339 wire shape.
#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// Serialization emits a UTC RFC3339 string.
    #[test]
    fn serializes_to_rfc3339_string() {
        let ts = Timestamp::from_utc(datetime!(2026-06-02 13:40:07 UTC));
        let json = serde_json::to_string(&ts).expect("serialize");
        assert_eq!(json, "\"2026-06-02T13:40:07Z\"");
    }

    /// RFC3339 UTC values roundtrip unchanged.
    #[test]
    fn rfc3339_roundtrip() {
        let ts = Timestamp::from_utc(datetime!(2026-06-02 13:40:07 UTC));
        let json = serde_json::to_string(&ts).expect("serialize");
        let back: Timestamp = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ts, back);
    }

    /// `now` always captures a UTC instant.
    #[test]
    fn now_is_utc() {
        let ts = Timestamp::now();
        assert_eq!(ts.as_offset_date_time().offset(), time::UtcOffset::UTC);
    }

    /// Non-UTC inputs are normalized before they are stored.
    #[test]
    fn from_utc_normalizes_offset() {
        let offset = UtcOffset::from_hms(2, 0, 0).expect("valid offset");
        let ts = Timestamp::from_utc(datetime!(2026-06-02 15:40:07 +02:00).to_offset(offset));
        assert_eq!(ts.as_offset_date_time(), datetime!(2026-06-02 13:40:07 UTC));
    }

    /// Deserializing an offset timestamp reserializes to canonical UTC.
    #[test]
    fn deserialize_normalizes_to_z() {
        let ts: Timestamp =
            serde_json::from_str("\"2026-06-02T15:40:07+02:00\"").expect("deserialize");
        let json = serde_json::to_string(&ts).expect("serialize");
        assert_eq!(json, "\"2026-06-02T13:40:07Z\"");
    }
}
