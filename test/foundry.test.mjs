// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import http from "node:http";
import test from "node:test";
import { gzipSync } from "node:zlib";
import {
  PLACEHOLDER_KEY, ProxyRefusal, screenModelRequest, startFoundryProxy, workloadIdentityTokens,
} from "../scripts/lib/foundry.mjs";

/** An HTTP server on a free port; resolves with its origin and server. */
async function serve(handler) {
  const server = http.createServer(handler);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { origin: `http://127.0.0.1:${server.address().port}`, server };
}

/**
 * A Foundry proxy in front of a stand-in for Foundry that records what
 * reaches it; resolves with the proxy's origin and the records.
 */
async function proxied(t, options = {}) {
  const seen = [];
  const upstream = await serve((request, response) => {
    const chunks = [];
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("end", () => {
      seen.push({ method: request.method, url: request.url, headers: request.headers, body: Buffer.concat(chunks).toString() });
      response.writeHead(200, { "content-type": "application/json" });
      response.end("{}");
    });
  });
  t.after(() => upstream.server.close());
  const proxy = await startFoundryProxy({ upstream: upstream.origin, token: async () => "entra-token", port: 0, ...options });
  t.after(() => proxy.close());
  return { origin: `http://127.0.0.1:${proxy.address().port}`, seen };
}

/** A POST of `{}` to `path` exactly as given, which fetch would normalize. */
function rawPost(origin, path) {
  return new Promise((resolve, reject) => {
    const request = http.request(`${origin}/`, { method: "POST", path, headers: { "content-type": "application/json" } }, (response) => {
      response.resume();
      response.on("end", () => resolve({ status: response.statusCode }));
    });
    request.on("error", reject);
    request.end("{}");
  });
}

/** A model request through the proxy, as a JSON body unless `body` is a string or Buffer. */
async function send(origin, path, { method = "POST", body, headers = {} } = {}) {
  const payload = body === undefined || typeof body === "string" || Buffer.isBuffer(body) ? body : JSON.stringify(body);
  const response = await fetch(`${origin}${path}`, {
    method, headers: { "content-type": "application/json", "api-key": PLACEHOLDER_KEY, ...headers }, body: payload,
  });
  const text = await response.text();
  return { status: response.status, error: response.ok ? undefined : JSON.parse(text).error };
}

test("tokens come from Entra ID for the pod's projected token, and are reused until near expiry", async () => {
  let clock = 1_000_000;
  let projected = "federated-1";
  const asked = [];
  const fetchImpl = async (url, options) => {
    asked.push({ url: new URL(url), method: options.method, form: new URLSearchParams(options.body) });
    return new Response(JSON.stringify({ access_token: `token-${asked.length}`, expires_in: 3600 }), { status: 200 });
  };
  const options = {
    tenantId: "tenant-guid", clientId: "uami", tokenFile: "/var/run/secrets/azure/tokens/azure-identity-token",
    fetchImpl, now: () => clock, readToken: async () => `${projected}\n`,
  };
  const token = workloadIdentityTokens(options);
  assert.deepEqual(await Promise.all([token(), token()]), ["token-1", "token-1"], "concurrent callers share a request");
  assert.equal(asked.length, 1);
  assert.equal(asked[0].method, "POST");
  assert.equal(asked[0].url.href, "https://login.microsoftonline.com/tenant-guid/oauth2/v2.0/token");
  assert.equal(asked[0].form.get("client_id"), "uami");
  assert.equal(asked[0].form.get("scope"), "https://cognitiveservices.azure.com/.default");
  assert.equal(asked[0].form.get("grant_type"), "client_credentials");
  assert.equal(asked[0].form.get("client_assertion_type"), "urn:ietf:params:oauth:client-assertion-type:jwt-bearer");
  assert.equal(asked[0].form.get("client_assertion"), "federated-1");
  clock += 50 * 60_000;
  assert.equal(await token(), "token-1", "still 10 minutes left");
  // The projected token rotates: each exchange reads it again.
  projected = "federated-2";
  clock += 6 * 60_000;
  assert.equal(await token(), "token-2", "renewed 5 minutes before expiry");
  assert.equal(asked[1].form.get("client_assertion"), "federated-2");

  const refused = workloadIdentityTokens({
    ...options,
    fetchImpl: async () => new Response(JSON.stringify({ error: "invalid_client" }), { status: 401 }),
  });
  await assert.rejects(refused(), /HTTP 401 invalid_client/);
  await assert.rejects(workloadIdentityTokens({})(), /no workload identity/);

  // Another service's tokens, such as Azure Monitor's for usage telemetry,
  // from the authority the webhook names.
  const monitor = workloadIdentityTokens({
    ...options, authorityHost: "https://login.microsoftonline.us", resource: "https://monitor.azure.com/",
  });
  await monitor();
  assert.equal(asked.at(-1).url.href, "https://login.microsoftonline.us/tenant-guid/oauth2/v2.0/token");
  assert.equal(asked.at(-1).form.get("scope"), "https://monitor.azure.com/.default");
});

