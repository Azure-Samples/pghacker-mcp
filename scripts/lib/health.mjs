// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { LOCAL_HOST } from "./config.mjs";
import { EXPECTED_TOOLS, listTools } from "./services.mjs";

/** The agents the app's plugin defines, which the portal must offer. */
export const APP_AGENTS = ["pghacker-crawler", "pghacker-dedup", "pghacker-eval", "pghacker-extract"];

/** An agent's short name, whatever form the portal lists it in. */
export function agentName(agent) {
  const name = typeof agent === "string" ? agent : agent?.name ?? agent?.id ?? "";
  return String(name).split(/[/:]/).pop();
}

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
  const roster = Array.isArray(bootstrap.creatableAgents) ? bootstrap.creatableAgents.map(agentName).sort() : null;
  if (!roster || roster.join(",") !== APP_AGENTS.join(",")) {
    throw new Error(`The portal must offer exactly the app's agents (${APP_AGENTS.join(", ")}), not ${roster?.join(", ") || "none"}.`);
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
    throw new Error("The localhost-only app must advertise disabled sign-in.");
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

  const page = await fetchImpl(`${base}/portal/`, { signal: AbortSignal.timeout(timeout) });
  const html = page.ok && page.headers.get("content-type")?.includes("text/html") ? await page.text() : "";
  if (!html.includes('src="/assets/')) {
    throw new Error("The browser portal did not serve its HTML application at /portal/.");
  }
  return { base, bootstrap, health };
}

/** The home page's marker, which PilotSwarm's page doesn't have. */
const HOME_MARKER = '<meta name="application-name" content="pghacker-mcp"';

/** The scripts that `html` loads from /static/: the page's, and its theme's before the first paint. */
function staticScripts(html) {
  return [...html.matchAll(/src="(\/static\/[A-Za-z0-9_.-]+\.js)"/g)].map((match) => match[1]);
}

/**
 * The site's routes through the edge on PORTAL_PORT, as scripts/lib/routes.mjs
 * gives them: the home page and its files, the portal's share links sent on
 * to the portal, the portal at /portal and /portal/, pghacker-mcp at /mcp, and
 * a 404 for anything else.
 */
export async function checkSite(config, { timeout = 30_000, fetchImpl = fetch, listToolsImpl = listTools } = {}) {
  const base = `http://${LOCAL_HOST}:${config.portalPort}`;
  const get = (route, init = {}) => fetchImpl(`${base}${route}`, { signal: AbortSignal.timeout(timeout), ...init });

  const home = await get("/");
  const html = await home.text();
  if (!home.ok || !home.headers.get("content-type")?.includes("text/html") || !html.includes(HOME_MARKER)) {
    throw new Error("The site did not serve the home page at /. Is home/dist built (npm run setup)?");
  }
  if (!/no-cache/.test(home.headers.get("cache-control") ?? "")) {
    throw new Error("The home page must be served no-cache: its asset names change with each build.");
  }
  // PilotSwarm's page polls "/" for assets/*.js names to see whether it was
  // redeployed; the home page must not look like a different build of it.
  if (/assets\/[A-Za-z0-9_.-]+\.js/.test(html)) {
    throw new Error("The home page refers to assets/*.js, which the portal would take for a new build of itself.");
  }
  const scripts = staticScripts(html);
  if (scripts.length === 0) {
    throw new Error("The home page names no script under /static/.");
  }
  for (const script of scripts) {
    const asset = await get(script);
    await asset.arrayBuffer();
    if (!asset.ok || !/immutable/.test(asset.headers.get("cache-control") ?? "")) {
      throw new Error(`The home page's script ${script} did not load from /static/ as an immutable file.`);
    }
  }

  const share = await get("/?session=smoke-test", { redirect: "manual" });
  if (share.status !== 302 || share.headers.get("location") !== "/portal/?session=smoke-test") {
    throw new Error("A portal share link (/?session=...) was not sent on to /portal/.");
  }
  for (const route of ["/portal", "/portal/"]) {
    const portal = await get(route);
    if (!portal.ok || !(await portal.text()).includes('src="/assets/')) {
      throw new Error(`The site did not serve the PilotSwarm portal at ${route}.`);
    }
  }
  const missing = await get("/no-such-page");
  await missing.arrayBuffer();
  if (missing.status !== 404) throw new Error(`An unknown path answered HTTP ${missing.status}, not 404.`);

  const mcp = `${base}/mcp`;
  const tools = await listToolsImpl(mcp, { timeout });
  const absent = EXPECTED_TOOLS.mcp.filter((tool) => !tools.includes(tool));
  if (absent.length) throw new Error(`pghacker-mcp at ${mcp} does not list ${absent.join(", ")}.`);
  return { base, home: `${base}/`, portal: `${base}/portal/`, mcp, tools };
}

const SIGN_IN = /github\.com\/login\/oauth\/authorize|\/oauth2\/(start|sign_in)/;

/** The portal's paths, which a separate public origin doesn't serve. */
const PORTAL_ROUTES = ["/portal/", "/api/v1/health", "/assets/check-public.js", "/oauth2/start", "/check-public"];

