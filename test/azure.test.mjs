// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import {
  FOUNDRY_PROXY, PORTS, USERS, VAULT_SECRETS, bootstrapEnvironment, componentEnvironments, componentSecrets,
  databaseUrl, databaseUrls, embedEnvironment, foundryCatalog, oauth2ProxyArgs, readAzureConfig, serviceHosts,
} from "../scripts/lib/azure.mjs";

const SECRETS = {
  PGHACKER_DB_ADMIN_PASSWORD: "admin-p@ss/w:rd?#1",
  PGH_INDEXER_PASSWORD: "indexer-secret-0123456789",
  PGH_READER_PASSWORD: "reader-secret-0123456789",
  PGH_SWARM_PASSWORD: "swarm-secret-0123456789",
};

/** The app's settings, as Terraform's kubernetes output gives them: no secret. */
const SETTINGS = {
  PGHACKER_DEPLOYMENT: "azure",
  PGHACKER_DB_HOST: "pghacker-example-hdb.abc.westus3.horizondb.azure.com",
  PGHACKER_DB_PORT: "5432",
  PGHACKER_DB_NAME: "pghacker",
  PGHACKER_DB_SSLMODE: "require",
  PGHACKER_DB_ADMIN_USER: "pghacker_admin",
  PGHACKER_FOUNDRY_ENDPOINT: "https://pghacker-example-foundry.openai.azure.com/",
  PGHACKER_FOUNDRY_SOL: "gpt-6.1-sol",
  PGHACKER_FOUNDRY_LUNA: "gpt-6-luna",
  PGHACKER_FOUNDRY_EMBEDDING: "text-embedding-3-small",
  PGHACKER_PUBLIC_URL: "https://mcp.example.org",
  PGHACKER_PORTAL_URL: "https://pghacker-abc.z01.azurefd.net",
  PGHACKER_PUBLIC_HOSTS: "mcp.example.org,pghacker-abc.z01.azurefd.net",
  PGHACKER_FRONT_DOOR_ID: "6e1b0e1c-0000-4000-8000-000000000000",
  PGHACKER_GITHUB_ORG: "contoso",
  PGHACKER_GITHUB_TEAM: "pghacker-operators",
  RUST_LOG: "debug",
};

/** A job's environment: the settings and its secrets. */
const ENV = { ...SETTINGS, ...SECRETS };

const NAMESPACE = "pghacker";
const REVISION = "sha256-0123456789ab";

/** Each component's environment from the app's settings, as the renderer makes it. */
function environments(settings = SETTINGS, options = {}) {
  return componentEnvironments(readAzureConfig(settings, { secrets: false, ...options }), { namespace: NAMESPACE, revision: REVISION });
}

/** An environment as a map: a secret shows as secret:<Secret>/<key>. */
function flat(entries) {
  return Object.fromEntries(entries.map((entry) => [
    entry.name,
    entry.valueFrom ? `secret:${entry.valueFrom.secretKeyRef.name}/${entry.valueFrom.secretKeyRef.key}` : entry.value,
  ]));
}

/** Every component's environment as a map. */
function flats(settings = SETTINGS, options = {}) {
  return Object.fromEntries(Object.entries(environments(settings, options)).map(([name, entries]) => [name, flat(entries)]));
}

/** The secrets a component's environment takes from Kubernetes Secrets. */
function secretsOf(entries) {
  return entries.filter((entry) => entry.valueFrom).map((entry) => `${entry.valueFrom.secretKeyRef.name}/${entry.valueFrom.secretKeyRef.key}`).sort();
}

const service = (name, port) => `http://${name}.${NAMESPACE}.svc.cluster.local:${port}`;

test("each component reads only its own secrets, from its identity's Kubernetes Secret, and holds no value", () => {
  const all = environments();
  assert.deepEqual(Object.keys(all).sort(), ["edge", "egressCheck", "home", "indexer", "mcp", "ops", "portal", "tokenProxy", "worker"]);
  assert.deepEqual(Object.fromEntries(Object.entries(all).map(([name, entries]) => [name, secretsOf(entries)])), {
    edge: ["edge-secrets/OAUTH2_PROXY_CLIENT_ID", "edge-secrets/OAUTH2_PROXY_CLIENT_SECRET", "edge-secrets/OAUTH2_PROXY_COOKIE_SECRET"],
    egressCheck: ["swarm-secrets/PGH_SWARM_PASSWORD"],
    home: [],
    indexer: ["indexer-secrets/PGH_INDEXER_PASSWORD"],
    mcp: ["mcp-secrets/PGH_READER_PASSWORD"],
    // Only the jobs read the administrator's password.
    ops: [
      "ops-secrets/PGHACKER_DB_ADMIN_PASSWORD", "ops-secrets/PGH_INDEXER_PASSWORD", "ops-secrets/PGH_READER_PASSWORD",
      "ops-secrets/PGH_SWARM_PASSWORD",
    ],
    portal: ["swarm-secrets/PGH_SWARM_PASSWORD"],
    // The token proxy reads none: its workload identity calls Foundry.
    tokenProxy: [],
    worker: ["swarm-secrets/PGH_SWARM_PASSWORD"],
  });
  // Each identity's list, which Terraform grants, names the same secrets.
  const config = readAzureConfig(SETTINGS, { secrets: false });
  const granted = componentSecrets(config);
  for (const [identity, secret] of [
    ["indexer", "indexer"], ["mcp", "mcp"], ["swarm", "worker"], ["swarm", "portal"], ["edge", "edge"], ["ops", "ops"],
  ]) {
    assert.deepEqual(secretsOf(all[secret]).map((ref) => ref.split("/")[1]), [...granted[identity]].sort(), identity);
  }
  for (const name of Object.values(Object.keys(VAULT_SECRETS))) assert.match(VAULT_SECRETS[name], /^[a-z0-9-]+$/);
  // A configuration read with the secrets, as a job's, gives no component a value.
  const withSecrets = componentEnvironments(readAzureConfig(ENV), { namespace: NAMESPACE });
  for (const [name, entries] of Object.entries(withSecrets)) {
    const text = JSON.stringify(entries);
    for (const [secret, raw] of Object.entries(SECRETS)) {
      assert.ok(!text.includes(raw) && !text.includes(encodeURIComponent(raw)), `${name} holds ${secret}`);
    }
    for (const entry of entries) assert.doesNotMatch(entry.name, /^(IDENTITY_|MSI_|AZURE_CLIENT_ID|AZURE_FEDERATED)/, name);
  }
  for (const entries of Object.values(all)) assert.equal(flat(entries).RUST_LOG, "debug");
});

