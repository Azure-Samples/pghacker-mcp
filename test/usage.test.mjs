// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { gunzipSync } from "node:zlib";
import {
  ACTION, SESSION_IDLE_MS, addressBytes, createUsage, isPublic, readUsageSettings, userId,
} from "../scripts/lib/usage.mjs";

const KEY = "test-usage-id-key-0123456789abcdef";
const IKEY = "11111111-2222-4333-8444-555555555555";
const FRONT_DOOR_ID = "6e1b0e1c-0000-4000-8000-000000000000";
// The same vectors as crates/pghacker-core/src/usage/identity.rs: the home
// page's users are the endpoint's.
const V4_USER = "57d404ffeaedb6e25cfdb7e423e6582d";
const V6_USER = "ed2b6a28eaba55c9854936ca33436922";

/** A fetch that records what it was sent and answers with `statuses`, then 200s. */
function fakeIngestion(statuses = []) {
  const sent = [];
  const fetchImpl = async (url, options) => {
    sent.push({
      url: String(url),
      headers: options.headers,
      events: gunzipSync(options.body).toString().split("\n").map((line) => JSON.parse(line)),
    });
    const status = statuses.shift() ?? 200;
    if (typeof status === "object") return new Response(JSON.stringify(status.body), { status: status.status });
    return new Response(status === 200 ? JSON.stringify({ itemsReceived: 1, itemsAccepted: 1 }) : "", { status });
  };
  return { sent, fetchImpl, events: () => sent.flatMap((request) => request.events) };
}

function settings(overrides = {}) {
  return readUsageSettings({
    PGHACKER_USAGE_ENDPOINT: "http://127.0.0.1:3111",
    PGHACKER_USAGE_IKEY: IKEY,
    PGHACKER_USAGE_ID_KEY: KEY,
    PGHACKER_FRONT_DOOR_ID: FRONT_DOOR_ID,
    CONTAINER_APP_REVISION: "pghacker-example--0000016",
    ...overrides,
  });
}

function visit({ fdid = FRONT_DOOR_ID, socketIp = "203.0.113.9", url = "/", headers = {} } = {}) {
  return {
    url,
    socket: { remoteAddress: "127.0.0.1" },
    headers: {
      ...(fdid ? { "x-azure-fdid": fdid } : {}),
      "x-azure-socketip": socketIp,
      "x-forwarded-for": "10.9.9.9",
      ...headers,
    },
  };
}

test("users are keyed hashes of the client, as pghacker-mcp computes them", () => {
  assert.equal(userId(KEY, "203.0.113.9"), V4_USER);
  assert.equal(userId(KEY, "::ffff:203.0.113.9"), V4_USER);
  assert.equal(userId(KEY, "2001:db8:1:2::1"), V6_USER);
  assert.equal(userId(KEY, "2001:db8:1:2:ffff::9"), V6_USER);
  assert.notEqual(userId(KEY, "2001:db8:1:3::1"), V6_USER);
  assert.notEqual(userId("another-key-another-key-another-key", "203.0.113.9"), V4_USER);
  assert.equal(userId(KEY, "not an address"), undefined);
  assert.deepEqual([...addressBytes("::")], new Array(16).fill(0));
  assert.deepEqual([...addressBytes("1::")], [0, 1, ...new Array(14).fill(0)]);
  assert.deepEqual([...addressBytes("fe80::1%eth0")].slice(0, 2), [0xfe, 0x80]);
});

test("only public addresses are public, as pghacker-mcp counts them", () => {
  for (const address of ["203.0.113.9", "8.8.8.8", "2001:db8::1", "2606:4700::1111"]) {
    assert.ok(isPublic(address), address);
  }
  for (const address of [
    "127.0.0.1", "10.1.2.3", "192.168.0.1", "172.16.0.1", "169.254.0.1", "0.0.0.0", "255.255.255.255",
    "::1", "::", "fd00::1", "fe80::1", "::ffff:127.0.0.1", "", "nope",
  ]) {
    assert.ok(!isPublic(address), address);
  }
  assert.ok(isPublic("172.32.0.1"), "outside 172.16/12");
});

