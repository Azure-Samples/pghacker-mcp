// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Sends events to the ingestion endpoint: their envelopes as JSON lines,
//! gzipped, a batch at a time. Recording only queues, so a slow or failing
//! endpoint costs events, never requests.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use flate2::Compression;
use flate2::write::GzEncoder;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_ENCODING, CONTENT_TYPE};
use serde_json::Value;
use tokio::sync::Notify;
use url::Url;

use crate::text::truncate_chars;

/// Events waiting at most; beyond, new ones are dropped.
const QUEUE: usize = 10_000;
/// Events in a request at most.
pub(crate) const BATCH: usize = 500;
/// How long a request may take.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The wait before a batch's one retry, after a failure that may pass.
const RETRY_AFTER: Duration = Duration::from_secs(2);
/// At most one warning about lost events this often; the rest are debug.
const WARN_EVERY: Duration = Duration::from_secs(60);

pub(crate) struct Sender {
    url: Url,
    http: reqwest::Client,
    retry_after: Duration,
    queue: Mutex<VecDeque<String>>,
    /// Woken when a batch is ready or the sender closes.
    pub(crate) wake: Notify,
    closed: AtomicBool,
    lost: AtomicU64,
    warned: Mutex<Option<Instant>>,
}

impl Sender {
    /// A sender to `endpoint`'s `/v2.1/track`.
    pub fn new(endpoint: &Url) -> Result<Self> {
        Self::with_retry_after(endpoint, RETRY_AFTER)
    }

    pub(crate) fn with_retry_after(endpoint: &Url, retry_after: Duration) -> Result<Self> {
        let url = endpoint
            .join("v2.1/track")
            .context("the ingestion endpoint isn't a base URL")?;
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .context("cannot build the usage telemetry's HTTP client")?;
        Ok(Self {
            url,
            http,
            retry_after,
            queue: Mutex::new(VecDeque::new()),
            wake: Notify::new(),
            closed: AtomicBool::new(false),
            lost: AtomicU64::new(0),
            warned: Mutex::new(None),
        })
    }

    /// Queues an envelope, a JSON line. Once closed, or with the queue full,
    /// it's lost.
    pub fn push(&self, line: String) {
        if self.is_closed() {
            return;
        }
        let queued = {
            let mut queue = self.queue.lock().expect("the queue's lock");
            if queue.len() >= QUEUE {
                None
            } else {
                queue.push_back(line);
                Some(queue.len())
            }
        };
        match queued {
            None => self.lose(1, &anyhow!("{QUEUE} events are already waiting")),
            Some(waiting) if waiting >= BATCH => self.wake.notify_one(),
            Some(_) => {}
        }
    }

    /// Sends everything queued, a batch at a time.
    pub async fn drain(&self) {
        loop {
            let batch: Vec<String> = {
                let mut queue = self.queue.lock().expect("the queue's lock");
                let n = queue.len().min(BATCH);
                queue.drain(..n).collect()
            };
            if batch.is_empty() {
                return;
            }
            match self.send(&batch).await {
                Ok(None) => {}
                Ok(Some((refused, reason))) => self.lose(
                    refused,
                    &anyhow!(
                        "the endpoint refused {refused} of {} events: {reason}",
                        batch.len()
                    ),
                ),
                Err(error) => self.lose(batch.len() as u64, &error),
            }
        }
    }

    /// Takes no more events: they're lost from now on.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Events lost so far.
    pub fn lost(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }

    /// Sends a batch, retrying once after a failure that may pass. Returns
    /// how many events the endpoint refused, and why, if it took only some.
    async fn send(&self, lines: &[String]) -> Result<Option<(u64, String)>> {
        let body = gzip(lines)?;
        let mut attempt = 1;
        loop {
            let answer = self
                .http
                .post(self.url.clone())
                .header(CONTENT_TYPE, "application/x-json-stream")
                .header(CONTENT_ENCODING, "gzip")
                .body(body.clone())
                .send()
                .await;
            let failure = match answer {
                Ok(response) if response.status() == StatusCode::PARTIAL_CONTENT => {
                    let answer: Value = response.json().await.unwrap_or_default();
                    return Ok(refused(&answer, lines.len() as u64));
                }
                Ok(response) if response.status().is_success() => return Ok(None),
                Ok(response) => {
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    let error = anyhow!("HTTP {status}: {}", truncate_chars(text.trim(), 300));
                    if !retryable(status) {
                        return Err(error);
                    }
                    error
                }
                Err(error) => anyhow::Error::new(error.without_url()),
            };
            if attempt == 2 {
                return Err(failure);
            }
            attempt += 1;
            tokio::time::sleep(self.retry_after).await;
        }
    }