test("components reach the database as their own roles, over verified TLS, with the password Kubernetes expands", () => {
  const all = environments();
  const roles = {
    worker: ["DATABASE_URL", "pgh_swarm", "PGH_SWARM_PASSWORD"],
    portal: ["DATABASE_URL", "pgh_swarm", "PGH_SWARM_PASSWORD"],
    egressCheck: ["DATABASE_URL", "pgh_swarm", "PGH_SWARM_PASSWORD"],
    indexer: ["PGHACKER_INDEXER_DATABASE_URL", "pgh_indexer", "PGH_INDEXER_PASSWORD"],
    mcp: ["PGHACKER_READER_DATABASE_URL", "pgh_reader", "PGH_READER_PASSWORD"],
    ops: ["PGHACKER_ADMIN_DATABASE_URL", "pghacker_admin", "PGHACKER_DB_ADMIN_PASSWORD"],
  };
  for (const [component, [variable, role, password]] of Object.entries(roles)) {
    const entries = all[component];
    const at = entries.findIndex((entry) => entry.name === variable);
    assert.equal(
      entries[at].value,
      `postgresql://${role}:$(${password})@${SETTINGS.PGHACKER_DB_HOST}:5432/pghacker?sslmode=require`,
      component,
    );
    // Kubernetes expands $(VAR) only from a variable defined before it.
    assert.ok(entries.findIndex((entry) => entry.name === password) < at, component);
  }
  // A plain setting's $( stays as it is.
  const odd = flat(environments({ ...SETTINGS, PGHACKER_PRIVACY_URL: "https://example.com/$(x)" }).home);
  assert.equal(odd.PGHACKER_PRIVACY_URL, "https://example.com/$$(x)");
  // The jobs' own URLs carry their passwords, encoded.
  const config = readAzureConfig(ENV);
  const admin = new URL(databaseUrls(config).admin);
  assert.equal(decodeURIComponent(admin.password), SECRETS.PGHACKER_DB_ADMIN_PASSWORD);
  assert.equal(admin.pathname, "/pghacker");
  assert.equal(new URL(databaseUrl(config, "u", "p", "postgres")).pathname, "/postgres");
  const bootstrap = bootstrapEnvironment(config, { PATH: "/bin", HOME: "/var/lib/pghacker" });
  assert.equal(new URL(bootstrap.PGHACKER_INDEXER_DATABASE_URL).username, "pgh_indexer");
  assert.equal(bootstrap.PGH_SWARM_PASSWORD, SECRETS.PGH_SWARM_PASSWORD);
  assert.equal(bootstrap.PATH, "/bin");
});

