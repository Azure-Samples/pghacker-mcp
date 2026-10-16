// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A client on the network, as the rate limiter and usage telemetry count
//! one: an IPv4 address, or an IPv6 client's /64, which one client holds.

use std::net::{IpAddr, Ipv6Addr};

/// The client at `ip`: itself, or for IPv6 its /64. An IPv4-mapped IPv6
/// address is its IPv4 address.
pub fn bucket(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(Ipv6Addr::from(
                u128::from(v6) & 0xffff_ffff_ffff_ffff_0000_0000_0000_0000,
            )),
        },
    }
}

/// Whether `ip` can be a client's public address: not loopback, private,
/// link-local or unspecified. Behind a proxy that isn't configured to name
/// the client, the address is the proxy's, often one of these.
pub fn is_public(ip: IpAddr) -> bool {
    match bucket(ip) {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast())
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn ipv6_clients_are_their_64_and_mapped_addresses_their_ipv4() {
        assert_eq!(bucket(ip("203.0.113.9")), ip("203.0.113.9"));
        assert_eq!(bucket(ip("2001:db8:1:2::1")), ip("2001:db8:1:2::"));
        assert_eq!(bucket(ip("2001:db8:1:2:ffff::7")), ip("2001:db8:1:2::"));
        assert_ne!(bucket(ip("2001:db8:1:3::1")), ip("2001:db8:1:2::"));
        assert_eq!(bucket(ip("::ffff:10.0.0.1")), ip("10.0.0.1"));
    }

    #[test]
    fn only_public_addresses_are_public() {
        for public in ["203.0.113.9", "8.8.8.8", "2001:db8::1", "2606:4700::1111"] {
            assert!(is_public(ip(public)), "{public}");
        }
        for not in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "172.16.0.1",
            "169.254.0.1",
            "0.0.0.0",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(ip(not)), "{not}");
        }
    }
}
