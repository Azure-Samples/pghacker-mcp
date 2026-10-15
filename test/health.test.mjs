import assert from "node:assert/strict";
import test from "node:test";
import { APP_AGENTS, checkPortal, checkPublic, checkSite } from "../scripts/lib/health.mjs";
import { EXPECTED_TOOLS } from "../scripts/lib/services.mjs";

const config = { portalPort: 3001, project: "pghacker-mcp-unit-test" };
const replies = {
  "/api/v1/bootstrap": { ok: true, mode: "local", workerCount: 0, creatableAgents: APP_AGENTS.map((name) => ({ name })) },
  "/api/v1/health": { ok: true, started: true, mode: "local", apiVersion: "v1" },
  "/api/v1/auth/config": { provider: "none", enabled: false },
  "/api/portal-config": { ok: true, portal: { branding: { title: "pghacker-mcp", pageTitle: "pghacker-mcp" } } },
  "/api/v1/workers": {
    ok: true,
    result: [{ workerNodeId: config.project, phase: "ready", updatedAt: new Date().toISOString() }],
  },
};

const PORTAL_PAGE = '<!doctype html><title>PilotSwarm</title><script type="module" crossorigin src="/assets/index-BrOn9Elg.js"></script>';
const HOME_PAGE = '<!doctype html><meta name="application-name" content="pghacker-mcp"><script type="module" crossorigin src="/static/index-abc.js"></script><script src="/static/theme-boot-def.js"></script>';
const IMMUTABLE = { "cache-control": "public, max-age=31536000, immutable" };

function fakeFetch(overrides = {}, calls = []) {
  return async (url) => {
    const route = new URL(url).pathname;
    calls.push(route);
    if (route === "/portal/") return new Response(PORTAL_PAGE, { headers: { "content-type": "text/html" } });
    const body = { ...replies, ...overrides }[route];
    if (!body) throw new Error(`Unexpected test route: ${route}`);
    return new Response(JSON.stringify(body), { headers: { "content-type": "application/json" } });
  };
}

test("readiness starts the runtime before checking health and branding", async () => {
  const calls = [];
  const result = await checkPortal(config, { fetchImpl: fakeFetch({}, calls) });
  assert.equal(calls[0], "/api/v1/bootstrap");
  assert.equal(result.base, "http://127.0.0.1:3001");
  assert.equal(result.health.started, true);
  assert.equal(calls.length, 6);
});

test("HTTP 200 is not readiness when the runtime has not started", async () => {
  await assert.rejects(checkPortal(config, {
    fetchImpl: fakeFetch({ "/api/v1/health": { ok: true, started: false, mode: "local" } }),
  }), /not just HTTP 200/);
});

