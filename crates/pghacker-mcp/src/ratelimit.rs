// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Per-client rate limiting for both listeners: a token bucket per client
//! (`pghacker_core::clients`: an IP address, an IPv6 client's /64), refilled
//! continuously; and the check that requests came through the deployment's
//! Front Door, for the public one.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use pghacker_core::clients;

#[derive(Clone)]
pub struct RateLimit {
    inner: Arc<Inner>,
}

struct Inner {
    per_second: f64,
    burst: f64,
    client_ip: ClientIp,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

/// Where a request's client address comes from, for the rate limit and
/// usage telemetry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientIp {
    /// The connection's peer.
    Socket,
    /// The first X-Forwarded-For address, which a client can choose: behind a
    /// proxy that drops clients' X-Forwarded-For only.
    FirstForwardedFor,
    /// A header that the proxy in front sets to the client's address and that
    /// clients can't, such as Azure Front Door's X-Azure-SocketIP. (Its
    /// X-Azure-ClientIP follows a client's X-Forwarded-For.)
    Header(HeaderName),
}

impl ClientIp {
    /// The client's address: where `self` says, else the connection's peer.
    pub fn of(&self, request: &Request) -> Option<IpAddr> {
        let header = match self {
            ClientIp::Socket => None,
            ClientIp::FirstForwardedFor => request
                .headers()
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(',').next()),
            ClientIp::Header(name) => request
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok()),
        };
        if let Some(ip) = header.and_then(|value| value.trim().parse().ok()) {
            return Some(ip);
        }
        request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip())
    }
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

impl RateLimit {
    /// `per_minute` requests a minute on average, bursts of up to `burst`,
    /// per client: the address `client_ip` finds, or the connection's peer.
    pub fn new(per_minute: u32, burst: u32, client_ip: ClientIp) -> Self {
        Self {
            inner: Arc::new(Inner {
                per_second: f64::from(per_minute.max(1)) / 60.0,
                burst: f64::from(burst.max(1)),
                client_ip,
                buckets: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Takes a token for `ip`, or returns how long until one is available.
    pub fn take(&self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        let mut buckets = self.inner.buckets.lock().expect("the rate limiter's lock");
        if buckets.len() > 100_000 {
            let idle = Duration::from_secs_f64(self.inner.burst / self.inner.per_second);
            buckets.retain(|_, bucket| now.duration_since(bucket.at) < idle);
        }
        let bucket = buckets.entry(clients::bucket(ip)).or_insert(Bucket {
            tokens: self.inner.burst,
            at: now,
        });
        let elapsed = now.duration_since(bucket.at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.inner.per_second).min(self.inner.burst);
        bucket.at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64(
                (1.0 - bucket.tokens) / self.inner.per_second,
            ))
        }
    }

    fn client(&self, request: &Request) -> Option<IpAddr> {
        self.inner.client_ip.of(request)
    }

    /// The clients with a bucket.
    #[cfg(test)]
    fn clients(&self) -> usize {
        self.inner.buckets.lock().unwrap().len()
    }
}

/// Requests that skip the checks: the health check.
pub(crate) fn exempt(request: &Request) -> bool {
    request.uri().path() == "/healthz"
}

/// The Front Door profile whose requests are served: its ID, which Front
/// Door sends as X-Azure-FDID.
#[derive(Clone)]
pub struct FrontDoor(pub Arc<str>);

/// The middleware: 403 for requests that didn't come through the
/// deployment's Front Door, whose IP ranges other Front Door profiles share.
pub async fn front_door(State(id): State<FrontDoor>, request: Request, next: Next) -> Response {
    let through = request
        .headers()
        .get("x-azure-fdid")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(&id.0));
    if through || exempt(&request) {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "use the public endpoint\n").into_response()
    }
}

/// The middleware: 429 with Retry-After when the client is over its rate.
pub async fn limit(State(limit): State<RateLimit>, request: Request, next: Next) -> Response {
    if exempt(&request) {
        return next.run(request).await;
    }
    let Some(ip) = limit.client(&request) else {
        return next.run(request).await;
    };
    match limit.take(ip, Instant::now()) {
        Ok(()) => next.run(request).await,
        Err(wait) => {
            // Rounded up: a client that waits as long is let in.
            let seconds = (wait.as_secs_f64().ceil() as u64).max(1);
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                "rate limit exceeded; retry later\n",
            )
                .into_response();
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refills_over_time_per_client() {
        let limit = RateLimit::new(60, 2, ClientIp::Socket);
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        let start = Instant::now();
        assert!(limit.take(a, start).is_ok());
        assert!(limit.take(a, start).is_ok());
        let wait = limit.take(a, start).unwrap_err();
        assert!(wait <= Duration::from_secs(1));
        assert!(limit.take(b, start).is_ok());
        assert!(limit.take(a, start + Duration::from_millis(1100)).is_ok());
    }

    #[test]
    fn idle_clients_are_forgotten_once_there_are_many() {
        let limit = RateLimit::new(60, 2, ClientIp::Socket);
        let start = Instant::now();
        for n in 0..=100_000u32 {
            limit
                .take(IpAddr::from(std::net::Ipv4Addr::from(n)), start)
                .unwrap();
        }
        assert_eq!(limit.clients(), 100_001);
        // Still refilling: kept.
        let soon = start + Duration::from_millis(1500);
        limit.take("203.0.113.9".parse().unwrap(), soon).unwrap();
        assert_eq!(limit.clients(), 100_002);
        // Full again: forgotten when the next client comes.
        let later = start + Duration::from_secs(3);
        limit.take("203.0.113.10".parse().unwrap(), later).unwrap();
        assert_eq!(limit.clients(), 2);
    }

    #[test]
    fn ipv6_clients_share_their_64() {
        let limit = RateLimit::new(60, 1, ClientIp::Socket);
        let start = Instant::now();
        assert!(
            limit
                .take("2001:db8:1:2::1".parse().unwrap(), start)
                .is_ok()
        );
        assert!(
            limit
                .take("2001:db8:1:2::ffff".parse().unwrap(), start)
                .is_err()
        );
        assert!(
            limit
                .take("2001:db8:1:3::1".parse().unwrap(), start)
                .is_ok()
        );
        assert!(
            limit
                .take("::ffff:10.0.0.1".parse().unwrap(), start)
                .is_ok()
        );
        assert!(limit.take("10.0.0.1".parse().unwrap(), start).is_err());
    }

    fn request(headers: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().uri("/mcp");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let mut request = builder.body(axum::body::Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5000))));
        request
    }

    #[test]
    fn finds_the_client_where_told() {
        let forged = [
            ("x-forwarded-for", "203.0.113.9, 198.51.100.7"),
            ("x-azure-socketip", "198.51.100.7"),
        ];
        let header = RateLimit::new(
            60,
            1,
            ClientIp::Header(HeaderName::from_static("x-azure-socketip")),
        );
        assert_eq!(
            header.client(&request(&forged)),
            Some("198.51.100.7".parse().unwrap())
        );
        assert_eq!(
            header.client(&request(&[])),
            Some("127.0.0.1".parse().unwrap())
        );
        let first = RateLimit::new(60, 1, ClientIp::FirstForwardedFor);
        assert_eq!(
            first.client(&request(&forged)),
            Some("203.0.113.9".parse().unwrap())
        );
        let socket = RateLimit::new(60, 1, ClientIp::Socket);
        assert_eq!(
            socket.client(&request(&forged)),
            Some("127.0.0.1".parse().unwrap())
        );
    }
}