test("agents reach the indexer and pghacker-mcp's internal listener by their Services; the public one is rate-limited and behind Front Door", () => {
  const { worker, portal, mcp, indexer, tokenProxy, ops } = flats();
  assert.equal(worker.PGHACKER_MCP_URL, `${service("mcp", PORTS.mcpInternal)}/mcp`);
  assert.equal(worker.PGHACKER_INDEXER_MCP_URL, `${service("indexer", PORTS.indexer)}/mcp`);
  assert.equal(mcp.PGHACKER_MCP_ADDR, `0.0.0.0:${PORTS.mcp}`);
  assert.equal(mcp.PGHACKER_MCP_INTERNAL_ADDR, `0.0.0.0:${PORTS.mcpInternal}`);
  // The internal listener takes the Service's names; the public one, the public hosts.
  assert.deepEqual(mcp.PGHACKER_INTERNAL_ALLOWED_HOSTS.split(","), serviceHosts("mcp", NAMESPACE));
  assert.ok(mcp.PGHACKER_INTERNAL_ALLOWED_HOSTS.split(",").includes("mcp.pghacker.svc.cluster.local"));
  assert.deepEqual(mcp.PGHACKER_ALLOWED_HOSTS.split(","), SETTINGS.PGHACKER_PUBLIC_HOSTS.split(","));
  assert.deepEqual(indexer.PGHACKER_ALLOWED_HOSTS.split(","), serviceHosts("indexer", NAMESPACE));
  assert.equal(indexer.PGHACKER_INDEXER_ADDR, `0.0.0.0:${PORTS.indexer}`);
  // Front Door's socket address: its X-Azure-ClientIP follows a client's X-Forwarded-For.
  assert.equal(mcp.PGHACKER_CLIENT_IP_HEADER, "x-azure-socketip");
  assert.equal(mcp.PGHACKER_FRONT_DOOR_ID, SETTINGS.PGHACKER_FRONT_DOOR_ID);
  assert.equal(mcp.PGHACKER_RATE_PER_MINUTE, "60");
  // A client's calls at once on the public listener: pghacker-mcp's default
  // unless Terraform sets mcp_calls_per_client.
  assert.equal(mcp.PGHACKER_MCP_CALLS_PER_CLIENT, "10");
  assert.equal(flats({ ...SETTINGS, PGHACKER_MCP_CALLS_PER_CLIENT: "20" }).mcp.PGHACKER_MCP_CALLS_PER_CLIENT, "20");
  for (const calls of ["0", "2.5", "1001"]) {
    assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_MCP_CALLS_PER_CLIENT: calls }), /PGHACKER_MCP_CALLS_PER_CLIENT/);
  }
  assert.equal(worker.PGHACKER_MCP_CALLS_PER_CLIENT, undefined);
  // The agents' listener has a limit of its own, which they share.
  assert.equal(mcp.PGHACKER_INTERNAL_RATE_PER_MINUTE, "300");
  assert.equal(mcp.PGHACKER_INTERNAL_RATE_BURST, "60");
  const throttled = flats({ ...SETTINGS, PGHACKER_INTERNAL_RATE_PER_MINUTE: "120", PGHACKER_INTERNAL_RATE_BURST: "20" });
  assert.equal(throttled.mcp.PGHACKER_INTERNAL_RATE_PER_MINUTE, "120");
  assert.equal(throttled.mcp.PGHACKER_INTERNAL_RATE_BURST, "20");
  assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_INTERNAL_RATE_PER_MINUTE: "0" }), /PGHACKER_INTERNAL_RATE_PER_MINUTE/);
  assert.equal(worker.PGHACKER_INTERNAL_RATE_PER_MINUTE, undefined);
  // The pools' sizes come from Terraform; without them, pghacker-mcp's defaults.
  assert.equal(mcp.PGHACKER_MCP_POOL, "16");
  assert.equal(mcp.PGHACKER_QUERY_POOL, "8");
  const sized = flats({ ...SETTINGS, PGHACKER_MCP_POOL: "64", PGHACKER_QUERY_POOL: "16" });
  assert.equal(sized.mcp.PGHACKER_MCP_POOL, "64");
  assert.equal(sized.mcp.PGHACKER_QUERY_POOL, "16");
  assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_MCP_POOL: "500" }));
  assert.equal(indexer.PGHACKER_SOURCES, "open");
  assert.equal(indexer.PGHACKER_EXTRACTION, "automatic");
  assert.equal(indexer.PGHACKER_KEEP_DOWNLOADS, "false");
  assert.equal(flats({ ...SETTINGS, PGHACKER_CRAWL: "off" }).indexer.PGHACKER_EXTRACTION, "manual");
  // The worker runs the crawl scheduler, whose models go through the token proxy.
  assert.equal(worker.PGHACKER_CRAWL_MODEL, "azure-foundry:gpt-6-luna");
  assert.equal(worker.PGHACKER_FOUNDRY_ENDPOINT, service("token-proxy", PORTS.foundry));
  assert.equal(worker.AZURE_OAI_KEY, "managed-identity");
  assert.equal(worker.PILOTSWARM_WORKER_CONCURRENCY, "20");
  assert.ok(Number(worker.PILOTSWARM_WORKER_SHUTDOWN_TIMEOUT_MS) <= 30_000);
  assert.equal(worker.PS_MODEL_PROVIDERS_PATH, "/etc/pghacker/model_providers.json");
  // The portal runs no worker, behind oauth2-proxy's identity headers.
  assert.equal(portal.WORKERS, "0");
  assert.equal(portal.PORTAL_HOST, "0.0.0.0");
  assert.equal(portal.PORTAL_AUTH_PROVIDER, "proxy");
  assert.equal(portal.PORTAL_AUTH_ALLOW_UNAUTHENTICATED, "false");
  assert.equal(portal.PORTAL_AUTHZ_DEFAULT_ROLE, "user");
  // The token proxy calls Foundry itself; the jobs reach the models through it.
  assert.equal(tokenProxy.PGHACKER_FOUNDRY_ENDPOINT, "https://pghacker-example-foundry.openai.azure.com");
  assert.equal(ops.PGHACKER_FOUNDRY_PROXY_URL, service("token-proxy", PORTS.foundry));
  assert.equal(readAzureConfig({ ...ENV, PGHACKER_FOUNDRY_PROXY_URL: ops.PGHACKER_FOUNDRY_PROXY_URL }).foundry.proxy, service("token-proxy", PORTS.foundry));
  assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_FOUNDRY_PROXY_URL: "http://token-proxy:3110/openai" }), /must be an http origin/);
});

