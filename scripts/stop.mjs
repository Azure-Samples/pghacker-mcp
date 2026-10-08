import { readLocalConfig } from "./lib/config.mjs";
import { runDatabase, stopPortal } from "./lib/processes.mjs";

try {
  const config = readLocalConfig();
  const stopped = await stopPortal(config);
  console.log(`[pghacker-mcp] ${stopped ? "Portal stopped." : "No recorded portal is running."}`);
  await runDatabase("stop", config);
  console.log("[pghacker-mcp] PostgreSQL stopped. Its data volume is preserved.");
} catch (error) {
  console.error(`[pghacker-mcp] Stop failed: ${error.message}`);
  process.exitCode = 1;
}
