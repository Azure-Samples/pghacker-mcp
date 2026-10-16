// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Usage telemetry for the home page: a HomePageView event for each visit, in
// the envelope pghacker-mcp's events use and with the same user IDs
// (crates/pghacker-core/src/usage), so the visitors and
// the endpoint's clients can be followed together. Off unless
// PGHACKER_USAGE_ENDPOINT is set. No cookies: a user is a keyed hash of the
// client's address, and a session ends after 30 idle minutes.

import { createHmac, randomUUID } from "node:crypto";
import net from "node:net";
import { setTimeout as delay } from "node:timers/promises";
import { gzipSync } from "node:zlib";

/** The names of the events that succeeded, and of those that failed. */
export const ACTION = "pghacker-mcp/action";
export const ERROR = "pghacker-mcp/error";
/** The product every event names in common.extname. */
export const PRODUCT = "pghacker-mcp";
export const SESSION_IDLE_MS = 30 * 60_000;

// As pghacker_core::usage: Application Insights' limits, and the sender's.
const MAX_KEY = 150;
const MAX_VALUE = 8192;
const QUEUE = 10_000;
const BATCH = 500;
const INTERVAL_MS = 5000;
const TIMEOUT_MS = 10_000;
const RETRY_AFTER_MS = 2000;
const RETRYABLE = new Set([408, 429, 500, 502, 503, 504]);
const WARN_EVERY_MS = 60_000;
const MAX_SESSIONS = 100_000;
const GUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** An address's 4 or 16 bytes (an IPv4-mapped IPv6 address's 4), or undefined. */
export function addressBytes(text) {
  const address = String(text ?? "").trim().replace(/%.*$/, "");
  const kind = net.isIP(address);
  if (kind === 4) return Buffer.from(address.split(".").map(Number));
  if (kind !== 6) return undefined;
  const [head, tail] = address.includes("::") ? address.split("::") : [address, undefined];
  const groups = (part) => (part ? part.split(":") : []);
  const front = groups(head);
  const back = groups(tail);
  const last = tail === undefined ? front : back;
  if (last.at(-1)?.includes(".")) {
    const [a, b, c, d] = last.pop().split(".").map(Number);
    last.push(((a << 8) | b).toString(16), ((c << 8) | d).toString(16));
  }
  const all = tail === undefined
    ? front
    : [...front, ...Array(8 - front.length - back.length).fill("0"), ...back];
  const bytes = Buffer.alloc(16);
  all.forEach((group, i) => bytes.writeUInt16BE(Number.parseInt(group, 16), i * 2));
  const mapped = bytes.subarray(0, 10).every((byte) => byte === 0) && bytes[10] === 0xff && bytes[11] === 0xff;
  return mapped ? bytes.subarray(12) : bytes;
}

/** Whether `text` can be a client's public address: not loopback, private, link-local or unspecified. */
export function isPublic(text) {
  const bytes = addressBytes(text);
  if (!bytes) return false;
  if (bytes.length === 4) {
    const [a, b] = bytes;
    return !(
      a === 127 || a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168)
      || (a === 169 && b === 254) || bytes.every((byte) => byte === 0) || bytes.every((byte) => byte === 255)
    );
  }
  const unspecified = bytes.every((byte) => byte === 0);
  const loopback = bytes.subarray(0, 15).every((byte) => byte === 0) && bytes[15] === 1;
  const uniqueLocal = (bytes[0] & 0xfe) === 0xfc;
  const linkLocal = bytes[0] === 0xfe && (bytes[1] & 0xc0) === 0x80;
  return !(unspecified || loopback || uniqueLocal || linkLocal);
}

/**
 * The user at `address`, as pghacker-mcp names them: in hex, the first 128
 * bits of HMAC-SHA256, under `key`, of "ipv4:" and the 4 bytes of the
 * address, or "ipv6:" and the 16 of its /64.
 */
export function userId(key, address) {
  const bytes = addressBytes(address);
  if (!bytes) return undefined;
  const client = Buffer.from(bytes);
  if (client.length === 16) client.fill(0, 8);
  return createHmac("sha256", key)
    .update(client.length === 4 ? "ipv4:" : "ipv6:")
    .update(client)
    .digest()
    .subarray(0, 16)
    .toString("hex");
}

/** At most `max` characters of `text`. */
function cut(text, max) {
  return text.length <= max ? text : Array.from(text).slice(0, max).join("");
}