test("the proxy forwards /openai/ requests with the token in place of the caller's key, streaming the answer", async (t) => {
  const seen = [];
  const upstream = await serve((request, response) => {
    let body = "";
    request.on("data", (chunk) => { body += chunk; });
    request.on("end", () => {
      seen.push({ url: request.url, headers: request.headers, body });
      response.writeHead(200, { "content-type": "text/event-stream", "x-ratelimit-remaining-tokens": "123" });
      response.write("data: one\n\n");
      setTimeout(() => response.end("data: two\n\n"), 20);
    });
  });
  t.after(() => upstream.server.close());
  const proxy = await startFoundryProxy({ upstream: upstream.origin, token: async () => "entra-token", port: 0 });
  t.after(() => proxy.close());
  const origin = `http://127.0.0.1:${proxy.address().port}`;

  const response = await fetch(`${origin}/openai/v1/responses?x=1`, {
    method: "POST",
    headers: { "content-type": "application/json", "api-key": PLACEHOLDER_KEY, authorization: `Bearer ${PLACEHOLDER_KEY}` },
    body: JSON.stringify({ model: "gpt-6-luna", input: "hi" }),
  });
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("x-ratelimit-remaining-tokens"), "123");
  assert.equal(await response.text(), "data: one\n\ndata: two\n\n");
  assert.equal(seen[0].url, "/openai/v1/responses?x=1");
  assert.equal(seen[0].headers.authorization, "Bearer entra-token");
  assert.equal(seen[0].headers["api-key"], undefined);
  assert.equal(seen[0].headers.host, new URL(upstream.origin).host);
  assert.deepEqual(JSON.parse(seen[0].body), { model: "gpt-6-luna", input: "hi" });

  const elsewhere = await fetch(`${origin}/admin`);
  assert.equal(elsewhere.status, 404);
  assert.equal(seen.length, 1, "only /openai/ reaches Foundry");
});

test("without a token the proxy answers 502 and forwards nothing", async (t) => {
  let forwarded = 0;
  const upstream = await serve((request, response) => {
    forwarded += 1;
    response.end();
  });
  t.after(() => upstream.server.close());
  const proxy = await startFoundryProxy({
    upstream: upstream.origin,
    token: async () => { throw new Error("the identity endpoint answered HTTP 500"); },
    port: 0,
  });
  t.after(() => proxy.close());
  const response = await fetch(`http://127.0.0.1:${proxy.address().port}/openai/v1/responses`, { method: "POST", body: "{}" });
  assert.equal(response.status, 502);
  assert.match((await response.json()).error.message, /HTTP 500/);
  assert.equal(forwarded, 0);
});

