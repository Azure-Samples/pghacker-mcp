// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { mkdirSync } from "node:fs";
import { once } from "node:events";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { readLocalConfig, portalEnvironment, LOCAL_HOST } from "./lib/config.mjs";
import {
  assertPortAvailable, preparePortalRecord, writePortalRecord, clearPortalRecord, runDatabase,
} from "./lib/processes.mjs";
import { createEdgeServer, createHomeHandler } from "./lib/edge.mjs";
import { createStarCounts } from "./lib/github-stars.mjs";
import { checkPortal } from "./lib/health.mjs";
import {
  ServiceSupervisor, bootstrapDatabase, buildHome, buildServices, checkServices, stopRecordedServices,
} from "./lib/services.mjs";
import { workerOptions } from "./lib/worker.mjs";

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
    home: createHomeHandler({ stars: createStarCounts() }),
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
  if (!env.GITHUB_TOKEN) {
    console.warn("[pghacker-mcp] No bootstrap token supplied. Configure a provider in Admin Console -> Model Providers before running agents.");
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
