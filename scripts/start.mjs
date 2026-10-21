// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { mkdirSync } from "node:fs";
import { once } from "node:events";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { readLocalConfig, portalEnvironment, LOCAL_HOST } from "./lib/config.mjs";
import {
  assertPortAvailable, preparePortalRecord, writePortalRecord, clearPortalRecord, runDatabase, runningPortal,
} from "./lib/processes.mjs";
import { needsBuild, parseStartArgs, startInBackground } from "./lib/background.mjs";
import { dataNotice, dataStatus, probeData } from "./lib/data.mjs";
import { createEdgeServer, createHomeHandler } from "./lib/edge.mjs";
import { createStarCounts } from "./lib/github-stars.mjs";
import { checkPortal } from "./lib/health.mjs";
import { Progress, formatDuration } from "./lib/progress.mjs";
import {
  ServiceSupervisor, bootstrapDatabase, buildHome, buildServices, checkServices, stopRecordedServices,
} from "./lib/services.mjs";
import { workerOptions } from "./lib/worker.mjs";

// npm start runs the app in the background (scripts/lib/background.mjs),
// with its output in .runtime/logs/start.log; npm start -- --foreground
// runs it in this terminal until Ctrl+C or npm run stop.
const NO_TOKEN = "No bootstrap token supplied. Configure a provider in Admin Console -> Model Providers before running agents.";

/** What npm start says when the database lacks the mail or the code; nothing if it can't tell. */
async function sayDataNotice(config, say) {
  try {
    const notice = dataNotice(await probeData(config));
    if (notice) say(notice);
  } catch (error) {
    say(`Could not check for data: ${error.message}`);
  }
}

let options;
try {
  options = parseStartArgs(process.argv.slice(2));
} catch (error) {
  console.error(`[pghacker-mcp] ${error.message}. Use npm start, or npm start -- --foreground.`);
  process.exit(2);
}
if (options.foreground) await foreground();
else await background();

async function background() {
  const say = (message) => console.log(`[pghacker-mcp] ${message}`);
  const progress = new Progress();
  const controller = new AbortController();
  let stopping = false;
  const onInterrupt = () => {
    if (stopping) process.exit(130);
    stopping = true;
    progress.print("[pghacker-mcp] Stopping the start; Ctrl+C again to leave it.");
    controller.abort();
  };
  process.on("SIGINT", onInterrupt);
  try {
    const config = readLocalConfig();
    const base = `http://${LOCAL_HOST}:${config.portalPort}`;
    const addresses = `the home page at ${base}/, the portal at ${base}/portal/ and pghacker-mcp at ${base}/mcp`;
    const log = path.relative(process.cwd(), path.join(config.runtimeDir, "logs", "start.log")) || "start.log";
    const env = portalEnvironment(config);
    const running = runningPortal(config);
    if (running) {
      say(`Already running (PID ${running.pid}): ${addresses}.`);
    } else {
      if (needsBuild(config)) say("The first start builds the Rust services (cargo build --release): this takes several minutes.");
      say(`Starting in the background; its log is ${log}.`);
      const started = await startInBackground(config, { progress, indent: "  ", signal: controller.signal });
      progress.end();
      say(`Ready in ${formatDuration(started.ms)}: ${addresses}.`);
      say(`Local-only, no sign-in. The log is ${log}, the services' are beside it. Stop with npm run stop.`);
    }
    await sayDataNotice(config, say);
    if (!running && !env.GITHUB_TOKEN) console.warn(`[pghacker-mcp] ${NO_TOKEN}`);
  } catch (error) {
    progress.end();
    if (error.logTail?.length) {
      console.error(`[pghacker-mcp] The end of ${path.relative(process.cwd(), error.log) || error.log}:`);
      for (const line of error.logTail) console.error(`  ${line}`);
    }
    if (error.interrupted) {
      console.error("[pghacker-mcp] Stopped. PostgreSQL may still be running: npm run stop stops it.");
      process.exitCode = 130;
    } else {
      console.error(`[pghacker-mcp] Startup failed: ${error.message}`);
      process.exitCode = 1;
    }
  } finally {
    process.off("SIGINT", onInterrupt);
  }
}