    fn lose(&self, count: u64, error: &anyhow::Error) {
        let total = self.lost.fetch_add(count, Ordering::Relaxed) + count;
        let now = Instant::now();
        let mut warned = self.warned.lock().expect("the warning's lock");
        if warned.is_none_or(|at| now.duration_since(at) >= WARN_EVERY) {
            *warned = Some(now);
            tracing::warn!(lost = count, total, error = %format!("{error:#}"), "usage events weren't sent");
        } else {
            tracing::debug!(lost = count, total, error = %format!("{error:#}"), "usage events weren't sent");
        }
    }
}

/// From a 206 answer, the events refused and the first reason given.
fn refused(answer: &Value, sent: u64) -> Option<(u64, String)> {
    let received = answer["itemsReceived"].as_u64().unwrap_or(sent);
    let accepted = answer["itemsAccepted"].as_u64().unwrap_or(0);
    let refused = received.saturating_sub(accepted);
    (refused > 0).then(|| {
        let reason = answer["errors"][0]["message"]
            .as_str()
            .unwrap_or("no reason given");
        (refused, truncate_chars(reason, 300).to_owned())
    })
}

/// Failures worth one more try: timeouts, throttling and outages.
fn retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

fn gzip(lines: &[String]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            encoder.write_all(b"\n")?;
        }
        encoder.write_all(line.as_bytes())?;
    }
    Ok(encoder.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::testing::Ingestion;
    use serde_json::json;

    fn sender(ingestion: &Ingestion) -> Sender {
        Sender::with_retry_after(&ingestion.url, Duration::from_millis(10)).unwrap()
    }

    #[tokio::test]
    async fn batches_arrive_as_gzipped_json_lines() {
        let ingestion = Ingestion::start().await;
        let sender = sender(&ingestion);
        for i in 0..(BATCH + 3) {
            sender.push(json!({ "n": i }).to_string());
        }
        sender.drain().await;
        let received = ingestion.received();
        assert_eq!(received.len(), BATCH + 3);
        assert_eq!(received[BATCH + 2]["n"], BATCH + 2);
        assert_eq!(ingestion.requests(), 2);
        assert_eq!(sender.lost(), 0);
    }

    #[tokio::test]
    async fn a_failure_that_may_pass_is_retried_once_then_the_batch_is_lost() {
        let ingestion = Ingestion::start().await;
        let sender = sender(&ingestion);

        ingestion.answer(&[503]);
        sender.push(json!({ "n": 1 }).to_string());
        sender.drain().await;
        assert_eq!(ingestion.received().len(), 1);
        assert_eq!(sender.lost(), 0);

        ingestion.answer(&[429, 503]);
        sender.push(json!({ "n": 2 }).to_string());
        sender.drain().await;
        assert_eq!(ingestion.received().len(), 1);
        assert_eq!(sender.lost(), 1);

        // Refusals aren't retried.
        ingestion.answer(&[400]);
        sender.push(json!({ "n": 3 }).to_string());
        sender.drain().await;
        assert_eq!(sender.lost(), 2);
        assert_eq!(ingestion.requests(), 5);
    }

    #[test]
    fn a_partial_answer_counts_the_refused_events() {
        let answer = json!({
            "itemsReceived": 3, "itemsAccepted": 1,
            "errors": [{ "index": 0, "statusCode": 400, "message": "Field 'name' is required" }],
        });
        assert_eq!(
            refused(&answer, 3),
            Some((2, "Field 'name' is required".to_owned()))
        );
        assert_eq!(
            refused(&json!({ "itemsReceived": 2, "itemsAccepted": 2 }), 2),
            None
        );
    }

    #[tokio::test]
    async fn closed_or_full_the_queue_loses_events() {
        let ingestion = Ingestion::start().await;
        let sender = sender(&ingestion);
        for i in 0..(QUEUE + 2) {
            sender.push(i.to_string());
        }
        assert_eq!(sender.lost(), 2);
        sender.close();
        sender.push("late".into());
        sender.drain().await;
        assert_eq!(ingestion.received().len(), QUEUE);
    }
}
