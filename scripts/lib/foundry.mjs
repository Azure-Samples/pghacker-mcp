// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Azure AI Foundry with an Entra identity rather than keys: key
// authentication is disabled by design. In AKS the token proxy
// (scripts/token-proxy.mjs), the only pod with an Azure identity, runs this
// proxy: it forwards /openai/ requests to the Foundry account with an Entra
// token in place of whatever key the caller sent, so PilotSwarm's
// OpenAI-compatible provider, the embedder and the crawl scheduler work with
// a placeholder key, and the identity's credentials stay in the proxy's pod.
// The same proxy, with Azure Monitor's tokens, carries usage telemetry to
// Application Insights (scripts/lib/monitor.mjs).
//
// PilotSwarm's agents reach the Foundry proxy, and run shell commands, so it
// forwards only the model operations the app uses (FOUNDRY_OPERATIONS), and
// keeps Foundry from reaching anything else for a caller: it removes hosted
// tools that reach the network (web search, remote MCP servers), refuses a
// body that would have Foundry fetch a URL, and sends the body as it parsed
// it, so that Foundry reads what was checked. The account's outbound access
// is restricted too (deploy/terraform/foundry.tf).
import { readFile } from "node:fs/promises";
import http from "node:http";
import https from "node:https";
import zlib from "node:zlib";

/** What callers send as their key; the proxy replaces it. */
export const PLACEHOLDER_KEY = "managed-identity";

/** The audience of Foundry's tokens. */
export const FOUNDRY_RESOURCE = "https://cognitiveservices.azure.com";

/** Where Entra ID issues tokens in Azure's public cloud. */
export const AUTHORITY_HOST = "https://login.microsoftonline.com/";
const REFRESH_BEFORE_MS = 5 * 60_000;
const HOP_BY_HOP = new Set([
  "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding",
  "upgrade", "host",
]);
/** Headers that some services read as another method than the one checked. */
const METHOD_OVERRIDES = new Set(["x-http-method", "x-http-method-override", "x-method-override"]);

/**
 * The most a checked request's body may hold, decompressed: far more than
 * the models' contexts take, and little enough that a caller can't fill the
 * proxy's memory with one request.
 */
export const MAX_REQUEST_BYTES = 32 * 1024 * 1024;

/**
 * The model operations that the Foundry proxy forwards, the only ones the app
 * uses: the Responses API, which PilotSwarm's sessions (through the Copilot
 * CLI's runtime) and the probe call; the model list, which the runtime may
 * read; and a deployment's embeddings, for the indexer, pghacker-mcp and the
 * probe. Not chat completions, whose On Your Data queries other services,
 * nor files, vector stores, batches, fine-tuning or stored responses.
 */
const FOUNDRY_OPERATIONS = [
  { method: "POST", path: /^\/openai\/v1\/responses$/, body: true },
  { method: "GET", path: /^\/openai\/v1\/models(?:\/[\w.:-]+)?$/, body: false },
  { method: "POST", path: /^\/openai\/deployments\/[\w.-]+\/embeddings$/, body: true },
];

/** The fields whose URL Foundry would fetch: an input image's, file's, audio's or video's. */
const FETCHED_FIELDS = new Set(["image_url", "file_url", "audio_url", "video_url"]);

