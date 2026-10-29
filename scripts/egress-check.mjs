// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Checks the PilotSwarm pool's lockdown from inside it: the egress-check job
// runs this on that pool with the worker's labels and service account, so the
// worker's NetworkPolicy and the firewall's rules for the pool's subnet apply
// to it as they do to the agents (deploy/README.md, "The cluster").
// What the worker needs must answer: the indexer, pghacker-mcp's internal
// listener, the token proxy and the database. Nothing else may: not the
// internet, not the endpoints that only the nodes' kubelet may reach, not the
// node's instance metadata (its identity's tokens) or Azure's WireServer, not
// the Kubernetes API, nor the other components (PGHACKER_EGRESS_DENIED adds
// their URLs). Any answer counts as reaching one, an untrusted certificate
// included (scripts/lib/egress.mjs). Reports in one JSON line; exits 1 when
// the pool can reach what it mustn't, or can't reach what it needs.
import { reach } from "./lib/egress.mjs";
import { psql } from "./lib/psql.mjs";

const env = process.env;
const started = Date.now();
const failures = [];

/** Outside the cluster: each must be refused. */
const OUTSIDE = [
  "https://www.postgresql.org/",
  "https://github.com/",
  "https://example.com/",
  // Platform endpoints that the firewall opens to the nodes for kubelet.
  "https://login.microsoftonline.com/",
  "https://management.azure.com/",
  "https://mcr.microsoft.com/v2/",
  // The node's instance metadata, which would give its identity's tokens.
  "http://169.254.169.254/metadata/instance?api-version=2021-02-01",
  // Azure's WireServer and its host agent plugin, which hold the node's
  // configuration, its bootstrap credentials among it.
  "http://168.63.129.16/?comp=versions",
  "http://168.63.129.16:32526/vmSettings",
  // The Kubernetes API, through its Service, whose certificate this process
  // doesn't trust: that it answers is enough.
  "https://kubernetes.default.svc/version",
];

const report = { ok: false, reachable: {}, refused: {} };
try {
  // Required paths must reach the application, not an Envoy denial response.
  const needed = {
    indexer: new URL("/healthz", env.PGHACKER_INDEXER_MCP_URL).href,
    mcp_internal: new URL("/healthz", env.PGHACKER_MCP_URL).href,
    token_proxy: new URL("/", env.PGHACKER_FOUNDRY_PROXY_URL).href,
  };
  for (const [name, url] of Object.entries(needed)) {
    const result = await reach(url, { timeoutMs: 15_000 });
    report.reachable[name] = { url, ...result };
    const expected = name === "token_proxy" ? 404 : 200;
    if (!result.answered || result.status !== expected) {
      failures.push(`can't reach ${name} (${url}): ${result.error ?? `HTTP ${result.status}, expected ${expected}`}`);
    }
  }
  try {
    const at = Date.now();
    await psql(env.DATABASE_URL, "SELECT 1", { timeoutMs: 60_000 });
    report.reachable.database = { answered: true, ms: Date.now() - at };
  } catch (error) {
    report.reachable.database = { answered: false, error: String(error.message).slice(0, 300) };
    failures.push(`can't reach the database: ${error.message}`);
  }

  // Refused: no answer at all, or Azure Firewall's own denial (HTTP 470).
  const denied = [...OUTSIDE, ...(env.PGHACKER_EGRESS_DENIED ?? "").split(",").map((url) => url.trim()).filter(Boolean)];
  for (const url of denied) {
    const result = await reach(url, { headers: url.includes("169.254.169.254") ? { Metadata: "true" } : {} });
    report.refused[url] = result;
    if (result.answered && result.status !== 470 && !result.proxyDenied) {
      failures.push(`reached ${url} (${result.status === undefined ? result.error : `HTTP ${result.status}`})`);
    }
  }
  // A sidecar accepts TCP locally even if its upstream is blocked. Require
  // an HTTP/TLS answer from the API, rather than counting a local handshake.
  if (env.KUBERNETES_SERVICE_HOST) {
    const port = env.KUBERNETES_SERVICE_PORT || "443";
    const host = env.KUBERNETES_SERVICE_HOST;
    const target = `https://${host.includes(":") ? `[${host}]` : host}:${port}/version`;
    const result = await reach(target);
    report.refused[target] = result;
    if (result.answered && !result.proxyDenied) failures.push(`reached the Kubernetes API (${target})`);
  }
} catch (error) {
  failures.push(error.message);
}
report.ok = failures.length === 0;
report.failures = failures;
report.seconds = Math.round((Date.now() - started) / 1000);
console.log(JSON.stringify(report));
process.exit(report.ok ? 0 : 1);
