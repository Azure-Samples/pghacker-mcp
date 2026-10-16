// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Usage telemetry's way to Application Insights. The resource takes only
// Entra-authenticated telemetry (local authentication is off), so the
// supervisor runs a second token proxy (scripts/lib/foundry.mjs) on loopback:
// it forwards the events of pghacker-mcp and the home page to the resource's
// ingestion endpoint with the managed identity's token for Azure Monitor.
// Neither process gets the identity's endpoint, or anything but this path.

import { startTokenProxy } from "./foundry.mjs";

/** The audience of Azure Monitor's tokens in Azure's public cloud. */
export const MONITOR_AUDIENCE = "https://monitor.azure.com";

/** Where Entra-authenticated telemetry goes on an ingestion endpoint. */
export const TRACK_PATH = "/v2.1/track";

const GUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * An Application Insights connection string's instrumentation key, ingestion
 * endpoint (its https origin) and token audience (AADAudience in sovereign
 * clouds, else Azure Monitor's).
 */
export function parseConnectionString(text) {
  const parts = {};
  for (const part of String(text ?? "").split(";")) {
    const at = part.indexOf("=");
    if (at > 0) parts[part.slice(0, at).trim().toLowerCase()] = part.slice(at + 1).trim();
  }
  const instrumentationKey = parts.instrumentationkey ?? "";
  if (!GUID.test(instrumentationKey)) {
    throw new Error("APPLICATIONINSIGHTS_CONNECTION_STRING must have the resource's InstrumentationKey.");
  }
  let ingestion;
  try {
    ingestion = new URL(parts.ingestionendpoint ?? "");
  } catch {
    ingestion = undefined;
  }
  if (ingestion?.protocol !== "https:") {
    throw new Error("APPLICATIONINSIGHTS_CONNECTION_STRING must have an https IngestionEndpoint.");
  }
  const audience = (parts.aadaudience || MONITOR_AUDIENCE).replace(/\/+$/, "");
  return { instrumentationKey, ingestionEndpoint: ingestion.origin, audience };
}

/**
 * Listens on `host`:`port` and forwards requests to /v2.1/track to
 * `upstream`, the ingestion endpoint, with `token()`'s bearer token;
 * resolves with the server once it listens.
 */
export function startMonitorProxy(options) {
  return startTokenProxy({ name: "Application Insights", prefix: TRACK_PATH, ...options });
}
