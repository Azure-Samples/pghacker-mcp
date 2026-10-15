// Checks a deployment's routes from outside, as a visitor and read-only
// (scripts/lib/health.mjs, checkPublic): the public home page, the portal
// behind GitHub sign-in, and the anonymous MCP endpoint.
//   npm run check:public -- --url "$(terraform -chdir=deploy/terraform output -raw public_url)"
import { parseArgs } from "node:util";
import { checkPublic } from "./lib/health.mjs";

try {
  const { values } = parseArgs({ options: { url: { type: "string" } } });
  const url = new URL(values.url ?? "");
  if (url.protocol !== "https:" || url.pathname !== "/" || url.search) {
    throw new Error("Give the deployment's public origin: --url https://<host>.");
  }
  const checks = await checkPublic(url.origin);
  for (const { name, ok, detail } of checks) {
    console.log(`${ok ? "✔" : "✖"} ${name}: ${detail}`);
  }
  const failed = checks.filter((check) => !check.ok).length;
  console.log(failed ? `[check:public] ${failed} of ${checks.length} checks failed.` : `[check:public] All ${checks.length} checks passed.`);
  process.exitCode = failed ? 1 : 0;
} catch (error) {
  console.error(`[check:public] ${error.code === "ERR_INVALID_URL" ? "Give the deployment's public origin: --url https://<host>." : error.message}`);
  process.exitCode = 1;
}
