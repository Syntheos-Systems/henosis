//! The billing/isolation subject. Deliberately minimal: Plutus owns roles, quota,
//! and entitlements, so nothing about those is baked into the substrate here.

use std::fmt;
use std::str::FromStr;

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

use crate::ids::TenantId;

/// An error returned when a tenant slug violates the canonical URL-safe grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantSlugError {
    /// Human-readable reason the slug was rejected.
    message: String,
}

/// Display `TenantSlugError` as its rejection reason.
impl fmt::Display for TenantSlugError {
    /// Write the tenant slug validation error message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Expose `TenantSlugError` through the standard error trait.
impl std::error::Error for TenantSlugError {}

/// A validated tenant slug for URL paths and compact display.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TenantSlug(String);

/// Constructors and accessors for tenant slugs.
impl TenantSlug {
    /// Build a slug from a string after enforcing the tenant slug grammar.
    pub fn new(value: &str) -> Result<Self, TenantSlugError> {
        validate_slug(value)?;
        Ok(Self(value.to_string()))
    }

    /// Borrow the slug as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Borrow a tenant slug as its string slice.
impl AsRef<str> for TenantSlug {
    /// Return the validated slug string.
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Display a tenant slug as its validated string.
impl fmt::Display for TenantSlug {
    /// Write the validated slug string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parse a tenant slug from a string using the canonical grammar.
impl FromStr for TenantSlug {
    /// Slug parsing errors describe the violated grammar rule.
    type Err = TenantSlugError;

    /// Parse and validate a tenant slug string.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Serialize a tenant slug as its string value.
impl Serialize for TenantSlug {
    /// Emit the validated slug string.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// Deserialize and validate a tenant slug from its string value.
impl<'de> Deserialize<'de> for TenantSlug {
    /// Parse a string slug and reject invalid values.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(&value).map_err(D::Error::custom)
    }
}

/// Enforce lowercase ASCII labels separated by single internal hyphens.
fn validate_slug(value: &str) -> Result<(), TenantSlugError> {
    if value.is_empty() {
        return Err(TenantSlugError {
            message: "tenant slug must not be empty".to_string(),
        });
    }
    if value.len() > 64 {
        return Err(TenantSlugError {
            message: "tenant slug must be at most 64 bytes".to_string(),
        });
    }
    if value.starts_with('-') || value.ends_with('-') {
        return Err(TenantSlugError {
            message: "tenant slug must not start or end with '-'".to_string(),
        });
    }
    if value.contains("--") {
        return Err(TenantSlugError {
            message: "tenant slug must not contain repeated hyphens".to_string(),
        });
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(TenantSlugError {
            message: "tenant slug must contain only lowercase ASCII letters, digits, and hyphens"
                .to_string(),
        });
    }

    Ok(())
}

/// A billing/isolation boundary. Minimal by design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tenant {
    /// Stable identity of this tenant.
    pub id: TenantId,
    /// URL-safe short name (e.g. `acme`).
    pub slug: TenantSlug,
}

/// Tests for tenant serialization and slug validation.
#[cfg(test)]
mod tests {
    use super::*;

    /// Tenant serialization preserves the string slug wire shape.
    #[test]
    fn tenant_roundtrip() {
        let t = Tenant {
            id: TenantId::new(),
            slug: TenantSlug::new("acme").expect("valid slug"),
        };
        let json = serde_json::to_string(&t).expect("serialize");
        let back: Tenant = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(t, back);
    }

    /// Valid slug examples cover single labels and hyphen-separated labels.
    #[test]
    fn slug_accepts_canonical_values() {
        for value in ["a", "acme", "acme-1", "a1-b2-c3"] {
            assert_eq!(TenantSlug::new(value).expect("valid").as_ref(), value);
        }
    }

    /// Invalid slug examples cover path, unicode, empty, and hyphen edge cases.
    #[test]
    fn slug_rejects_invalid_values() {
        for value in [
            "",
            "../root",
            "Acme",
            "acme_",
            "acme--prod",
            "-acme",
            "acme-",
            "å",
        ] {
            assert!(TenantSlug::new(value).is_err(), "{value} should fail");
        }
    }

    /// Deserializing a tenant rejects invalid slugs at the boundary.
    #[test]
    fn tenant_deserialize_rejects_invalid_slug() {
        let json = format!(
            "{{\"id\":\"{}\",\"slug\":\"../root\"}}",
            TenantId::new().as_uuid()
        );
        let err = serde_json::from_str::<Tenant>(&json).expect_err("invalid slug");
        assert!(err.to_string().contains("tenant slug"));
    }

    /// Deserializing a tenant rejects misspelled or extra fields.
    #[test]
    fn tenant_deserialize_rejects_unknown_fields() {
        let json = format!(
            "{{\"id\":\"{}\",\"slug\":\"acme\",\"slgu\":\"oops\"}}",
            TenantId::new().as_uuid()
        );
        let err = serde_json::from_str::<Tenant>(&json).expect_err("unknown field");
        assert!(err.to_string().contains("unknown field"));
    }
}
