import { readLocalConfig } from "./lib/config.mjs";
import { checkPortal } from "./lib/health.mjs";

try {
  const { base } = await checkPortal(readLocalConfig());
  console.log(`[smoke] ${base}: runtime ready, one worker, no sign-in, pghacker-mcp branding.`);
} catch (error) {
  console.error(`[smoke] ${error.message}`);
  process.exitCode = 1;
}
