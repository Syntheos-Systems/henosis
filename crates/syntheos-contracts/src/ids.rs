//! Strongly-typed UUID newtypes. A `PrincipalId` can never be passed where a
//! `TenantId` is expected; the type system enforces the distinction.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Generate a new random (v8) identifier.
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(new_v8())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::from_str(s)?))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_generates_v8() {
        let id = PrincipalId::new();
        // UUID v8 has version nibble 8.
        assert_eq!(id.0.get_version_num(), 8);
    }

    #[test]
    fn new_ids_are_unique() {
        assert_ne!(TaskId::new(), TaskId::new());
    }

    #[test]
    fn display_fromstr_roundtrip() {
        let id = EventId::new();
        let s = id.to_string();
        let parsed = EventId::from_str(&s).expect("valid uuid string");
        assert_eq!(id, parsed);
    }

    #[test]
    fn serde_is_transparent_string() {
        let id = TenantId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        // Transparent: serializes as a bare quoted UUID string, not an object.
        assert_eq!(json, format!("\"{}\"", id.0));
        let back: TenantId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }
}
