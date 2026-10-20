// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Azure AI Foundry with the app's managed identity rather than keys: key
// authentication is disabled by design. The supervisor runs this proxy on
// loopback: it forwards /openai/ requests to the Foundry account with an
// Entra token in place of whatever key the caller sent, so PilotSwarm's
// OpenAI-compatible provider, the embedder and the crawl scheduler work with
// a placeholder key, and the identity's credentials stay in the supervisor.
// The same proxy, with Azure Monitor's tokens, carries usage telemetry to
// Application Insights (scripts/lib/monitor.mjs).
import http from "node:http";
import https from "node:https";

/** What callers send as their key; the proxy replaces it. */
export const PLACEHOLDER_KEY = "managed-identity";

/** The audience of Foundry's tokens. */
export const FOUNDRY_RESOURCE = "https://cognitiveservices.azure.com";
const REFRESH_BEFORE_MS = 5 * 60_000;
const HOP_BY_HOP = new Set([
  "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding",
  "upgrade", "host",
]);

/**
 * Tokens for `resource` (Foundry's by default) from the Container Apps
 * identity endpoint (IDENTITY_ENDPOINT, IDENTITY_HEADER) for the
 * user-assigned identity `clientId`: a function that returns one, cached
 * until five minutes before it expires, with concurrent callers sharing a
 * request.
 */
export function managedIdentityTokens({
  endpoint, header, clientId, resource = FOUNDRY_RESOURCE, fetchImpl = fetch, now = Date.now,
}) {
  let cached = null;
  let pending = null;
  return async function token() {
    if (!endpoint || !header) throw new Error("the container has no managed identity (IDENTITY_ENDPOINT)");
    if (cached && cached.expiresAt - now() > REFRESH_BEFORE_MS) return cached.value;
    pending ??= (async () => {
      const url = new URL(endpoint);
      url.searchParams.set("resource", resource);
      url.searchParams.set("api-version", "2019-08-01");
      if (clientId) url.searchParams.set("client_id", clientId);
      const response = await fetchImpl(url, {
        headers: { "x-identity-header": header },
        signal: AbortSignal.timeout(30_000),
      });
      const body = await response.json().catch(() => ({}));
      if (!response.ok || !body.access_token) {
        const reason = body.error_description ?? body.message ?? body.error ?? "";
        throw new Error(`the identity endpoint answered HTTP ${response.status} ${String(reason).slice(0, 200)}`.trim());
      }
      const expiresAt = Number(body.expires_on) * 1000;
      cached = { value: body.access_token, expiresAt: Number.isFinite(expiresAt) && expiresAt > 0 ? expiresAt : now() + 60 * 60_000 };
      return cached.value;
    })().finally(() => {
      pending = null;
    });
    return pending;
  };
}

function sendError(response, status, code, message) {
  if (response.headersSent) {
    response.destroy();
    return;
  }
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify({ error: { code, message } }));
}

/**
 * Listens on `host`:`port` and forwards /openai/ requests to `upstream`
 * (the account's origin) with `token()`'s bearer token; resolves with the
 * server once it listens.
 */
export function startFoundryProxy(options) {
  return startTokenProxy({ name: "Foundry", prefix: "/openai/", ...options });
}

/**
 * Listens on `host`:`port` and forwards the requests whose path starts with
 * `prefix` to `upstream`, the origin of the service `name`, with `token()`'s
 * bearer token in place of any key the caller sent; resolves with the server
 * once it listens.
 */
export function startTokenProxy({ name, prefix, upstream, token, port, host = "127.0.0.1", log = () => {} }) {
  const target = new URL(upstream);
  const client = target.protocol === "https:" ? https : http;
  const agent = new client.Agent({ keepAlive: true, maxSockets: 128 });
  const server = http.createServer(async (request, response) => {
    if (!request.url?.startsWith(prefix)) {
      sendError(response, 404, "not_found", `Only ${prefix} requests go to ${name}.`);
      return;
    }
    let bearer;
    try {
      bearer = await token();
    } catch (error) {
      log(`No token for ${name}: ${error.message}`);
      sendError(response, 502, "managed_identity", `No token for ${name}: ${error.message}`);
      return;
    }
    const headers = {};
    for (const [name, value] of Object.entries(request.headers)) {
      if (!HOP_BY_HOP.has(name) && name !== "authorization" && name !== "api-key") headers[name] = value;
    }
    headers.host = target.host;
    headers.authorization = `Bearer ${bearer}`;
    const forwarded = client.request({
      protocol: target.protocol,
      hostname: target.hostname,
      port: target.port || (target.protocol === "https:" ? 443 : 80),
      method: request.method,
      path: request.url,
      headers,
      agent,
    }, (answer) => {
      const out = {};
      for (const [name, value] of Object.entries(answer.headers)) if (!HOP_BY_HOP.has(name)) out[name] = value;
      response.writeHead(answer.statusCode ?? 502, out);
      answer.pipe(response);
    });
    forwarded.on("error", (error) => {
      log(`A ${name} request failed: ${error.message}`);
      sendError(response, 502, "upstream", `${name} didn't answer: ${error.message}`);
    });
    response.on("close", () => {
      if (!response.writableFinished) forwarded.destroy();
    });
    request.pipe(forwarded);
  });
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, host, () => {
      server.off("error", reject);
      resolve(server);
    });
  });
}
