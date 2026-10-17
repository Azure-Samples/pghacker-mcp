// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import path from "node:path";
import test from "node:test";
import {
  MONITOR_PROXY, PORTS, USERS, databaseUrl, databaseUrls, embedEnvironment, foundryCatalog, layout, lookupUser,
  oauth2ProxyArgs, processEnvironments, readAzureConfig,
} from "../scripts/lib/azure.mjs";

const SECRETS = {
  PGHACKER_DB_ADMIN_PASSWORD: "admin-p@ss/w:rd?#1",
  PGH_INDEXER_PASSWORD: "indexer-secret-0123456789",
  PGH_READER_PASSWORD: "reader-secret-0123456789",
  PGH_SWARM_PASSWORD: "swarm-secret-0123456789",
  OAUTH2_PROXY_CLIENT_ID: "github-client-id-0123",
  OAUTH2_PROXY_CLIENT_SECRET: "github-client-secret-0123456789",
  OAUTH2_PROXY_COOKIE_SECRET: "cookie-secret-0123456789abcdefgh",
};

const ENV = {
  ...SECRETS,
  PGHACKER_DEPLOYMENT: "azure",
  PGHACKER_DB_HOST: "pghacker-example-hdb.abc.westus3.horizondb.azure.com",
  PGHACKER_DB_PORT: "5432",
  PGHACKER_DB_NAME: "pghacker",
  PGHACKER_DB_SSLMODE: "require",
  PGHACKER_DB_ADMIN_USER: "pghacker_admin",
  AZURE_CLIENT_ID: "uami-client-id",
  PGHACKER_FOUNDRY_ENDPOINT: "https://pghacker-example-foundry.openai.azure.com/",
  PGHACKER_FOUNDRY_SOL: "gpt-6.1-sol",
  PGHACKER_FOUNDRY_LUNA: "gpt-6-luna",
  PGHACKER_FOUNDRY_EMBEDDING: "text-embedding-3-small",
  PGHACKER_PUBLIC_URL: "https://mcp.example.org",
  PGHACKER_PORTAL_URL: "https://pghacker-abc.z01.azurefd.net",
  PGHACKER_PUBLIC_HOSTS: "mcp.example.org,pghacker-abc.z01.azurefd.net,pghacker-example.env.westus3.azurecontainerapps.io",
  PGHACKER_FRONT_DOOR_ID: "6e1b0e1c-0000-4000-8000-000000000000",
  PGHACKER_GITHUB_ORG: "example-org",
  PGHACKER_GITHUB_TEAM: "example-team",
  // The platform's own settings, which no process may get.
  IDENTITY_ENDPOINT: "http://localhost:42356/msi/token",
  IDENTITY_HEADER: "identity-header-secret",
  MSI_ENDPOINT: "http://localhost:42356/msi/token",
  MSI_SECRET: "msi-secret",
  RUST_LOG: "debug",
};

/** Which processes may see each secret. */
const ALLOWED = {
  PGHACKER_DB_ADMIN_PASSWORD: ["bootstrap"],
  PGH_INDEXER_PASSWORD: ["bootstrap", "indexer"],
  PGH_READER_PASSWORD: ["bootstrap", "mcp"],
  PGH_SWARM_PASSWORD: ["bootstrap", "swarm", "crawl"],
  OAUTH2_PROXY_CLIENT_ID: ["proxy"],
  OAUTH2_PROXY_CLIENT_SECRET: ["proxy"],
  OAUTH2_PROXY_COOKIE_SECRET: ["proxy"],
  IDENTITY_HEADER: [],
  MSI_SECRET: [],
};

