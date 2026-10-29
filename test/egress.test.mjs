// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import http from "node:http";
import net from "node:net";
import test from "node:test";
import { connects, isCertificateError, reach } from "../scripts/lib/egress.mjs";

/** A fetch that fails as undici does, with `code` as its cause's. */
function failing(code) {
  return async () => {
    throw new TypeError("fetch failed", { cause: Object.assign(new Error(code), { code }) });
  };
}

/** A server on a free port on loopback; resolves with its port and a function that stops it. */
async function listen(server) {
  const sockets = new Set();
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    port: server.address().port,
    stop: () => new Promise((resolve) => {
      for (const socket of sockets) socket.destroy();
      server.close(resolve);
    }),
  };
}

test("a server that answers is reached, even with a certificate that doesn't verify, as the Kubernetes API's", async () => {
  for (const code of [
    "DEPTH_ZERO_SELF_SIGNED_CERT", "SELF_SIGNED_CERT_IN_CHAIN", "UNABLE_TO_VERIFY_LEAF_SIGNATURE",
    "UNABLE_TO_GET_ISSUER_CERT_LOCALLY", "ERR_TLS_CERT_ALTNAME_INVALID", "CERT_HAS_EXPIRED",
  ]) {
    assert.ok(isCertificateError(code), code);
    const result = await reach("https://kubernetes.default.svc/version", { fetchImpl: failing(code) });
    assert.deepEqual([result.answered, result.error], [true, code], code);
  }
  // Azure Firewall resets the HTTPS connections it denies; a NetworkPolicy drops them.
  for (const code of ["ECONNRESET", "UND_ERR_SOCKET", "ECONNREFUSED", "ENOTFOUND", "EHOSTUNREACH", "ETIMEDOUT"]) {
    assert.ok(!isCertificateError(code), code);
    const result = await reach("https://example.com/", { fetchImpl: failing(code) });
    assert.deepEqual([result.answered, result.error], [false, code], code);
  }

  // Any HTTP status is an answer, Azure Firewall's denial (470) among them,
  // which the check tells apart by its status.
  const firewall = await listen(http.createServer((request, response) => {
    response.writeHead(470);
    response.end("Action: Deny");
  }));
  try {
    const result = await reach(`http://127.0.0.1:${firewall.port}/`);
    assert.deepEqual([result.answered, result.status], [true, 470]);
  } finally {
    await firewall.stop();
  }

  // Silence, as from a dropped connection, is no answer.
  const silent = await listen(net.createServer(() => {}));
  try {
    const result = await reach(`http://127.0.0.1:${silent.port}/`, { timeoutMs: 200 });
    assert.deepEqual([result.answered, result.error], [false, "timeout"]);
  } finally {
    await silent.stop();
  }
});

test("a TCP connection that opens is a reach; one that's refused isn't", async () => {
  const server = await listen(net.createServer((socket) => socket.end()));
  const opened = await connects("127.0.0.1", server.port);
  assert.equal(opened.connected, true);
  await server.stop();
  const refused = await connects("127.0.0.1", server.port, { timeoutMs: 2000 });
  assert.equal(refused.connected, false);
  assert.ok(refused.error, "it says why");
});

test("Envoy upstream-connect failures are not mistaken for a reachable destination", async () => {
  const result = await reach("http://metadata.invalid", {
    fetchImpl: async () => new Response("upstream connect error or disconnect/reset before headers. connection timeout", {
      status: 503, headers: { server: "envoy" },
    }),
  });
  assert.equal(result.proxyDenied, true);
  for (const [body, headers] of [["application unavailable", { server: "envoy" }],
    ["upstream connect error or disconnect/reset before headers.", {}]]) {
    const answer = await reach("http://application.invalid", {
      fetchImpl: async () => new Response(body, { status: 503, headers }),
    });
    assert.equal(answer.answered, true);
    assert.equal(answer.proxyDenied, undefined);
  }
});
