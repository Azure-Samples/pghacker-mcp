// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A fake ingestion endpoint for tests: it answers as Application Insights'
//! does, or with the statuses it's told to, and keeps the envelopes it takes.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use flate2::read::GzDecoder;
use serde_json::{Value, json};
use url::Url;

#[derive(Clone, Default)]
struct Shared {
    received: Arc<Mutex<Vec<Value>>>,
    statuses: Arc<Mutex<VecDeque<u16>>>,
    requests: Arc<AtomicUsize>,
}

/// The endpoint, listening on loopback until the test's runtime ends.
pub struct Ingestion {
    /// Its origin, for [`Settings::endpoint`](super::Settings::endpoint).
    pub url: Url,
    shared: Shared,
}

impl Ingestion {
    pub async fn start() -> Self {
        let shared = Shared::default();
        let app = Router::new()
            .route("/v2.1/track", post(track))
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let addr = listener.local_addr().expect("the port's address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            url: Url::parse(&format!("http://{addr}/")).expect("a URL"),
            shared,
        }
    }

    /// The statuses of the next requests, after which it takes them all.
    pub fn answer(&self, statuses: &[u16]) {
        self.shared
            .statuses
            .lock()
            .expect("the statuses' lock")
            .extend(statuses);
    }

    /// The envelopes taken so far.
    pub fn received(&self) -> Vec<Value> {
        self.shared
            .received
            .lock()
            .expect("the envelopes' lock")
            .clone()
    }

    /// Requests received so far, taken or not.
    pub fn requests(&self) -> usize {
        self.shared.requests.load(Ordering::SeqCst)
    }
}

async fn track(
    State(shared): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    shared.requests.fetch_add(1, Ordering::SeqCst);
    let scripted = shared
        .statuses
        .lock()
        .expect("the statuses' lock")
        .pop_front();
    if let Some(status) = scripted.filter(|status| *status != 200) {
        return (
            StatusCode::from_u16(status).expect("a status"),
            String::new(),
        );
    }
    let gzipped = headers
        .get("content-encoding")
        .is_some_and(|value| value == "gzip");
    let mut text = String::new();
    if !gzipped || GzDecoder::new(&body[..]).read_to_string(&mut text).is_err() {
        return (StatusCode::BAD_REQUEST, "not gzip".into());
    }
    let lines: Result<Vec<Value>, _> = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(serde_json::from_str)
        .collect();
    let Ok(lines) = lines else {
        return (StatusCode::BAD_REQUEST, "not JSON lines".into());
    };
    let n = lines.len();
    shared
        .received
        .lock()
        .expect("the envelopes' lock")
        .extend(lines);
    (
        StatusCode::OK,
        json!({ "itemsReceived": n, "itemsAccepted": n, "errors": [] }).to_string(),
    )
}
