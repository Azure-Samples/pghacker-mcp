// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import http from "node:http";
import test from "node:test";
import { gzipSync } from "node:zlib";
import { MONITOR_AUDIENCE, TRACK_PATH, parseConnectionString, startMonitorProxy } from "../scripts/lib/monitor.mjs";

const IKEY = "11111111-2222-4333-8444-555555555555";

test("a connection string gives the key, the ingestion endpoint's origin and the token's audience", () => {
  const parsed = parseConnectionString(
    `InstrumentationKey=${IKEY};IngestionEndpoint=https://westus3-1.in.applicationinsights.azure.com/;`
      + "LiveEndpoint=https://westus3.livediagnostics.monitor.azure.com/;ApplicationId=22222222-0000-4000-8000-000000000000",
  );
  assert.deepEqual(parsed, {
    instrumentationKey: IKEY,
    ingestionEndpoint: "https://westus3-1.in.applicationinsights.azure.com",
    audience: MONITOR_AUDIENCE,
  });
  const sovereign = parseConnectionString(
    `InstrumentationKey=${IKEY};IngestionEndpoint=https://usgovvirginia-1.in.applicationinsights.azure.us/;AADAudience=https://monitor.azure.us/`,
  );
  assert.equal(sovereign.audience, "https://monitor.azure.us");
  assert.throws(() => parseConnectionString("IngestionEndpoint=https://x.example/"), /InstrumentationKey/);
  assert.throws(
    () => parseConnectionString(`InstrumentationKey=${IKEY};IngestionEndpoint=http://x.example/`),
    /https IngestionEndpoint/,
  );
  assert.throws(() => parseConnectionString(`InstrumentationKey=${IKEY}`), /https IngestionEndpoint/);
  assert.throws(() => parseConnectionString(undefined), /InstrumentationKey/);
});

test("the proxy forwards only telemetry, with the token, its gzipped body as it is", async (t) => {
  const seen = [];
  const upstream = http.createServer((request, response) => {
    const chunks = [];
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("end", () => {
      seen.push({ method: request.method, url: request.url, headers: request.headers, body: Buffer.concat(chunks) });
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ itemsReceived: 1, itemsAccepted: 1, errors: [] }));
    });
  });
  await new Promise((resolve) => upstream.listen(0, "127.0.0.1", resolve));
  t.after(() => upstream.close());
  const origin = `http://127.0.0.1:${upstream.address().port}`;
  const proxy = await startMonitorProxy({ upstream: origin, token: async () => "monitor-token", port: 0 });
  t.after(() => proxy.close());
  const base = `http://127.0.0.1:${proxy.address().port}`;

  const body = gzipSync('{"name":"Microsoft.ApplicationInsights.Event"}');
  const response = await fetch(`${base}${TRACK_PATH}`, {
    method: "POST",
    headers: { "content-type": "application/x-json-stream", "content-encoding": "gzip" },
    body,
  });
  assert.equal(response.status, 200);
  assert.equal((await response.json()).itemsAccepted, 1);
  assert.equal(seen[0].url, TRACK_PATH);
  assert.equal(seen[0].headers.authorization, "Bearer monitor-token");
  assert.equal(seen[0].headers["content-encoding"], "gzip");
  assert.ok(seen[0].body.equals(body));

  for (const elsewhere of ["/v2/track", "/openai/v1/responses", "/"]) {
    assert.equal((await fetch(`${base}${elsewhere}`, { method: "POST", body: "{}" })).status, 404, elsewhere);
  }
  assert.equal(seen.length, 1);
});
