// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import { once } from "node:events";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { CACHE, createEdgeServer, createHomeHandler } from "../scripts/lib/edge.mjs";

const PAGE = '<!doctype html><meta name="application-name" content="pghacker-mcp"><script type="module" src="/static/index-abc.js"></script>';

/** A built home page, as Vite writes it. */
function distFixture(t) {
  const dir = mkdtempSync(path.join(os.tmpdir(), "pghacker-home-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  mkdirSync(path.join(dir, "static"));
  writeFileSync(path.join(dir, "index.html"), PAGE);
  writeFileSync(path.join(dir, "static", "index-abc.js"), "console.log(1);\n");
  writeFileSync(path.join(dir, "favicon.svg"), "<svg xmlns='http://www.w3.org/2000/svg'/>");
  writeFileSync(path.join(dir, "robots.txt"), "User-agent: *\n");
  return dir;
}

async function listen(t, server) {
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => {
    server.closeAllConnections?.();
    server.close();
  });
  return server.address().port;
}

/** A stand-in for the portal or pghacker-mcp: it says what it got. */
async function fakeUpstream(t, name) {
  const requests = [];
  const server = http.createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    requests.push({ method: request.method, url: request.url, body: Buffer.concat(chunks).toString() });
    if (request.url.endsWith("/stream")) {
      response.writeHead(200, { "content-type": "text/event-stream" });
      response.write("data: one\n\n");
      setTimeout(() => response.end("data: two\n\n"), 300);
      return;
    }
    response.writeHead(200, { "content-type": "application/json", "x-upstream": name, "set-cookie": ["a=1", "b=2"] });
    response.end(JSON.stringify({ name, url: request.url }));
  });
  server.on("upgrade", (request, socket) => {
    requests.push({ upgrade: request.url, headers: request.headers, closed: once(socket, "close") });
    socket.write("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n");
    socket.on("data", (data) => socket.write(data));
    // As a WebSocket server does: the client's close ends the connection.
    socket.on("end", () => socket.end());
  });
  const port = await listen(t, server);
  return { requests, target: { host: "127.0.0.1", port } };
}

async function siteFixture(t) {
  const portal = await fakeUpstream(t, "portal");
  const mcp = await fakeUpstream(t, "mcp");
  const edge = createEdgeServer({
    home: createHomeHandler({ root: distFixture(t) }),
    targets: { portal: portal.target, mcp: mcp.target },
  });
  const port = await listen(t, edge);
  return { base: `http://127.0.0.1:${port}`, port, portal, mcp };
}

/** A request with exactly this path, which fetch would normalize. */
function rawGet(port, rawPath) {
  return new Promise((resolve, reject) => {
    http.get({ host: "127.0.0.1", port, path: rawPath }, (response) => {
      response.resume();
      response.once("end", () => resolve(response.statusCode));
    }).once("error", reject);
  });
}

test("the home page is checked again on each visit, under a strict policy", async (t) => {
  const { base } = await siteFixture(t);
  const page = await fetch(`${base}/`);
  assert.equal(page.status, 200);
  assert.equal(page.headers.get("content-type"), "text/html; charset=utf-8");
  assert.equal(page.headers.get("cache-control"), CACHE.page);
  assert.match(page.headers.get("content-security-policy"), /default-src 'self'.*frame-ancestors 'none'/);
  assert.equal(page.headers.get("x-content-type-options"), "nosniff");
  assert.equal(await page.text(), PAGE);

  const head = await fetch(`${base}/`, { method: "HEAD" });
  assert.equal(head.status, 200);
  assert.equal(head.headers.get("content-length"), String(Buffer.byteLength(PAGE)));
  assert.equal(await head.text(), "");
});

