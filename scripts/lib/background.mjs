// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// npm start's default: the app in the background. It runs scripts/start.mjs
// --foreground as a detached child, with its output in
// .runtime/logs/start.log, and waits until the portal answers or the child
// exits, showing the log's latest line meanwhile. The child writes the
// portal's record and takes the project's name as its title, as in the
// foreground, so npm run stop stops it as before. npm run load starts the
// app the same way.

import { spawn } from "node:child_process";
import { closeSync, existsSync, mkdirSync, openSync, readSync, fstatSync, writeSync } from "node:fs";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { parseArgs } from "node:util";
import { checkPortal } from "./health.mjs";
import { readPortalRecord, runningPortal } from "./processes.mjs";
import { formatDuration, stripAnsi } from "./progress.mjs";
import { SERVICES, binaryPath } from "./services.mjs";

/** The start log's first line, before its date; the progress shows the lines after it. */
const LOG_HEADER = "[pghacker-mcp] npm start, in the background, at ";
/** How long a start may take: the first one builds the Rust services. */
const START_TIMEOUT_MS = 30 * 60_000;

/** npm start's arguments: --foreground keeps it in the terminal. */
export function parseStartArgs(argv) {
  const { values } = parseArgs({
    args: argv,
    options: { foreground: { type: "boolean", default: false } },
    strict: true,
    allowPositionals: false,
  });
  return { foreground: values.foreground };
}

export function startLogPath(config) {
  return path.join(config.runtimeDir, "logs", "start.log");
}

/** Whether a start will build the Rust services first, which takes minutes the first time. */
export function needsBuild(config) {
  return Object.values(SERVICES).some((service) => !existsSync(binaryPath(config, service.binary)));
}

/** The last `bytes` of `file`, or "" if there's none. */
export function readTail(file, bytes = 16_384) {
  let fd;
  try {
    fd = openSync(file, "r");
  } catch {
    return "";
  }
  try {
    const { size } = fstatSync(fd);
    const length = Math.min(size, bytes);
    const buffer = Buffer.alloc(length);
    readSync(fd, buffer, 0, length, size - length);
    return buffer.toString("utf8");
  } finally {
    closeSync(fd);
  }
}

/** The last `count` non-blank lines of `text`, without ANSI codes; a carriage return ends a line too. */
export function tailLines(text, count = 20) {
  return stripAnsi(text).split(/\r\n|\r|\n/).map((line) => line.trimEnd()).filter((line) => line.trim()).slice(-count);
}

/** The start log's latest line, for the progress; none until the start writes one. */
export function latestLogLine(config, max = 100) {
  const line = tailLines(readTail(startLogPath(config), 4096), 1).find((text) => !text.startsWith(LOG_HEADER));
  // A plain stream doesn't cut lines to the terminal, and some log lines run long.
  return line && line.length > max ? `${line.slice(0, max - 1)}…` : line;
}

/**
 * Waits for `child` to start the app: until `ready()` says it has (the
 * elapsed milliseconds), or `child` exits or `timeoutMs` passes (an error).
 * Meanwhile `progress` shows the elapsed time and `latest()`, the log's
 * latest line.
 */
export async function waitForStart({
  child, ready, latest = () => "", progress, indent = "", pollMs = 1000, timeoutMs = START_TIMEOUT_MS,
  now = Date.now, interrupted = () => false,
}) {
  const started = now();
  let exited = child.exitCode !== null || child.signalCode !== null
    ? { code: child.exitCode, signal: child.signalCode } : null;
  const onExit = (code, signal) => {
    exited = { code, signal };
  };
  child.once("exit", onExit);
  const failure = (message, extra = {}) => Object.assign(new Error(message), extra);
  try {
    for (;;) {
      if (exited) {
        if (interrupted()) throw failure("Stopped while starting.", { interrupted: true });
        throw failure(`The app exited while starting (${exited.signal || `exit ${exited.code}`}).`);
      }
      if (await ready()) return now() - started;
      if (now() - started > timeoutMs) {
        throw failure(`The app didn't start in ${formatDuration(timeoutMs)}; it may still be starting.`, { running: true });
      }
      progress?.live(() => [`${indent}{spin} ${formatDuration(now() - started)}  ${latest() ?? ""}`.trimEnd()], "starting");
      await delay(pollMs);
    }
  } finally {
    child.off("exit", onExit);
  }
}

/** Whether the child `pid` wrote the portal's record and the portal answers. */
async function portalReady(config, pid) {
  let record;
  try {
    record = readPortalRecord(config);
  } catch {
    return false;
  }
  if (record?.pid !== pid) return false;
  try {
    await checkPortal(config, { timeout: 5000 });
    return true;
  } catch {
    return false;
  }
}

/**
 * Starts the app in the background, unless it's running: `{ already: true,
 * pid }` then. Otherwise `{ pid, ms, log }` once it's ready. `signal`, an
 * AbortSignal, stops a start under way, as Ctrl+C stops one in the
 * foreground. A failure's error carries the log's tail (`logTail`) and path.
 */
export async function startInBackground(config, { progress, indent = "", signal, pollMs = 1000, timeoutMs } = {}) {
  const running = runningPortal(config);
  if (running) return { already: true, pid: running.pid };
  const log = startLogPath(config);
  mkdirSync(path.dirname(log), { recursive: true });
  const fd = openSync(log, "w", 0o600);
  let child;
  try {
    writeSync(fd, `${LOG_HEADER}${new Date().toISOString()}\n`);
    child = spawn(process.execPath, [path.join(config.root, "scripts", "start.mjs"), "--foreground"], {
      cwd: config.root, detached: true, stdio: ["ignore", fd, fd], env: process.env,
    });
  } finally {
    closeSync(fd);
  }
  child.unref();
  await new Promise((resolve, reject) => {
    child.once("spawn", resolve);
    child.once("error", reject);
  });

  let interrupted = false;
  // Ctrl+C in the terminal reaches the whole foreground process group; the
  // detached child has a group of its own, which gets it from here.
  const stop = () => {
    interrupted = true;
    try {
      process.kill(-child.pid, "SIGINT");
    } catch {
      // Already gone.
    }
  };
  signal?.addEventListener("abort", stop, { once: true });
  try {
    const ms = await waitForStart({
      child,
      ready: () => portalReady(config, child.pid),
      latest: () => latestLogLine(config),
      progress,
      indent,
      pollMs,
      timeoutMs,
      interrupted: () => interrupted,
    });
    return { already: false, pid: child.pid, ms, log };
  } catch (error) {
    error.log = log;
    error.logTail = tailLines(readTail(log), 25);
    throw error;
  } finally {
    signal?.removeEventListener("abort", stop);
  }
}
