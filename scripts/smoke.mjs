import { readLocalConfig } from "./lib/config.mjs";
import { checkPortal } from "./lib/health.mjs";
import { checkServices } from "./lib/services.mjs";

try {
  const config = readLocalConfig();
  const tools = await checkServices(config);
  const { base } = await checkPortal(config);
  console.log(`[smoke] pghacker-indexer lists ${tools.indexer.length} tools; pghacker-mcp lists ${tools.mcp.length}.`);
  console.log(`[smoke] ${base}: runtime ready, one worker, the app's agents, no sign-in, pghacker-mcp branding.`);
} catch (error) {
  console.error(`[smoke] ${error.message}`);
  process.exitCode = 1;
}