test("fingerprinted files are kept a year, the page's other files an hour", async (t) => {
  const { base } = await siteFixture(t);
  const script = await fetch(`${base}/static/index-abc.js`);
  assert.equal(script.status, 200);
  assert.equal(script.headers.get("content-type"), "text/javascript; charset=utf-8");
  assert.equal(script.headers.get("cache-control"), "public, max-age=31536000, immutable");
  const icon = await fetch(`${base}/favicon.svg`);
  assert.equal(icon.headers.get("content-type"), "image/svg+xml");
  assert.equal(icon.headers.get("cache-control"), "public, max-age=3600");
  assert.equal((await fetch(`${base}/robots.txt`)).headers.get("content-type"), "text/plain; charset=utf-8");
});

test("the portal's share links go on to the portal, with their query", async (t) => {
  const { base } = await siteFixture(t);
  for (const query of ["?session=abc&view=canvas&slot=2", "?canvasShare=token-1"]) {
    const link = await fetch(`${base}/${query}`, { redirect: "manual" });
    assert.equal(link.status, 302, query);
    assert.equal(link.headers.get("location"), `/portal/${query}`);
  }
  assert.equal((await fetch(`${base}/?utm_source=mail`)).status, 200);
});

test("given the portal's origin, as in Azure, share links go on to the portal there, from any host", async (t) => {
  // A custom domain serves only the home page and /mcp: Front Door's host name serves the portal.
  const portal = "https://pghacker-example.z01.azurefd.net";
  const port = await listen(t, http.createServer(createHomeHandler({ root: distFixture(t), portal })));
  for (const query of ["?session=abc&view=canvas&slot=2", "?canvasShare=token-1"]) {
    const link = await fetch(`http://127.0.0.1:${port}/${query}`, { redirect: "manual", headers: { host: "mcp.example.org" } });
    assert.equal(link.status, 302, query);
    assert.equal(link.headers.get("location"), `${portal}/portal/${query}`);
  }
  // The page's own redirect stays on the request's host.
  const index = await fetch(`http://127.0.0.1:${port}/index.html`, { redirect: "manual" });
  assert.equal(index.headers.get("location"), "/");
  for (const bad of [`${portal}/`, `${portal}/portal/`, "pghacker-example.z01.azurefd.net", "file:///tmp"]) {
    assert.throws(() => createHomeHandler({ portal: bad }), /must be given as an origin/, bad);
  }
});

test("anything else is a 404; other methods a 405; the health check answers", async (t) => {
  const { base, port } = await siteFixture(t);
  const missing = await fetch(`${base}/no-such-page`);
  assert.equal(missing.status, 404);
  assert.equal(missing.headers.get("cache-control"), CACHE.page);
  assert.equal((await fetch(`${base}/static/missing.js`)).status, 404);
  assert.equal((await fetch(`${base}/static/nested/index-abc.js`)).status, 404);
  const index = await fetch(`${base}/index.html`, { redirect: "manual" });
  assert.equal(index.status, 301);
  assert.equal(index.headers.get("location"), "/");
  for (const raw of ["/static/..%2Findex.html", "/static/..%2F..%2F..%2Fetc%2Fpasswd", "/..%2F..%2Fetc%2Fpasswd"]) {
    assert.equal(await rawGet(port, raw), 404, raw);
  }
  const post = await fetch(`${base}/`, { method: "POST", body: "x" });
  assert.equal(post.status, 405);
  assert.equal(post.headers.get("allow"), "GET, HEAD");
  const health = await fetch(`${base}/healthz`);
  assert.equal(health.status, 200);
  assert.equal(await health.text(), "ok\n");
});

