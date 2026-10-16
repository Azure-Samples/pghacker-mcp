// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { resolvePortalConfigFromPluginDirs } from "pilotswarm/host";
import {
  APP_ROOT, ensureRolePasswords, initialEnv, initializeEnv, parseLocalConfig, portalEnvironment, projectName,
  readLocalConfig, serviceEnvironments,
} from "../scripts/lib/config.mjs";
import { workerOptions } from "../scripts/lib/worker.mjs";

const password = "unit-test-password-for-local-postgres";

test("local configuration matches the isolated database and disables portal-managed workers", () => {
  const config = parseLocalConfig(initialEnv(password));
  assert.equal(config.postgresPort, 55433);
  assert.equal(config.portalPort, 3001);
  assert.equal(config.values.WORKERS, "0");
  assert.equal(new URL(config.databaseUrl).hostname, "127.0.0.1");
  assert.equal(decodeURIComponent(new URL(config.databaseUrl).password), password);
});

test("separate worktrees have separate Compose project identities", () => {
  assert.equal(projectName("/tmp/app-a"), projectName("/tmp/app-a"));
  assert.notEqual(projectName("/tmp/app-a"), projectName("/tmp/app-b"));
  assert.match(projectName("/tmp/app-a"), /^pghacker-mcp-[a-f0-9]{8}$/);
});

test("setup creates private configuration and never replaces an existing file", () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "pghacker-config-"));
  try {
    assert.equal(initializeEnv(root), true);
    assert.equal(statSync(path.join(root, ".env")).mode & 0o777, 0o600);
    const original = readFileSync(path.join(root, ".env"), "utf8");
    assert.equal(readLocalConfig(root).values.POSTGRES_PASSWORD.length, 64);
    assert.equal(initializeEnv(root), false);
    assert.equal(readFileSync(path.join(root, ".env"), "utf8"), original);
    writeFileSync(path.join(root, ".env"), "user-owned configuration");
    assert.equal(initializeEnv(root), false);
    assert.equal(readFileSync(path.join(root, ".env"), "utf8"), "user-owned configuration");
  } finally {
    rmSync(root, { recursive: true });
  }
});

test("missing configuration reports the required setup command", () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "pghacker-missing-"));
  try {
    assert.throws(() => readLocalConfig(root), /Run npm run setup first/);
  } finally {
    rmSync(root, { recursive: true });
  }
});

test("remote database URLs and connection-string host overrides are rejected", () => {
  assert.throws(() => parseLocalConfig(initialEnv(password).replace("127.0.0.1:55433", "remote.example:55433")), /127\.0\.0\.1/);
  assert.throws(() => parseLocalConfig(initialEnv(password).replace("/pghacker_mcp\n", "/pghacker_mcp?host=remote.example\n")), /without query parameters/);
  assert.throws(() => parseLocalConfig(initialEnv(password).replace("POSTGRES_PORT=55433", "POSTGRES_PORT=55434")), /PostgreSQL port/);
});

test("credential mismatch is explicit without disclosing the password", () => {
  const text = initialEnv(password).replace(`POSTGRES_PASSWORD=${password}`, "POSTGRES_PASSWORD=different-unit-test-password");
  assert.throws(() => parseLocalConfig(text), (error) => {
    assert.match(error.message, /must match/);
    assert.equal(error.message.includes(password), false);
    assert.equal(error.message.includes("different-unit-test-password"), false);
    return true;
  });
});

test("the starter refuses public listeners, remote mode, auth changes, and additional workers", () => {
  for (const [oldValue, newValue] of [
    ["PORTAL_HOST=127.0.0.1", "PORTAL_HOST=0.0.0.0"],
    ["PORTAL_AUTH_PROVIDER=none", "PORTAL_AUTH_PROVIDER=entra"],
    ["PORTAL_AUTHZ_DEFAULT_ROLE=admin", "PORTAL_AUTHZ_DEFAULT_ROLE=none"],
    ["PORTAL_AUTH_ALLOW_UNAUTHENTICATED=true", "PORTAL_AUTH_ALLOW_UNAUTHENTICATED=false"],
    ["WORKERS=0", "WORKERS=4"],
    ["PORTAL_TUI_MODE=local", "PORTAL_TUI_MODE=remote"],
  ]) {
    assert.throws(() => parseLocalConfig(initialEnv(password).replace(oldValue, newValue)), /local-only starter/);
  }
});