test("the proxy forwards only the model operations the app uses, at the path it checked", async (t) => {
  const { origin, seen } = await proxied(t);
  const allowed = [
    ["POST", "/openai/v1/responses", { model: "gpt-6-luna", input: "hi" }],
    ["GET", "/openai/v1/models", undefined],
    ["GET", "/openai/v1/models/gpt-6-luna", undefined],
    ["POST", "/openai/deployments/text-embedding-3-small/embeddings?api-version=2024-10-21", { input: ["ping"] }],
  ];
  for (const [method, path, body] of allowed) {
    assert.equal((await send(origin, path, { method, body })).status, 200, `${method} ${path}`);
  }
  assert.deepEqual(seen.map((request) => `${request.method} ${request.url}`), allowed.map(([method, path]) => `${method} ${path}`));
  assert.equal(seen[1].body, "", "a GET sends no body");

  // Files, vector stores, batches, fine-tuning, stored responses and chat
  // completions (On Your Data) never reach Foundry.
  for (const [method, path] of [
    ["POST", "/openai/v1/files"],
    ["GET", "/openai/v1/vector_stores"],
    ["POST", "/openai/v1/batches"],
    ["POST", "/openai/v1/fine_tuning/jobs"],
    ["GET", "/openai/v1/responses/resp_1"],
    ["DELETE", "/openai/v1/responses"],
    ["POST", "/openai/v1/chat/completions"],
    ["POST", "/openai/deployments/gpt-6-luna/chat/completions"],
    // Escapes aren't decoded into a path the check would allow.
    ["POST", "/openai/v1/%72esponses"],
  ]) {
    const result = await send(origin, path, { method, body: method === "GET" ? undefined : {} });
    assert.equal(result.status, 403, `${method} ${path}`);
    assert.equal(result.error.code, "operation_not_allowed", `${method} ${path}`);
  }
  assert.equal(seen.length, allowed.length, "nothing refused reached Foundry");

  // Dot segments, which fetch would resolve before sending, resolve before
  // the check, and the request goes where it was checked.
  assert.equal((await rawPost(origin, "/openai/v1/responses/../files")).status, 403);
  assert.equal(seen.length, allowed.length);
  assert.equal((await rawPost(origin, "/openai/v1/files/../responses")).status, 200);
  assert.equal(seen.at(-1).url, "/openai/v1/responses");

  // A method override can't turn a checked POST into another operation.
  await send(origin, "/openai/v1/responses", { body: { input: "hi" }, headers: { "x-http-method-override": "DELETE" } });
  assert.equal(seen.at(-1).headers["x-http-method-override"], undefined);
});

test("the proxy removes hosted tools that reach the network, and refuses what would have Foundry fetch a URL", async (t) => {
  const logged = [];
  const { origin, seen } = await proxied(t, { log: (message) => logged.push(message) });
  const tools = [
    { type: "function", name: "bash", parameters: { type: "object", properties: { image_url: { type: "string" } } } },
    { type: "custom", name: "apply_patch" },
    { type: "tool_search" },
    { type: "web_search" },
    { type: "web_search_preview" },
    { type: "mcp", server_label: "x", server_url: "https://attacker.example/mcp" },
    { type: "mcp", server_label: "y", connector_id: "connector_gmail" },
  ];
  assert.equal((await send(origin, "/openai/v1/responses", { body: { model: "gpt-6-luna", tools, input: "hi" } })).status, 200);
  assert.deepEqual(JSON.parse(seen.at(-1).body).tools.map((tool) => tool.type), ["function", "custom", "tool_search"]);
  assert.match(logged.join("\n"), /Removed hosted tools that reach the network from a model request: web_search, web_search_preview, mcp\./);

  // Inline images and files go through; URLs that Foundry would fetch don't.
  const image = (url) => ({ input: [{ role: "user", content: [{ type: "input_image", image_url: url }] }] });
  assert.equal((await send(origin, "/openai/v1/responses", { body: image("data:image/png;base64,iVBORw0KGgo=") })).status, 200);
  const before = seen.length;
  for (const body of [
    image("https://attacker.example/?d=secret"),
    image("http://10.0.0.1/"),
    { input: [{ role: "user", content: [{ type: "input_file", file_url: "https://attacker.example/f.pdf" }] }] },
    // Chat's shape, nested in a function's output.
    { input: [{ type: "function_call_output", call_id: "c", output: [{ type: "image_url", image_url: { url: "https://x.example/" } }] }] },
    { messages: [], data_sources: [{ type: "azure_search", parameters: { endpoint: "https://attacker.search.windows.net" } }] },
    { messages: [], web_search_options: {} },
  ]) {
    const result = await send(origin, "/openai/v1/responses", { body });
    assert.equal(result.status, 403, JSON.stringify(body));
    assert.match(result.error.code, /^(url_not_allowed|feature_not_allowed)$/);
  }
  assert.equal(seen.length, before, "nothing refused reached Foundry");
  assert.ok(!logged.join("\n").includes("secret"), "a refused URL, which may carry what an agent read, isn't logged");
});