test("the egress check tries what the agents mustn't reach: the portal, the public listener, telemetry, the home page and the edge", () => {
  const { egressCheck, worker } = flats();
  assert.equal(egressCheck.PGHACKER_INDEXER_MCP_URL, worker.PGHACKER_INDEXER_MCP_URL);
  assert.equal(egressCheck.PGHACKER_MCP_URL, worker.PGHACKER_MCP_URL);
  assert.equal(egressCheck.DATABASE_URL, worker.DATABASE_URL);
  assert.deepEqual(egressCheck.PGHACKER_EGRESS_DENIED.split(",").map((url) => new URL(url).host).sort(), [
    `edge.${NAMESPACE}.svc.cluster.local`,
    `home.${NAMESPACE}.svc.cluster.local:${PORTS.home}`,
    `mcp.${NAMESPACE}.svc.cluster.local:${PORTS.mcp}`,
    `portal.${NAMESPACE}.svc.cluster.local:${PORTS.portal}`,
    `token-proxy.${NAMESPACE}.svc.cluster.local:${PORTS.monitor}`,
  ]);
});

test("PGHACKER_CODE_HISTORY names the history schema of the indexer and pghacker-mcp, and nothing else", () => {
  for (const env of Object.values(flats())) assert.equal("PGHACKER_CODE_HISTORY" in env, false);
  for (const [name, env] of Object.entries(flats({ ...SETTINGS, PGHACKER_CODE_HISTORY: " pghacker " }))) {
    assert.equal(env.PGHACKER_CODE_HISTORY, ["indexer", "mcp"].includes(name) ? "pghacker" : undefined, name);
  }
  for (const bad of ["Pghacker", "pghacker; DROP", "1history", "a".repeat(64)]) {
    assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_CODE_HISTORY: bad }), /must name a schema/, bad);
  }
});

test("the extraction order's settings reach the indexer only, and are checked", () => {
  const names = ["PGHACKER_EXTRACTION_ORDER", "PGHACKER_EXTRACTION_NEW_MAIL_SHARE", "PGHACKER_EXTRACTION_NEW_MAIL_DAYS"];
  for (const env of Object.values(flats())) {
    for (const name of names) assert.equal(name in env, false, name);
  }
  const set = {
    PGHACKER_EXTRACTION_ORDER: "largest",
    PGHACKER_EXTRACTION_NEW_MAIL_SHARE: " 0.25 ",
    PGHACKER_EXTRACTION_NEW_MAIL_DAYS: "3",
  };
  for (const [component, env] of Object.entries(flats({ ...SETTINGS, ...set }))) {
    for (const name of names) {
      assert.equal(env[name], component === "indexer" ? set[name].trim() : undefined, `${component} ${name}`);
    }
  }
  for (const [name, bad] of [
    ["PGHACKER_EXTRACTION_ORDER", "newest"],
    ["PGHACKER_EXTRACTION_NEW_MAIL_SHARE", "0"],
    ["PGHACKER_EXTRACTION_NEW_MAIL_SHARE", "1.5"],
    ["PGHACKER_EXTRACTION_NEW_MAIL_SHARE", "half"],
    ["PGHACKER_EXTRACTION_NEW_MAIL_DAYS", "0"],
    ["PGHACKER_EXTRACTION_NEW_MAIL_DAYS", "2.5"],
  ]) {
    assert.throws(() => readAzureConfig({ ...ENV, [name]: bad }), new RegExp(name), `${name}=${bad}`);
  }
});

test("the home page's server gets the page, its address and the portal's origin only", () => {
  const { home } = flats();
  assert.equal(home.PORT, String(PORTS.home));
  assert.equal(home.HOST, "0.0.0.0");
  assert.equal(flat(componentEnvironments(readAzureConfig(SETTINGS, { secrets: false, root: "/app" }), { namespace: NAMESPACE }).home).PGHACKER_HOME_DIR, "/app/home/dist");
  assert.ok(home.PGHACKER_HOME_DIR.endsWith("/home/dist"));
  // Share links go on to the portal, on Front Door's host name, never the public domain.
  assert.equal(home.PGHACKER_PORTAL_URL, "https://pghacker-abc.z01.azurefd.net");
  assert.deepEqual(Object.keys(home).sort(), ["HOME", "HOST", "PGHACKER_HOME_DIR", "PGHACKER_PORTAL_URL", "PORT", "RUST_LOG"]);
  // Each component's user is its own, and each port too.
  assert.equal(new Set(Object.values(USERS)).size, Object.keys(USERS).length);
  assert.equal(new Set(Object.values(PORTS)).size, Object.keys(PORTS).length);
});

const USAGE_SETTINGS = {
  ...SETTINGS,
  APPLICATIONINSIGHTS_CONNECTION_STRING:
    "InstrumentationKey=11111111-2222-4333-8444-555555555555;IngestionEndpoint=https://westus3-1.in.applicationinsights.azure.com/;"
    + "LiveEndpoint=https://westus3.livediagnostics.monitor.azure.com/;ApplicationId=22222222-0000-4000-8000-000000000000",
};
const USAGE_ID_KEY = "usage-id-key-0123456789abcdefghijklmnopqrstuvwxyz";

