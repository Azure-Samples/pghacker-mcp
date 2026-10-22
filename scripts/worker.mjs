// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The AKS deployment's PilotSwarm worker, on the PilotSwarm node pool. It runs
// every session's turns, the crawl's extraction and deduplication and the
// portal's, with the agents' built-in shell. Its pod holds no Azure or
// Kubernetes token and no secret but pgh_swarm's password, and its
// NetworkPolicy and the firewall let it reach only the indexer, pghacker-mcp's
// internal listener, the token proxy and its database (deploy/README.md,
// "The cluster").
//
// With --crawl it also runs the crawl scheduler (scripts/crawl.mjs --watch)
// as a child process in the same container, so that the scheduler's memory
// back-pressure sees the sessions' memory, and restarts it when it exits.
// SIGTERM stops the scheduler, then drains the worker for up to
// PILOTSWARM_WORKER_SHUTDOWN_TIMEOUT_MS, within the pod's grace period.
import { spawn } from "node:child_process";
import { once } from "node:events";
import os from "node:os";
import path from "node:path";

const env = process.env;
const log = (message) => console.log(`[worker] ${message}`);
const crawl = process.argv.includes("--crawl");
const SCHEDULER = path.join(import.meta.dirname, "crawl.mjs");
const RESTART_WINDOW_MS = 10 * 60_000;

const { PilotSwarmWorker } = await import("pilotswarm-sdk");
const worker = new PilotSwarmWorker({
  store: env.DATABASE_URL,
  workerNodeId: `pghacker-${os.hostname()}`,
  sessionStateDir: env.SESSION_STATE_DIR,
  pluginDirs: [env.PLUGIN_DIRS],
  modelProvidersPath: env.PS_MODEL_PROVIDERS_PATH,
  disableManagementAgents: true,
  logLevel: "error",
});
if (worker.systemAgents.length !== 0) throw new Error("The app must not load automatic system agents.");

let stopping = false;
let scheduler = null;
let restarts = [];

/** The crawl scheduler, restarted with backoff when it exits. */
function startScheduler() {
  if (stopping) return;
  const child = spawn(process.execPath, [SCHEDULER, "--watch"], { env, stdio: "inherit" });
  scheduler = child;
  child.once("error", (error) => log(`The crawl scheduler failed to start: ${error.message}`));
  child.once("exit", (code, signal) => {
    scheduler = null;
    if (stopping) return;
    const now = Date.now();
    restarts = [...restarts.filter((at) => now - at < RESTART_WINDOW_MS), now];
    const wait = Math.min(60_000, 1000 * 2 ** (restarts.length - 1));
    log(`The crawl scheduler exited (${signal || `exit ${code}`}); restarting in ${wait / 1000} s.`);
    setTimeout(startScheduler, wait).unref();
  });
}

async function stopScheduler() {
  const child = scheduler;
  if (!child || child.exitCode !== null) return;
  const exited = once(child, "exit");
  child.kill("SIGTERM");
  const timer = setTimeout(() => child.kill("SIGKILL"), 20_000);
  await exited;
  clearTimeout(timer);
}

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, async () => {
    if (stopping) return;
    stopping = true;
    log(`${signal}: stopping the crawl scheduler, then draining.`);
    await stopScheduler();
    try {
      await worker.gracefulShutdown();
    } catch (error) {
      console.error(`[worker] The worker didn't drain cleanly: ${error.message}`);
    }
    // The SDK's database pools outlive the drain.
    process.exit(0);
  });
}

await worker.start();
log(`Worker ${os.hostname()} polling for sessions${crawl ? ", with the crawl scheduler" : ""}.`);
if (crawl) startScheduler();
