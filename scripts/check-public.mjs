// Checks a deployment's routes from outside, as a visitor and read-only
// (scripts/lib/health.mjs, checkPublic): the public home page and the
// anonymous MCP endpoint, and the portal behind GitHub sign-in on its own
// host, Front Door's, which a custom domain doesn't serve.
//   npm run check:public -- --url "$(terraform -chdir=deploy/terraform output -raw public_url)" \
//     --portal-url "$(terraform -chdir=deploy/terraform output -raw portal_url)"
import { parseArgs } from "node:util";
import { checkPublic } from "./lib/health.mjs";

/** The https origin of `value`, a URL whose path is one of `paths`. */
function origin(value, paths, usage) {
  const url = URL.parse(value ?? "");
  if (!url || url.protocol !== "https:" || !paths.includes(url.pathname) || url.search) throw new Error(usage);
  return url.origin;
}

try {
  const { values } = parseArgs({ options: { url: { type: "string" }, "portal-url": { type: "string" } } });
  const url = origin(values.url, ["/"], "Give the deployment's public origin: --url https://<host>.");
  const portal = values["portal-url"] === undefined
    ? url
    : origin(values["portal-url"], ["/", "/portal/"], "Give the portal's URL: --portal-url https://<host>/portal/.");
  const checks = await checkPublic(url, { portal });
  for (const { name, ok, detail } of checks) {
    console.log(`${ok ? "✔" : "✖"} ${name}: ${detail}`);
  }
  const failed = checks.filter((check) => !check.ok).length;
  console.log(failed ? `[check:public] ${failed} of ${checks.length} checks failed.` : `[check:public] All ${checks.length} checks passed.`);
  process.exitCode = failed ? 1 : 0;
} catch (error) {
  console.error(`[check:public] ${error.message}`);
  process.exitCode = 1;
}
