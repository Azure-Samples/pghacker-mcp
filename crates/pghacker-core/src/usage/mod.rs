// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Usage telemetry: an event for each request to pghacker-mcp's MCP
//! endpoint, sent to Application Insights' ingestion API. Off unless
//! PGHACKER_USAGE_ENDPOINT is set; PRIVACY.md has the notice.
//!
//! Events wait in a bounded queue and leave in gzipped batches every few
//! seconds, so recording never waits on the network. In Azure the endpoint
//! is the supervisor's proxy, which adds the managed identity's token
//! (scripts/lib/monitor.mjs).
//!
//! An event carries the client's address only in its `ai.location.ip` tag,
//! from which Application Insights derives a country, region and city before
//! it discards the address. A keyed hash of the address tells users apart,
//! and the server starts a new session after 30 idle minutes.

mod event;
mod http;
mod identity;
pub(crate) mod mcp;
mod sender;
pub mod testing;

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use url::Url;
use uuid::Uuid;

pub use event::{ACTION, ERROR, Event, MAX_KEY, MAX_VALUE, tags};
pub use http::{Endpoint, Recorder, Scope, record};
pub use identity::{Identity, SESSION_IDLE};

use crate::config::Env;
use identity::Identities;
use sender::Sender;

/// The product every event names in `common.extname`.
pub const PRODUCT: &str = "pghacker-mcp";

/// How often queued events are sent, unless a full batch is ready sooner.
const INTERVAL: Duration = Duration::from_secs(5);
/// How long shutting down waits for the last events to be sent.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// Where events go, and what every event carries.
#[derive(Clone)]
pub struct Settings {
    /// The ingestion endpoint's origin: Application Insights' (a connection
    /// string's IngestionEndpoint) or a proxy's. Events go to its
    /// `/v2.1/track`.
    pub endpoint: Url,
    /// The resource's instrumentation key, which names it in each event.
    pub instrumentation_key: String,
    /// The key of the users' keyed hashes.
    pub id_key: Vec<u8>,
    /// The service: `ai.cloud.role`.
    pub role: String,
    /// Its version: `ai.application.ver` and `common.extversion`.
    pub version: String,
    /// Its instance, `ai.cloud.roleInstance`: the Container App revision.
    pub instance: Option<String>,
}

/// The telemetry's handle: cheap to clone, and off by default.
#[derive(Clone, Default)]
pub struct Usage {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    settings: Settings,
    identities: Arc<Identities>,
    sender: Arc<Sender>,
}

impl fmt::Debug for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Usage").field("on", &self.is_on()).finish()
    }
}

impl Usage {
    pub fn off() -> Self {
        Self::default()
    }

    /// Usage telemetry as the environment configures it for `role` at
    /// `version`: off without PGHACKER_USAGE_ENDPOINT; with it,
    /// PGHACKER_USAGE_IKEY and PGHACKER_USAGE_ID_KEY are required, and
    /// CONTAINER_APP_REVISION names the instance.
    pub fn from_env(env: &Env, role: &str, version: &str) -> Result<Self> {
        let Some(endpoint) = env.get("PGHACKER_USAGE_ENDPOINT") else {
            return Ok(Self::off());
        };
        let endpoint = Url::parse(endpoint.trim())
            .context("PGHACKER_USAGE_ENDPOINT must be a URL, such as http://127.0.0.1:3111")?;
        ensure!(
            matches!(endpoint.scheme(), "http" | "https"),
            "PGHACKER_USAGE_ENDPOINT must be an http or https URL"
        );
        let instrumentation_key = env.require("PGHACKER_USAGE_IKEY")?.trim().to_owned();
        Uuid::parse_str(&instrumentation_key)
            .context("PGHACKER_USAGE_IKEY must be the resource's instrumentation key, a GUID")?;
        let id_key = env.require("PGHACKER_USAGE_ID_KEY")?.trim();
        ensure!(
            id_key.chars().count() >= 32,
            "PGHACKER_USAGE_ID_KEY must be at least 32 characters"
        );
        Self::new(Settings {
            endpoint,
            instrumentation_key,
            id_key: id_key.as_bytes().to_vec(),
            role: role.to_owned(),
            version: version.to_owned(),
            instance: env.get("CONTAINER_APP_REVISION").map(str::to_owned),
        })
    }

    /// Usage telemetry to `settings.endpoint`, sending in the background.
    /// Needs a Tokio runtime.
    pub fn new(settings: Settings) -> Result<Self> {
        let sender = Sender::new(&settings.endpoint)?;
        Ok(Self::start(settings, sender))
    }

    fn start(settings: Settings, sender: Sender) -> Self {
        let identities = Arc::new(Identities::new(settings.id_key.clone()));
        let sender = Arc::new(sender);
        tokio::spawn(send_in_background(sender.clone(), identities.clone()));
        tracing::info!(endpoint = %settings.endpoint, "usage telemetry is on");
        Self {
            inner: Some(Arc::new(Inner {
                settings,
                identities,
                sender,
            })),
        }
    }

    pub fn is_on(&self) -> bool {
        self.inner.is_some()
    }

    /// The identity of the client at `ip` (a public address) at `now`.
    pub fn identify(&self, ip: IpAddr, now: Instant) -> Option<Identity> {
        let inner = self.inner.as_ref()?;
        Some(inner.identities.identify(ip, now))
    }

