// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The form's `requestState`: a random ask id sealed by rmcp's
//! `RequestStateCodec` (HMAC-SHA256), with an expiry and, as associated
//! data, the client's user hash and the tool called. Only the retry of that
//! call, from that client and before the state expires, can answer the ask.
//! It's signed, not encrypted: it carries no personal data, only the random
//! id and the expiry. That an ask is answered once is kept by the server.

use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::model::{RequestStateCodec, SealOptions};
use uuid::Uuid;

/// How long the user can take to answer.
pub(super) const LIFETIME: Duration = Duration::from_secs(6 * 3600);
/// A state's characters at most.
pub(super) const MAX_STATE: usize = 200;
const PREFIX: &str = "fb2.";

pub(super) struct Tokens {
    codec: RequestStateCodec,
    lifetime: Duration,
}

impl Tokens {
    pub(super) fn new(key: &[u8; 32], lifetime: Duration) -> Result<Self> {
        let codec = RequestStateCodec::try_new(key.to_vec()).context("the feedback states' key")?;
        Ok(Self { codec, lifetime })
    }

    /// A new state for `user`'s call of `tool`, and its ask id.
    pub(super) fn issue(&self, user: &str, tool: &str) -> (String, Uuid) {
        let id = Uuid::new_v4();
        let bound = bound(user, tool);
        let options = SealOptions::new()
            .associated_data(&bound)
            .ttl(self.lifetime);
        let sealed = self.codec.seal_with(id.as_bytes(), &options);
        (format!("{PREFIX}{sealed}"), id)
    }

    /// The ask id of `state`, if it's a state issued for `user`'s call of
    /// `tool` that hasn't expired. The codec can't tell a forged state from
    /// another client's or another tool's: all fail its check.
    pub(super) fn open(&self, state: &str, user: &str, tool: &str) -> Option<Uuid> {
        if state.len() > MAX_STATE {
            return None;
        }
        let sealed = state.strip_prefix(PREFIX)?;
        let bytes = self.codec.open_with(sealed, &bound(user, tool)).ok()?;
        Uuid::from_slice(&bytes).ok()
    }

    /// Whether `state` looks like one of these, whether or not it opens.
    pub(super) fn is_ours(state: &str) -> bool {
        state.starts_with(PREFIX)
    }
}

/// The associated data: the user and the tool, apart.
fn bound(user: &str, tool: &str) -> Vec<u8> {
    [user.as_bytes(), b"\0", tool.as_bytes()].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "57d404ffeaedb6e25cfdb7e423e6582d";

    #[test]
    fn a_state_opens_for_its_client_and_tool_only_until_it_expires() {
        let tokens = Tokens::new(&[3; 32], LIFETIME).unwrap();
        let (state, id) = tokens.issue(USER, "search");
        assert!(
            state.starts_with("fb2.") && state.len() <= MAX_STATE,
            "{state}"
        );
        assert!(Tokens::is_ours(&state));
        assert_eq!(tokens.open(&state, USER, "search"), Some(id));
        // Another client's, another tool's, altered, cut short, or from
        // another key.
        assert_eq!(
            tokens.open(&state, "ed2b6a28eaba55c9854936ca33436922", "search"),
            None
        );
        assert_eq!(tokens.open(&state, USER, "query"), None);
        let mut altered = state.clone().into_bytes();
        let last = altered.len() - 1;
        altered[last] = if altered[last] == b'A' { b'B' } else { b'A' };
        assert_eq!(
            tokens.open(&String::from_utf8(altered).unwrap(), USER, "search"),
            None
        );
        assert_eq!(tokens.open(&state[..state.len() - 4], USER, "search"), None);
        assert_eq!(
            tokens.open(state.trim_start_matches("fb2."), USER, "search"),
            None
        );
        let other = Tokens::new(&[4; 32], LIFETIME).unwrap();
        assert_eq!(other.open(&state, USER, "search"), None);
        assert_eq!(
            tokens.open(&format!("{state}{}", "x".repeat(MAX_STATE)), USER, "search"),
            None
        );
        // Every state is new.
        assert_ne!(tokens.issue(USER, "search").0, state);
        assert!(!Tokens::is_ours("eyJsb2NhdGlvbiI6Ik5ldyBZb3JrIn0"));
    }

    #[test]
    fn a_state_expires() {
        let tokens = Tokens::new(&[3; 32], Duration::from_millis(1)).unwrap();
        let (state, _) = tokens.issue(USER, "search");
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(tokens.open(&state, USER, "search"), None);
    }
}
