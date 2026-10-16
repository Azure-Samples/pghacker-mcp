// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The feedback id: a random value sealed by rmcp's `RequestStateCodec`
//! (HMAC-SHA256) with the client's user hash as associated data and an
//! expiry, so that only the client it was issued to can use it, and only for
//! a while. It's signed, not encrypted: it carries no personal data, only
//! the random value and the expiry.

use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::model::{RequestStateCodec, SealOptions};
use uuid::Uuid;

/// How long an id can be used.
pub(super) const LIFETIME: Duration = Duration::from_secs(6 * 3600);
/// An id's characters at most, as the tool's schema says.
pub(super) const MAX_ID: usize = 200;
const PREFIX: &str = "fb1.";

pub(super) struct Tokens {
    codec: RequestStateCodec,
    lifetime: Duration,
}

impl Tokens {
    pub(super) fn new(key: &[u8; 32], lifetime: Duration) -> Result<Self> {
        let codec = RequestStateCodec::try_new(key.to_vec()).context("the feedback ids' key")?;
        Ok(Self { codec, lifetime })
    }

    /// A new id for `user`, and its random value.
    pub(super) fn issue(&self, user: &str) -> (String, Uuid) {
        let value = Uuid::new_v4();
        let options = SealOptions::new()
            .associated_data(user.as_bytes())
            .ttl(self.lifetime);
        let sealed = self.codec.seal_with(value.as_bytes(), &options);
        (format!("{PREFIX}{sealed}"), value)
    }

    /// The random value of `id`, if it's an id issued to `user` that hasn't
    /// expired. The codec can't tell a forged id from another client's: both
    /// fail its check.
    pub(super) fn open(&self, id: &str, user: &str) -> Option<Uuid> {
        if id.len() > MAX_ID {
            return None;
        }
        let sealed = id.strip_prefix(PREFIX)?;
        let bytes = self.codec.open_with(sealed, user.as_bytes()).ok()?;
        Uuid::from_slice(&bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "57d404ffeaedb6e25cfdb7e423e6582d";

    #[test]
    fn an_id_opens_for_its_client_only_until_it_expires() {
        let tokens = Tokens::new(&[3; 32], LIFETIME).unwrap();
        let (id, value) = tokens.issue(USER);
        assert!(id.starts_with("fb1.") && id.len() <= MAX_ID, "{id}");
        assert_eq!(tokens.open(&id, USER), Some(value));
        // Another client's, altered, cut short, or from another key.
        assert_eq!(tokens.open(&id, "ed2b6a28eaba55c9854936ca33436922"), None);
        let mut altered = id.clone().into_bytes();
        let last = altered.len() - 1;
        altered[last] = if altered[last] == b'A' { b'B' } else { b'A' };
        assert_eq!(
            tokens.open(&String::from_utf8(altered).unwrap(), USER),
            None
        );
        assert_eq!(tokens.open(&id[..id.len() - 4], USER), None);
        assert_eq!(tokens.open(id.trim_start_matches("fb1."), USER), None);
        let other = Tokens::new(&[4; 32], LIFETIME).unwrap();
        assert_eq!(other.open(&id, USER), None);
        assert_eq!(
            tokens.open(&format!("{id}{}", "x".repeat(MAX_ID)), USER),
            None
        );
        // Every id is new.
        assert_ne!(tokens.issue(USER).0, id);
    }

    #[test]
    fn an_id_expires() {
        let tokens = Tokens::new(&[3; 32], Duration::from_millis(1)).unwrap();
        let (id, _) = tokens.issue(USER);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(tokens.open(&id, USER), None);
    }
}
