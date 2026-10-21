// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import test from "node:test";
import { parseStartArgs, tailLines, waitForStart } from "../scripts/lib/background.mjs";
import { DATA_SQL, dataNotice, dataStatus, parseLoaded, probeData } from "../scripts/lib/data.mjs";
import { Progress } from "../scripts/lib/progress.mjs";

test("npm start runs in the background unless told --foreground", () => {
  assert.deepEqual(parseStartArgs([]), { foreground: false });
  assert.deepEqual(parseStartArgs(["--foreground"]), { foreground: true });
  assert.throws(() => parseStartArgs(["--background"]), /Unknown option/);
  assert.throws(() => parseStartArgs(["now"]), /positional/);
});

test("a log's tail: its last lines, without ANSI codes, a carriage return ending a line", () => {
  assert.deepEqual(tailLines("a\n\x1b[32mb\x1b[0m\r\nc\rd\n\n", 2), ["c", "d"]);
  assert.deepEqual(tailLines("", 5), []);
  assert.deepEqual(tailLines("one\ntwo\n", 5), ["one", "two"]);
});

/** A child process that hasn't exited yet. */
function fakeChild() {
  const child = new EventEmitter();
  child.exitCode = null;
  child.signalCode = null;
  return child;
}

const quiet = () => new Progress({ stream: { isTTY: false, write() {} }, timer: false });

test("the wait for a start ends once the app is ready", async () => {
  let polls = 0;
  const ms = await waitForStart({
    child: fakeChild(), ready: async () => ++polls === 3, progress: quiet(), pollMs: 1, timeoutMs: 10_000,
  });
  assert.equal(polls, 3);
  assert.ok(ms >= 0);
});

test("a child that exits while starting fails the wait, saying how", async () => {
  const child = fakeChild();
  setTimeout(() => child.emit("exit", 1, null), 5);
  await assert.rejects(
    waitForStart({ child, ready: async () => false, progress: quiet(), pollMs: 2, timeoutMs: 10_000 }),
    /The app exited while starting \(exit 1\)/,
  );
  const stopped = fakeChild();
  setTimeout(() => stopped.emit("exit", null, "SIGINT"), 5);
  await assert.rejects(
    waitForStart({ child: stopped, ready: async () => false, pollMs: 2, interrupted: () => true }),
    (error) => error.interrupted === true && /Stopped while starting/.test(error.message),
  );
});

test("a start that takes too long fails the wait, and may still be running", async () => {
  let now = 0;
  await assert.rejects(
    waitForStart({ child: fakeChild(), ready: async () => { now += 1000; return false; }, pollMs: 1, timeoutMs: 5000, now: () => now }),
    (error) => error.running === true && /didn't start/.test(error.message),
  );
});

test("the wait shows the elapsed time and the log's latest line", async () => {
  let text = "";
  const progress = new Progress({ stream: { isTTY: false, write(chunk) { text += chunk; } }, timer: false });
  let polls = 0;
  await waitForStart({ child: fakeChild(), ready: async () => ++polls === 2, latest: () => "Compiling pghacker-mcp", progress, indent: "  ", pollMs: 1 });
  assert.match(text, /^ {2}0 s {2}Compiling pghacker-mcp\n$/);
});

test("what's loaded comes from pghacker-mcp's query tool, and npm start names what's missing", async () => {
  assert.deepEqual(parseLoaded("mail,code\n1,0\n\n(1 row)"), { mail: true, code: false });
  assert.throws(() => parseLoaded("error"), /Unexpected answer/);
  assert.equal(dataStatus({ mail: false, code: false }), "empty");
  assert.equal(dataStatus({ mail: true, code: false }), "mail");
  assert.equal(dataStatus({ mail: false, code: true }), "code");
  assert.equal(dataStatus({ mail: true, code: true }), "ready");
  assert.match(dataNotice({ mail: false, code: false }), /^No data loaded yet: run npm run load/);
  assert.match(dataNotice({ mail: true, code: false }), /--code-only/);
  assert.match(dataNotice({ mail: false, code: true }), /--mail-only/);
  assert.equal(dataNotice({ mail: true, code: true }), null);

  const calls = [];
  const fetchImpl = async (url, init) => {
    const request = JSON.parse(init.body);
    calls.push({ url, request });
    const result = request.method === "initialize"
      ? { protocolVersion: "2025-06-18", serverInfo: { name: "unit", version: "1" }, capabilities: {} }
      : { content: [{ type: "text", text: "mail,code\n0,1\n\n(1 row)" }] };
    return new Response(JSON.stringify({ jsonrpc: "2.0", id: request.id, result }), { headers: { "content-type": "application/json" } });
  };
  assert.deepEqual(await probeData({ mcpPort: 3102 }, { fetchImpl }), { mail: false, code: true });
  assert.equal(calls[1].url, "http://127.0.0.1:3102/mcp");
  assert.deepEqual(calls[1].request.params, { name: "query", arguments: { sql: DATA_SQL } });
});