test("invalid or conflicting ports are rejected", () => {
  for (const port of ["0", "65536", "3.5", "3001oops"]) {
    assert.throws(() => parseLocalConfig(initialEnv(password).replace("PORTAL_PORT=3001", `PORTAL_PORT=${port}`)), /integer between/);
  }
  assert.throws(() => parseLocalConfig(initialEnv(password).replace("PORTAL_PORT=3001", "PORTAL_PORT=55433")), /different ports/);
});

test("placeholder passwords cannot start a runtime", () => {
  const text = readFileSync(path.join(APP_ROOT, ".env.example"), "utf8");
  assert.throws(() => parseLocalConfig(text), /real local password/);
});

test("ambient fleet, worker, storage, plugin, and Node settings cannot redirect the runtime", () => {
  const config = parseLocalConfig(initialEnv(password));
  const env = portalEnvironment(config, {
    PATH: "/test/path",
    HOME: "/test/home",
    DATABASE_URL: "postgresql://remote.example/shared",
    HORIZON_DATABASE_URL: "postgresql://remote.example/facts",
    PILOTSWARM_CMS_FACTS_DATABASE_URL: "postgresql://remote.example/cms",
    AZURE_STORAGE_CONNECTION_STRING: "unit-test-shared-storage",
    PLUGIN_DIRS: "/other/plugin",
    SESSION_STATE_DIR: "/other/sessions",
    WORKERS: "99",
    PORTAL_HOST: "0.0.0.0",
    PORTAL_TUI_MODE: "remote",
    NODE_OPTIONS: "--require /other/injection.js",
    GITHUB_TOKEN: "unit-test-bootstrap-token",
  });
  assert.equal(env.PATH, "/test/path");
  assert.equal(env.DATABASE_URL, config.databaseUrl);
  assert.equal(env.WORKERS, "0");
  assert.equal(env.PORTAL_HOST, "127.0.0.1");
  assert.equal(env.PORTAL_TUI_MODE, "local");
  assert.equal(env.PLUGIN_DIRS, path.join(config.root, "plugin"));
  assert.equal(env.SESSION_STATE_DIR, path.join(config.runtimeDir, "sessions"));
  for (const key of ["HORIZON_DATABASE_URL", "PILOTSWARM_CMS_FACTS_DATABASE_URL", "AZURE_STORAGE_CONNECTION_STRING", "NODE_OPTIONS"]) {
    assert.equal(key in env, false);
  }
  assert.equal(env.GITHUB_TOKEN, "unit-test-bootstrap-token");
});

test("explicit token configuration takes priority and no token is invented", () => {
  const config = parseLocalConfig(`${initialEnv(password)}GITHUB_TOKEN=unit-test-configured-token\n`);
  assert.equal(portalEnvironment(config, { GITHUB_TOKEN: "unit-test-ambient-token" }).GITHUB_TOKEN, "unit-test-configured-token");
  assert.equal(portalEnvironment(parseLocalConfig(initialEnv(password)), {}).GITHUB_TOKEN, undefined);
});

test("PilotSwarm resolves app branding through the supported plugin API", () => {
  const config = resolvePortalConfigFromPluginDirs([path.join(APP_ROOT, "plugin")]);
  assert.equal(config.branding.title, "pghacker-mcp");
  assert.equal(config.branding.pageTitle, "pghacker-mcp");
  assert.equal(config.branding.splash, "pghacker-mcp");
  assert.equal(config.ui.loadingMessage, "Starting pghacker-mcp");
  assert.equal(config.auth.provider, "none");
});

test("the app-owned worker uses the public management-agent opt-out", () => {
  const config = parseLocalConfig(initialEnv(password));
  const options = workerOptions(config, portalEnvironment(config, {}));
  assert.equal(options.workerNodeId, config.project);
  assert.equal(options.store, config.databaseUrl);
  assert.equal(options.disableManagementAgents, true);
  assert.equal(options.githubToken, undefined);
  assert.deepEqual(options.pluginDirs, [path.join(config.root, "plugin")]);
});