test("usage telemetry reaches pghacker-mcp and the home page through the token proxy, with Application Insights only", () => {
  const config = readAzureConfig(USAGE_SETTINGS, { secrets: false });
  assert.deepEqual(config.usage, {
    instrumentationKey: "11111111-2222-4333-8444-555555555555",
    ingestionEndpoint: "https://westus3-1.in.applicationinsights.azure.com",
    audience: "https://monitor.azure.com",
    connectionString: USAGE_SETTINGS.APPLICATIONINSIGHTS_CONNECTION_STRING,
    revision: undefined,
    details: false,
  });
  const all = environments(USAGE_SETTINGS);
  const maps = Object.fromEntries(Object.entries(all).map(([name, entries]) => [name, flat(entries)]));
  for (const name of ["mcp", "home"]) {
    const env = maps[name];
    assert.equal(env.PGHACKER_USAGE_ENDPOINT, service("token-proxy", PORTS.monitor), name);
    assert.equal(env.PGHACKER_USAGE_IKEY, "11111111-2222-4333-8444-555555555555", name);
    // The users' key, from the component's own Secret.
    assert.equal(env.PGHACKER_USAGE_ID_KEY, `secret:${name}-secrets/PGHACKER_USAGE_ID_KEY`, name);
    assert.equal(env.CONTAINER_APP_REVISION, REVISION, name);
    assert.equal(env.PGHACKER_FRONT_DOOR_ID, SETTINGS.PGHACKER_FRONT_DOOR_ID, name);
  }
  assert.deepEqual(componentSecrets(config).home, ["PGHACKER_USAGE_ID_KEY"]);
  assert.ok(componentSecrets(config).mcp.includes("PGHACKER_USAGE_ID_KEY"));
  // The token proxy alone gets the connection string, to sign and send the events.
  for (const [name, env] of Object.entries(maps)) {
    assert.equal(
      env.APPLICATIONINSIGHTS_CONNECTION_STRING,
      name === "tokenProxy" ? USAGE_SETTINGS.APPLICATIONINSIGHTS_CONNECTION_STRING : undefined,
      name,
    );
    assert.equal(
      Object.values(env).some((value) => String(value).includes("PGHACKER_USAGE_ID_KEY")),
      ["mcp", "home"].includes(name),
      name,
    );
  }
  // Details are pghacker-mcp's alone, and off unless set.
  assert.equal(maps.mcp.PGHACKER_USAGE_DETAILS, "false");
  assert.equal(maps.home.PGHACKER_USAGE_DETAILS, undefined);
  assert.equal(flats({ ...USAGE_SETTINGS, PGHACKER_USAGE_DETAILS: "true" }).mcp.PGHACKER_USAGE_DETAILS, "true");
  assert.throws(
    () => readAzureConfig({ ...USAGE_SETTINGS, PGHACKER_USAGE_DETAILS: "yes" }, { secrets: false }),
    /PGHACKER_USAGE_DETAILS must be true or false/,
  );

  // Without Application Insights, no component records usage; nor do the jobs.
  for (const env of [
    ...Object.values(flats()),
    ...Object.values(flats(USAGE_SETTINGS, { serve: false })),
  ]) {
    assert.ok(!Object.keys(env).some((key) => key.startsWith("PGHACKER_USAGE_")));
  }
  assert.deepEqual(componentSecrets(readAzureConfig(SETTINGS, { secrets: false })).home, []);
  // Read with its secrets, as a component's own environment, the key is checked.
  const withKey = { ...USAGE_SETTINGS, ...SECRETS, PGHACKER_USAGE_ID_KEY: USAGE_ID_KEY };
  assert.equal(readAzureConfig(withKey).usage.idKey, USAGE_ID_KEY);
  assert.throws(() => readAzureConfig({ ...withKey, PGHACKER_USAGE_ID_KEY: "" }), /needs PGHACKER_USAGE_ID_KEY/);
  assert.throws(() => readAzureConfig({ ...withKey, PGHACKER_USAGE_ID_KEY: "too-short" }), /at least 32 characters/);
  assert.throws(
    () => readAzureConfig({ ...USAGE_SETTINGS, APPLICATIONINSIGHTS_CONNECTION_STRING: "IngestionEndpoint=https://x.example/" }, { secrets: false }),
    /InstrumentationKey/,
  );
});

