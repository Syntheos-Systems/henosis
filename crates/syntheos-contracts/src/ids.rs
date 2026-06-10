//! Strongly-typed UUID newtypes. A `PrincipalId` can never be passed where a
//! `TenantId` is expected; the type system enforces the distinction.

use std::fmt;
use std::str::FromStr;

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// An error returned when an identifier is not a valid syntheos UUID v8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdError {
    /// Human-readable reason the identifier was rejected.
    message: String,
}

/// Display `IdError` as its rejection reason.
impl fmt::Display for IdError {
    /// Write the identifier validation error message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Expose `IdError` through the standard error trait.
impl std::error::Error for IdError {}

/// Convert a UUID parser error into an identifier validation error.
impl From<uuid::Error> for IdError {
    /// Preserve UUID parser failures as identifier validation messages.
    fn from(err: uuid::Error) -> Self {
        Self {
            message: err.to_string(),
        }
    }
}

/// Validate that a UUID carries the syntheos v8 version nibble.
fn validate_v8(uuid: Uuid) -> Result<Uuid, IdError> {
    if uuid.get_version_num() == 8 {
        Ok(uuid)
    } else {
        Err(IdError {
            message: format!("expected UUID v8, got UUID v{}", uuid.get_version_num()),
        })
    }
}

/// Generate a fresh random UUID v8.
///
/// v8 (custom) is used rather than v4 to match the syntheos wire convention
/// already used by `syntheos-memory-gateway`. The 16 bytes are random;
/// `Uuid::new_v8` stamps the version (8) and RFC 4122 variant bits.
fn new_v8() -> Uuid {
    let buf: [u8; 16] = rand::random();
    Uuid::new_v8(buf)
}

/// Define a transparent newtype over `Uuid` with `new`/`Display`/`FromStr`.
macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        /// A strongly-typed syntheos UUID v8 identifier.
        pub struct $name(Uuid);

        /// Constructors and accessors for the identifier newtype.
        impl $name {
            /// Generate a new random (v8) identifier.
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(new_v8())
            }

            /// Build this identifier from a UUID after enforcing the v8 invariant.
            pub fn from_uuid(uuid: Uuid) -> Result<Self, IdError> {
                validate_v8(uuid).map(Self)
            }

            /// Borrow the underlying UUID value.
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        /// Serialize the identifier as the canonical hyphenated UUID string.
        impl Serialize for $name {
            /// Emit the same UUID string representation as the wrapped `Uuid`.
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                self.0.serialize(serializer)
            }
        }

        /// Deserialize and validate the identifier as a syntheos UUID v8.
        impl<'de> Deserialize<'de> for $name {
            /// Parse a UUID string then reject non-v8 identifiers.
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let uuid = Uuid::deserialize(deserializer)?;
                Self::from_uuid(uuid).map_err(D::Error::custom)
            }
        }

        /// Display the identifier as a canonical hyphenated UUID string.
        impl fmt::Display for $name {
            /// Write the canonical hyphenated UUID string.
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        /// Parse the identifier from a canonical UUID string and enforce v8.
        impl FromStr for $name {
            /// Identifier parsing errors include UUID syntax and v8 validation failures.
            type Err = IdError;

            /// Parse a UUID string then reject non-v8 identifiers.
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::from_uuid(Uuid::from_str(s)?)
            }
        }
    };
}

id_newtype!(
    /// Identifies a [`crate::principal::Principal`] (human, agent, service, or integration).
    PrincipalId
);
id_newtype!(
    /// Identifies a [`crate::tenant::Tenant`] (billing/isolation subject).
    TenantId
);
id_newtype!(
    /// Identifies a Chiasm task.
    TaskId
);
id_newtype!(
    /// Identifies an Axon event.
    EventId
);

/// Tests for ID generation, parsing, and serde validation.
#[cfg(test)]
mod tests {
    use super::*;

    /// `new` stamps UUID v8 identifiers.
    #[test]
    fn new_generates_v8() {
        let id = PrincipalId::new();
        // UUID v8 has version nibble 8.
        assert_eq!(id.as_uuid().get_version_num(), 8);
    }

    /// Fresh task identifiers do not collide in ordinary use.
    #[test]
    fn new_ids_are_unique() {
        assert_ne!(TaskId::new(), TaskId::new());
    }

    /// Display and FromStr preserve the canonical string wire shape.
    #[test]
    fn display_fromstr_roundtrip() {
        let id = EventId::new();
        let s = id.to_string();
        let parsed = EventId::from_str(&s).expect("valid uuid string");
        assert_eq!(id, parsed);
    }

    /// Serde keeps the bare UUID string wire shape while validating v8.
    #[test]
    fn serde_is_uuid_string() {
        let id = TenantId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        // Wire form: a bare quoted UUID string, not an object.
        assert_eq!(json, format!("\"{}\"", id.as_uuid()));
        let back: TenantId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    /// Parsing rejects UUIDs that are syntactically valid but not v8.
    #[test]
    fn fromstr_rejects_non_v8_uuid() {
        let v4 = "550e8400-e29b-41d4-a716-446655440000";
        let err = PrincipalId::from_str(v4).expect_err("v4 must be rejected");
        assert!(err.to_string().contains("expected UUID v8"));
    }

    /// Deserialization rejects nil UUIDs and other non-v8 values.
    #[test]
    fn serde_rejects_non_v8_uuid() {
        let json = format!("\"{}\"", Uuid::nil());
        let err = serde_json::from_str::<TenantId>(&json).expect_err("nil must be rejected");
        assert!(err.to_string().contains("expected UUID v8"));
    }
}