test("the roles need real passwords and every listener its own port", () => {
  const text = initialEnv(password);
  assert.throws(() => parseLocalConfig(text.replace(/PGH_READER_PASSWORD=.*/, "PGH_READER_PASSWORD=short")), /PGH_READER_PASSWORD/);
  assert.throws(() => parseLocalConfig(text.replace(/PGH_INDEXER_PASSWORD=.*\n/, "")), /PGH_INDEXER_PASSWORD/);
  assert.throws(() => parseLocalConfig(`${text}PGHACKER_MCP_PORT=3001\n`), /different ports/);
  const config = parseLocalConfig(text);
  assert.equal(config.indexerPort, 3101);
  assert.equal(config.mcpPort, 3102);
  // PORTAL_PORT is the site's; PilotSwarm's own listener is behind it.
  assert.equal(config.portalPort, 3001);
  assert.equal(config.portalInternalPort, 3002);
  assert.equal(portalEnvironment(config).PORT, "3002");
  assert.equal(parseLocalConfig(`${text}PGHACKER_PORTAL_INTERNAL_PORT=3999\n`).portalInternalPort, 3999);
  for (const port of ["3001", "3101", "3102", "55433"]) {
    assert.throws(() => parseLocalConfig(`${text}PGHACKER_PORTAL_INTERNAL_PORT=${port}\n`), /different ports/, port);
  }
  assert.throws(() => parseLocalConfig(`${text}PGHACKER_PORTAL_INTERNAL_PORT=0\n`), /integer between/);
});

test("setup adds role passwords to an .env from before them, once", () => {
  const root = mkdtempSync(path.join(os.tmpdir(), "pghacker-roles-"));
  try {
    const legacy = initialEnv(password).split("\n").filter((line) => !line.includes("PGH_") && !line.includes("pghacker roles")).join("\n");
    writeFileSync(path.join(root, ".env"), legacy, { mode: 0o600 });
    assert.throws(() => readLocalConfig(root), /PGH_INDEXER_PASSWORD/);
    assert.equal(ensureRolePasswords(root), true);
    const config = readLocalConfig(root);
    assert.equal(config.values.PGH_INDEXER_PASSWORD.length, 64);
    assert.notEqual(config.values.PGH_INDEXER_PASSWORD, config.values.PGH_READER_PASSWORD);
    assert.equal(ensureRolePasswords(root), false);
  } finally {
    rmSync(root, { recursive: true });
  }
});

test("each process gets only the credentials it needs", () => {
  const config = parseLocalConfig(`${initialEnv(password)}HORIZON_EMBED_URL=https://embed.example/embeddings\nHORIZON_EMBED_API_KEY=unit-test-key\n`);
  const services = serviceEnvironments(config, { PATH: "/test/path", DATABASE_URL: "postgresql://remote.example/x" });
  const role = (url) => decodeURIComponent(new URL(url).username);
  assert.equal(role(services.bootstrap.PGHACKER_ADMIN_DATABASE_URL), "pghacker");
  assert.equal(role(services.indexer.PGHACKER_INDEXER_DATABASE_URL), "pgh_indexer");
  assert.equal(role(services.mcp.PGHACKER_READER_DATABASE_URL), "pgh_reader");
  assert.equal(new URL(services.mcp.PGHACKER_READER_DATABASE_URL).hostname, "127.0.0.1");
  for (const key of ["PGHACKER_ADMIN_DATABASE_URL", "DATABASE_URL", "PGHACKER_READER_DATABASE_URL", "PGH_READER_PASSWORD"]) {
    assert.equal(key in services.indexer, false, key);
  }
  for (const key of ["PGHACKER_ADMIN_DATABASE_URL", "DATABASE_URL", "PGHACKER_INDEXER_DATABASE_URL", "PGH_INDEXER_PASSWORD"]) {
    assert.equal(key in services.mcp, false, key);
  }
  assert.equal(services.indexer.HORIZON_EMBED_API_KEY, "unit-test-key");
  assert.equal(services.mcp.HORIZON_EMBED_URL, "https://embed.example/embeddings");
  assert.equal(services.indexer.PATH, "/test/path");
  assert.equal(services.mcp.PGHACKER_RATE_PER_MINUTE, "3000");
  assert.equal(services.mcp.PGHACKER_RATE_BURST, "500");
  assert.equal(
    serviceEnvironments(parseLocalConfig(`${initialEnv(password)}PGHACKER_RATE_PER_MINUTE=120\n`), {}).mcp.PGHACKER_RATE_PER_MINUTE,
    "120",
  );
  const portal = portalEnvironment(config, {});
  assert.equal(portal.PILOTSWARM_WORKER_CONCURRENCY, "8");
  assert.equal(portalEnvironment(parseLocalConfig(`${initialEnv(password)}PILOTSWARM_WORKER_CONCURRENCY=12\n`), {}).PILOTSWARM_WORKER_CONCURRENCY, "12");
  assert.equal(portal.PGHACKER_MCP_URL, "http://127.0.0.1:3102/mcp");
  assert.equal(portal.PGHACKER_INDEXER_MCP_URL, "http://127.0.0.1:3101/mcp");
  // Agents pointed at another pghacker-mcp turn its feedback off: no person
  // reads their answers.
  const agents = (url, inherited = {}) =>
    portalEnvironment(parseLocalConfig(`${initialEnv(password)}${url ? `PGHACKER_AGENT_MCP_URL=${url}\n` : ""}`), inherited)
      .PGHACKER_MCP_URL;
  assert.equal(agents("https://mcp.example.org/mcp"), "https://mcp.example.org/mcp?feedback=off");
  assert.equal(agents("https://example.test/mcp?trace=1&feedback=on"), "https://example.test/mcp?trace=1&feedback=off");
  assert.equal(agents("", { PGHACKER_AGENT_MCP_URL: "https://mcp.example.org/mcp" }), "https://mcp.example.org/mcp?feedback=off");
  assert.throws(() => agents("not a url"), /PGHACKER_AGENT_MCP_URL must be a URL/);
  for (const key of ["HORIZON_EMBED_API_KEY", "PGH_INDEXER_PASSWORD", "PGH_READER_PASSWORD"]) {
    assert.equal(key in portal, false, key);
  }
});

