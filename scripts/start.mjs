import { mkdirSync } from "node:fs";
import path from "node:path";
import { readLocalConfig, portalEnvironment, LOCAL_HOST } from "./lib/config.mjs";
import {
  assertPortAvailable, preparePortalRecord, writePortalRecord, clearPortalRecord, runDatabase,
} from "./lib/processes.mjs";
import { checkPortal } from "./lib/health.mjs";
import { workerOptions } from "./lib/worker.mjs";

let server;
let worker;
let workerStop;
function stopWorker() {
  if (worker) workerStop ??= worker.stop();
  return workerStop;
}

try {
  const config = readLocalConfig();
  preparePortalRecord(config);
  await assertPortAvailable(config.portalPort);
  await runDatabase("up", config);

  const env = portalEnvironment(config);
  for (const key of Object.keys(process.env)) {
    if (!(key in env)) delete process.env[key];
  }
  Object.assign(process.env, env);
  process.chdir(config.root);
  process.title = config.project;
  for (const directory of ["sessions", "artifacts", "exports", "logs"]) {
    mkdirSync(path.join(config.runtimeDir, directory), { recursive: true });
  }
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
    throw new Error("The blank starter must not load automatic system agents.");
  }
  for (const signal of ["SIGINT", "SIGTERM"]) {
    process.once(signal, () => {
      stopWorker().catch((error) => {
        console.error(`[pghacker-mcp] Worker shutdown failed: ${error.message}`);
        process.exitCode = 1;
      });
    });
  }
  await worker.start();

  const { startServer } = await import("pilotswarm/web");
  server = await startServer({ port: config.portalPort, host: LOCAL_HOST, workers: 0 });
  if (server.address()?.address !== LOCAL_HOST) {
    throw new Error("The portal is not bound exclusively to 127.0.0.1.");
  }
  const { base } = await checkPortal(config);
  console.log(`[pghacker-mcp] Ready at ${base}`);
  console.log("[pghacker-mcp] Local-only, no sign-in. Stop with Ctrl+C or npm run stop.");
  if (!env.GITHUB_TOKEN) {
    console.warn("[pghacker-mcp] No bootstrap token supplied. Configure a provider in Admin Console -> Model Providers before running agents.");
  }
} catch (error) {
  console.error(`[pghacker-mcp] Startup failed: ${error.message}`);
  process.exitCode = 1;
  const deadline = setTimeout(() => process.exit(1), 15_000);
  deadline.unref();
  if (server) {
    try {
      await server.stopPortal();
    } catch (shutdownError) {
      console.error(`[pghacker-mcp] Shutdown failed: ${shutdownError.message}`);
    }
  }
  try {
    await stopWorker();
  } catch (shutdownError) {
    console.error(`[pghacker-mcp] Worker shutdown failed: ${shutdownError.message}`);
  }
}