test("the privacy statement that the home page's footer links to reaches the home page's server alone, and only if the deployment has one", () => {
  const privacyUrl = "https://example.com/privacy";
  assert.equal(readAzureConfig(ENV).public.privacyUrl, undefined);
  assert.equal(readAzureConfig({ ...ENV, PGHACKER_PRIVACY_URL: ` ${privacyUrl} ` }).public.privacyUrl, privacyUrl);
  for (const [name, env] of Object.entries(flats({ ...SETTINGS, PGHACKER_PRIVACY_URL: privacyUrl }))) {
    assert.equal(env.PGHACKER_PRIVACY_URL, name === "home" ? privacyUrl : undefined, name);
  }
  // Unset or blank, there is no setting, so no link.
  for (const blank of [undefined, "", "  "]) {
    for (const [name, env] of Object.entries(flats({ ...SETTINGS, PGHACKER_PRIVACY_URL: blank }))) {
      assert.ok(!("PGHACKER_PRIVACY_URL" in env), `${name} with ${JSON.stringify(blank)}`);
    }
  }
  // The jobs serve no page.
  assert.equal(readAzureConfig({ ...ENV, PGHACKER_PRIVACY_URL: privacyUrl }, { serve: false }).public, undefined);
  for (const bad of [
    "http://example.com/privacy", "https://user@example.com/privacy", "javascript:alert(1)", "example.com/privacy",
    "https://example.com/a b", `https://example.com/${"a".repeat(2048)}`,
  ]) {
    assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_PRIVACY_URL: bad }), /PGHACKER_PRIVACY_URL must be an https URL/, bad);
  }
});

test("the copyright link that the home page's footer shows reaches the home page's server alone, with its text, and only if the deployment has one", () => {
  const link = { PGHACKER_COPYRIGHT_URL: "https://example.com/", PGHACKER_COPYRIGHT_TEXT: "© 2026 Example" };
  assert.equal(readAzureConfig(ENV).public.copyright, undefined);
  assert.deepEqual(readAzureConfig({ ...ENV, ...link }).public.copyright, { url: "https://example.com/", text: "© 2026 Example" });
  assert.deepEqual(
    readAzureConfig({ ...ENV, PGHACKER_COPYRIGHT_URL: " https://example.com/ ", PGHACKER_COPYRIGHT_TEXT: "  © 2026 Example  " }).public.copyright,
    { url: "https://example.com/", text: "© 2026 Example" },
  );
  for (const [name, env] of Object.entries(flats({ ...SETTINGS, ...link }))) {
    const own = { PGHACKER_COPYRIGHT_URL: env.PGHACKER_COPYRIGHT_URL, PGHACKER_COPYRIGHT_TEXT: env.PGHACKER_COPYRIGHT_TEXT };
    assert.deepEqual(own, name === "home" ? link : { PGHACKER_COPYRIGHT_URL: undefined, PGHACKER_COPYRIGHT_TEXT: undefined }, name);
  }
  // Unset or blank, there is no link.
  for (const blank of [undefined, "", "  "]) {
    const env = { ...SETTINGS, PGHACKER_COPYRIGHT_URL: blank, PGHACKER_COPYRIGHT_TEXT: blank };
    assert.equal(readAzureConfig(env, { secrets: false }).public.copyright, undefined, JSON.stringify(blank));
    for (const [name, componentEnv] of Object.entries(flats(env))) {
      assert.ok(!Object.keys(componentEnv).some((key) => key.startsWith("PGHACKER_COPYRIGHT_")), `${name} with ${JSON.stringify(blank)}`);
    }
  }
  // The jobs serve no page.
  assert.equal(readAzureConfig({ ...ENV, ...link }, { serve: false }).public, undefined);
  // The URL and the text go together, and each must be valid.
  const unmatched = /PGHACKER_COPYRIGHT_URL and PGHACKER_COPYRIGHT_TEXT must be set together/;
  for (const bad of [
    { PGHACKER_COPYRIGHT_URL: "https://example.com/" },
    { PGHACKER_COPYRIGHT_TEXT: "© 2026 Example" },
    { ...link, PGHACKER_COPYRIGHT_URL: "http://example.com/" },
    { ...link, PGHACKER_COPYRIGHT_URL: "https://user@example.com/" },
    { ...link, PGHACKER_COPYRIGHT_URL: "example.com" },
    { ...link, PGHACKER_COPYRIGHT_TEXT: "© 2026\nExample" },
    { ...link, PGHACKER_COPYRIGHT_TEXT: "x".repeat(101) },
  ]) {
    assert.throws(() => readAzureConfig({ ...ENV, ...bad }), unmatched, JSON.stringify(bad));
  }
});