/** A request that a proxy refuses: the caller gets `status`, and an error with `code`. */
export class ProxyRefusal extends Error {
  constructor(status, code, message) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

/** A hosted tool that reaches the network for the caller: web search, or a remote MCP server or connector. */
function reachesNetwork(tool) {
  if (!tool || typeof tool !== "object") return false;
  const type = typeof tool.type === "string" ? tool.type : "";
  return type === "mcp" || type.startsWith("web_search") || "server_url" in tool || "connector_id" in tool;
}

/**
 * Screens a model request's body (an object): removes the hosted tools that
 * reach the network, which the Copilot CLI may offer a provider (its native
 * web search), and refuses a body that would have Foundry fetch a URL (an
 * input image, file, audio or video by URL rather than inline data) or
 * query another service (On Your Data's data_sources, chat's
 * web_search_options). Returns the screened body and the removed tools' types.
 */
export function screenModelRequest(body) {
  for (const key of ["data_sources", "web_search_options"]) {
    if (Object.hasOwn(body, key)) {
      throw new ProxyRefusal(403, "feature_not_allowed", `${key} would have Foundry query another service.`);
    }
  }
  const removed = [];
  let screened = body;
  if (Array.isArray(body.tools)) {
    const tools = body.tools.filter((tool) => {
      if (!reachesNetwork(tool)) return true;
      removed.push(typeof tool.type === "string" ? tool.type : "unknown");
      return false;
    });
    if (removed.length > 0) screened = { ...body, tools };
  }
  // Every URL anywhere in the body that Foundry would fetch must be inline data.
  const pending = [screened];
  while (pending.length > 0) {
    const value = pending.pop();
    const entries = Array.isArray(value) ? value.map((inner) => [undefined, inner]) : Object.entries(value);
    for (const [key, inner] of entries) {
      if (key !== undefined && FETCHED_FIELDS.has(key)) {
        const url = typeof inner === "string" ? inner : inner?.url;
        if (typeof url === "string" && !/^data:/i.test(url.trim())) {
          throw new ProxyRefusal(403, "url_not_allowed", `${key} must be inline data (a data: URL): Foundry would fetch it.`);
        }
      }
      if (inner && typeof inner === "object") pending.push(inner);
    }
  }
  return { body: screened, removed };
}

/**
 * The Foundry proxy's check of a request (startTokenProxy's `route`): one of
 * FOUNDRY_OPERATIONS, with its JSON body screened (screenModelRequest), then
 * sent as parsed, so that Foundry can't read it otherwise, as by a
 * duplicated key. `note(kind, message)` logs what it removed.
 */
export async function foundryRoute({ method, path, read, note = () => {} }) {
  const operation = FOUNDRY_OPERATIONS.find((candidate) => candidate.method === method && candidate.path.test(path));
  if (!operation) {
    throw new ProxyRefusal(403, "operation_not_allowed", `${method} ${path.slice(0, 200)} isn't a model operation that pghacker-mcp uses.`);
  }
  if (!operation.body) return { body: null };
  let parsed;
  try {
    parsed = JSON.parse((await read()).toString("utf8"));
  } catch (error) {
    if (error instanceof ProxyRefusal) throw error;
    throw new ProxyRefusal(400, "invalid_json", "The body must be a JSON object.");
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new ProxyRefusal(400, "invalid_json", "The body must be a JSON object.");
  }
  const { body, removed } = screenModelRequest(parsed);
  if (removed.length > 0) {
    note("removed-tools", `Removed hosted tools that reach the network from a model request: ${[...new Set(removed)].join(", ")}.`);
  }
  return { body: Buffer.from(JSON.stringify(body)) };
}

/**
 * A request's body, decompressed (gzip, deflate or br), refused (413) past
 * `limit` bytes before or after.
 */
async function readBody(request, limit) {
  const chunks = [];
  let size = 0;
  for await (const chunk of request) {
    size += chunk.length;
    if (size > limit) throw new ProxyRefusal(413, "too_large", `A request's body may hold at most ${limit} bytes.`);
    chunks.push(chunk);
  }
  const raw = Buffer.concat(chunks);
  const encoding = String(request.headers["content-encoding"] ?? "").trim().toLowerCase();
  const decode = {
    "": null, identity: null, gzip: zlib.gunzipSync, "x-gzip": zlib.gunzipSync, deflate: zlib.inflateSync,
    br: zlib.brotliDecompressSync,
  };
  if (!Object.hasOwn(decode, encoding)) {
    throw new ProxyRefusal(415, "unsupported_encoding", `Content-Encoding ${encoding.slice(0, 40)} isn't supported.`);
  }
  if (!decode[encoding]) return raw;
  try {
    return decode[encoding](raw, { maxOutputLength: limit });
  } catch (error) {
    if (error.code === "ERR_BUFFER_TOO_LARGE") {
      throw new ProxyRefusal(413, "too_large", `A request's body may hold at most ${limit} bytes.`);
    }
    throw new ProxyRefusal(400, "bad_encoding", `The body isn't valid ${encoding}.`);
  }
}

/** Logs each kind of message at most once a minute, with a count of those held back. */
function throttledLog(log, intervalMs = 60_000) {
  const last = new Map();
  return (kind, message) => {
    const now = Date.now();
    const previous = last.get(kind);
    if (previous && now - previous.at < intervalMs) {
      previous.held += 1;
      return;
    }
    last.set(kind, { at: now, held: 0 });
    log(previous?.held ? `${message} (and ${previous.held} more like it)` : message);
  };
}

/**
 * Tokens for `resource` (Foundry's by default) through AKS workload
 * identity: the service account token that the webhook projects into the pod
 * (AZURE_FEDERATED_TOKEN_FILE), exchanged at Entra ID (AZURE_AUTHORITY_HOST,
 * AZURE_TENANT_ID) for an access token of the user-assigned identity
 * `clientId` (AZURE_CLIENT_ID). Returns a function that gives one, cached
 * until five minutes before it expires, with concurrent callers sharing a
 * request. The projected token rotates, so each exchange reads it again.
 */
export function workloadIdentityTokens({
  tenantId, clientId, tokenFile, authorityHost = AUTHORITY_HOST, resource = FOUNDRY_RESOURCE,
  fetchImpl = fetch, now = Date.now, readToken = (file) => readFile(file, "utf8"),
}) {
  let cached = null;
  let pending = null;
  return async function token() {
    if (!tenantId || !clientId || !tokenFile) {
      throw new Error("the pod has no workload identity (AZURE_TENANT_ID, AZURE_CLIENT_ID, AZURE_FEDERATED_TOKEN_FILE)");
    }
    if (cached && cached.expiresAt - now() > REFRESH_BEFORE_MS) return cached.value;
    pending ??= (async () => {
      const assertion = String(await readToken(tokenFile)).trim();
      const base = authorityHost.endsWith("/") ? authorityHost : `${authorityHost}/`;
      const response = await fetchImpl(new URL(`${encodeURIComponent(tenantId)}/oauth2/v2.0/token`, base), {
        method: "POST",
        headers: { "content-type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({
          client_id: clientId,
          scope: `${resource.replace(/\/+$/, "")}/.default`,
          grant_type: "client_credentials",
          client_assertion_type: "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
          client_assertion: assertion,
        }).toString(),
        signal: AbortSignal.timeout(30_000),
      });
      const body = await response.json().catch(() => ({}));
      if (!response.ok || !body.access_token) {
        const reason = body.error_description ?? body.error ?? "";
        throw new Error(`Entra ID answered HTTP ${response.status} ${String(reason).slice(0, 200)}`.trim());
      }
      const expiresIn = Number(body.expires_in);
      const lifetimeMs = Number.isFinite(expiresIn) && expiresIn > 0 ? expiresIn * 1000 : 60 * 60_000;
      cached = { value: body.access_token, expiresAt: now() + lifetimeMs };
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
 * Listens on `host`:`port` and forwards the model operations the app uses
 * (foundryRoute) to `upstream`, the account's origin, with `token()`'s bearer
 * token; resolves with the server once it listens.
 */
export function startFoundryProxy(options) {
  return startTokenProxy({ name: "Foundry", prefix: "/openai/", ...options, route: foundryRoute });
}

/**
 * Listens on `host`:`port` and forwards the requests whose path starts with
 * `prefix` to `upstream`, the origin of the service `name`, with `token()`'s
 * bearer token in place of any key the caller sent; resolves with the server
 * once it listens. The path is sent as checked, its dot segments resolved.
 * `route({ method, path, read, note })`, if given, checks each request first:
 * it throws a ProxyRefusal, or returns the body to send in place of the
 * caller's (null for none), which `read()` gives it, up to `maxBodyBytes`.
 */
export function startTokenProxy({
  name, prefix, upstream, token, port, host = "127.0.0.1", log = () => {}, route, maxBodyBytes = MAX_REQUEST_BYTES,
}) {
  const target = new URL(upstream);
  const client = target.protocol === "https:" ? https : http;
  const agent = new client.Agent({ keepAlive: true, maxSockets: 128 });
  const note = throttledLog(log);
  const server = http.createServer(async (request, response) => {
    let url;
    try {
      url = new URL(request.url ?? "", "http://proxy.invalid");
    } catch {
      sendError(response, 400, "bad_request", "The request's target isn't a valid path.");
      return;
    }
    if (!url.pathname.startsWith(prefix)) {
      sendError(response, 404, "not_found", `Only ${prefix} requests go to ${name}.`);
      return;
    }
    let checked;
    if (route) {
      try {
        checked = await route({ method: request.method, path: url.pathname, read: () => readBody(request, maxBodyBytes), note });
      } catch (error) {
        if (!(error instanceof ProxyRefusal)) {
          // The caller went away while sending its body.
          response.destroy();
          return;
        }
        note(`refused-${error.code}`, `Refused ${request.method} ${url.pathname.slice(0, 200)} for ${name}: ${error.message}`);
        response.setHeader("connection", "close");
        sendError(response, error.status, error.code, error.message);
        return;
      }
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
      if (!HOP_BY_HOP.has(name) && !METHOD_OVERRIDES.has(name) && name !== "authorization" && name !== "api-key") {
        headers[name] = value;
      }
    }
    if (checked) {
      for (const name of ["content-length", "content-encoding", "content-type"]) delete headers[name];
      if (checked.body) {
        headers["content-type"] = "application/json";
        headers["content-length"] = String(checked.body.length);
      }
    }
    headers.host = target.host;
    headers.authorization = `Bearer ${bearer}`;
    const forwarded = client.request({
      protocol: target.protocol,
      hostname: target.hostname,
      port: target.port || (target.protocol === "https:" ? 443 : 80),
      method: request.method,
      path: `${url.pathname}${url.search}`,
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
    if (checked) {
      forwarded.end(checked.body ?? undefined);
      request.resume();
    } else {
      request.pipe(forwarded);
    }
  });
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, host, () => {
      server.off("error", reject);
      resolve(server);
    });
  });
}
