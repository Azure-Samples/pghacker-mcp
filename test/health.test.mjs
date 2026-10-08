import assert from "node:assert/strict";
import test from "node:test";
import { checkPortal } from "../scripts/lib/health.mjs";

const config = { portalPort: 3001, project: "pghacker-mcp-unit-test" };
const replies = {
  "/api/v1/bootstrap": { ok: true, mode: "local", workerCount: 0, creatableAgents: [] },
  "/api/v1/health": { ok: true, started: true, mode: "local", apiVersion: "v1" },
  "/api/v1/auth/config": { provider: "none", enabled: false },
  "/api/portal-config": { ok: true, portal: { branding: { title: "pghacker-mcp", pageTitle: "pghacker-mcp" } } },
  "/api/v1/workers": {
    ok: true,
    result: [{ workerNodeId: config.project, phase: "ready", updatedAt: new Date().toISOString() }],
  },
};

function fakeFetch(overrides = {}, calls = []) {
  return async (url) => {
    const route = new URL(url).pathname;
    calls.push(route);
    if (route === "/") return new Response("<html></html>", { headers: { "content-type": "text/html" } });
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

test("the actual worker count and blank agent roster are checked", async () => {
  await assert.rejects(checkPortal(config, {
    fetchImpl: fakeFetch({ "/api/v1/bootstrap": { ...replies["/api/v1/bootstrap"], workerCount: 4 } }),
  }), /additional portal-managed workers/);
  await assert.rejects(checkPortal(config, {
    fetchImpl: fakeFetch({ "/api/v1/bootstrap": { ...replies["/api/v1/bootstrap"], creatableAgents: [{ name: "demo" }] } }),
  }), /agent roster/);
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