async function foreground() {
  let server;
  let edge;
  let worker;
  let workerStop;
  let services;
  function stopWorker() {
    if (worker) workerStop ??= worker.stop();
    return workerStop;
  }
  async function stopServices() {
    if (services) await services.stop();
  }

  try {
    const config = readLocalConfig();
    preparePortalRecord(config);
    stopRecordedServices(config);
    for (const port of [config.portalPort, config.portalInternalPort, config.indexerPort, config.mcpPort]) {
      await assertPortAvailable(port);
    }
    await runDatabase("up", config);

    console.log("[pghacker-mcp] Building pghacker-indexer and pghacker-mcp (cargo build --release).");
    await buildServices(config);
    console.log("[pghacker-mcp] Building the home page (home/dist).");
    await buildHome(config);
    await bootstrapDatabase(config);
    for (const directory of ["sessions", "artifacts", "exports", "logs", "cache", "git"]) {
      mkdirSync(path.join(config.runtimeDir, directory), { recursive: true });
    }
    services = new ServiceSupervisor(config);
    services.start();
    await checkServices(config);
    console.log(`[pghacker-mcp] pghacker-indexer at http://${LOCAL_HOST}:${config.indexerPort}/mcp, pghacker-mcp at http://${LOCAL_HOST}:${config.mcpPort}/mcp.`);

    const env = portalEnvironment(config);
    for (const key of Object.keys(process.env)) {
      if (!(key in env)) delete process.env[key];
    }
    Object.assign(process.env, env);
    process.chdir(config.root);
    process.title = config.project;
    writePortalRecord(config);
    process.once("exit", () => {
      try {
        clearPortalRecord(config);
      } catch (error) {
        console.error(`[pghacker-mcp] Could not clean up the process record: ${error.message}`);
      }
    });

    const { PilotSwarmWorker } = await import("pilotswarm-sdk");
    worker = new PilotSwarmWorker(workerOptions(config, env));
    if (worker.systemAgents.length !== 0) {
      throw new Error("The app must not load automatic system agents.");
    }
    for (const signal of ["SIGINT", "SIGTERM"]) {
      process.once(signal, () => {
        // The portal closes itself on the same signal.
        const portalClosed = server ? once(server, "close").catch(() => {}) : Promise.resolve();
        Promise.allSettled([stopWorker(), stopServices()]).then(async (results) => {
          for (const result of results) {
            if (result.status === "rejected") {
              console.error(`[pghacker-mcp] Shutdown failed: ${result.reason?.message ?? result.reason}`);
              process.exitCode = 1;
            }
          }
          // Database pools inside the SDK outlive a graceful stop and would keep
          // the process alive: exit once the portal has closed.
          await Promise.race([portalClosed, delay(10_000)]);
          process.exit();
        });
      });
    }
    await worker.start();

    const { startServer } = await import("pilotswarm/web");
    server = await startServer({ port: config.portalInternalPort, host: LOCAL_HOST, workers: 0 });
    if (server.address()?.address !== LOCAL_HOST) {
      throw new Error("The portal is not bound exclusively to 127.0.0.1.");
    }
    // The site, as oauth2-proxy routes it in Azure: the home page at /, the
    // portal at /portal/ (with its /api and /assets), pghacker-mcp at /mcp.
    edge = createEdgeServer({
      // Only locally: the page says when the database lacks the mail or the code.
      home: createHomeHandler({
        stars: createStarCounts(),
        dataStatus: async () => dataStatus(await probeData(config, { timeout: 2500 })),
      }),
      targets: {
        portal: { host: LOCAL_HOST, port: config.portalInternalPort },
        mcp: { host: LOCAL_HOST, port: config.mcpPort },
      },
    });
    await new Promise((resolve, reject) => {
      edge.once("error", reject);
      edge.listen(config.portalPort, LOCAL_HOST, () => {
        edge.off("error", reject);
        resolve();
      });
    });
    if (edge.address()?.address !== LOCAL_HOST) {
      throw new Error("The site is not bound exclusively to 127.0.0.1.");
    }
    const { base } = await checkPortal(config);
    console.log(`[pghacker-mcp] Ready: the home page at ${base}/, the portal at ${base}/portal/ and pghacker-mcp at ${base}/mcp.`);
    console.log("[pghacker-mcp] Local-only, no sign-in. Stop with Ctrl+C or npm run stop.");
    await sayDataNotice(config, (message) => console.log(`[pghacker-mcp] ${message}`));
    if (!env.GITHUB_TOKEN) {
      console.warn(`[pghacker-mcp] ${NO_TOKEN}`);
    }
  } catch (error) {
    console.error(`[pghacker-mcp] Startup failed: ${error.message}`);
    process.exitCode = 1;
    const deadline = setTimeout(() => process.exit(1), 30_000);
    deadline.unref();
    if (edge) {
      edge.close();
      edge.closeAllConnections();
    }
    if (server) {
      try {
        await server.stopPortal();
      } catch (shutdownError) {
        console.error(`[pghacker-mcp] Shutdown failed: ${shutdownError.message}`);
      }
    }
    for (const stop of [stopWorker, stopServices]) {
      try {
        await stop();
      } catch (shutdownError) {
        console.error(`[pghacker-mcp] Shutdown failed: ${shutdownError.message}`);
      }
    }
  }
}