/**
 * A deployment's routes as a visitor sees them, read-only. On `origin`, the
 * public one: the home page and its files without sign-in, the portal's
 * share links sent on to /portal/ on the portal's host, and pghacker-mcp
 * answering anonymously. On `portal`'s (Front Door's own host name, the same
 * origin without a custom domain): the portal, its files and anything else
 * behind GitHub sign-in, which comes back to that host's callback; the
 * portal's API answering 401; and both, whatever X-Forwarded-Uri a client
 * forges. A separate public origin must answer 404 for the portal's paths.
 * Each check is { name, ok, detail }; nothing signs in or changes anything.
 */
export async function checkPublic(origin, {
  portal = origin, timeout = 30_000, fetchImpl = fetch, listToolsImpl = listTools,
} = {}) {
  const base = new URL(origin).origin;
  const portalBase = new URL(portal).origin;
  const request = (at, route, headers = {}) => fetchImpl(`${at}${route}`, {
    headers, redirect: "manual", signal: AbortSignal.timeout(timeout),
  });
  const get = (route) => request(base, route);
  const status = async (at, route, expected, headers) => {
    const response = await request(at, route, headers);
    await response.arrayBuffer();
    return { ok: response.status === expected, detail: `HTTP ${response.status}` };
  };
  const signIn = async (route, headers) => {
    const response = await request(portalBase, route, headers);
    await response.arrayBuffer();
    const location = response.headers.get("location") ?? "";
    return { ok: response.status === 302 && SIGN_IN.test(location), detail: `HTTP ${response.status} ${location.split("?")[0]}`.trim() };
  };
  const tools = async (at) => {
    const listed = await listToolsImpl(`${at}/mcp`, { timeout, fetchImpl });
    const missing = EXPECTED_TOOLS.mcp.filter((tool) => !listed.includes(tool));
    return { ok: missing.length === 0, detail: missing.length ? `missing ${missing.join(", ")}` : `${listed.length} tools` };
  };
  const checks = [];
  const check = async (name, run) => {
    try {
      checks.push({ name, ...(await run()) });
    } catch (error) {
      checks.push({ name, ok: false, detail: error.message });
    }
  };

  let scripts = [];
  await check("the home page, without sign-in", async () => {
    const response = await get("/");
    const html = await response.text();
    scripts = staticScripts(html);
    const ok = response.status === 200 && html.includes(HOME_MARKER)
      && /no-cache/.test(response.headers.get("cache-control") ?? "")
      && Boolean(response.headers.get("content-security-policy"))
      && !/assets\/[A-Za-z0-9_.-]+\.js/.test(html);
    return { ok, detail: `HTTP ${response.status}, ${response.headers.get("cache-control") ?? "no cache-control"}` };
  });
  await check("its scripts from /static/, kept a year", async () => {
    if (scripts.length === 0) return { ok: false, detail: "the page names no /static/ script" };
    const results = [];
    for (const script of scripts) {
      const response = await get(script);
      await response.arrayBuffer();
      results.push({ script, ok: response.status === 200 && /immutable/.test(response.headers.get("cache-control") ?? ""), status: response.status });
    }
    return {
      ok: results.every((result) => result.ok),
      detail: results.map((result) => `HTTP ${result.status} ${result.script}`).join(", "),
    };
  });
  // A share link can name any of the hosts, such as one made while the
  // portal was on a custom domain: the home page sends it on to the
  // portal's own host.
  for (const route of ["/?session=check-public", "/?canvasShare=check-public"]) {
    await check(`${route} goes on to the portal's host`, async () => {
      const response = await get(route);
      await response.arrayBuffer();
      const location = response.headers.get("location");
      const expected = `${portalBase}/portal/${route.slice(1)}`;
      return { ok: response.status === 302 && location === expected, detail: `HTTP ${response.status} ${location ?? ""}`.trim() };
    });
  }
  await check("pghacker-mcp lists its tools anonymously at /mcp", () => tools(base));
  if (portalBase !== base) {
    for (const route of PORTAL_ROUTES) {
      await check(`the public domain answers 404 for ${route}`, () => status(base, route, 404));
    }
    await check("pghacker-mcp answers at the portal's host's /mcp too", () => tools(portalBase));
  }

  for (const route of ["/portal/", "/portal", "/assets/check-public.js", "/check-public"]) {
    await check(`${route} asks for GitHub sign-in`, () => signIn(route));
  }
  await check("sign-in comes back to the portal's host", async () => {
    const response = await request(portalBase, "/portal/");
    await response.arrayBuffer();
    const callback = URL.parse(response.headers.get("location") ?? "")?.searchParams.get("redirect_uri");
    return { ok: callback === `${portalBase}/oauth2/callback`, detail: callback ?? `HTTP ${response.status}, no redirect_uri` };
  });
  await check("the portal's API answers 401 without sign-in", () => status(portalBase, "/api/v1/health", 401));
  for (const claimed of ["/mcp", "/"]) {
    const forged = { "x-forwarded-uri": claimed };
    await check(`/portal/ asks for GitHub sign-in, claiming X-Forwarded-Uri: ${claimed}`, () => signIn("/portal/", forged));
    await check(`the portal's API answers 401, claiming X-Forwarded-Uri: ${claimed}`, () => status(portalBase, "/api/v1/health", 401, forged));
  }
  return checks;
}