test("feedback's settings reach pghacker-mcp alone, and only with usage telemetry", () => {
  const settings = (env) => Object.entries(flats(env))
    .filter(([, componentEnv]) => Object.keys(componentEnv).some((key) => key.startsWith("PGHACKER_FEEDBACK_")))
    .map(([name, componentEnv]) => [
      name,
      Object.fromEntries(Object.entries(componentEnv).filter(([key]) => key.startsWith("PGHACKER_FEEDBACK_"))),
    ]);
  const defaults = {
    PGHACKER_FEEDBACK_COMMENTS: "false",
    PGHACKER_FEEDBACK_QUESTION_GAP: "180",
    PGHACKER_FEEDBACK_SESSION_GAP: "600",
    PGHACKER_FEEDBACK_MIN_QUESTIONS: "3",
    PGHACKER_FEEDBACK_DEBUG: "false",
  };
  const withFeedback = { ...USAGE_SETTINGS, PGHACKER_FEEDBACK_RATE: "0.1", PGHACKER_FEEDBACK_COMMENTS: "false" };
  assert.deepEqual(settings(withFeedback), [["mcp", { PGHACKER_FEEDBACK_RATE: "0.1", ...defaults }]]);
  assert.deepEqual(readAzureConfig(withFeedback, { secrets: false }).feedback, {
    rate: 0.1, comments: false, questionGap: 180, sessionGap: 600, minQuestions: 3, debug: false,
  });
  // As Terraform sets them all.
  const all = {
    ...withFeedback,
    PGHACKER_FEEDBACK_QUESTION_GAP: "120",
    PGHACKER_FEEDBACK_SESSION_GAP: "900",
    PGHACKER_FEEDBACK_MIN_QUESTIONS: "0",
    PGHACKER_FEEDBACK_CLIENTS: "copilot-cli,Visual Studio Code",
    PGHACKER_FEEDBACK_DEBUG: "true",
  };
  assert.deepEqual(settings(all), [["mcp", {
    PGHACKER_FEEDBACK_RATE: "0.1",
    PGHACKER_FEEDBACK_COMMENTS: "false",
    PGHACKER_FEEDBACK_QUESTION_GAP: "120",
    PGHACKER_FEEDBACK_SESSION_GAP: "900",
    PGHACKER_FEEDBACK_MIN_QUESTIONS: "0",
    PGHACKER_FEEDBACK_CLIENTS: "copilot-cli,Visual Studio Code",
    PGHACKER_FEEDBACK_DEBUG: "true",
  }]]);
  // Unset, off; without usage telemetry, nothing at all.
  assert.deepEqual(settings(USAGE_SETTINGS), [["mcp", { PGHACKER_FEEDBACK_RATE: "0", ...defaults }]]);
  assert.deepEqual(settings({ ...SETTINGS, PGHACKER_FEEDBACK_RATE: "0.1" }), []);
  assert.equal(readAzureConfig({ ...ENV, PGHACKER_FEEDBACK_RATE: "0.1" }).feedback, undefined);
  const check = (env) => readAzureConfig(env, { secrets: false });
  for (const rate of ["1.5", "-0.1", "ten percent"]) {
    assert.throws(() => check({ ...USAGE_SETTINGS, PGHACKER_FEEDBACK_RATE: rate }), /PGHACKER_FEEDBACK_RATE must be a number from 0 to 1/);
  }
  assert.throws(() => check({ ...USAGE_SETTINGS, PGHACKER_FEEDBACK_COMMENTS: "yes" }), /PGHACKER_FEEDBACK_COMMENTS must be true or false/);
  assert.throws(() => check({ ...USAGE_SETTINGS, PGHACKER_FEEDBACK_DEBUG: "always" }), /PGHACKER_FEEDBACK_DEBUG must be true or false/);
  for (const [name, bad, error] of [
    ["PGHACKER_FEEDBACK_QUESTION_GAP", "0", /PGHACKER_FEEDBACK_QUESTION_GAP must be an integer from 1 to 3600/],
    ["PGHACKER_FEEDBACK_SESSION_GAP", "180", /PGHACKER_FEEDBACK_SESSION_GAP must be more than PGHACKER_FEEDBACK_QUESTION_GAP/],
    ["PGHACKER_FEEDBACK_MIN_QUESTIONS", "101", /PGHACKER_FEEDBACK_MIN_QUESTIONS must be an integer from 0 to 100/],
    ["PGHACKER_FEEDBACK_MIN_QUESTIONS", "2.5", /PGHACKER_FEEDBACK_MIN_QUESTIONS must be an integer/],
  ]) {
    assert.throws(() => check({ ...USAGE_SETTINGS, [name]: bad }), error, `${name}=${bad}`);
  }
});

test("oauth2-proxy signs in the team for the portal, passes /mcp and the home page through, and gets no secret in its arguments", () => {
  const args = oauth2ProxyArgs(readAzureConfig(ENV));
  for (const expected of [
    "--http-address=0.0.0.0:8080", "--provider=github", "--github-org=contoso",
    "--github-team=pghacker-operators", "--skip-auth-route=^/mcp$", "--skip-auth-route=GET=^/$",
    "--skip-auth-route=GET=^/static/", "--api-route=^/api/",
    `--upstream=http://127.0.0.1:${PORTS.mcp}/mcp`, `--upstream=http://127.0.0.1:${PORTS.portal}/portal`,
    `--upstream=http://127.0.0.1:${PORTS.portal}/portal/`, `--upstream=http://127.0.0.1:${PORTS.portal}/api/`,
    `--upstream=http://127.0.0.1:${PORTS.portal}/assets/`, `--upstream=http://127.0.0.1:${PORTS.home}/`,
    "--redirect-url=https://pghacker-abc.z01.azurefd.net/oauth2/callback", "--cookie-secure=true",
    "--pass-user-headers=true",
  ]) {
    assert.ok(args.includes(expected), expected);
  }
  // The portal no longer takes every path: the home page does.
  assert.ok(!args.includes(`--upstream=http://127.0.0.1:${PORTS.portal}/`));
  assert.equal(args.filter((arg) => arg.startsWith("--upstream=")).length, 6);
  assert.ok(!args.some((arg) => arg.startsWith("--github-user")));
  const text = args.join(" ");
  for (const secret of Object.values(SECRETS)) assert.ok(!text.includes(secret));
  const users = oauth2ProxyArgs(readAzureConfig({ ...ENV, PGHACKER_GITHUB_USERS: "example-user, another-example-user" }));
  assert.ok(users.includes("--github-user=example-user,another-example-user"));
  // In the cluster, the upstreams are the components' Services.
  const cluster = oauth2ProxyArgs(readAzureConfig(ENV), {
    origins: { mcp: service("mcp", PORTS.mcp), portal: service("portal", PORTS.portal), home: service("home", PORTS.home) },
  });
  assert.ok(cluster.includes(`--upstream=${service("mcp", PORTS.mcp)}/mcp`));
  assert.ok(cluster.includes(`--upstream=${service("portal", PORTS.portal)}/api/`));
});