/** A usage event of `action`: tags, properties and measurements. */
export function createEvent(action) {
  const event = { failed: false, time: new Date(), tags: {}, properties: {}, measurements: {} };
  setProperty(event, "action", action);
  return event;
}

/** A property, cut to `max` characters, unless its key is too long. */
export function setProperty(event, key, value, max = MAX_VALUE) {
  if (key && key.length <= MAX_KEY) event.properties[key] = cut(String(value), Math.min(max, MAX_VALUE));
}

/** The event in its Application Insights envelope. */
export function envelope(event, instrumentationKey) {
  const base = { ver: 2, name: event.failed ? ERROR : ACTION };
  if (Object.keys(event.properties).length) base.properties = event.properties;
  if (Object.keys(event.measurements).length) base.measurements = event.measurements;
  return {
    ver: 1,
    name: "Microsoft.ApplicationInsights.Event",
    time: event.time.toISOString(),
    iKey: instrumentationKey,
    tags: event.tags,
    data: { baseType: "EventData", baseData: base },
  };
}

function value(text) {
  return typeof text === "string" && text.trim() !== "" ? text.trim() : undefined;
}

/**
 * The settings in the environment: the ingestion endpoint (the supervisor's
 * proxy), the instrumentation key, the users' key, the instance, and the
 * Front Door profile whose requests name their client; or undefined, off.
 */
export function readUsageSettings(env = process.env) {
  const endpoint = value(env.PGHACKER_USAGE_ENDPOINT);
  if (!endpoint) return undefined;
  const url = new URL(endpoint);
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error("PGHACKER_USAGE_ENDPOINT must be an http or https URL.");
  }
  const instrumentationKey = value(env.PGHACKER_USAGE_IKEY) ?? "";
  if (!GUID.test(instrumentationKey)) {
    throw new Error("PGHACKER_USAGE_IKEY must be the resource's instrumentation key, a GUID.");
  }
  const idKey = value(env.PGHACKER_USAGE_ID_KEY) ?? "";
  if (Array.from(idKey).length < 32) throw new Error("PGHACKER_USAGE_ID_KEY must be at least 32 characters.");
  return {
    endpoint: url,
    instrumentationKey,
    idKey,
    instance: value(env.CONTAINER_APP_REVISION),
    frontDoorId: value(env.PGHACKER_FRONT_DOOR_ID),
  };
}

const OFF = Object.freeze({
  on: false,
  lost: 0,
  pageView() {},
  record() {},
  identify() { return undefined; },
  async flush() {},
  async close() {},
});

/**
 * Usage telemetry for `role` with `settings` (readUsageSettings), sending in
 * the background; without settings, a recorder that records nothing.
 */
