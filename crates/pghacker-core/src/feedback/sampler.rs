// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The draw: whether a client's UTC clock hour is armed for an ask. It's a
//! keyed hash of the client and the hour, so every replica, retry and restart
//! draws the same, and no stored state decides who is asked.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

pub(super) struct Sampler {
    key: [u8; 32],
}

impl Sampler {
    pub(super) fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// Whether `user`'s hour `hour` (unix seconds over 3600) is armed at
    /// `rate`: whether the first 53 bits of HMAC-SHA256 of "arm", the user
    /// and the hour, as a fraction of 2⁵³, are under `rate`.
    pub(super) fn armed(&self, user: &str, hour: u64, rate: f64) -> bool {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC takes keys of any length");
        mac.update(b"arm\0");
        mac.update(user.as_bytes());
        mac.update(b"\0");
        mac.update(&hour.to_be_bytes());
        let hash = mac.finalize().into_bytes();
        let mut first = [0u8; 8];
        first.copy_from_slice(&hash[..8]);
        let draw = (u64::from_be_bytes(first) >> 11) as f64 / (1u64 << 53) as f64;
        draw < rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_draw_is_stable_and_near_its_rate() {
        let sampler = Sampler::new([7; 32]);
        let armed = (0..100_000u64)
            .filter(|hour| sampler.armed("57d404ffeaedb6e25cfdb7e423e6582d", *hour, 0.1))
            .count();
        // 0.1 within three standard deviations (0.095%).
        assert!((9_700..=10_300).contains(&armed), "{armed}");
        for hour in 0..1_000 {
            assert_eq!(sampler.armed("u", hour, 0.1), sampler.armed("u", hour, 0.1));
            assert!(!sampler.armed("u", hour, 0.0));
            assert!(sampler.armed("u", hour, 1.0));
        }
        // Users, and keys, draw apart.
        let a: Vec<bool> = (0..200).map(|hour| sampler.armed("a", hour, 0.5)).collect();
        let b: Vec<bool> = (0..200).map(|hour| sampler.armed("b", hour, 0.5)).collect();
        assert_ne!(a, b);
        let other = Sampler::new([8; 32]);
        let c: Vec<bool> = (0..200).map(|hour| other.armed("a", hour, 0.5)).collect();
        assert_ne!(a, c);
    }
}
