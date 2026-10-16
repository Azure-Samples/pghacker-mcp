// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Who a client is to usage telemetry: a keyed hash of its address, which
//! tells users apart without keeping the address, and its current session.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::clients;

/// A user's ID and their current session's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub user: String,
    pub session: String,
}

/// A session ends after this long without a request.
pub const SESSION_IDLE: Duration = Duration::from_secs(30 * 60);

/// Sessions kept at most; beyond, the idle ones are forgotten.
const MAX_SESSIONS: usize = 100_000;

pub(crate) struct Identities {
    key: Vec<u8>,
    sessions: Mutex<HashMap<String, Session>>,
}

struct Session {
    id: String,
    seen: Instant,
}

impl Identities {
    pub fn new(key: Vec<u8>) -> Self {
        Self {
            key,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// The user at `ip`: in hex, the first 128 bits of HMAC-SHA256, under the
    /// key, of "ipv4:" and the 4 bytes of the address, or "ipv6:" and the 16
    /// of its /64. scripts/lib/usage.mjs computes the same for the home page.
    pub fn user(&self, ip: IpAddr) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC takes keys of any length");
        match clients::bucket(ip) {
            IpAddr::V4(v4) => {
                mac.update(b"ipv4:");
                mac.update(&v4.octets());
            }
            IpAddr::V6(v6) => {
                mac.update(b"ipv6:");
                mac.update(&v6.octets());
            }
        }
        hex::encode(&mac.finalize().into_bytes()[..16])
    }

    /// The client at `ip` at `now`: its user, and its session, a new one
    /// after [`SESSION_IDLE`] without a request.
    pub fn identify(&self, ip: IpAddr, now: Instant) -> Identity {
        let user = self.user(ip);
        let mut sessions = self.sessions.lock().expect("the sessions' lock");
        if sessions.len() >= MAX_SESSIONS && !sessions.contains_key(&user) {
            sessions.retain(|_, session| now.duration_since(session.seen) < SESSION_IDLE);
            if sessions.len() >= MAX_SESSIONS {
                sessions.clear();
            }
        }
        let session = match sessions.get_mut(&user) {
            Some(session) if now.duration_since(session.seen) < SESSION_IDLE => {
                session.seen = now;
                session.id.clone()
            }
            _ => {
                let id = Uuid::new_v4().simple().to_string();
                sessions.insert(
                    user.clone(),
                    Session {
                        id: id.clone(),
                        seen: now,
                    },
                );
                id
            }
        };
        Identity { user, session }
    }

    /// Forgets the sessions idle for [`SESSION_IDLE`].
    pub fn prune(&self, now: Instant) {
        self.sessions
            .lock()
            .expect("the sessions' lock")
            .retain(|_, session| now.duration_since(session.seen) < SESSION_IDLE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test-usage-id-key-0123456789abcdef";

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn users_are_keyed_hashes_of_the_client() {
        let ids = Identities::new(KEY.to_vec());
        // The same vectors as test/usage.test.mjs: the home page's users are
        // the endpoint's.
        assert_eq!(ids.user(ip("203.0.113.9")), V4_USER);
        assert_eq!(ids.user(ip("2001:db8:1:2::1")), V6_USER);
        assert_eq!(ids.user(ip("2001:db8:1:2:ffff::9")), V6_USER);
        assert_eq!(ids.user(ip("::ffff:203.0.113.9")), V4_USER);
        assert_ne!(ids.user(ip("203.0.113.10")), V4_USER);
        let other = Identities::new(b"another-key-another-key-another-key".to_vec());
        assert_ne!(other.user(ip("203.0.113.9")), V4_USER);
        assert_eq!(V4_USER.len(), 32);
    }

    #[test]
    fn a_session_ends_after_half_an_hour_idle() {
        let ids = Identities::new(KEY.to_vec());
        let start = Instant::now();
        let first = ids.identify(ip("203.0.113.9"), start);
        let later = ids.identify(ip("203.0.113.9"), start + Duration::from_secs(29 * 60));
        assert_eq!(first, later);
        // Each request extends the session.
        let still = ids.identify(ip("203.0.113.9"), start + Duration::from_secs(58 * 60));
        assert_eq!(first, still);
        let next = ids.identify(ip("203.0.113.9"), start + Duration::from_secs(89 * 60));
        assert_eq!(next.user, first.user);
        assert_ne!(next.session, first.session);
        let other = ids.identify(ip("198.51.100.7"), start);
        assert_ne!(other.session, first.session);

        ids.prune(start + Duration::from_secs(120 * 60));
        assert!(ids.sessions.lock().unwrap().is_empty());
    }

    const V4_USER: &str = "57d404ffeaedb6e25cfdb7e423e6582d";
    const V6_USER: &str = "ed2b6a28eaba55c9854936ca33436922";
}
