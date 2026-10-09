//! Per-client rate limiting for the public endpoint: a token bucket per IP
//! address, refilled continuously.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

#[derive(Clone)]
pub struct RateLimit {
    inner: Arc<Inner>,
}

struct Inner {
    per_second: f64,
    burst: f64,
    trust_forwarded: bool,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

impl RateLimit {
    /// `per_minute` requests a minute on average, bursts of up to `burst`.
    /// With `trust_forwarded`, the client is the first X-Forwarded-For address
    /// (behind a trusted proxy only).
    pub fn new(per_minute: u32, burst: u32, trust_forwarded: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                per_second: f64::from(per_minute.max(1)) / 60.0,
                burst: f64::from(burst.max(1)),
                trust_forwarded,
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
        let bucket = buckets.entry(ip).or_insert(Bucket {
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
        if self.inner.trust_forwarded
            && let Some(ip) = request
                .headers()
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(',').next())
                .and_then(|first| first.trim().parse().ok())
        {
            return Some(ip);
        }
        request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip())
    }
}

/// The middleware: 429 with Retry-After when the client is over its rate.
pub async fn limit(State(limit): State<RateLimit>, request: Request, next: Next) -> Response {
    let Some(ip) = limit.client(&request) else {
        return next.run(request).await;
    };
    match limit.take(ip, Instant::now()) {
        Ok(()) => next.run(request).await,
        Err(wait) => {
            let seconds = wait.as_secs().max(1);
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
        let limit = RateLimit::new(60, 2, false);
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
}