test("each process sees only its own secrets, and none the managed identity", () => {
  const environments = processEnvironments(readAzureConfig(ENV));
  assert.deepEqual(Object.keys(environments).sort(), ["bootstrap", "crawl", "home", "indexer", "mcp", "proxy", "swarm"]);
  for (const [secret, allowed] of Object.entries(ALLOWED)) {
    const raw = ENV[secret];
    for (const [name, env] of Object.entries(environments)) {
      const text = Object.values(env).join("\n");
      const holds = text.includes(raw) || text.includes(encodeURIComponent(raw));
      assert.equal(holds, allowed.includes(name), `${name} ${holds ? "sees" : "lacks"} ${secret}`);
    }
  }
  for (const env of Object.values(environments)) {
    for (const key of Object.keys(env)) assert.doesNotMatch(key, /^(IDENTITY_|MSI_|AZURE_CLIENT_ID)/);
    assert.equal(env.RUST_LOG, "debug");
  }
});

test("processes reach the database as their own roles, over verified TLS", () => {
  const config = readAzureConfig(ENV);
  const environments = processEnvironments(config);
  const swarm = new URL(environments.swarm.DATABASE_URL);
  assert.equal(swarm.username, "pgh_swarm");
  assert.equal(swarm.searchParams.get("sslmode"), "require");
  assert.equal(new URL(environments.indexer.PGHACKER_INDEXER_DATABASE_URL).username, "pgh_indexer");
  assert.equal(new URL(environments.mcp.PGHACKER_READER_DATABASE_URL).username, "pgh_reader");
  const admin = new URL(databaseUrls(config).admin);
  assert.equal(decodeURIComponent(admin.password), SECRETS.PGHACKER_DB_ADMIN_PASSWORD);
  assert.equal(admin.pathname, "/pghacker");
  assert.equal(new URL(databaseUrl(config, "u", "p", "postgres")).pathname, "/postgres");
});

test("agents use the internal pghacker-mcp listener; the public one is rate-limited and behind Front Door", () => {
  const environments = processEnvironments(readAzureConfig(ENV));
  assert.equal(environments.swarm.PGHACKER_MCP_URL, `http://127.0.0.1:${PORTS.mcpInternal}/mcp`);
  assert.equal(environments.swarm.PGHACKER_INDEXER_MCP_URL, `http://127.0.0.1:${PORTS.indexer}/mcp`);
  assert.equal(environments.swarm.PORTAL_HOST, "127.0.0.1");
  assert.equal(environments.swarm.PORTAL_AUTH_PROVIDER, "proxy");
  assert.equal(environments.swarm.PORTAL_AUTH_ALLOW_UNAUTHENTICATED, "false");
  assert.equal(environments.swarm.PORTAL_AUTHZ_DEFAULT_ROLE, "user");
  assert.equal(environments.mcp.PGHACKER_MCP_ADDR, `127.0.0.1:${PORTS.mcp}`);
  assert.equal(environments.mcp.PGHACKER_MCP_INTERNAL_ADDR, `127.0.0.1:${PORTS.mcpInternal}`);
  // Front Door's socket address: its X-Azure-ClientIP follows a client's X-Forwarded-For.
  assert.equal(environments.mcp.PGHACKER_CLIENT_IP_HEADER, "x-azure-socketip");
  assert.equal(environments.mcp.PGHACKER_FRONT_DOOR_ID, ENV.PGHACKER_FRONT_DOOR_ID);
  assert.equal(environments.mcp.PGHACKER_RATE_PER_MINUTE, "60");
  assert.deepEqual(environments.mcp.PGHACKER_ALLOWED_HOSTS.split(","), ENV.PGHACKER_PUBLIC_HOSTS.split(","));
  assert.equal(environments.indexer.PGHACKER_SOURCES, "open");
  assert.equal(environments.indexer.PGHACKER_EXTRACTION, "automatic");
  assert.equal(environments.indexer.PGHACKER_KEEP_DOWNLOADS, "false");
  assert.equal(environments.crawl.PGHACKER_CRAWL_MODEL, "azure-foundry:gpt-6-luna");
  // Foundry is reached through the supervisor's proxy, which adds the token.
  assert.equal(environments.crawl.PGHACKER_FOUNDRY_ENDPOINT, `http://127.0.0.1:${PORTS.foundry}`);
  assert.equal(environments.swarm.AZURE_OAI_KEY, "managed-identity");
  assert.equal(environments.crawl.AZURE_OAI_KEY, "managed-identity");
  assert.equal(environments.swarm.PILOTSWARM_WORKER_CONCURRENCY, "20");
});

