import assert from "node:assert/strict";
import test from "node:test";
import {
  PORTS, USERS, databaseUrl, databaseUrls, embedEnvironment, foundryCatalog, layout, lookupUser, oauth2ProxyArgs,
  processEnvironments, readAzureConfig,
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
  PGHACKER_PUBLIC_URL: "https://pghacker-abc.z01.azurefd.net",
  PGHACKER_PUBLIC_HOSTS: "pghacker-abc.z01.azurefd.net,pghacker-example.env.westus3.azurecontainerapps.io",
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
  assert.deepEqual(Object.keys(environments).sort(), ["bootstrap", "crawl", "indexer", "mcp", "proxy", "swarm"]);
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
  assert.equal(environments.mcp.PGHACKER_CLIENT_IP_HEADER, "x-azure-clientip");
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

test("oauth2-proxy signs in the team for the portal, passes /mcp through, and gets no secret in its arguments", () => {
  const args = oauth2ProxyArgs(readAzureConfig(ENV));
  for (const expected of [
    "--http-address=0.0.0.0:8080", "--provider=github", "--github-org=example-org",
    "--github-team=example-team", "--skip-auth-route=^/mcp$",
    `--upstream=http://127.0.0.1:${PORTS.mcp}/mcp`, `--upstream=http://127.0.0.1:${PORTS.portal}/`,
    "--redirect-url=https://pghacker-abc.z01.azurefd.net/oauth2/callback", "--cookie-secure=true",
    "--pass-user-headers=true",
  ]) {
    assert.ok(args.includes(expected), expected);
  }
  assert.ok(!args.some((arg) => arg.startsWith("--github-user")));
  const text = args.join(" ");
  for (const secret of Object.values(SECRETS)) assert.ok(!text.includes(secret));
  const users = oauth2ProxyArgs(readAzureConfig({ ...ENV, PGHACKER_GITHUB_USERS: "example-user, another-example-user" }));
  assert.ok(users.includes("--github-user=example-user,another-example-user"));
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
  assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_PORTAL_ROLE: "owner" }), /user or admin/);
  assert.throws(() => readAzureConfig({ ...ENV, PGHACKER_CRAWL_START_SESSIONS: "32" }), /must not exceed/);
  // The jobs need neither the public URL nor oauth2-proxy's secrets.
  const job = readAzureConfig({ ...ENV, PGHACKER_PUBLIC_URL: "", OAUTH2_PROXY_CLIENT_SECRET: "" }, { serve: false });
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
});
