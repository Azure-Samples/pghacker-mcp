import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { initialEnv, parseLocalConfig } from "../scripts/lib/config.mjs";
import { assertPortAvailable, readPortalRecord, stopPortal } from "../scripts/lib/processes.mjs";

test("occupied ports are reported without stopping their listener", async () => {
  const server = createServer();
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    await assert.rejects(assertPortAvailable(server.address().port), /EADDRINUSE/);
    assert.equal(server.listening, true);
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
});

test("process records reject negative PIDs and other app identities", () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "pghacker-pid-"));
  try {
    const config = parseLocalConfig(initialEnv(), root);
    mkdirSync(config.runtimeDir);
    for (const record of [
      { pid: -1, root: config.root, project: config.project, port: 3001 },
      { pid: process.pid, root: config.root, project: "other-app", port: 3001 },
    ]) {
      writeFileSync(path.join(config.runtimeDir, "portal.json"), JSON.stringify(record));
      assert.throws(() => readPortalRecord(config), /does not belong/);
    }
  } finally {
    rmSync(root, { recursive: true });
  }
});

test("stop refuses a reused PID rather than signalling an unrelated process", async () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "pghacker-stop-"));
  try {
    const config = parseLocalConfig(initialEnv(), root);
    mkdirSync(config.runtimeDir);
    const record = path.join(config.runtimeDir, "portal.json");
    writeFileSync(record, JSON.stringify({
      pid: process.pid, root: config.root, project: config.project, port: 3001,
    }));
    await assert.rejects(stopPortal(config), /refusing to stop another process/);
    assert.equal(existsSync(record), true);
    assert.doesNotThrow(() => process.kill(process.pid, 0));
  } finally {
    rmSync(root, { recursive: true });
  }
});