test("a page view names its client only through this deployment's Front Door", async () => {
  const ingestion = fakeIngestion();
  const usage = createUsage(settings(), { fetchImpl: ingestion.fetchImpl, version: "0.1.0" });
  const start = Date.now();
  usage.pageView(visit({
    url: "/?utm_source=newsletter&utm_campaign=announcement&x=1",
    headers: { referer: "https://news.example.org/2026/10/pghacker/", "user-agent": "Mozilla/5.0 test" },
  }), start);
  usage.pageView(visit(), start + SESSION_IDLE_MS - 1000);
  usage.pageView(visit(), start + 2 * SESSION_IDLE_MS);
  // Another Front Door profile's request, or none: no client.
  usage.pageView(visit({ fdid: "00000000-0000-4000-8000-000000000000" }), start);
  usage.pageView(visit({ fdid: "" }), start);
  await usage.flush();

  const [first, again, later, other, none] = ingestion.events();
  assert.equal(ingestion.sent.length, 1);
  assert.equal(ingestion.sent[0].url, "http://127.0.0.1:3111/v2.1/track");
  assert.equal(ingestion.sent[0].headers["content-encoding"], "gzip");
  assert.equal(ingestion.sent[0].headers["content-type"], "application/x-json-stream");

  assert.equal(first.name, "Microsoft.ApplicationInsights.Event");
  assert.equal(first.iKey, IKEY);
  assert.equal(first.data.baseData.name, ACTION);
  assert.deepEqual(first.data.baseData.properties, {
    action: "HomePageView",
    endpoint: "public",
    path: "/",
    utm_source: "newsletter",
    utm_campaign: "announcement",
    referrerHost: "news.example.org",
    userAgent: "Mozilla/5.0 test",
    "common.extname": "pghacker-mcp",
    "common.extversion": "0.1.0",
  });
  assert.equal(first.tags["ai.location.ip"], "203.0.113.9");
  assert.equal(first.tags["ai.user.id"], V4_USER);
  assert.match(first.tags["ai.session.id"], /^[0-9a-f]{32}$/);
  assert.equal(first.tags["ai.cloud.role"], "pghacker-home");
  assert.equal(first.tags["ai.cloud.roleInstance"], "pghacker-example--0000016");
  assert.equal(first.tags["ai.operation.name"], "GET /");
  assert.ok(!JSON.stringify(first).includes("10.9.9.9"), "X-Forwarded-For isn't the client");

  assert.equal(again.tags["ai.session.id"], first.tags["ai.session.id"], "within half an hour");
  assert.notEqual(later.tags["ai.session.id"], first.tags["ai.session.id"], "after half an hour idle");
  assert.equal(later.tags["ai.user.id"], V4_USER);
  for (const event of [other, none]) {
    assert.equal(event.tags["ai.location.ip"], "0.0.0.0");
    assert.equal(event.tags["ai.user.id"], undefined);
    assert.ok(!JSON.stringify(event).includes("203.0.113.9"));
  }
  assert.equal(usage.lost, 0);
  await usage.close();
});

test("without a Front Door profile, the connection's peer is the client", async () => {
  const ingestion = fakeIngestion();
  const usage = createUsage(settings({ PGHACKER_FRONT_DOOR_ID: "" }), { fetchImpl: ingestion.fetchImpl });
  usage.pageView({ ...visit(), socket: { remoteAddress: "198.51.100.7" } });
  usage.pageView(visit());
  await usage.flush();
  const [direct, local] = ingestion.events();
  assert.equal(direct.tags["ai.location.ip"], "198.51.100.7");
  assert.equal(local.tags["ai.location.ip"], "0.0.0.0", "a loopback peer is no client");
  await usage.close();
});

test("a failure that may pass is retried once, then the batch is lost; refusals are counted", async () => {
  const ingestion = fakeIngestion([
    503, 200,
    429, 503,
    400,
    { status: 206, body: { itemsReceived: 2, itemsAccepted: 1, errors: [{ index: 1, statusCode: 400, message: "bad" }] } },
  ]);
  const logged = [];
  const usage = createUsage(settings(), {
    fetchImpl: ingestion.fetchImpl, retryAfterMs: 1, log: (message) => logged.push(message),
  });
  usage.pageView(visit());
  await usage.flush();
  assert.equal(usage.lost, 0);
  usage.pageView(visit());
  await usage.flush();
  assert.equal(usage.lost, 1);
  usage.pageView(visit());
  await usage.flush();
  assert.equal(usage.lost, 2, "a refusal isn't retried");
  usage.pageView(visit());
  usage.pageView(visit());
  await usage.flush();
  assert.equal(usage.lost, 3);
  assert.equal(ingestion.sent.length, 6);
  assert.equal(logged.length, 1, "one warning a minute");
  assert.match(logged[0], /HTTP 429|HTTP 503/);

  await usage.close();
  usage.pageView(visit());
  await usage.flush();
  assert.equal(ingestion.sent.length, 6, "closed, it takes no more");
});

test("without settings nothing is recorded, and bad settings are refused", async () => {
  assert.equal(readUsageSettings({}), undefined);
  const off = createUsage(undefined);
  assert.equal(off.on, false);
  off.pageView(visit());
  await off.close();
  assert.throws(() => settings({ PGHACKER_USAGE_IKEY: "nope" }), /PGHACKER_USAGE_IKEY/);
  assert.throws(() => settings({ PGHACKER_USAGE_ID_KEY: "short" }), /at least 32/);
  assert.throws(() => settings({ PGHACKER_USAGE_ENDPOINT: "ftp://example" }), /http or https/);
});
