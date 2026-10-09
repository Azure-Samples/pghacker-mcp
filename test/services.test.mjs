import assert from "node:assert/strict";
import test from "node:test";
import { listTools } from "../scripts/lib/services.mjs";

function mcpFetch(format, calls = []) {
  return async (url, init) => {
    const request = JSON.parse(init.body);
    calls.push(request.method);
    const result = request.method === "initialize"
      ? { protocolVersion: "2025-06-18", serverInfo: { name: "unit", version: "1" }, capabilities: {} }
      : { tools: [{ name: "search" }, { name: "grep" }] };
    const body = JSON.stringify({ jsonrpc: "2.0", id: request.id, result });
    return format === "sse"
      ? new Response(`event: message\ndata: ${body}\n\n`, { headers: { "content-type": "text/event-stream" } })
      : new Response(body, { headers: { "content-type": "application/json" } });
  };
}

test("tools are listed through an initialize handshake, as JSON or SSE", async () => {
  for (const format of ["json", "sse"]) {
    const calls = [];
    assert.deepEqual(await listTools("http://127.0.0.1:1/mcp", { fetchImpl: mcpFetch(format, calls) }), ["search", "grep"]);
    assert.deepEqual(calls, ["initialize", "tools/list"]);
  }
});

test("an HTTP error from an MCP server is reported", async () => {
  await assert.rejects(
    listTools("http://127.0.0.1:1/mcp", { fetchImpl: async () => new Response("rate limit exceeded", { status: 429 }) }),
    /HTTP 429/,
  );
});
