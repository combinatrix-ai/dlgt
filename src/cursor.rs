//! Opaque forward-observation cursor.
//!
//! A cursor is `f1.` followed by base64url of a compact serialized struct. The
//! codec version lives inside the payload and unknown fields are tolerated, so
//! a later delivery mode can extend the struct without breaking readers. The
//! scope binds to the immutable internal Session UID, never to the public
//! Session ID, because Claude rotates that ID on rekey.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const CURSOR_PREFIX: &str = "f1.";
pub const CURSOR_CODEC_VERSION: u32 = 1;
/// Scope covering every Session owned by one daemon.
pub const SCOPE_ALL: &str = "all";

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Cursor {
    /// Codec version of the payload itself.
    pub v: u32,
    /// Daemon boot UUID. State is memory-only, so a cursor never survives it.
    pub b: String,
    /// One Session UID, or `all`.
    pub s: String,
    /// Global lifecycle event watermark.
    #[serde(default)]
    pub e: i64,
    /// Per-Session watermarks keyed by Session UID.
    #[serde(default)]
    pub p: BTreeMap<String, SessionCursor>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionCursor {
    /// Stable absolute row watermark.
    #[serde(default)]
    pub r: u64,
    /// Screen epoch observed when the cursor was issued.
    #[serde(default)]
    pub ep: u64,
    /// Highest fully delivered terminal result execution sequence.
    #[serde(default)]
    pub x: i64,
    /// Execution sequence of a partially delivered final text, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub px: Option<i64>,
    /// Byte offset already delivered for that final text.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub po: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Cursor {
    pub fn new(instance_id: &str, scope: &str) -> Self {
        Self {
            v: CURSOR_CODEC_VERSION,
            b: instance_id.to_owned(),
            s: scope.to_owned(),
            e: 0,
            p: BTreeMap::new(),
        }
    }

    pub fn session(&self, uid: &str) -> SessionCursor {
        self.p.get(uid).copied().unwrap_or_default()
    }

    pub fn set_session(&mut self, uid: &str, session: SessionCursor) {
        self.p.insert(uid.to_owned(), session);
    }

    pub fn encode(&self) -> Result<String> {
        let payload = serde_json::to_vec(self)?;
        Ok(format!(
            "{CURSOR_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        ))
    }

    /// Decode a caller-supplied cursor and validate it against this daemon.
    ///
    /// Every failure is a structured, non-zero error whose recovery is one
    /// cursorless baseline fetch.
    pub fn decode(text: &str, instance_id: &str) -> Result<Self> {
        let Some(payload) = text.strip_prefix(CURSOR_PREFIX) else {
            bail!(
                "CURSOR_VERSION_UNSUPPORTED: cursor prefix is not {CURSOR_PREFIX:?}; fetch without --cursor to recover"
            );
        };
        let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
            bail!("CURSOR_INVALID: cursor payload is not valid base64url");
        };
        let Ok(cursor) = serde_json::from_slice::<Self>(&bytes) else {
            bail!("CURSOR_INVALID: cursor payload is not a valid cursor document");
        };
        if cursor.v > CURSOR_CODEC_VERSION {
            bail!(
                "CURSOR_VERSION_UNSUPPORTED: cursor codec version {} is newer than {CURSOR_CODEC_VERSION}",
                cursor.v
            );
        }
        if cursor.b != instance_id {
            bail!(
                "CURSOR_EXPIRED: cursor belongs to a previous daemon instance; fetch without --cursor to recover"
            );
        }
        Ok(cursor)
    }

    /// Reject a cursor addressed at a different Session or aggregation scope.
    pub fn require_scope(&self, scope: &str) -> Result<()> {
        if self.s == scope {
            return Ok(());
        }
        bail!(
            "CURSOR_SCOPE_MISMATCH: cursor scope {:?} does not address {scope:?}; fetch without --cursor to recover",
            self.s
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{CURSOR_PREFIX, Cursor, SCOPE_ALL, SessionCursor};
    use base64::Engine as _;

    fn sample() -> Cursor {
        let mut cursor = Cursor::new("boot-1", "su_abc");
        cursor.e = 104;
        cursor.set_session(
            "su_abc",
            SessionCursor {
                r: 512,
                ep: 3,
                x: 7,
                px: Some(8),
                po: 4_096,
            },
        );
        cursor
    }

    #[test]
    fn round_trips_every_watermark_through_an_opaque_token() {
        let encoded = sample()
            .encode()
            .unwrap_or_else(|error| panic!("failed to encode cursor: {error}"));
        assert!(encoded.starts_with(CURSOR_PREFIX));
        assert!(!encoded.contains('='), "cursor must be base64url unpadded");

        let decoded = Cursor::decode(&encoded, "boot-1")
            .unwrap_or_else(|error| panic!("failed to decode cursor: {error}"));
        assert_eq!(decoded, sample());
        assert_eq!(decoded.session("su_abc").r, 512);
        assert_eq!(decoded.session("su_missing"), SessionCursor::default());
    }

    #[test]
    fn an_unknown_prefix_is_an_unsupported_codec() {
        let error = Cursor::decode("v1.abc", "boot-1")
            .err()
            .unwrap_or_else(|| panic!("unsupported prefix unexpectedly decoded"));
        assert!(error.to_string().contains("CURSOR_VERSION_UNSUPPORTED"));
    }

    #[test]
    fn a_newer_payload_version_is_not_reinterpreted() {
        let mut cursor = sample();
        cursor.v = 99;
        let encoded = cursor
            .encode()
            .unwrap_or_else(|error| panic!("failed to encode cursor: {error}"));
        let error = Cursor::decode(&encoded, "boot-1")
            .err()
            .unwrap_or_else(|| panic!("future cursor unexpectedly decoded"));
        assert!(error.to_string().contains("CURSOR_VERSION_UNSUPPORTED"));
    }

    #[test]
    fn another_daemon_instance_expires_the_cursor() {
        let encoded = sample()
            .encode()
            .unwrap_or_else(|error| panic!("failed to encode cursor: {error}"));
        let error = Cursor::decode(&encoded, "boot-2")
            .err()
            .unwrap_or_else(|| panic!("foreign cursor unexpectedly decoded"));
        assert!(error.to_string().contains("CURSOR_EXPIRED"));
    }

    #[test]
    fn scope_mismatch_is_reported_instead_of_silently_rebasing() {
        let cursor = sample();
        assert!(cursor.require_scope("su_abc").is_ok());
        let error = cursor
            .require_scope(SCOPE_ALL)
            .err()
            .unwrap_or_else(|| panic!("scope mismatch unexpectedly accepted"));
        assert!(error.to_string().contains("CURSOR_SCOPE_MISMATCH"));
    }

    #[test]
    fn unknown_payload_fields_are_tolerated_for_forward_compatibility() {
        let payload = br#"{"v":1,"b":"boot-1","s":"su_abc","e":9,"consumer":"leader"}"#;
        let encoded = format!(
            "{CURSOR_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        );
        let decoded = Cursor::decode(&encoded, "boot-1")
            .unwrap_or_else(|error| panic!("failed to decode cursor: {error}"));
        assert_eq!(decoded.e, 9);
    }
}
