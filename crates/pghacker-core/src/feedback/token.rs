// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The form's `requestState`: a random ask id sealed by rmcp's
//! `RequestStateCodec` (HMAC-SHA256), with an expiry and, as associated
//! data, the app and the tool called. Only a retry of that tool's call by
//! that app, before the state expires, can answer the ask, from whatever
//! address: behind gateways such as Global Secure Access, a client reaches
//! the server from a new address on each connection. It's signed, not
//! encrypted: it carries no personal data, only the random id and the
//! expiry. That an ask is answered once is kept by the server.

use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::model::{RequestStateCodec, SealOptions};
use uuid::Uuid;

/// How long the user can take to answer.
pub(super) const LIFETIME: Duration = Duration::from_secs(6 * 3600);
/// A state's characters at most.
pub(super) const MAX_STATE: usize = 200;
const PREFIX: &str = "fb3.";
/// States sealed to the asked address, which no longer open.
const OLD_PREFIXES: &[&str] = &["fb2."];

pub(super) struct Tokens {
    codec: RequestStateCodec,
    lifetime: Duration,
}

impl Tokens {
    pub(super) fn new(key: &[u8; 32], lifetime: Duration) -> Result<Self> {
        let codec = RequestStateCodec::try_new(key.to_vec()).context("the feedback states' key")?;
        Ok(Self { codec, lifetime })
    }

    /// A new state for `app`'s call of `tool`, and its ask id.
    pub(super) fn issue(&self, app: &str, tool: &str) -> (String, Uuid) {
        let id = Uuid::new_v4();
        let bound = bound(app, tool);
        let options = SealOptions::new()
            .associated_data(&bound)
            .ttl(self.lifetime);
        let sealed = self.codec.seal_with(id.as_bytes(), &options);
        (format!("{PREFIX}{sealed}"), id)
    }

    /// The ask id of `state`, if it's a state issued for `app`'s call of
    /// `tool` that hasn't expired. The codec can't tell a forged state from
    /// another app's or another tool's: all fail its check.
    pub(super) fn open(&self, state: &str, app: &str, tool: &str) -> Option<Uuid> {
        if state.len() > MAX_STATE {
            return None;
        }
        let sealed = state.strip_prefix(PREFIX)?;
        let bytes = self.codec.open_with(sealed, &bound(app, tool)).ok()?;
        Uuid::from_slice(&bytes).ok()
    }

    /// Whether `state` looks like one of these, whether or not it opens.
    pub(super) fn is_ours(state: &str) -> bool {
        std::iter::once(PREFIX)
            .chain(OLD_PREFIXES.iter().copied())
            .any(|prefix| state.starts_with(prefix))
    }
}

/// The associated data: the app and the tool, apart.
fn bound(app: &str, tool: &str) -> Vec<u8> {
    [app.as_bytes(), b"\0", tool.as_bytes()].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "copilot-cli";

    #[test]
    fn a_state_opens_for_its_app_and_tool_only_until_it_expires() {
        let tokens = Tokens::new(&[3; 32], LIFETIME).unwrap();
        let (state, id) = tokens.issue(APP, "search");
        assert!(
            state.starts_with("fb3.") && state.len() <= MAX_STATE,
            "{state}"
        );
        assert!(Tokens::is_ours(&state));
        assert_eq!(tokens.open(&state, APP, "search"), Some(id));
        // Another app's, another tool's, altered, cut short, or from
        // another key.
        assert_eq!(tokens.open(&state, "codex-mcp-client", "search"), None);
        assert_eq!(tokens.open(&state, APP, "query"), None);
        let mut altered = state.clone().into_bytes();
        let last = altered.len() - 1;
        altered[last] = if altered[last] == b'A' { b'B' } else { b'A' };
        assert_eq!(
            tokens.open(&String::from_utf8(altered).unwrap(), APP, "search"),
            None
        );
        assert_eq!(tokens.open(&state[..state.len() - 4], APP, "search"), None);
        assert_eq!(
            tokens.open(state.trim_start_matches("fb3."), APP, "search"),
            None
        );
        let other = Tokens::new(&[4; 32], LIFETIME).unwrap();
        assert_eq!(other.open(&state, APP, "search"), None);
        assert_eq!(
            tokens.open(&format!("{state}{}", "x".repeat(MAX_STATE)), APP, "search"),
            None
        );
        // Every state is new.
        assert_ne!(tokens.issue(APP, "search").0, state);
        assert!(!Tokens::is_ours("eyJsb2NhdGlvbiI6Ik5ldyBZb3JrIn0"));
    }

    #[test]
    fn a_state_sealed_to_an_address_is_ours_but_never_opens() {
        let tokens = Tokens::new(&[3; 32], LIFETIME).unwrap();
        let (state, _) = tokens.issue(APP, "search");
        let old = format!("fb2.{}", state.trim_start_matches("fb3."));
        assert!(Tokens::is_ours(&old));
        assert_eq!(tokens.open(&old, APP, "search"), None);
    }

    #[test]
    fn a_state_expires() {
        let tokens = Tokens::new(&[3; 32], Duration::from_millis(1)).unwrap();
        let (state, _) = tokens.issue(APP, "search");
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(tokens.open(&state, APP, "search"), None);
    }
}