export function createUsage(settings, {
  role = "pghacker-home", version = "0.1.0", fetchImpl = fetch, retryAfterMs = RETRY_AFTER_MS,
  log = (message) => console.error(message),
} = {}) {
  if (!settings) return OFF;
  const url = new URL("v2.1/track", settings.endpoint);
  const queue = [];
  const sessions = new Map();
  let lost = 0;
  let warnedAt = -Infinity;
  let closed = false;
  let draining = Promise.resolve();

  function lose(count, error) {
    lost += count;
    const now = Date.now();
    if (now - warnedAt >= WARN_EVERY_MS) {
      warnedAt = now;
      log(`${count} usage events weren't sent (${lost} in all): ${error.message}`);
    }
  }

  function prune(now = Date.now()) {
    for (const [user, session] of sessions) {
      if (now - session.seen >= SESSION_IDLE_MS) sessions.delete(user);
    }
  }

  /** The user and session of the client at a public `address` at `now`. */
  function identify(address, now = Date.now()) {
    if (!isPublic(address)) return undefined;
    const user = userId(settings.idKey, address);
    let session = sessions.get(user);
    if (session && now - session.seen < SESSION_IDLE_MS) {
      session.seen = now;
    } else {
      if (sessions.size >= MAX_SESSIONS) prune(now);
      if (sessions.size >= MAX_SESSIONS) sessions.clear();
      session = { id: randomUUID().replaceAll("-", ""), seen: now };
      sessions.set(user, session);
    }
    return { user, session: session.id };
  }

  /** Sends a batch, retrying once after a failure that may pass; returns the events refused. */
  async function send(lines) {
    const body = gzipSync(lines.join("\n"));
    let failure;
    for (let attempt = 1; attempt <= 2; attempt += 1) {
      if (attempt > 1) await delay(retryAfterMs);
      let response;
      try {
        response = await fetchImpl(url, {
          method: "POST",
          headers: { "content-type": "application/x-json-stream", "content-encoding": "gzip" },
          body,
          signal: AbortSignal.timeout(TIMEOUT_MS),
        });
      } catch (error) {
        failure = error;
        continue;
      }
      if (response.status === 206) {
        const answer = await response.json().catch(() => ({}));
        const refused = (answer.itemsReceived ?? lines.length) - (answer.itemsAccepted ?? 0);
        return refused > 0 ? { count: refused, reason: answer.errors?.[0]?.message ?? "no reason given" } : undefined;
      }
      if (response.ok) {
        await response.arrayBuffer().catch(() => undefined);
        return undefined;
      }
      const text = (await response.text().catch(() => "")).trim().slice(0, 300);
      failure = new Error(`HTTP ${response.status}${text ? `: ${text}` : ""}`);
      if (!RETRYABLE.has(response.status)) break;
    }
    throw failure;
  }

  async function drain() {
    while (queue.length) {
      const batch = queue.splice(0, BATCH);
      try {
        const refused = await send(batch);
        if (refused) lose(refused.count, new Error(`the endpoint refused ${refused.count} of ${batch.length}: ${refused.reason}`));
      } catch (error) {
        lose(batch.length, error);
      }
    }
  }

  /** Sends what's queued; one send at a time. */
  function flush() {
    draining = draining.then(drain);
    return draining;
  }

  /** Queues `event` with what every event carries: the service, its version and instance. */
  function record(event) {
    if (closed) return;
    Object.assign(event.tags, {
      "ai.cloud.role": role,
      "ai.application.ver": version,
      "ai.internal.sdkVersion": `${PRODUCT}:${version}`,
      ...(settings.instance ? { "ai.cloud.roleInstance": settings.instance } : {}),
    });
    setProperty(event, "common.extname", PRODUCT);
    setProperty(event, "common.extversion", version);
    if (queue.length >= QUEUE) {
      lose(1, new Error(`${QUEUE} events are already waiting`));
      return;
    }
    queue.push(JSON.stringify(envelope(event, settings.instrumentationKey)));
    if (queue.length >= BATCH) flush();
  }

  /**
   * A visit to the page, from `request`: its client, which only requests
   * through this deployment's Front Door name (in X-Azure-SocketIP; without
   * a Front Door, the connection's peer), the path, the campaign in its
   * utm_ parameters, the referrer's host and the User-Agent.
   */
  function pageView(request, now = Date.now()) {
    const { headers } = request;
    let address;
    if (settings.frontDoorId) {
      const fdid = String(headers["x-azure-fdid"] ?? "").trim().toLowerCase();
      if (fdid === settings.frontDoorId.toLowerCase()) address = value(headers["x-azure-socketip"]);
    } else {
      address = request.socket?.remoteAddress;
    }
    const identity = address ? identify(address, now) : undefined;
    const event = createEvent("HomePageView");
    Object.assign(event.tags, {
      // Without an address of its own, Application Insights would locate the
      // event's sender, this server.
      "ai.location.ip": identity ? address.replace(/%.*$/, "") : "0.0.0.0",
      "ai.operation.id": randomUUID().replaceAll("-", ""),
      "ai.operation.name": "GET /",
      ...(identity ? { "ai.user.id": identity.user, "ai.session.id": identity.session } : {}),
    });
    setProperty(event, "endpoint", "public");
    const url = new URL(request.url ?? "/", "http://localhost");
    setProperty(event, "path", url.pathname, 256);
    for (const name of ["utm_source", "utm_medium", "utm_campaign"]) {
      const campaign = url.searchParams.get(name);
      if (campaign) setProperty(event, name, campaign, 128);
    }
    const referrer = value(headers.referer);
    if (referrer) {
      try {
        setProperty(event, "referrerHost", new URL(referrer).host, 256);
      } catch {
        // Not a URL: no referrer.
      }
    }
    if (value(headers["user-agent"])) setProperty(event, "userAgent", headers["user-agent"], 512);
    record(event);
  }

  const timer = setInterval(() => {
    flush();
    prune();
  }, INTERVAL_MS);
  timer.unref();

  return {
    on: true,
    get lost() {
      return lost;
    },
    pageView,
    record,
    identify,
    flush,
    /** Takes no more events, and sends what's queued, waiting 5 seconds at most. */
    async close() {
      closed = true;
      clearInterval(timer);
      await Promise.race([flush(), delay(5000, undefined, { ref: false })]);
    },
  };
}
