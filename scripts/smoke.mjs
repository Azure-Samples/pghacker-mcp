// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { readLocalConfig, serviceUrl } from "./lib/config.mjs";
import { checkPortal, checkSite } from "./lib/health.mjs";
import { checkServices, checkSkills } from "./lib/services.mjs";

try {
  const config = readLocalConfig();
  const tools = await checkServices(config);
  const skills = await checkSkills(serviceUrl(config.mcpPort));
  const { base } = await checkPortal(config);
  const site = await checkSite(config);
  console.log(`[smoke] pghacker-indexer lists ${tools.indexer.length} tools; pghacker-mcp lists ${tools.mcp.length}.`);
  console.log(`[smoke] pghacker-mcp serves the skills ${skills.join(", ")}, each file matching its digest.`);
  console.log(`[smoke] ${base}/portal/: runtime ready, one worker, the app's agents, no sign-in, pghacker-mcp branding.`);
  console.log(`[smoke] ${site.home}: the home page and its files, share links sent to the portal, the portal at /portal, pghacker-mcp at /mcp (${site.tools.length} tools).`);
} catch (error) {
  console.error(`[smoke] ${error.message}`);
  process.exitCode = 1;
}