    /// Queues `event`, with what every event carries: the service, its
    /// version and instance.
    pub fn record(&self, mut event: Event) {
        let Some(inner) = &self.inner else {
            return;
        };
        let settings = &inner.settings;
        event
            .tag(tags::ROLE, settings.role.clone())
            .tag(tags::VERSION, settings.version.clone())
            .tag(tags::SDK, format!("{PRODUCT}:{}", settings.version))
            .property("common.extname", PRODUCT)
            .property("common.extversion", &settings.version);
        if let Some(instance) = &settings.instance {
            event.tag(tags::ROLE_INSTANCE, instance.clone());
        }
        let envelope = event.envelope(&settings.instrumentation_key);
        inner.sender.push(envelope.to_string());
    }

    /// Sends what's queued now.
    pub async fn flush(&self) {
        if let Some(inner) = &self.inner {
            inner.sender.drain().await;
        }
    }

    /// Takes no more events, and sends what's queued, waiting a few seconds
    /// at most.
    pub async fn shutdown(&self) {
        if let Some(inner) = &self.inner {
            inner.sender.close();
            if tokio::time::timeout(SHUTDOWN_WAIT, inner.sender.drain())
                .await
                .is_err()
            {
                tracing::warn!("usage events were still waiting at shutdown");
            }
        }
    }

    /// Events lost so far: refused, failed to send, or over the queue.
    pub fn lost(&self) -> u64 {
        self.inner.as_ref().map_or(0, |inner| inner.sender.lost())
    }
}

/// Sends the queue every [`INTERVAL`], or when a batch is ready, until the
/// sender closes; forgets idle sessions as it goes.
async fn send_in_background(sender: Arc<Sender>, identities: Arc<Identities>) {
    loop {
        tokio::select! {
            () = tokio::time::sleep(INTERVAL) => {}
            () = sender.wake.notified() => {}
        }
        sender.drain().await;
        identities.prune(Instant::now());
        if sender.is_closed() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testing::Ingestion;

    const IKEY: &str = "11111111-2222-4333-8444-555555555555";
    const ID_KEY: &str = "test-usage-id-key-0123456789abcdef";

    #[test]
    fn the_environment_turns_it_on_with_its_keys() {
        let off = Usage::from_env(&Env::from_pairs([("PGHACKER_USAGE_IKEY", IKEY)]), "r", "1");
        assert!(!off.unwrap().is_on());

        let missing = Env::from_pairs([("PGHACKER_USAGE_ENDPOINT", "http://127.0.0.1:1")]);
        let error = Usage::from_env(&missing, "r", "1").unwrap_err();
        assert!(
            format!("{error:#}").contains("PGHACKER_USAGE_IKEY"),
            "{error:#}"
        );

        let short = Env::from_pairs([
            ("PGHACKER_USAGE_ENDPOINT", "http://127.0.0.1:1"),
            ("PGHACKER_USAGE_IKEY", IKEY),
            ("PGHACKER_USAGE_ID_KEY", "short"),
        ]);
        let error = Usage::from_env(&short, "r", "1").unwrap_err();
        assert!(format!("{error:#}").contains("32"), "{error:#}");

        let bad_key = Env::from_pairs([
            ("PGHACKER_USAGE_ENDPOINT", "http://127.0.0.1:1"),
            ("PGHACKER_USAGE_IKEY", "not-a-guid"),
            ("PGHACKER_USAGE_ID_KEY", ID_KEY),
        ]);
        assert!(Usage::from_env(&bad_key, "r", "1").is_err());
    }

    #[tokio::test]
    async fn events_carry_the_service_and_reach_the_endpoint() {
        let ingestion = Ingestion::start().await;
        let env = Env::from_pairs([
            ("PGHACKER_USAGE_ENDPOINT", ingestion.url.as_str()),
            ("PGHACKER_USAGE_IKEY", IKEY),
            ("PGHACKER_USAGE_ID_KEY", ID_KEY),
            ("CONTAINER_APP_REVISION", "app--rev1"),
        ]);
        let usage = Usage::from_env(&env, "pghacker-mcp", "0.1.0").unwrap();
        assert!(usage.is_on());
        let mut event = Event::new("McpToolCall");
        event.property("toolName", "search");
        usage.record(event);
        usage.shutdown().await;
        // Shut down, it takes no more.
        usage.record(Event::new("late"));
        usage.flush().await;

        let received = ingestion.received();
        assert_eq!(received.len(), 1, "{received:?}");
        let envelope = &received[0];
        assert_eq!(envelope["iKey"], IKEY);
        assert_eq!(envelope["tags"]["ai.cloud.role"], "pghacker-mcp");
        assert_eq!(envelope["tags"]["ai.cloud.roleInstance"], "app--rev1");
        assert_eq!(envelope["tags"]["ai.application.ver"], "0.1.0");
        let properties = &envelope["data"]["baseData"]["properties"];
        assert_eq!(properties["common.extname"], "pghacker-mcp");
        assert_eq!(properties["common.extversion"], "0.1.0");
        assert_eq!(properties["toolName"], "search");
        assert_eq!(usage.lost(), 0);
    }

    #[test]
    fn off_it_records_nothing_and_identifies_no_one() {
        let usage = Usage::off();
        usage.record(Event::new("McpToolCall"));
        assert!(
            usage
                .identify("203.0.113.9".parse().unwrap(), Instant::now())
                .is_none()
        );
        assert_eq!(usage.lost(), 0);
    }
}
