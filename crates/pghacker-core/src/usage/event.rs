// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! One usage event, and the Application Insights envelope it travels in: a
//! custom event (`customEvents`, AppEvents in Log Analytics) with tags,
//! string properties (customDimensions) and numbers (customMeasurements).

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};

use crate::text::truncate_chars;

/// The name of an event that succeeded.
pub const ACTION: &str = "pghacker-mcp/action";
/// The name of an event that failed or was turned away.
pub const ERROR: &str = "pghacker-mcp/error";

/// The envelope tags that events set.
pub mod tags {
    /// The address Application Insights derives the country, region and city
    /// from, and then discards.
    pub const LOCATION_IP: &str = "ai.location.ip";
    pub const USER: &str = "ai.user.id";
    pub const SESSION: &str = "ai.session.id";
    pub const OPERATION: &str = "ai.operation.id";
    pub const OPERATION_NAME: &str = "ai.operation.name";
    pub const ROLE: &str = "ai.cloud.role";
    pub const ROLE_INSTANCE: &str = "ai.cloud.roleInstance";
    pub const VERSION: &str = "ai.application.ver";
    pub const SDK: &str = "ai.internal.sdkVersion";
}

/// Application Insights drops properties with longer keys, and cuts longer
/// values.
pub const MAX_KEY: usize = 150;
pub const MAX_VALUE: usize = 8192;

/// A usage event.
#[derive(Clone, Debug)]
pub struct Event {
    failed: bool,
    time: DateTime<Utc>,
    tags: BTreeMap<&'static str, String>,
    properties: BTreeMap<String, String>,
    measurements: BTreeMap<String, f64>,
}

impl Event {
    /// An event of `action`, its `action` property, at this moment.
    pub fn new(action: &str) -> Self {
        let mut event = Self {
            failed: false,
            time: Utc::now(),
            tags: BTreeMap::new(),
            properties: BTreeMap::new(),
            measurements: BTreeMap::new(),
        };
        event.property("action", action);
        event
    }

    /// Marks the event failed: it's named [`ERROR`] rather than [`ACTION`].
    pub fn fail(&mut self) -> &mut Self {
        self.failed = true;
        self
    }

    pub fn is_failed(&self) -> bool {
        self.failed
    }

    pub fn tag(&mut self, key: &'static str, value: impl Into<String>) -> &mut Self {
        self.tags.insert(key, value.into());
        self
    }

    /// A string property, cut to [`MAX_VALUE`] characters. A property with a
    /// key longer than [`MAX_KEY`] is dropped.
    pub fn property(&mut self, key: impl Into<String>, value: impl AsRef<str>) -> &mut Self {
        self.property_cut(key, value.as_ref(), MAX_VALUE);
        self
    }

    /// A string property cut to `max` characters (at most [`MAX_VALUE`]).
    /// Returns whether anything was lost: the value cut, or the property
    /// dropped for its key.
    pub fn property_cut(&mut self, key: impl Into<String>, value: &str, max: usize) -> bool {
        let key = key.into();
        if key.is_empty() || key.chars().count() > MAX_KEY {
            return true;
        }
        let kept = truncate_chars(value, max.min(MAX_VALUE));
        self.properties.insert(key, kept.to_owned());
        kept.len() < value.len()
    }

    /// A number. Non-finite numbers, which JSON can't carry, are dropped.
    pub fn measure(&mut self, key: impl Into<String>, value: f64) -> &mut Self {
        let key = key.into();
        if value.is_finite() && !key.is_empty() && key.chars().count() <= MAX_KEY {
            self.measurements.insert(key, value);
        }
        self
    }

    pub fn get_tag(&self, key: &str) -> Option<&str> {
        self.tags.get(key).map(String::as_str)
    }

    pub fn get_property(&self, key: &str) -> Option<&str> {
        self.properties.get(key).map(String::as_str)
    }

    pub fn get_measurement(&self, key: &str) -> Option<f64> {
        self.measurements.get(key).copied()
    }

    /// The event in its envelope, for the resource whose instrumentation key
    /// is `instrumentation_key`.
    pub fn envelope(&self, instrumentation_key: &str) -> Value {
        let mut base = Map::new();
        base.insert("ver".into(), json!(2));
        base.insert(
            "name".into(),
            json!(if self.failed { ERROR } else { ACTION }),
        );
        if !self.properties.is_empty() {
            base.insert("properties".into(), json!(self.properties));
        }
        if !self.measurements.is_empty() {
            base.insert("measurements".into(), json!(self.measurements));
        }
        json!({
            "ver": 1,
            "name": "Microsoft.ApplicationInsights.Event",
            "time": self.time.to_rfc3339_opts(SecondsFormat::Millis, true),
            "iKey": instrumentation_key,
            "tags": self.tags,
            "data": { "baseType": "EventData", "baseData": base },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_envelope_is_a_custom_event_named_by_its_outcome() {
        let mut event = Event::new("McpToolCall");
        event
            .tag(tags::USER, "u1")
            .property("toolName", "search")
            .measure("durationMs", 12.5);
        let envelope = event.envelope("00000000-0000-4000-8000-000000000000");
        assert_eq!(envelope["name"], "Microsoft.ApplicationInsights.Event");
        assert_eq!(envelope["iKey"], "00000000-0000-4000-8000-000000000000");
        assert_eq!(envelope["tags"]["ai.user.id"], "u1");
        assert!(envelope["time"].as_str().unwrap().ends_with('Z'));
        let base = &envelope["data"]["baseData"];
        assert_eq!(envelope["data"]["baseType"], "EventData");
        assert_eq!(base["name"], ACTION);
        assert_eq!(base["properties"]["action"], "McpToolCall");
        assert_eq!(base["properties"]["toolName"], "search");
        assert_eq!(base["measurements"]["durationMs"], 12.5);

        event.fail();
        assert_eq!(event.envelope("k")["data"]["baseData"]["name"], ERROR);
    }

    #[test]
    fn values_are_cut_and_long_keys_dropped_as_ingestion_would() {
        let mut event = Event::new("a");
        event.property("long", "é".repeat(MAX_VALUE + 10));
        assert_eq!(
            event.get_property("long").unwrap().chars().count(),
            MAX_VALUE
        );
        assert!(event.property_cut("short", "abcdef", 3));
        assert_eq!(event.get_property("short"), Some("abc"));
        assert!(!event.property_cut("whole", "abc", 3));
        assert!(event.property_cut("k".repeat(MAX_KEY + 1), "dropped", 10));
        event.property("k".repeat(MAX_KEY + 1), "dropped");
        assert_eq!(event.properties.len(), 4);
        event.measure("nan", f64::NAN).measure("inf", f64::INFINITY);
        assert!(event.measurements.is_empty());
        let envelope = event.envelope("k");
        assert!(envelope["data"]["baseData"].get("measurements").is_none());
    }
}