test("Foundry reads the body that the proxy checked: as parsed, decompressed, and within its limit", async (t) => {
  const { origin, seen } = await proxied(t, { maxBodyBytes: 4096 });
  // A duplicated key: JSON.parse keeps the last, and so does what's sent,
  // whatever another parser would have kept.
  const duplicated = '{"model":"gpt-6-luna","tools":[{"type":"mcp","server_url":"https://attacker.example"}],"tools":[]}';
  assert.equal((await send(origin, "/openai/v1/responses", { body: duplicated })).status, 200);
  assert.equal(seen.at(-1).body, '{"model":"gpt-6-luna","tools":[]}');
  assert.equal(seen.at(-1).headers["content-length"], String(Buffer.byteLength(seen.at(-1).body)));

  // A compressed body is checked decompressed, and sent so.
  const compressed = gzipSync(JSON.stringify({ input: "hi", tools: [{ type: "web_search" }] }));
  assert.equal((await send(origin, "/openai/v1/responses", { body: compressed, headers: { "content-encoding": "gzip" } })).status, 200);
  assert.deepEqual(JSON.parse(seen.at(-1).body), { input: "hi", tools: [] });
  assert.equal(seen.at(-1).headers["content-encoding"], undefined);

  const before = seen.length;
  const refusals = [
    [{ body: JSON.stringify({ input: "x".repeat(5000) }) }, 413, "too_large"],
    // A small body that decompresses past the limit.
    [{ body: gzipSync(JSON.stringify({ input: "x".repeat(50_000) })), headers: { "content-encoding": "gzip" } }, 413, "too_large"],
    [{ body: "{}", headers: { "content-encoding": "compress" } }, 415, "unsupported_encoding"],
    [{ body: "{not json" }, 400, "invalid_json"],
    [{ body: "[]" }, 400, "invalid_json"],
  ];
  for (const [options, status, code] of refusals) {
    const result = await send(origin, "/openai/v1/responses", options);
    assert.deepEqual([result.status, result.error.code], [status, code]);
  }
  assert.equal(seen.length, before, "nothing refused reached Foundry");
});

test("screening leaves a request that reaches nothing as it was", () => {
  const body = {
    model: "gpt-6.1-sol",
    input: [
      { role: "user", content: [{ type: "input_text", text: "See https://www.postgresql.org/message-id/x" }] },
      { type: "function_call", call_id: "c", name: "fetch", arguments: '{"image_url":"https://x.example/"}' },
    ],
    tools: [{ type: "function", name: "fetch", parameters: { type: "object", properties: { url: { type: "string" } } } }],
    stream: true,
  };
  const screened = screenModelRequest(body);
  assert.equal(screened.body, body);
  assert.deepEqual(screened.removed, []);
  assert.throws(() => screenModelRequest({ input: [{ content: [{ image_url: { url: " HTTPS://x.example/" } }] }] }), ProxyRefusal);
});
