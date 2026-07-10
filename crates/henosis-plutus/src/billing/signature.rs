//! Stripe webhook signature verification (`Stripe-Signature` header scheme).
//!
//! Stripe signs each webhook delivery with an HMAC-SHA256 keyed by the endpoint's webhook
//! signing secret. The `Stripe-Signature` header carries a comma-separated list of
//! `key=value` segments: a `t` timestamp and one or more `v1` hex-encoded signatures
//! (Stripe sends more than one `v1` during signing-secret rollover, so any match is
//! accepted). The signed payload is `"{timestamp}.{raw body}"` -- the literal timestamp
//! digits, a literal `.`, then the exact bytes Stripe sent, never re-encoded and never
//! assumed to be UTF-8.
//!
//! FAIL-CLOSED INVARIANT: [`verify_stripe_signature`] has exactly one path to `Ok(())` --
//! the header parses, the timestamp is within [`DEFAULT_TOLERANCE_SECS`] of `now`, and a
//! constant-time comparison finds at least one `v1` candidate that matches the HMAC
//! computed over the raw body with the caller's secret. Every other path (missing header,
//! malformed segment, bad timestamp, expired/future timestamp, no matching signature)
//! returns a specific [`SignatureError`] variant. There is no `_ => Ok(())` fallback
//! anywhere in this file, the comparison never short-circuits on the first mismatch, and
//! no attacker-controlled input can reach an `unwrap()`/`expect()`/panic.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::{Choice, ConstantTimeEq};

/// Default acceptable clock skew, in seconds, between the webhook's `t` timestamp and the
/// verifier's `now`. Matches Stripe's documented default tolerance. The boundary is
/// inclusive: a skew exactly equal to this value is accepted.
pub const DEFAULT_TOLERANCE_SECS: i64 = 300;

/// HMAC-SHA256 keyed MAC, used both to verify Stripe webhooks and, in tests, to sign
/// self-generated fixtures.
type HmacSha256 = Hmac<Sha256>;

/// Errors returned by [`verify_stripe_signature`] and [`parse_signature_header`].
///
/// Every variant names a distinct rejection reason so callers (and tests) can assert on
/// the exact failure mode via `matches!` rather than parsing an error string.
#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    /// The `Stripe-Signature` header was empty.
    #[error("stripe-signature header is missing")]
    MissingHeader,
    /// A header segment was not in `key=value` form (no `=` present).
    #[error("stripe-signature header segment is malformed: {0}")]
    MalformedHeader(String),
    /// The header had no `t=` segment.
    #[error("stripe-signature header has no timestamp (t=) segment")]
    MissingTimestamp,
    /// The header had no `v1=` segment at all.
    #[error("stripe-signature header has no signature (v1=) segment")]
    MissingSignature,
    /// The `t=` value could not be parsed as an integer Unix timestamp.
    #[error("stripe-signature timestamp is not a valid integer: {0}")]
    InvalidTimestamp(String),
    /// The timestamp's absolute distance from `now` exceeds [`DEFAULT_TOLERANCE_SECS`].
    #[error("stripe-signature timestamp outside tolerance window: skew={skew_secs}s")]
    TimestampOutOfTolerance {
        /// The absolute skew, in seconds, between `now` and the header's `t`.
        skew_secs: i64,
    },
    /// At least one `v1` candidate was present, but none matched the computed HMAC.
    #[error("stripe-signature does not match any provided v1 candidate")]
    NoMatchingSignature,
}

/// The parsed structure of a `Stripe-Signature` header: one timestamp and every `v1`
/// signature candidate that hex-decoded successfully (Stripe may send more than one
/// during signing-secret rotation).
#[derive(Debug)]
struct StripeSignatureHeader {
    /// The `t=` value, parsed as a Unix timestamp in seconds.
    timestamp: i64,
    /// Every `v1=` value that hex-decoded successfully, in header order. Non-hex values
    /// are dropped here rather than aborting the parse; length mismatches against the
    /// computed HMAC are filtered later, during comparison.
    v1: Vec<Vec<u8>>,
}

