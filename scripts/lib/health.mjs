import { LOCAL_HOST } from "./config.mjs";

async function getJson(base, route, timeout, fetchImpl) {
  const response = await fetchImpl(`${base}${route}`, { signal: AbortSignal.timeout(timeout) });
  let data;
  try {
    data = await response.json();
  } catch (error) {
    throw new Error(`${route} did not return JSON (HTTP ${response.status}).`, { cause: error });
  }
  if (!response.ok || data?.ok === false) {
    const detail = data?.error?.message || data?.error || response.statusText;
    throw new Error(`${route} failed (HTTP ${response.status}): ${detail}.`);
  }
  return data;
}

export async function checkPortal(config, { timeout = 90_000, fetchImpl = fetch } = {}) {
  const base = `http://${LOCAL_HOST}:${config.portalPort}`;
  const bootstrap = await getJson(base, "/api/v1/bootstrap", timeout, fetchImpl);
  if (bootstrap.ok !== true || bootstrap.mode !== "local" || bootstrap.workerCount !== 0) {
    throw new Error("Portal bootstrap must report local mode without additional portal-managed workers.");
  }
  if (!Array.isArray(bootstrap.creatableAgents) || bootstrap.creatableAgents.length !== 0) {
    throw new Error("The blank starter must not expose an app-specific agent roster.");
  }

  const [health, auth, portal, registry] = await Promise.all([
    getJson(base, "/api/v1/health", timeout, fetchImpl),
    getJson(base, "/api/v1/auth/config", timeout, fetchImpl),
    getJson(base, "/api/portal-config", timeout, fetchImpl),
    getJson(base, "/api/v1/workers", timeout, fetchImpl),
  ]);
  if (health.ok !== true || health.started !== true || health.mode !== "local") {
    throw new Error("Portal health must report a started local runtime, not just HTTP 200.");
  }
  if (auth.provider !== "none" || auth.enabled !== false) {
    throw new Error("The localhost-only starter must advertise disabled sign-in.");
  }
  if (portal.ok !== true || portal.portal?.branding?.title !== "pghacker-mcp"
    || portal.portal?.branding?.pageTitle !== "pghacker-mcp") {
    throw new Error("The portal did not load pghacker-mcp branding.");
  }
  if (registry.ok !== true || !Array.isArray(registry.result)) {
    throw new Error("The portal did not return its worker registry.");
  }
  const owned = registry.result.filter((entry) => entry.workerNodeId === config.project);
  const heartbeat = owned.length === 1 && typeof owned[0].updatedAt === "string"
    ? Date.parse(owned[0].updatedAt) : Number.NaN;
  if (owned.length !== 1 || owned[0].phase !== "ready"
    || !Number.isFinite(heartbeat)
    || Math.abs(Date.now() - heartbeat) > 90_000) {
    throw new Error("Exactly one app-owned SDK worker must be registered and ready.");
  }

  const page = await fetchImpl(base, { signal: AbortSignal.timeout(timeout) });
  if (!page.ok || !page.headers.get("content-type")?.includes("text/html")) {
    throw new Error("The browser portal did not serve its HTML application.");
  }
  return { base, bootstrap, health };
}
