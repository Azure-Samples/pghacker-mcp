import { readLocalConfig } from "./lib/config.mjs";
import { runDatabase, stopPortal } from "./lib/processes.mjs";
import { stopRecordedServices } from "./lib/services.mjs";

try {
  const config = readLocalConfig();
  const stopped = await stopPortal(config);
  console.log(`[pghacker-mcp] ${stopped ? "Portal and services stopped." : "No recorded portal is running."}`);
  const orphans = stopRecordedServices(config);
  if (orphans) console.log(`[pghacker-mcp] Stopped ${orphans} service process(es) left by an earlier run.`);
  await runDatabase("stop", config);
  console.log("[pghacker-mcp] PostgreSQL stopped. Its data volume is preserved.");
} catch (error) {
  console.error(`[pghacker-mcp] Stop failed: ${error.message}`);
  process.exitCode = 1;
}
