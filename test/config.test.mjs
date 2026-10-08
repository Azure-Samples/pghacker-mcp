import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { resolvePortalConfigFromPluginDirs } from "pilotswarm/host";
import {
  APP_ROOT, initialEnv, initializeEnv, parseLocalConfig, portalEnvironment, projectName, readLocalConfig,
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