test("the actual worker count and the app's agent roster are checked", async () => {
  await assert.rejects(checkPortal(config, {
    fetchImpl: fakeFetch({ "/api/v1/bootstrap": { ...replies["/api/v1/bootstrap"], workerCount: 4 } }),
  }), /additional portal-managed workers/);
  for (const creatableAgents of [[], [{ name: "demo" }], APP_AGENTS.slice(1).map((name) => ({ name })),
    [...APP_AGENTS, "generic-crawler"].map((name) => ({ name }))]) {
    await assert.rejects(checkPortal(config, {
      fetchImpl: fakeFetch({ "/api/v1/bootstrap": { ...replies["/api/v1/bootstrap"], creatableAgents } }),
    }), /exactly the app's agents/);
  }
  const namespaced = APP_AGENTS.map((name) => ({ name: `pghacker-mcp:${name}` }));
  await checkPortal(config, {
    fetchImpl: fakeFetch({ "/api/v1/bootstrap": { ...replies["/api/v1/bootstrap"], creatableAgents: namespaced } }),
  });
});

test("the app-owned worker must actually be registered, ready, and recently alive", async () => {
  for (const result of [
    [],
    [{ workerNodeId: "other-app", phase: "ready", updatedAt: new Date().toISOString() }],
    [{ workerNodeId: config.project, phase: "starting", updatedAt: new Date().toISOString() }],
    [{ workerNodeId: config.project, phase: "ready", updatedAt: "2000-01-01T00:00:00Z" }],
    [{ workerNodeId: config.project, phase: "ready", updatedAt: "2999-01-01T00:00:00Z" }],
    [{ workerNodeId: config.project, phase: "ready", updatedAt: "invalid" }],
  ]) {
    await assert.rejects(checkPortal(config, {
      fetchImpl: fakeFetch({ "/api/v1/workers": { ok: true, result } }),
    }), /app-owned SDK worker must be registered and ready/);
  }
});

test("auth and both branding titles must match the starter", async () => {
  await assert.rejects(checkPortal(config, {
    fetchImpl: fakeFetch({ "/api/v1/auth/config": { provider: "entra", enabled: true } }),
  }), /disabled sign-in/);
  await assert.rejects(checkPortal(config, {
    fetchImpl: fakeFetch({ "/api/portal-config": { ok: true, portal: { branding: { title: "PilotSwarm", pageTitle: "PilotSwarm" } } } }),
  }), /did not load pghacker-mcp/);
});

test("API failures are not treated as a running application", async () => {
  await assert.rejects(checkPortal(config, {
    fetchImpl: async () => new Response(JSON.stringify({
      ok: false, error: { code: "INTERNAL_ERROR", message: "Runtime unavailable" },
    }), { status: 500 }),
  }), /bootstrap failed \(HTTP 500\): Runtime unavailable/);
});

test("non-JSON API responses fail explicitly", async () => {
  await assert.rejects(checkPortal(config, {
    fetchImpl: async () => new Response("<html>unexpected</html>"),
  }), /did not return JSON/);
});

test("the portal's page must be PilotSwarm's, at /portal/", async () => {
  await assert.rejects(checkPortal(config, {
    fetchImpl: async (url) => {
      const route = new URL(url).pathname;
      if (route === "/portal/") return new Response(HOME_PAGE, { headers: { "content-type": "text/html" } });
      return fakeFetch()(url);
    },
  }), /HTML application at \/portal\//);
});

const html = (body, headers = {}) => new Response(body, { headers: { "content-type": "text/html; charset=utf-8", ...headers } });

/** The site through the edge, as it should answer; `overrides` by path and query. */
function siteFetch(overrides = {}) {
  const routes = {
    "/": () => html(HOME_PAGE, { "cache-control": "no-cache" }),
    "/static/index-abc.js": () => new Response("export {};", {
      headers: { "content-type": "text/javascript", ...IMMUTABLE },
    }),
    "/static/theme-boot-def.js": () => new Response("(() => {})();", {
      headers: { "content-type": "text/javascript", ...IMMUTABLE },
    }),
    "/?session=smoke-test": () => new Response(null, { status: 302, headers: { location: "/portal/?session=smoke-test" } }),
    "/portal": () => html(PORTAL_PAGE),
    "/portal/": () => html(PORTAL_PAGE),
    "/no-such-page": () => new Response("Not found", { status: 404 }),
    ...overrides,
  };
  return async (url) => {
    const { pathname, search } = new URL(url);
    const route = routes[`${pathname}${search}`];
    if (!route) throw new Error(`Unexpected test route: ${pathname}${search}`);
    return route();
  };
}
const allTools = async () => EXPECTED_TOOLS.mcp;

test("the site serves the home page, its files, the portal and /mcp on one origin", async () => {
  const site = await checkSite(config, { fetchImpl: siteFetch(), listToolsImpl: allTools });
  assert.equal(site.home, "http://127.0.0.1:3001/");
  assert.equal(site.portal, "http://127.0.0.1:3001/portal/");
  assert.equal(site.mcp, "http://127.0.0.1:3001/mcp");
});

test("the site's checks catch each broken route", async () => {
  const cases = [
    [{ "/": () => html(PORTAL_PAGE, { "cache-control": "no-cache" }) }, /home page at \//],
    [{ "/": () => html(HOME_PAGE, { "cache-control": "public, max-age=600" }) }, /no-cache/],
    [{ "/": () => html(HOME_PAGE.replace("/static/", "/assets/"), { "cache-control": "no-cache" }) }, /assets\/\*\.js/],
    [{ "/static/index-abc.js": () => new Response("x", { headers: { "cache-control": "no-cache" } }) }, /immutable/],
    [{ "/static/theme-boot-def.js": () => new Response("Not found", { status: 404 }) }, /theme-boot-def\.js/],
    [{ "/": () => html(HOME_PAGE.replaceAll(/ src="\/static\/[^"]+"/g, ""), { "cache-control": "no-cache" }) }, /no script under \/static\//],
    [{ "/?session=smoke-test": () => html(HOME_PAGE) }, /share link/],
    [{ "/portal": () => html(HOME_PAGE) }, /portal at \/portal\./],
    [{ "/portal/": () => new Response("Not found", { status: 404 }) }, /portal at \/portal\/\./],
    [{ "/no-such-page": () => html(HOME_PAGE) }, /not 404/],
  ];
  for (const [overrides, expected] of cases) {
    await assert.rejects(checkSite(config, { fetchImpl: siteFetch(overrides), listToolsImpl: allTools }), expected);
  }
  await assert.rejects(checkSite(config, {
    fetchImpl: siteFetch(),
    listToolsImpl: async () => EXPECTED_TOOLS.mcp.slice(1),
  }), new RegExp(`does not list ${EXPECTED_TOOLS.mcp[0]}`));
});

/** A deployment as a visitor sees it through Front Door and oauth2-proxy. */
function deploymentFetch(overrides = {}) {
  const signIn = () => new Response(null, {
    status: 302,
    headers: { location: "https://github.com/login/oauth/authorize?client_id=example&state=x" },
  });
  const routes = {
    "/": () => html(HOME_PAGE, { "cache-control": "no-cache", "content-security-policy": "default-src 'self'" }),
    "/static/index-abc.js": () => new Response("export {};", { headers: IMMUTABLE }),
    "/static/theme-boot-def.js": () => new Response("(() => {})();", { headers: IMMUTABLE }),
    "/?session=check-public": () => new Response(null, { status: 302, headers: { location: "/portal/?session=check-public" } }),
    "/portal/": signIn,
    "/portal": signIn,
    "/assets/check-public.js": signIn,
    "/check-public": signIn,
    "/api/v1/health": () => new Response('{"error":"Unauthorized"}', { status: 401 }),
    ...overrides,
  };
  return async (url) => {
    const { pathname, search } = new URL(url);
    const route = routes[`${pathname}${search}`];
    if (!route) throw new Error(`Unexpected test route: ${pathname}${search}`);
    return route();
  };
}

test("a deployment checks out from outside: home public, portal behind sign-in, MCP anonymous", async () => {
  const checks = await checkPublic("https://pghacker-example.z01.azurefd.net", {
    fetchImpl: deploymentFetch(),
    listToolsImpl: allTools,
  });
  assert.equal(checks.length, 9);
  assert.deepEqual(checks.filter((check) => !check.ok), []);
});

test("each failure of a deployment's routes is reported, not thrown", async () => {
  const portalOpen = () => html(PORTAL_PAGE);
  const checks = await checkPublic("https://pghacker-example.z01.azurefd.net/", {
    fetchImpl: deploymentFetch({
      "/": () => html(PORTAL_PAGE),
      "/static/theme-boot-def.js": () => new Response("Not found", { status: 404 }),
      "/portal/": portalOpen,
      "/api/v1/health": () => new Response("{}", { status: 200 }),
    }),
    listToolsImpl: async () => { throw new Error("HTTP 429"); },
  });
  const failed = checks.filter((check) => !check.ok).map((check) => check.name);
  assert.deepEqual(failed, [
    "the home page, without sign-in",
    "its scripts from /static/, kept a year",
    "/portal/ asks for GitHub sign-in",
    "the portal's API answers 401 without sign-in",
    "pghacker-mcp lists its tools anonymously at /mcp",
  ]);
  assert.match(checks.at(-1).detail, /HTTP 429/);
});
