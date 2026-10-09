import assert from "node:assert/strict";
import http from "node:http";
import test from "node:test";
import { PLACEHOLDER_KEY, managedIdentityTokens, startFoundryProxy } from "../scripts/lib/foundry.mjs";

/** An HTTP server on a free port; resolves with its origin and server. */
async function serve(handler) {
  const server = http.createServer(handler);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { origin: `http://127.0.0.1:${server.address().port}`, server };
}

test("tokens come from the identity endpoint for the identity, and are reused until near expiry", async () => {
  let clock = 1_000_000;
  const asked = [];
  const fetchImpl = async (url, options) => {
    asked.push({ url: new URL(url), header: options.headers["x-identity-header"] });
    return new Response(JSON.stringify({
      access_token: `token-${asked.length}`,
      expires_on: String(Math.floor(clock / 1000) + 3600),
    }), { status: 200 });
  };
  const token = managedIdentityTokens({ endpoint: "http://identity.local/msi/token", header: "h", clientId: "uami", fetchImpl, now: () => clock });
  assert.deepEqual(await Promise.all([token(), token()]), ["token-1", "token-1"], "concurrent callers share a request");
  assert.equal(asked.length, 1);
  assert.equal(asked[0].url.searchParams.get("resource"), "https://cognitiveservices.azure.com");
  assert.equal(asked[0].url.searchParams.get("client_id"), "uami");
  assert.equal(asked[0].header, "h");
  clock += 50 * 60_000;
  assert.equal(await token(), "token-1", "still 10 minutes left");
  clock += 6 * 60_000;
  assert.equal(await token(), "token-2", "renewed 5 minutes before expiry");

  const refused = managedIdentityTokens({
    endpoint: "http://identity.local/msi/token", header: "h", clientId: "uami",
    fetchImpl: async () => new Response(JSON.stringify({ error: "invalid_request" }), { status: 400 }),
  });
  await assert.rejects(refused(), /HTTP 400 invalid_request/);
  await assert.rejects(managedIdentityTokens({})(), /no managed identity/);
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
