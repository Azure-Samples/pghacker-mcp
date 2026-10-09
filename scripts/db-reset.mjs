import { spawnSync } from "node:child_process";
import { readLocalConfig, serviceEnvironments } from "./lib/config.mjs";
import { processIsAlive, readPortalRecord, runDatabase } from "./lib/processes.mjs";
import { binaryPath } from "./lib/services.mjs";

// Drops schema pghacker and the graph pghacker_graph (PilotSwarm's own data
// stays), then bootstraps and migrates them again. For local development.
try {
  const config = readLocalConfig();
  const record = readPortalRecord(config);
  if (record && processIsAlive(record.pid)) throw new Error("Stop the app first: npm run stop.");
  await runDatabase("up", config);
  const result = spawnSync(binaryPath(config, "pghacker-indexer"), ["reset", "--yes"], {
    cwd: config.root, env: serviceEnvironments(config).bootstrap, stdio: "inherit",
  });
  if (result.status !== 0) throw new Error("pghacker-indexer reset failed.");
  console.log("[pghacker-mcp] Reset schema pghacker and graph pghacker_graph.");
} catch (error) {
  console.error(`[pghacker-mcp] ${error.message}`);
  process.exitCode = 1;
}