test("without a built page, the home page is a 404, not an error", async (t) => {
  const dir = mkdtempSync(path.join(os.tmpdir(), "pghacker-home-empty-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const port = await listen(t, http.createServer(createHomeHandler({ root: path.join(dir, "dist") })));
  assert.equal((await fetch(`http://127.0.0.1:${port}/`)).status, 404);
});

test("the edge passes the portal's paths and /mcp on as they are, and the answers back", async (t) => {
  const { base, portal, mcp } = await siteFixture(t);
  for (const route of ["/portal", "/portal/?session=abc", "/api/v1/health", "/assets/index-x.js"]) {
    const answer = await fetch(`${base}${route}`);
    assert.equal(answer.status, 200, route);
    assert.equal(answer.headers.get("x-upstream"), "portal", route);
    assert.deepEqual(answer.headers.getSetCookie(), ["a=1", "b=2"], route);
    assert.equal((await answer.json()).url, route);
  }
  const call = await fetch(`${base}/mcp`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
  });
  assert.equal((await call.json()).name, "mcp");
  assert.equal(mcp.requests.at(-1).body, '{"jsonrpc":"2.0","id":1,"method":"tools/list"}');
  assert.equal(portal.requests.length, 4);
});

test("streams pass through as they come", async (t) => {
  const { port } = await siteFixture(t);
  const started = Date.now();
  const first = await new Promise((resolve, reject) => {
    http.get({ host: "127.0.0.1", port, path: "/api/v1/stream" }, (response) => {
      response.once("data", (chunk) => {
        resolve({ chunk: chunk.toString(), after: Date.now() - started });
        response.destroy();
      });
    }).once("error", reject);
  });
  assert.equal(first.chunk, "data: one\n\n");
  assert.ok(first.after < 250, `the first event took ${first.after} ms`);
});

test("an upstream that is down answers 502", async (t) => {
  const closed = net.createServer();
  closed.listen(0, "127.0.0.1");
  await once(closed, "listening");
  const port = closed.address().port;
  closed.close();
  const edge = createEdgeServer({
    home: createHomeHandler({ root: distFixture(t) }),
    targets: { portal: { host: "127.0.0.1", port }, mcp: { host: "127.0.0.1", port } },
  });
  const edgePort = await listen(t, edge);
  const answer = await fetch(`http://127.0.0.1:${edgePort}/portal/`);
  assert.equal(answer.status, 502);
});

/** Sends `request` on a raw socket to `port` and resolves with the first data. */
function rawExchange(port, request) {
  const socket = net.connect(port, "127.0.0.1");
  const next = () => new Promise((resolve, reject) => {
    socket.once("data", (data) => resolve(data.toString()));
    socket.once("error", reject);
  });
  socket.write(request);
  return { socket, next };
}

test("WebSockets to the portal are spliced through both ways", async (t) => {
  const { port, portal } = await siteFixture(t);
  const { socket, next } = rawExchange(port, [
    "GET /api/v1/ws HTTP/1.1",
    "Host: 127.0.0.1",
    "Upgrade: websocket",
    "Connection: Upgrade",
    "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
    "Sec-WebSocket-Version: 13",
    "",
    "",
  ].join("\r\n"));
  t.after(() => socket.destroy());
  assert.match(await next(), /^HTTP\/1\.1 101 Switching Protocols/);
  socket.write("ping");
  assert.equal(await next(), "ping");
  const upgrade = portal.requests.at(-1);
  assert.equal(upgrade.upgrade, "/api/v1/ws");
  assert.equal(upgrade.headers["sec-websocket-key"], "dGhlIHNhbXBsZSBub25jZQ==");
  // The client's close reaches the portal, and nothing is left open.
  socket.end();
  await upgrade.closed;
  await once(socket, "close");
});

test("upgrades on the home page's paths are refused", async (t) => {
  const { port } = await siteFixture(t);
  const { socket, next } = rawExchange(port, "GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n");
  t.after(() => socket.destroy());
  assert.match(await next(), /^HTTP\/1\.1 404 Not Found/);
});

test("a client that resets a refused upgrade doesn't take the edge down", async (t) => {
  const { base, port } = await siteFixture(t);
  for (let i = 0; i < 20; i++) {
    const socket = net.connect(port, "127.0.0.1");
    await once(socket, "connect");
    socket.write("GET /portal-ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n");
    socket.resetAndDestroy();
    await once(socket, "close");
  }
  // Uncaught, a reset would have crashed this process before now.
  await delay(100);
  assert.equal((await fetch(`${base}/healthz`)).status, 200);
});