test("PGHACKER_CODE_HISTORY names the history schema of the indexer and pghacker-mcp, and nothing else", () => {
  const off = processEnvironments(readAzureConfig(ENV));
  for (const env of Object.values(off)) assert.equal("PGHACKER_CODE_HISTORY" in env, false);
  const on = processEnvironments(readAzureConfig({ ...ENV, PGHACKER_CODE_HISTORY: " pghacker " }));
  for (const [name, env] of Object.entries(on)) {
    assert.equal(env.PGHACKER_CODE_HISTORY, ["indexer", "mcp"].includes(name) ? "pghacker" : undefined, name);
  }
  for (const bad of ["Pghacker", "pghacker; DROP", "1history", "a".repeat(64)]) {
    assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_CODE_HISTORY: bad }), /must name a schema/, bad);
  }
});

test("the extraction order's settings reach the indexer only, and are checked", () => {
  const names = ["PGHACKER_EXTRACTION_ORDER", "PGHACKER_EXTRACTION_NEW_MAIL_SHARE", "PGHACKER_EXTRACTION_NEW_MAIL_DAYS"];
  for (const env of Object.values(processEnvironments(readAzureConfig(ENV)))) {
    for (const name of names) assert.equal(name in env, false, name);
  }
  const set = {
    PGHACKER_EXTRACTION_ORDER: "largest",
    PGHACKER_EXTRACTION_NEW_MAIL_SHARE: " 0.25 ",
    PGHACKER_EXTRACTION_NEW_MAIL_DAYS: "3",
  };
  for (const [process, env] of Object.entries(processEnvironments(readAzureConfig({ ...ENV, ...set })))) {
    for (const name of names) {
      assert.equal(env[name], process === "indexer" ? set[name].trim() : undefined, `${process} ${name}`);
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

test("the home page's server runs as its own user with the page, its port and the portal's origin only", () => {
  const config = readAzureConfig(ENV);
  const { home } = processEnvironments(config);
  // Not oauth2-proxy's user, which could read that process's secrets.
  assert.equal(home.USER, USERS.home);
  assert.equal(new Set(Object.values(USERS)).size, Object.keys(USERS).length);
  assert.equal(home.HOME, layout(config).home.home);
  assert.equal(home.PORT, String(PORTS.home));
  assert.equal(home.PGHACKER_HOME_DIR, path.join(config.root, "home", "dist"));
  // Share links go on to the portal, on Front Door's host name, never the public domain.
  assert.equal(home.PGHACKER_PORTAL_URL, "https://pghacker-abc.z01.azurefd.net");
  assert.deepEqual(Object.keys(home).sort(), ["HOME", "LOGNAME", "PATH", "PGHACKER_HOME_DIR", "PGHACKER_PORTAL_URL", "PORT", "RUST_LOG", "USER"]);
  assert.equal(new Set(Object.values(PORTS)).size, Object.keys(PORTS).length);
});

const USAGE_ENV = {
  ...ENV,
  APPLICATIONINSIGHTS_CONNECTION_STRING:
    "InstrumentationKey=11111111-2222-4333-8444-555555555555;IngestionEndpoint=https://westus3-1.in.applicationinsights.azure.com/;"
    + "LiveEndpoint=https://westus3.livediagnostics.monitor.azure.com/;ApplicationId=22222222-0000-4000-8000-000000000000",
  PGHACKER_USAGE_ID_KEY: "usage-id-key-0123456789abcdefghijklmnopqrstuvwxyz",
  CONTAINER_APP_REVISION: "pghacker-example--0000016",
};

test("usage telemetry reaches pghacker-mcp and the home page through the supervisor's proxy, with Application Insights only", () => {
  const config = readAzureConfig(USAGE_ENV);
  assert.deepEqual(config.usage, {
    instrumentationKey: "11111111-2222-4333-8444-555555555555",
    ingestionEndpoint: "https://westus3-1.in.applicationinsights.azure.com",
    audience: "https://monitor.azure.com",
    idKey: USAGE_ENV.PGHACKER_USAGE_ID_KEY,
    revision: "pghacker-example--0000016",
    details: false,
  });
  const environments = processEnvironments(config);
  for (const name of ["mcp", "home"]) {
    const env = environments[name];
    assert.equal(env.PGHACKER_USAGE_ENDPOINT, MONITOR_PROXY, name);
    assert.equal(env.PGHACKER_USAGE_IKEY, "11111111-2222-4333-8444-555555555555", name);
    assert.equal(env.PGHACKER_USAGE_ID_KEY, USAGE_ENV.PGHACKER_USAGE_ID_KEY, name);
    assert.equal(env.CONTAINER_APP_REVISION, "pghacker-example--0000016", name);
    assert.equal(env.PGHACKER_FRONT_DOOR_ID, ENV.PGHACKER_FRONT_DOOR_ID, name);
  }
  assert.equal(MONITOR_PROXY, `http://127.0.0.1:${PORTS.monitor}`);
  // Details are pghacker-mcp's alone, and off unless set.
  assert.equal(environments.mcp.PGHACKER_USAGE_DETAILS, "false");
  assert.equal(environments.home.PGHACKER_USAGE_DETAILS, undefined);
  const detailed = readAzureConfig({ ...USAGE_ENV, PGHACKER_USAGE_DETAILS: "true" });
  assert.equal(detailed.usage.details, true);
  assert.equal(processEnvironments(detailed).mcp.PGHACKER_USAGE_DETAILS, "true");
  assert.throws(
    () => readAzureConfig({ ...USAGE_ENV, PGHACKER_USAGE_DETAILS: "yes" }),
    /PGHACKER_USAGE_DETAILS must be true or false/,
  );
  // The users' key reaches those two alone, and the connection string none:
  // the supervisor's proxy holds the way in.
  for (const [name, env] of Object.entries(environments)) {
    const text = Object.values(env).join("\n");
    assert.equal(text.includes(USAGE_ENV.PGHACKER_USAGE_ID_KEY), ["mcp", "home"].includes(name), name);
    assert.ok(!text.includes("IngestionEndpoint"), name);
  }

  // Without Application Insights, no process records usage; nor do the jobs.
  for (const env of [
    ...Object.values(processEnvironments(readAzureConfig(ENV))),
    ...Object.values(processEnvironments(readAzureConfig(USAGE_ENV, { serve: false }))),
  ]) {
    assert.ok(!Object.keys(env).some((key) => key.startsWith("PGHACKER_USAGE_")));
  }
  assert.throws(
    () => readAzureConfig({ ...USAGE_ENV, PGHACKER_USAGE_ID_KEY: "" }),
    /needs PGHACKER_USAGE_ID_KEY/,
  );
  assert.throws(
    () => readAzureConfig({ ...USAGE_ENV, PGHACKER_USAGE_ID_KEY: "too-short" }),
    /at least 32 characters/,
  );
  assert.throws(
    () => readAzureConfig({ ...USAGE_ENV, APPLICATIONINSIGHTS_CONNECTION_STRING: "IngestionEndpoint=https://x.example/" }),
    /InstrumentationKey/,
  );
});

test("feedback's settings reach pghacker-mcp alone, and only with usage telemetry", () => {
  const settings = (env) => {
    const environments = processEnvironments(readAzureConfig(env));
    return Object.entries(environments)
      .filter(([, processEnv]) => Object.keys(processEnv).some((key) => key.startsWith("PGHACKER_FEEDBACK_")))
      .map(([name, processEnv]) => [
        name,
        Object.fromEntries(Object.entries(processEnv).filter(([key]) => key.startsWith("PGHACKER_FEEDBACK_"))),
      ]);
  };
  const defaults = {
    PGHACKER_FEEDBACK_COMMENTS: "false",
    PGHACKER_FEEDBACK_QUESTION_GAP: "180",
    PGHACKER_FEEDBACK_SESSION_GAP: "600",
    PGHACKER_FEEDBACK_MIN_QUESTIONS: "3",
  };
  const withFeedback = { ...USAGE_ENV, PGHACKER_FEEDBACK_RATE: "0.1", PGHACKER_FEEDBACK_COMMENTS: "false" };
  assert.deepEqual(settings(withFeedback), [["mcp", { PGHACKER_FEEDBACK_RATE: "0.1", ...defaults }]]);
  assert.deepEqual(readAzureConfig(withFeedback).feedback, {
    rate: 0.1, comments: false, questionGap: 180, sessionGap: 600, minQuestions: 3,
  });
  // As Terraform sets them all.
  const all = {
    ...withFeedback,
    PGHACKER_FEEDBACK_QUESTION_GAP: "120",
    PGHACKER_FEEDBACK_SESSION_GAP: "900",
    PGHACKER_FEEDBACK_MIN_QUESTIONS: "0",
    PGHACKER_FEEDBACK_CLIENTS: "copilot-cli,Visual Studio Code",
  };
  assert.deepEqual(settings(all), [["mcp", {
    PGHACKER_FEEDBACK_RATE: "0.1",
    PGHACKER_FEEDBACK_COMMENTS: "false",
    PGHACKER_FEEDBACK_QUESTION_GAP: "120",
    PGHACKER_FEEDBACK_SESSION_GAP: "900",
    PGHACKER_FEEDBACK_MIN_QUESTIONS: "0",
    PGHACKER_FEEDBACK_CLIENTS: "copilot-cli,Visual Studio Code",
  }]]);
  // Unset, off; without usage telemetry, nothing at all.
  assert.deepEqual(settings(USAGE_ENV), [["mcp", { PGHACKER_FEEDBACK_RATE: "0", ...defaults }]]);
  assert.deepEqual(settings({ ...ENV, PGHACKER_FEEDBACK_RATE: "0.1" }), []);
  assert.equal(readAzureConfig({ ...ENV, PGHACKER_FEEDBACK_RATE: "0.1" }).feedback, undefined);
  for (const rate of ["1.5", "-0.1", "ten percent"]) {
    assert.throws(() => readAzureConfig({ ...USAGE_ENV, PGHACKER_FEEDBACK_RATE: rate }), /PGHACKER_FEEDBACK_RATE must be a number from 0 to 1/);
  }
  assert.throws(
    () => readAzureConfig({ ...USAGE_ENV, PGHACKER_FEEDBACK_COMMENTS: "yes" }),
    /PGHACKER_FEEDBACK_COMMENTS must be true or false/,
  );
  for (const [name, bad, error] of [
    ["PGHACKER_FEEDBACK_QUESTION_GAP", "0", /PGHACKER_FEEDBACK_QUESTION_GAP must be an integer from 1 to 3600/],
    ["PGHACKER_FEEDBACK_SESSION_GAP", "180", /PGHACKER_FEEDBACK_SESSION_GAP must be more than PGHACKER_FEEDBACK_QUESTION_GAP/],
    ["PGHACKER_FEEDBACK_MIN_QUESTIONS", "101", /PGHACKER_FEEDBACK_MIN_QUESTIONS must be an integer from 0 to 100/],
    ["PGHACKER_FEEDBACK_MIN_QUESTIONS", "2.5", /PGHACKER_FEEDBACK_MIN_QUESTIONS must be an integer/],
  ]) {
    assert.throws(() => readAzureConfig({ ...USAGE_ENV, [name]: bad }), error, `${name}=${bad}`);
  }
});

test("oauth2-proxy signs in the team for the portal, passes /mcp and the home page through, and gets no secret in its arguments", () => {
  const args = oauth2ProxyArgs(readAzureConfig(ENV));
  for (const expected of [
    "--http-address=0.0.0.0:8080", "--provider=github", "--github-org=example-org",
    "--github-team=example-team", "--skip-auth-route=^/mcp$", "--skip-auth-route=GET=^/$",
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
  assert.throws(() => readAzureConfig({ ...ENV, PGH_SWARM_PASSWORD: "" }), /PGH_SWARM_PASSWORD/);
  assert.throws(() => readAzureConfig({ ...ENV, AZURE_CLIENT_ID: "" }), /AZURE_CLIENT_ID/);
  assert.throws(() => readAzureConfig({ ...ENV, OAUTH2_PROXY_COOKIE_SECRET: undefined }), /OAUTH2_PROXY_COOKIE_SECRET/);
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
  // The jobs need neither the public and portal URLs nor oauth2-proxy's secrets.
  const job = readAzureConfig(
    { ...ENV, PGHACKER_PUBLIC_URL: "", PGHACKER_PORTAL_URL: "", OAUTH2_PROXY_CLIENT_SECRET: "" },
    { serve: false },
  );
  const environments = processEnvironments(job);
  assert.equal(environments.mcp, undefined);
  assert.equal(environments.proxy, undefined);
  assert.ok(environments.bootstrap.PGHACKER_ADMIN_DATABASE_URL);
});

test("PilotSwarm's catalog is Foundry's Sol and Luna over the Responses API, and embeddings use the same account, all through the proxy", () => {
  const config = readAzureConfig(ENV);
  const catalog = foundryCatalog(config.foundry);
  assert.equal(catalog.defaultModel, "azure-foundry:gpt-6.1-sol");
  const [provider] = catalog.providers;
  assert.equal(provider.type, "openai");
  assert.equal(provider.wireApi, "responses");
  assert.equal(provider.baseUrl, `http://127.0.0.1:${PORTS.foundry}/openai/v1`);
  assert.equal(provider.apiKey, "env:AZURE_OAI_KEY");
  assert.deepEqual(provider.models.map((m) => m.name), ["gpt-6.1-sol", "gpt-6-luna"]);
  const embed = embedEnvironment(config.foundry);
  assert.equal(embed.HORIZON_EMBED_URL,
    `http://127.0.0.1:${PORTS.foundry}/openai/deployments/text-embedding-3-small/embeddings?api-version=2024-10-21`);
  assert.equal(embed.HORIZON_EMBED_DIM, "1536");
  assert.equal(embed.HORIZON_EMBED_API_KEY, "managed-identity");
  assert.equal(config.foundry.endpoint, "https://pghacker-example-foundry.openai.azure.com");
  assert.equal(config.foundry.clientId, "uami-client-id");
});

test("users come from /etc/passwd and each has its own directories", () => {
  const passwd = "root:x:0:0:root:/root:/bin/bash\npgh-agent:x:10001:10001::/home/pgh-agent:/usr/sbin/nologin\n";
  assert.deepEqual(lookupUser(USERS.agent, passwd), { name: "pgh-agent", uid: 10001, gid: 10001, home: "/home/pgh-agent" });
  assert.throws(() => lookupUser(USERS.proxy, passwd), /no user pgh-proxy/);
  const files = layout(readAzureConfig(ENV));
  assert.ok(files.agent.sessions.startsWith("/var/lib/pghacker/agent/"));
  assert.ok(files.services.git.startsWith("/var/lib/pghacker/svc/"));
  assert.ok(files.home.home.startsWith("/var/lib/pghacker/home-page/"));
});