test("PGHACKER_CODE_HISTORY, from .env or the shell, reaches the indexer and pghacker-mcp only", () => {
  const services = (text, inherited = {}) => serviceEnvironments(parseLocalConfig(`${initialEnv(password)}${text}`), inherited);
  for (const env of Object.values(services(""))) assert.equal("PGHACKER_CODE_HISTORY" in env, false);
  const fromFile = services("PGHACKER_CODE_HISTORY=pghacker_history\n", { PGHACKER_CODE_HISTORY: "pghacker" });
  assert.equal(fromFile.indexer.PGHACKER_CODE_HISTORY, "pghacker_history");
  assert.equal(fromFile.mcp.PGHACKER_CODE_HISTORY, "pghacker_history");
  assert.equal("PGHACKER_CODE_HISTORY" in fromFile.bootstrap, false);
  assert.equal(services("", { PGHACKER_CODE_HISTORY: "pghacker" }).mcp.PGHACKER_CODE_HISTORY, "pghacker");
  assert.throws(() => services("PGHACKER_CODE_HISTORY=pghacker.code\n"), /must name a schema/);
});

test("the extraction order's settings, from .env or the shell, reach the indexer only", () => {
  const services = (text, inherited = {}) => serviceEnvironments(parseLocalConfig(`${initialEnv(password)}${text}`), inherited);
  for (const env of Object.values(services(""))) {
    assert.equal("PGHACKER_EXTRACTION_ORDER" in env, false);
  }
  const fromFile = services(
    "PGHACKER_EXTRACTION_ORDER=largest\nPGHACKER_EXTRACTION_NEW_MAIL_SHARE=0.3\n",
    { PGHACKER_EXTRACTION_ORDER: "lanes", PGHACKER_EXTRACTION_NEW_MAIL_DAYS: "2" },
  );
  assert.equal(fromFile.indexer.PGHACKER_EXTRACTION_ORDER, "largest");
  assert.equal(fromFile.indexer.PGHACKER_EXTRACTION_NEW_MAIL_SHARE, "0.3");
  assert.equal(fromFile.indexer.PGHACKER_EXTRACTION_NEW_MAIL_DAYS, "2");
  for (const process of ["bootstrap", "mcp"]) {
    assert.equal("PGHACKER_EXTRACTION_ORDER" in fromFile[process], false, process);
  }
  assert.throws(() => services("PGHACKER_EXTRACTION_NEW_MAIL_DAYS=0\n"), /PGHACKER_EXTRACTION_NEW_MAIL_DAYS/);
});

test("the plugin's agents load with the MCP servers they name", async () => {
  const { loadAgentFiles } = await import("pilotswarm-sdk");
  const agents = Object.fromEntries(loadAgentFiles(path.join(APP_ROOT, "plugin", "agents")).map((agent) => [agent.name, agent]));
  assert.deepEqual(Object.keys(agents).sort(), ["pghacker-crawler", "pghacker-dedup", "pghacker-eval", "pghacker-extract"]);
  assert.deepEqual(agents["pghacker-extract"].mcpServers, ["pghacker-indexer"]);
  assert.deepEqual(agents["pghacker-eval"].mcpServers, ["pghacker-mcp"]);
  const catalog = JSON.parse(readFileSync(path.join(APP_ROOT, "plugin", ".mcp.json"), "utf8"));
  assert.equal(catalog["pghacker-mcp"].url, "${PGHACKER_MCP_URL}");
  assert.equal(catalog["pghacker-indexer"].url, "${PGHACKER_INDEXER_MCP_URL}");
});