/// Parse a raw `Stripe-Signature` header into its timestamp and `v1` signature candidates.
///
/// Unknown segment keys (e.g. a future `v0=`/`v2=` scheme) are ignored, not errors. A
/// segment with no `=` is always [`SignatureError::MalformedHeader`], since that indicates
/// a genuinely corrupt header rather than a forward-compatible unknown field. Kept as a
/// free function (rather than inlined into [`verify_stripe_signature`]) so the parsing
/// logic is independently unit-testable.
fn parse_signature_header(header: &str) -> Result<StripeSignatureHeader, SignatureError> {
    if header.is_empty() {
        return Err(SignatureError::MissingHeader);
    }

    let mut timestamp: Option<i64> = None;
    let mut v1_candidates: Vec<Vec<u8>> = Vec::new();
    let mut saw_v1_segment = false;

    for segment in header.split(',') {
        let (key, value) = segment
            .split_once('=')
            .ok_or_else(|| SignatureError::MalformedHeader(segment.to_string()))?;
        match key {
            "t" => {
                let parsed = value
                    .parse::<i64>()
                    .map_err(|_| SignatureError::InvalidTimestamp(value.to_string()))?;
                timestamp = Some(parsed);
            }
            "v1" => {
                saw_v1_segment = true;
                // A non-hex v1 value is skipped as an unusable candidate rather than
                // aborting the whole parse -- a hostile header should not be able to turn
                // a signature mismatch into a different error class.
                if let Ok(bytes) = hex::decode(value) {
                    v1_candidates.push(bytes);
                }
            }
            _ => {
                // Unknown key -- ignored by design (forward compatibility).
            }
        }
    }

    let timestamp = timestamp.ok_or(SignatureError::MissingTimestamp)?;
    if !saw_v1_segment {
        return Err(SignatureError::MissingSignature);
    }
    if v1_candidates.is_empty() {
        // A v1 segment was present but every value failed to hex-decode. Fail closed as a
        // signature mismatch (not a parse error) -- this is indistinguishable, from the
        // caller's perspective, from an all-garbage v1 list that decoded fine but matched
        // nothing.
        return Err(SignatureError::NoMatchingSignature);
    }

    Ok(StripeSignatureHeader {
        timestamp,
        v1: v1_candidates,
    })
}

/// Verify a Stripe webhook's `Stripe-Signature` header against the endpoint's signing
/// secret and the exact raw request body bytes.
///
/// `now` is injected (rather than read from the system clock) so verification is
/// deterministic in tests and callers control the tolerance window explicitly. See the
/// module docs for the fail-closed invariant this function upholds.
pub fn verify_stripe_signature(
    secret: &str,
    header: &str,
    body: &[u8],
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), SignatureError> {
    let parsed = parse_signature_header(header)?;

    // Tolerance check happens before any HMAC computation: an out-of-window timestamp is
    // rejected without spending time hashing the body. Use checked arithmetic throughout
    // so an absurd `t` (attacker-controlled) cannot overflow-panic on subtraction or abs.
    let skew = match now.timestamp().checked_sub(parsed.timestamp) {
        Some(diff) => match diff.checked_abs() {
            Some(abs) => abs,
            // diff == i64::MIN: abs() would itself overflow. An absurd t this far from
            // `now` is certainly out of tolerance, so fail closed with a saturated skew.
            None => i64::MAX,
        },
        // Subtraction overflowed (an absurd t near i64::MIN/MAX against a normal `now`).
        // Fail closed rather than panic.
        None => i64::MAX,
    };
    if skew > DEFAULT_TOLERANCE_SECS {
        return Err(SignatureError::TimestampOutOfTolerance { skew_secs: skew });
    }

    // Compute the expected HMAC over "{t}." followed by the raw, un-decoded body bytes.
    // The secret is used exactly as given -- never trimmed -- and the body is hashed as
    // bytes so a non-UTF8 payload verifies identically to a UTF-8 one.
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("{}.", parsed.timestamp).as_bytes());
    mac.update(body);
    let expected = mac.finalize().into_bytes();

    // Compare against every v1 candidate in constant time, folding the per-candidate
    // Choice with bitwise OR so every candidate is always inspected -- no early return on
    // the first (mis)match, which would otherwise leak which candidate position matched.
    let mut any_match = Choice::from(0u8);
    for candidate in &parsed.v1 {
        if candidate.len() != expected.len() {
            // Length is public information (it is visible in the header itself), so a
            // plain length check up front leaks nothing about the secret or the correct
            // signature's content.
            continue;
        }
        any_match |= expected.as_slice().ct_eq(candidate.as_slice());
    }

    if bool::from(any_match) {
        Ok(())
    } else {
        Err(SignatureError::NoMatchingSignature)
    }
}

