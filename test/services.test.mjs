// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { skillToolName } from "../scripts/lib/evalcheck.mjs";
import { checkSkills, listTools } from "../scripts/lib/services.mjs";

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

test("a rate-limited request is retried when the server says when", async () => {
  const calls = [];
  const ok = mcpFetch("json", calls);
  let limited = 0;
  const fetchImpl = async (url, init) => {
    if (limited++ < 2) return new Response("rate limit exceeded", { status: 429, headers: { "retry-after": "0" } });
    return ok(url, init);
  };
  assert.deepEqual(await listTools("http://127.0.0.1:1/mcp", { fetchImpl }), ["search", "grep"]);
  assert.deepEqual(calls, ["initialize", "tools/list"]);
});

test("skills are listed, and each file read and checked against its digest, and against its tool", async () => {
  const { createHash } = await import("node:crypto");
  const text = "---\nname: demo\ndescription: d\n---\nSteps.\n";
  const entry = (served) => ({
    uri: "skill://demo/SKILL.md",
    frontmatter: { name: "demo", description: "d" },
    resources: [{ uri: "skill://demo/SKILL.md", size: Buffer.byteLength(text), digest: `sha256:${createHash("sha256").update(served).digest("hex")}` }],
  });
  const server = (listed, { tool, loaded = text } = {}) => {
    const calls = [];
    const fetchImpl = async (url, init) => {
      const request = JSON.parse(init.body);
      calls.push(request.method);
      const result = request.method === "skills/list" ? { skills: [listed] }
        : request.method === "resources/read" ? { contents: [{ uri: request.params.uri, text }] }
          : request.method === "tools/list" ? { tools: tool ? [{ name: "search" }, { name: tool }] : [{ name: "search" }] }
            : request.method === "tools/call" ? { content: [{ type: "text", text: loaded }] }
              : { protocolVersion: "2025-06-18", serverInfo: { name: "unit", version: "1" }, capabilities: {} };
      return new Response(JSON.stringify({ jsonrpc: "2.0", id: request.id, result }), { headers: { "content-type": "application/json" } });
    };
    return { calls, fetchImpl };
  };
  assert.equal(skillToolName("review-as-author"), "load_skill_review_as_author");
  // Without the tool listed: the files only.
  const good = server(entry(text));
  assert.deepEqual(await checkSkills("http://127.0.0.1:1/mcp", { fetchImpl: good.fetchImpl }), ["demo"]);
  assert.deepEqual(good.calls, ["initialize", "skills/list", "initialize", "tools/list", "initialize", "resources/read"]);
  const stale = server(entry(`${text}changed`));
  await assert.rejects(checkSkills("http://127.0.0.1:1/mcp", { fetchImpl: stale.fetchImpl }), /doesn't match its listing/);
  // With it: the tool returns what resources/read does.
  const tooled = server(entry(text), { tool: "load_skill_demo" });
  assert.deepEqual(await checkSkills("http://127.0.0.1:1/mcp", { fetchImpl: tooled.fetchImpl }), [
    "demo (load_skill_demo returns it too)",
  ]);
  assert.equal(tooled.calls.at(-1), "tools/call");
  const drifted = server(entry(text), { tool: "load_skill_demo", loaded: `${text}more` });
  await assert.rejects(checkSkills("http://127.0.0.1:1/mcp", { fetchImpl: drifted.fetchImpl }), /load_skill_demo doesn't return/);
});
