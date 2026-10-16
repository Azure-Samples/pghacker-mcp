// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

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

const PUBLIC = "https://mcp.example.org";
const PORTAL = "https://pghacker-example.z01.azurefd.net";

/**
 * A deployment as a visitor sees it through Front Door and oauth2-proxy: the
 * portal and every other path on `portal`, Front Door's host name, and only
 * the public paths on `origin`. `overrides` replace answers by URL; each gets
 * the request's headers.
 */
function deploymentFetch({ origin = PUBLIC, portal = PORTAL, overrides = {} } = {}) {
  const signIn = () => new Response(null, {
    status: 302,
    headers: {
      location: `https://github.com/login/oauth/authorize?client_id=example&redirect_uri=${encodeURIComponent(`${portal}/oauth2/callback`)}&state=x`,
    },
  });
  // The home page sends share links on to the portal's host, from either.
  const shareLink = (route) => () => new Response(null, { status: 302, headers: { location: `${portal}/portal/${route.slice(1)}` } });
  const site = {
    "/": () => html(HOME_PAGE, { "cache-control": "no-cache", "content-security-policy": "default-src 'self'" }),
    "/static/index-abc.js": () => new Response("export {};", { headers: IMMUTABLE }),
    "/static/theme-boot-def.js": () => new Response("(() => {})();", { headers: IMMUTABLE }),
    "/?session=check-public": shareLink("/?session=check-public"),
    "/?canvasShare=check-public": shareLink("/?canvasShare=check-public"),
  };
  const portalRoutes = {
    ...site,
    "/portal/": signIn,
    "/portal": signIn,
    "/assets/check-public.js": signIn,
    "/check-public": signIn,
    "/oauth2/start": signIn,
    "/api/v1/health": () => new Response('{"error":"Unauthorized"}', { status: 401 }),
  };
  // Front Door answers 404 for a path that no route of the host takes.
  const notServed = () => new Response("Not found", { status: 404 });
  const publicRoutes = Object.fromEntries(Object.keys(portalRoutes).map((route) => [route, site[route] ?? notServed]));
  const at = (host, routes) => Object.entries(routes).map(([route, answer]) => [`${host}${route}`, answer]);
  const answers = {
    ...Object.fromEntries(at(origin, origin === portal ? portalRoutes : publicRoutes)),
    ...Object.fromEntries(at(portal, portalRoutes)),
    ...overrides,
  };
  return async (url, { headers = {} } = {}) => {
    const answer = answers[url];
    if (!answer) throw new Error(`Unexpected test URL: ${url}`);
    return answer(headers);
  };
}

const PUBLIC_CHECKS = [
  "the home page, without sign-in",
  "its scripts from /static/, kept a year",
  "/?session=check-public goes on to the portal's host",
  "/?canvasShare=check-public goes on to the portal's host",
  "pghacker-mcp lists its tools anonymously at /mcp",
];
const PORTAL_CHECKS = [
  "/portal/ asks for GitHub sign-in",
  "/portal asks for GitHub sign-in",
  "/assets/check-public.js asks for GitHub sign-in",
  "/check-public asks for GitHub sign-in",
  "sign-in comes back to the portal's host",
  "the portal's API answers 401 without sign-in",
  "/portal/ asks for GitHub sign-in, claiming X-Forwarded-Uri: /mcp",
  "the portal's API answers 401, claiming X-Forwarded-Uri: /mcp",
  "/portal/ asks for GitHub sign-in, claiming X-Forwarded-Uri: /",
  "the portal's API answers 401, claiming X-Forwarded-Uri: /",
];

test("a deployment checks out from outside: home and MCP public on its domain, the portal behind sign-in on Front Door's host alone", async () => {
  const checks = await checkPublic(PUBLIC, { portal: `${PORTAL}/portal/`, fetchImpl: deploymentFetch(), listToolsImpl: allTools });
  assert.deepEqual(checks.map((check) => check.name), [
    ...PUBLIC_CHECKS,
    "the public domain answers 404 for /portal/",
    "the public domain answers 404 for /api/v1/health",
    "the public domain answers 404 for /assets/check-public.js",
    "the public domain answers 404 for /oauth2/start",
    "the public domain answers 404 for /check-public",
    "pghacker-mcp answers at the portal's host's /mcp too",
    ...PORTAL_CHECKS,
  ]);
  assert.deepEqual(checks.filter((check) => !check.ok), []);
});

test("without a custom domain, one host serves the home page, MCP and the portal", async () => {
  const checks = await checkPublic(PORTAL, {
    fetchImpl: deploymentFetch({ origin: PORTAL }),
    listToolsImpl: allTools,
  });
  assert.deepEqual(checks.map((check) => check.name), [...PUBLIC_CHECKS, ...PORTAL_CHECKS]);
  assert.deepEqual(checks.filter((check) => !check.ok), []);
});

test("each failure of a deployment's routes is reported, not thrown", async () => {
  const portalPage = () => html(PORTAL_PAGE);
  const signInAt = (host) => () => new Response(null, {
    status: 302,
    headers: { location: `https://github.com/login/oauth/authorize?redirect_uri=${encodeURIComponent(`${host}/oauth2/callback`)}&state=x` },
  });
  // A forged X-Forwarded-Uri gets past sign-in, as before oauth2-proxy stopped trusting it.
  const bypass = (answer) => (headers) => (headers["x-forwarded-uri"] ? portalPage() : answer());
  const checks = await checkPublic(PUBLIC, {
    portal: PORTAL,
    fetchImpl: deploymentFetch({
      overrides: {
        [`${PUBLIC}/`]: portalPage,
        [`${PUBLIC}/static/theme-boot-def.js`]: () => new Response("Not found", { status: 404 }),
        // A share link sent on to /portal/ on the public domain, which doesn't serve it.
        [`${PUBLIC}/?session=check-public`]: () => new Response(null, { status: 302, headers: { location: "/portal/?session=check-public" } }),
        [`${PUBLIC}/portal/`]: signInAt(PUBLIC),
        [`${PORTAL}/portal/`]: bypass(signInAt(PUBLIC)),
        [`${PORTAL}/api/v1/health`]: bypass(() => new Response('{"error":"Unauthorized"}', { status: 401 })),
      },
    }),
    listToolsImpl: async () => { throw new Error("HTTP 429"); },
  });
  const failed = checks.filter((check) => !check.ok);
  assert.deepEqual(failed.map((check) => check.name), [
    "the home page, without sign-in",
    "its scripts from /static/, kept a year",
    "/?session=check-public goes on to the portal's host",
    "pghacker-mcp lists its tools anonymously at /mcp",
    "the public domain answers 404 for /portal/",
    "pghacker-mcp answers at the portal's host's /mcp too",
    "sign-in comes back to the portal's host",
    "/portal/ asks for GitHub sign-in, claiming X-Forwarded-Uri: /mcp",
    "the portal's API answers 401, claiming X-Forwarded-Uri: /mcp",
    "/portal/ asks for GitHub sign-in, claiming X-Forwarded-Uri: /",
    "the portal's API answers 401, claiming X-Forwarded-Uri: /",
  ]);
  const detail = (name) => failed.find((check) => check.name === name).detail;
  assert.equal(detail("/?session=check-public goes on to the portal's host"), "HTTP 302 /portal/?session=check-public");
  assert.match(detail("pghacker-mcp lists its tools anonymously at /mcp"), /HTTP 429/);
  assert.equal(detail("sign-in comes back to the portal's host"), `${PUBLIC}/oauth2/callback`);
});
