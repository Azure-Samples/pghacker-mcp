// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! How many requests the public listener serves at once: a few per client
//! (`pghacker_core::clients`: an IP address, an IPv6 client's /64), answered
//! 429 past that, and so many in all, past which a request waits a while for
//! its turn and is then answered 503. A request is admitted before its call
//! runs, so before the call embeds a query or takes a connection: a client
//! within its rate can't hold every connection of the pool, nor every
//! socket to the embeddings service. Calls past the connections wait for
//! one, as the agents' calls do.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use pghacker_core::clients;
use tokio::sync::Semaphore;

use crate::ratelimit::{ClientIp, exempt};

#[derive(Clone)]
pub struct Admission {
    inner: Arc<Inner>,
}

struct Inner {
    per_client: usize,
    turns: Semaphore,
    wait: Duration,
    client_ip: ClientIp,
    /// Each client's requests running or waiting for their turn.
    clients: Mutex<HashMap<IpAddr, usize>>,
}

/// A client's place among its requests, given back when dropped.
struct Place {
    inner: Arc<Inner>,
    client: IpAddr,
}

impl Drop for Place {
    fn drop(&mut self) {
        let mut clients = self.inner.clients.lock().expect("admission's lock");
        if let Some(count) = clients.get_mut(&self.client) {
            *count -= 1;
            if *count == 0 {
                clients.remove(&self.client);
            }
        }
    }
}

impl Admission {
    /// At most `total` requests at once, `per_client` of them a client's;
    /// one past `total` waits up to `wait` for its turn.
    pub fn new(total: usize, per_client: usize, wait: Duration, client_ip: ClientIp) -> Self {
        let total = total.max(1);
        Self {
            inner: Arc::new(Inner {
                per_client: per_client.clamp(1, total),
                turns: Semaphore::new(total),
                wait,
                client_ip,
                clients: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// A place for `ip`'s client, or none while it has `per_client`
    /// requests.
    fn enter(&self, ip: IpAddr) -> Option<Place> {
        let client = clients::bucket(ip);
        let mut clients = self.inner.clients.lock().expect("admission's lock");
        let count = clients.entry(client).or_insert(0);
        if *count >= self.inner.per_client {
            return None;
        }
        *count += 1;
        Some(Place {
            inner: Arc::clone(&self.inner),
            client,
        })
    }

    /// The clients with requests.
    #[cfg(test)]
    fn clients(&self) -> usize {
        self.inner.clients.lock().unwrap().len()
    }
}

/// The middleware: 429 for a client over its share, and 503 once a request
/// has waited its while for a turn; both with Retry-After.
pub async fn admit(State(admission): State<Admission>, request: Request, next: Next) -> Response {
    if exempt(&request) {
        return next.run(request).await;
    }
    let inner = &admission.inner;
    let _place = match inner.client_ip.of(&request) {
        Some(ip) => match admission.enter(ip) {
            Some(place) => Some(place),
            None => {
                return refused(
                    StatusCode::TOO_MANY_REQUESTS,
                    "too many requests at once from this client; retry when one finishes\n",
                );
            }
        },
        None => None,
    };
    let Ok(Ok(_turn)) = tokio::time::timeout(inner.wait, inner.turns.acquire()).await else {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "the server is busy; retry shortly\n",
        );
    };
    next.run(request).await
}

fn refused(status: StatusCode, message: &'static str) -> Response {
    let mut response = (status, message).into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_has_its_share_and_gets_it_back() {
        let admission = Admission::new(4, 2, Duration::from_secs(1), ClientIp::Socket);
        let ip: IpAddr = "203.0.113.9".parse().unwrap();
        let first = admission.enter(ip).unwrap();
        let second = admission.enter(ip).unwrap();
        assert!(admission.enter(ip).is_none());
        // Another client, and IPv6 clients by their /64.
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        let neighbor: IpAddr = "2001:db8::2".parse().unwrap();
        let _third = admission.enter(v6).unwrap();
        let _fourth = admission.enter(neighbor).unwrap();
        assert!(admission.enter(v6).is_none());
        drop(first);
        let again = admission.enter(ip).unwrap();
        drop(second);
        drop(again);
        assert_eq!(admission.clients(), 1);
    }

    #[test]
    fn a_share_is_at_least_one_and_at_most_the_total() {
        let admission = Admission::new(2, 5, Duration::from_secs(1), ClientIp::Socket);
        let ip: IpAddr = "203.0.113.9".parse().unwrap();
        let _held = [admission.enter(ip).unwrap(), admission.enter(ip).unwrap()];
        assert!(admission.enter(ip).is_none());
        let admission = Admission::new(0, 0, Duration::from_secs(1), ClientIp::Socket);
        let _one = admission.enter(ip).unwrap();
        assert!(admission.enter(ip).is_none());
    }
}