test("oauth2-proxy trusts no forwarded header, nor any client address", () => {
  const args = oauth2ProxyArgs(readAzureConfig(ENV));
  // Trusting forwarded headers would let a forged X-Forwarded-Uri choose the
  // path that the skip-auth routes match; trusting IPs would skip sign-in.
  assert.deepEqual(args.filter((arg) => /^--(reverse-proxy|trusted-(proxy-)?ip)\b/.test(arg)), ["--reverse-proxy=false"]);
});

test("oauth2-proxy signs users in on the portal's host alone, Front Door's, never the public domain's", () => {
  const args = oauth2ProxyArgs(readAzureConfig(ENV));
  // The public domain serves only the public paths (deploy/terraform/frontdoor.tf).
  assert.ok(args.includes("--redirect-url=https://pghacker-abc.z01.azurefd.net/oauth2/callback"));
  assert.deepEqual(args.filter((arg) => arg.startsWith("--whitelist-domain=")), ["--whitelist-domain=pghacker-abc.z01.azurefd.net"]);
  assert.ok(!args.join(" ").includes("mcp.example.org"));
});

test("the configuration refuses what would be unsafe or incomplete", () => {
  // A job's environment must have the secrets; the app's settings have none.
  assert.throws(() => readAzureConfig({ ...ENV, PGH_SWARM_PASSWORD: "" }), /PGH_SWARM_PASSWORD/);
  assert.throws(() => readAzureConfig(SETTINGS), /PGHACKER_DB_ADMIN_PASSWORD/);
  assert.ok(readAzureConfig(SETTINGS, { secrets: false }));
  assert.equal(readAzureConfig(SETTINGS, { secrets: false }).db.passwords, undefined);
  assert.throws(
    () => readAzureConfig({ ...ENV, PGHACKER_GITHUB_ORG: "", PGHACKER_GITHUB_TEAM: "" }),
    /must not admit every GitHub user/,
  );
  assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_PUBLIC_URL: "http://example.org" }), /https origin/);
  assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_PORTAL_URL: undefined }), /PGHACKER_PORTAL_URL/);
  assert.throws(
    () => readAzureConfig({ ...ENV, PGHACKER_PORTAL_URL: "https://pghacker-abc.z01.azurefd.net/portal/" }),
    /PGHACKER_PORTAL_URL must be an https origin/,
  );
  assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_PORTAL_ROLE: "owner" }), /user or admin/);
  assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_CRAWL_START_SESSIONS: "32" }), /must not exceed/);
  // The jobs need neither the public and portal URLs nor sign-in.
  const job = readAzureConfig({ ...ENV, PGHACKER_PUBLIC_URL: "", PGHACKER_PORTAL_URL: "" }, { serve: false });
  const jobs = componentEnvironments(job, { namespace: NAMESPACE });
  assert.equal(jobs.mcp, undefined);
  assert.equal(jobs.edge, undefined);
  assert.equal(jobs.home, undefined);
  assert.ok(flat(jobs.ops).PGHACKER_ADMIN_DATABASE_URL);
});

test("PilotSwarm's catalog is Foundry's Sol and Luna over the Responses API, and embeddings use the same account, all through the token proxy", () => {
  const config = readAzureConfig(ENV);
  const tokenProxy = service("token-proxy", PORTS.foundry);
  const catalog = foundryCatalog(config.foundry, tokenProxy);
  assert.equal(catalog.defaultModel, "azure-foundry:gpt-6.1-sol");
  const [provider] = catalog.providers;
  assert.equal(provider.type, "openai");
  assert.equal(provider.wireApi, "responses");
  assert.equal(provider.baseUrl, `${tokenProxy}/openai/v1`);
  assert.equal(provider.apiKey, "env:AZURE_OAI_KEY");
  assert.deepEqual(provider.models.map((m) => m.name), ["gpt-6.1-sol", "gpt-6-luna"]);
  // Unset, the token proxy is on loopback, as in a test.
  assert.equal(foundryCatalog(config.foundry).providers[0].baseUrl, `${FOUNDRY_PROXY}/openai/v1`);
  const embed = embedEnvironment(config.foundry, tokenProxy);
  assert.equal(embed.HORIZON_EMBED_URL,
    `${tokenProxy}/openai/deployments/text-embedding-3-small/embeddings?api-version=2024-10-21`);
  assert.equal(embed.HORIZON_EMBED_DIM, "1536");
  assert.equal(embed.HORIZON_EMBED_API_KEY, "managed-identity");
  assert.equal(config.foundry.endpoint, "https://pghacker-example-foundry.openai.azure.com");
});