/// Compute a valid `Stripe-Signature` header value for `body` at `timestamp`.
///
/// Mirrors the production HMAC construction exactly, so tests are self-generating and never
/// embed a signature or secret captured from anywhere real. Exposed under
/// `#[cfg(any(test, feature = "test-helpers"))]` -- the same posture as `FrozenClock` -- so
/// external test modules (e.g. `syntheos-server`'s webhook route tests) can sign a fixture
/// body without taking their own dependency on `hmac`/`sha2`. Never compiled into a release
/// build: a signing oracle has no place in the shipped binary.
#[cfg(any(test, feature = "test-helpers"))]
pub fn sign_stripe_payload(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(body);
    let sig = hex::encode(mac.finalize().into_bytes());
    format!("t={timestamp},v1={sig}")
}

/// Tests for the `Stripe-Signature` parser and the constant-time verifier.
#[cfg(test)]
mod tests {
    use super::sign_stripe_payload as sign;
    use super::*;
    use chrono::TimeZone;

    /// Fixture signing secret. Not a real Stripe secret -- fake, used only to compute and
    /// verify HMACs within these tests.
    const TEST_SECRET: &str = "whsec_test_secret";

    /// A fixed "now" instant so test timestamps can be computed relative to it without
    /// depending on the system clock.
    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0).unwrap()
    }

    /// A valid signature over the exact body is accepted.
    #[test]
    fn valid_signature_is_accepted() {
        let body = b"{\"id\":\"evt_1\"}";
        let t = now().timestamp();
        let header = sign(TEST_SECRET, t, body);
        assert!(verify_stripe_signature(TEST_SECRET, &header, body, now()).is_ok());
    }

    /// A signature computed over one body does not verify against a different body.
    #[test]
    fn tampered_body_is_rejected() {
        let body = b"{\"id\":\"evt_1\"}";
        let t = now().timestamp();
        let header = sign(TEST_SECRET, t, body);
        let tampered: &[u8] = b"{\"id\":\"evt_2\"}";
        let err = verify_stripe_signature(TEST_SECRET, &header, tampered, now()).unwrap_err();
        assert!(matches!(err, SignatureError::NoMatchingSignature));
    }

    /// Flipping a hex digit in the v1 value invalidates the signature without changing its
    /// length, so it exercises the constant-time comparison path (not the hex-decode path).
    #[test]
    fn tampered_signature_is_rejected() {
        let body: &[u8] = b"payload";
        let t = now().timestamp();
        let header = sign(TEST_SECRET, t, body);
        let mut chars: Vec<char> = header.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == '0' { '1' } else { '0' };
        let corrupted: String = chars.into_iter().collect();
        let err = verify_stripe_signature(TEST_SECRET, &corrupted, body, now()).unwrap_err();
        assert!(matches!(err, SignatureError::NoMatchingSignature));
    }

    /// A timestamp more than the tolerance window in the past is rejected.
    #[test]
    fn expired_timestamp_is_rejected() {
        let body: &[u8] = b"payload";
        let t = now().timestamp() - 1_000;
        let header = sign(TEST_SECRET, t, body);
        let err = verify_stripe_signature(TEST_SECRET, &header, body, now()).unwrap_err();
        assert!(matches!(err, SignatureError::TimestampOutOfTolerance { .. }));
    }

    /// A timestamp more than the tolerance window in the future is rejected too -- the
    /// tolerance check is symmetric, not just an expiry check.
    #[test]
    fn future_timestamp_is_rejected() {
        let body: &[u8] = b"payload";
        let t = now().timestamp() + 1_000;
        let header = sign(TEST_SECRET, t, body);
        let err = verify_stripe_signature(TEST_SECRET, &header, body, now()).unwrap_err();
        assert!(matches!(err, SignatureError::TimestampOutOfTolerance { .. }));
    }

    /// A skew of exactly `DEFAULT_TOLERANCE_SECS` is accepted (inclusive boundary).
    #[test]
    fn skew_of_exactly_tolerance_is_accepted() {
        let body: &[u8] = b"payload";
        let t = now().timestamp() - DEFAULT_TOLERANCE_SECS;
        let header = sign(TEST_SECRET, t, body);
        assert!(verify_stripe_signature(TEST_SECRET, &header, body, now()).is_ok());
    }

    /// A skew of `DEFAULT_TOLERANCE_SECS + 1` is rejected, and the reported skew is exact.
    #[test]
    fn skew_of_tolerance_plus_one_is_rejected() {
        let body: &[u8] = b"payload";
        let t = now().timestamp() - (DEFAULT_TOLERANCE_SECS + 1);
        let header = sign(TEST_SECRET, t, body);
        let err = verify_stripe_signature(TEST_SECRET, &header, body, now()).unwrap_err();
        match err {
            SignatureError::TimestampOutOfTolerance { skew_secs } => {
                assert_eq!(skew_secs, DEFAULT_TOLERANCE_SECS + 1);
            }
            other => panic!("expected TimestampOutOfTolerance, got {other:?}"),
        }
    }

    /// Extract the hex `v1` value from a header produced by [`sign`].
    fn v1_of(header: &str) -> &str {
        header
            .split(",v1=")
            .nth(1)
            .expect("sign() always produces a v1 segment")
    }

    /// When multiple `v1` entries are present and only the second one matches, the
    /// signature still verifies -- the comparison loop must not stop at the first mismatch.
    ///
    /// The decoy is a full-length (32-byte) non-matching signature, so it survives the
    /// length guard and is actually fed to `ct_eq`. That is what makes this a real
    /// regression test for the non-short-circuiting `Choice` fold: a verifier rewritten to
    /// return on the first `ct_eq` miss would fail here, whereas a wrong-length decoy would
    /// be skipped by the length guard and let such a bug pass unnoticed.
    #[test]
    fn multiple_v1_entries_second_matching_is_accepted() {
        let body: &[u8] = b"payload";
        let t = now().timestamp();
        let valid_header = sign(TEST_SECRET, t, body);
        let decoy = hex::encode([0xAAu8; 32]);
        let header = format!("t={t},v1={decoy},v1={}", v1_of(&valid_header));
        assert!(verify_stripe_signature(TEST_SECRET, &header, body, now()).is_ok());
    }

    /// A `v1` candidate that hex-decodes but has the wrong byte length is skipped by the
    /// length guard rather than aborting the scan, so a later valid candidate still wins.
    #[test]
    fn wrong_length_v1_candidate_is_skipped() {
        let body: &[u8] = b"payload";
        let t = now().timestamp();
        let valid_header = sign(TEST_SECRET, t, body);
        // "deadbeef" hex-decodes fine but is 4 bytes, not the 32 an HMAC-SHA256 produces.
        let header = format!("t={t},v1=deadbeef,v1={}", v1_of(&valid_header));
        assert!(verify_stripe_signature(TEST_SECRET, &header, body, now()).is_ok());
    }

    /// A duplicate `t=` segment resolves to the last one seen, and the signature must be
    /// computed over that same timestamp. Pins the last-wins parse semantics so a future
    /// refactor cannot silently switch to first-wins and change what payload gets signed.
    #[test]
    fn duplicate_timestamp_segments_last_wins() {
        let body: &[u8] = b"payload";
        let good_t = now().timestamp();
        let stale_t = good_t - 10_000;
        let signed_over_good = sign(TEST_SECRET, good_t, body);
        let header = format!("t={stale_t},t={good_t},v1={}", v1_of(&signed_over_good));
        assert!(
            verify_stripe_signature(TEST_SECRET, &header, body, now()).is_ok(),
            "last t= wins, so the signature over good_t verifies"
        );

        // The mirror case: signing over the first (discarded) timestamp must NOT verify.
        let signed_over_stale = sign(TEST_SECRET, stale_t, body);
        let header = format!("t={stale_t},t={good_t},v1={}", v1_of(&signed_over_stale));
        let err = verify_stripe_signature(TEST_SECRET, &header, body, now()).unwrap_err();
        assert!(matches!(err, SignatureError::NoMatchingSignature));
    }

    /// Unrecognized segment keys (a hypothetical future scheme, and arbitrary junk) are
    /// ignored rather than causing an error, as long as a valid t/v1 pair is also present.
    #[test]
    fn unknown_segments_are_ignored() {
        let body: &[u8] = b"payload";
        let t = now().timestamp();
        let valid_header = sign(TEST_SECRET, t, body);
        let header = format!("v0=junk,{valid_header},stray=text");
        assert!(verify_stripe_signature(TEST_SECRET, &header, body, now()).is_ok());
    }

    /// An empty header is rejected as `MissingHeader`.
    #[test]
    fn empty_header_is_missing_header() {
        let err = verify_stripe_signature(TEST_SECRET, "", b"payload", now()).unwrap_err();
        assert!(matches!(err, SignatureError::MissingHeader));
    }

    /// A header with a `v1` segment but no `t` segment is `MissingTimestamp`.
    #[test]
    fn header_missing_timestamp_segment() {
        let body: &[u8] = b"payload";
        let t = now().timestamp();
        let valid_header = sign(TEST_SECRET, t, body);
        let v1_only = valid_header
            .split_once(',')
            .expect("sign() output has a comma between t and v1")
            .1;
        let err = verify_stripe_signature(TEST_SECRET, v1_only, body, now()).unwrap_err();
        assert!(matches!(err, SignatureError::MissingTimestamp));
    }

    /// A header with a `t` segment but no `v1` segment at all is `MissingSignature`.
    #[test]
    fn header_missing_v1_segment() {
        let t = now().timestamp();
        let header = format!("t={t}");
        let err = verify_stripe_signature(TEST_SECRET, &header, b"payload", now()).unwrap_err();
        assert!(matches!(err, SignatureError::MissingSignature));
    }

    /// A non-integer `t` value is `InvalidTimestamp`.
    #[test]
    fn non_integer_timestamp_is_invalid() {
        let header = "t=not-a-number,v1=abcd";
        let err = verify_stripe_signature(TEST_SECRET, header, b"payload", now()).unwrap_err();
        assert!(matches!(err, SignatureError::InvalidTimestamp(_)));
    }

    /// A `v1` value that is not valid hex is skipped as a candidate; with no other v1
    /// candidates left, this surfaces as `NoMatchingSignature`.
    #[test]
    fn non_hex_v1_is_no_matching_signature() {
        let t = now().timestamp();
        let header = format!("t={t},v1=not-hex-zzz");
        let err = verify_stripe_signature(TEST_SECRET, &header, b"payload", now()).unwrap_err();
        assert!(matches!(err, SignatureError::NoMatchingSignature));
    }

    /// A header segment with no `=` at all is `MalformedHeader`, exercised directly
    /// against the parser to prove it is independently testable.
    #[test]
    fn segment_without_equals_is_malformed_header() {
        let err = parse_signature_header("t=123,junk,v1=abc123").unwrap_err();
        assert!(matches!(err, SignatureError::MalformedHeader(_)));
    }

    /// Non-UTF8 body bytes verify correctly -- the body is hashed as raw bytes, never
    /// converted through a `String`.
    #[test]
    fn non_utf8_body_verifies_correctly() {
        let body: &[u8] = &[0xff, 0xfe, 0x00, 0xd8, 0x80];
        let t = now().timestamp();
        let header = sign(TEST_SECRET, t, body);
        assert!(verify_stripe_signature(TEST_SECRET, &header, body, now()).is_ok());
    }

    /// Verifying with the wrong secret is rejected even though the header is well-formed
    /// and the timestamp is in tolerance.
    #[test]
    fn wrong_secret_is_rejected() {
        let body: &[u8] = b"payload";
        let t = now().timestamp();
        let header = sign(TEST_SECRET, t, body);
        let err =
            verify_stripe_signature("whsec_wrong_secret", &header, body, now()).unwrap_err();
        assert!(matches!(err, SignatureError::NoMatchingSignature));
    }

    /// An empty body verifies correctly against its own signature.
    #[test]
    fn empty_body_verifies_correctly() {
        let body: &[u8] = b"";
        let t = now().timestamp();
        let header = sign(TEST_SECRET, t, body);
        assert!(verify_stripe_signature(TEST_SECRET, &header, body, now()).is_ok());
    }

    /// An absurd timestamp (`i64::MIN`) cannot overflow-panic the skew computation; it is
    /// simply treated as maximally out of tolerance.
    #[test]
    fn extreme_timestamp_does_not_panic() {
        let header = format!("t={},v1=deadbeef", i64::MIN);
        let result = verify_stripe_signature(TEST_SECRET, &header, b"payload", now());
        assert!(matches!(
            result,
            Err(SignatureError::TimestampOutOfTolerance { .. })
        ));
    }
}
