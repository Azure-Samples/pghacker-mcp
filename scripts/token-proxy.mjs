// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The token proxy: the AKS deployment's only pod with an Azure identity
// (workload identity, through the federated token that the webhook projects
// into it). It forwards model requests to Foundry, and usage telemetry to
// Application Insights when the deployment has it, each with an Entra token in
// place of the caller's placeholder key (scripts/lib/foundry.mjs,
// scripts/lib/monitor.mjs), so no other pod holds a credential for either. Its
// health server answers the probes: /healthz while it runs, /readyz while it
// can get a Foundry token.
import http from "node:http";
import { PORTS } from "./lib/azure.mjs";
import { FOUNDRY_RESOURCE, startFoundryProxy, workloadIdentityTokens } from "./lib/foundry.mjs";
import { parseConnectionString, startMonitorProxy } from "./lib/monitor.mjs";

const env = process.env;
const log = (message) => console.log(`[token-proxy] ${message}`);

const endpoint = (env.PGHACKER_FOUNDRY_ENDPOINT ?? "").trim().replace(/\/+$/, "");
if (!/^https:\/\/[^/]+$/.test(endpoint)) {
  throw new Error("PGHACKER_FOUNDRY_ENDPOINT must be the Foundry account's origin, such as https://name.openai.azure.com.");
}
const identity = {
  tenantId: env.AZURE_TENANT_ID,
  clientId: env.AZURE_CLIENT_ID,
  tokenFile: env.AZURE_FEDERATED_TOKEN_FILE,
  authorityHost: env.AZURE_AUTHORITY_HOST || undefined,
};

const foundryToken = workloadIdentityTokens({ ...identity, resource: FOUNDRY_RESOURCE });
const servers = [
  await startFoundryProxy({ upstream: endpoint, token: foundryToken, port: PORTS.foundry, host: "0.0.0.0", log }),
];
log(`Foundry proxy on :${PORTS.foundry} for ${endpoint}.`);

if (env.APPLICATIONINSIGHTS_CONNECTION_STRING) {
  const usage = parseConnectionString(env.APPLICATIONINSIGHTS_CONNECTION_STRING);
  const monitorToken = workloadIdentityTokens({ ...identity, resource: usage.audience });
  servers.push(await startMonitorProxy({
    upstream: usage.ingestionEndpoint, token: monitorToken, port: PORTS.monitor, host: "0.0.0.0", log,
  }));
  log(`Application Insights proxy on :${PORTS.monitor} for ${usage.ingestionEndpoint}.`);
} else {
  log("No APPLICATIONINSIGHTS_CONNECTION_STRING: usage telemetry is off.");
}

const health = http.createServer(async (request, response) => {
  const reply = (status, body) => {
    response.writeHead(status, { "content-type": "application/json" });
    response.end(`${JSON.stringify(body)}\n`);
  };
  if (request.url === "/healthz") {
    reply(200, { alive: true });
  } else if (request.url === "/readyz") {
    try {
      await Promise.race([
        foundryToken(),
        new Promise((_, reject) => setTimeout(() => reject(new Error("no token within 10 s")), 10_000).unref()),
      ]);
      reply(200, { ready: true });
    } catch (error) {
      reply(503, { ready: false, error: String(error.message).slice(0, 300) });
    }
  } else {
    reply(404, { error: "not found" });
  }
});
health.listen(PORTS.health, "0.0.0.0");
foundryToken().then(
  () => log("Got a Foundry token with the workload identity."),
  (error) => log(`No Foundry token yet: ${error.message}`),
);

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, () => {
    log(`${signal}: stopping.`);
    for (const server of [...servers, health]) {
      server.close();
      server.closeIdleConnections?.();
    }
    setTimeout(() => process.exit(0), 5000).unref();
  });
}
