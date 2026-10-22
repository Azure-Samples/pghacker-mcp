// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The AKS deployment's PilotSwarm portal: the web server alone, with no worker
// (that runs in its own pod, scripts/worker.mjs, so no agent shares this
// process or its loopback). It listens on the pod's address (PORTAL_HOST)
// behind oauth2-proxy, which signs users in with GitHub and passes who they
// are in headers (PORTAL_AUTH_PROVIDER=proxy). The portal trusts those
// headers, so a NetworkPolicy admits oauth2-proxy alone.
import { once } from "node:events";
import { setTimeout as delay } from "node:timers/promises";

const env = process.env;
const log = (message) => console.log(`[portal] ${message}`);

const host = env.PORTAL_HOST || "0.0.0.0";
const { startServer } = await import("pilotswarm/web");
const server = await startServer({ port: Number(env.PORT), host, workers: 0 });
log(`Portal on ${host}:${env.PORT}, signed in through oauth2-proxy.`);

let stopping = false;
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, async () => {
    if (stopping) return;
    stopping = true;
    log(`${signal}: stopping.`);
    const closed = once(server, "close").catch(() => {});
    server.close();
    server.closeIdleConnections?.();
    // The runtime's database pools outlive the server: exit once it closed.
    await Promise.race([closed, delay(10_000)]);
    process.exit(0);
  });
}
