import { createHash, randomBytes } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseEnv } from "node:util";

export const APP_ROOT = fileURLToPath(new URL("../../", import.meta.url));
export const APP_NAME = "pghacker-mcp";
export const LOCAL_HOST = "127.0.0.1";

const LOCAL_SETTINGS = {
  PORTAL_HOST: LOCAL_HOST,
  PORTAL_AUTH_PROVIDER: "none",
  PORTAL_AUTHZ_DEFAULT_ROLE: "admin",
  PORTAL_AUTH_ALLOW_UNAUTHENTICATED: "true",
  WORKERS: "0",
  PORTAL_TUI_MODE: "local",
};

const PLATFORM_ENV_KEYS = [
  "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "TMP", "TEMP",
  "LANG", "LC_ALL", "LC_CTYPE", "TERM", "COLORTERM",
  "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS",
  "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "no_proxy",
  "DOCKER_CONFIG",
];

export function projectName(root = APP_ROOT) {
  const suffix = createHash("sha256").update(path.resolve(root)).digest("hex").slice(0, 8);
  return `${APP_NAME}-${suffix}`;
}

export function initialEnv(password = randomBytes(32).toString("hex")) {
  return [
    `POSTGRES_USER=pghacker`,
    `POSTGRES_DB=pghacker_mcp`,
    `POSTGRES_PASSWORD=${password}`,
    `POSTGRES_PORT=55433`,
    `DATABASE_URL=postgresql://pghacker:${encodeURIComponent(password)}@${LOCAL_HOST}:55433/pghacker_mcp`,
    "",
    "PORTAL_PORT=3001",
    ...Object.entries(LOCAL_SETTINGS).map(([key, value]) => `${key}=${value}`),
    "",
  ].join("\n");
}

export function initializeEnv(root = APP_ROOT) {
  try {
    writeFileSync(path.join(root, ".env"), initialEnv(), { flag: "wx", mode: 0o600 });
    return true;
  } catch (error) {
    if (error.code === "EEXIST") return false;
    throw error;
  }
}

function required(values, name) {
  const value = values[name];
  if (!value) throw new Error(`.env must define ${name}. Run npm run setup first.`);
  return value;
}

function portNumber(values, name) {
  const value = required(values, name);
  if (!/^\d+$/.test(value) || Number(value) < 1 || Number(value) > 65535) {
    throw new Error(`${name} must be an integer between 1 and 65535.`);
  }
  return Number(value);
}

export function parseLocalConfig(text, root = APP_ROOT) {
  const values = parseEnv(text);
  for (const [key, expected] of Object.entries(LOCAL_SETTINGS)) {
    if (required(values, key) !== expected) {
      throw new Error(`${key} must be ${expected} for this local-only starter.`);
    }
  }

  const postgresPort = portNumber(values, "POSTGRES_PORT");
  const portalPort = portNumber(values, "PORTAL_PORT");
  if (postgresPort === portalPort) throw new Error("PostgreSQL and the portal need different ports.");

  const postgresUser = required(values, "POSTGRES_USER");
  const postgresDatabase = required(values, "POSTGRES_DB");
  for (const [name, value] of [["POSTGRES_USER", postgresUser], ["POSTGRES_DB", postgresDatabase]]) {
    if (!/^[a-z][a-z0-9_]{0,62}$/.test(value)) {
      throw new Error(`${name} must be a lowercase PostgreSQL identifier.`);
    }
  }
  const postgresPassword = required(values, "POSTGRES_PASSWORD");
  if (postgresPassword.length < 16 || postgresPassword === "replace-with-a-generated-local-password") {
    throw new Error("POSTGRES_PASSWORD must be a real local password of at least 16 characters.");
  }

  const databaseUrl = required(values, "DATABASE_URL");
  let database;
  try {
    database = new URL(databaseUrl);
    if (decodeURIComponent(database.username) !== postgresUser
      || decodeURIComponent(database.password) !== postgresPassword
      || decodeURIComponent(database.pathname) !== `/${postgresDatabase}`) {
      throw new Error("Credentials differ.");
    }
  } catch {
    throw new Error("DATABASE_URL must match the PostgreSQL credentials in .env.");
  }
  if (!["postgres:", "postgresql:"].includes(database.protocol)
    || database.hostname !== LOCAL_HOST
    || Number(database.port) !== postgresPort
    || database.search || database.hash) {
    throw new Error("DATABASE_URL must point only to this app's PostgreSQL port on 127.0.0.1, without query parameters.");
  }

  return {
    root: path.resolve(root),
    project: projectName(root),
    runtimeDir: path.join(path.resolve(root), ".runtime"),
    databaseUrl,
    postgresPort,
    portalPort,
    values,
  };
}

export function readLocalConfig(root = APP_ROOT) {
  let text;
  try {
    text = readFileSync(path.join(root, ".env"), "utf8");
  } catch (error) {
    if (error.code === "ENOENT") throw new Error(".env is missing. Run npm run setup first.");
    throw error;
  }
  return parseLocalConfig(text, root);
}

export function platformEnvironment(inherited = process.env) {
  return Object.fromEntries(
    PLATFORM_ENV_KEYS.filter((key) => inherited[key] !== undefined).map((key) => [key, inherited[key]]),
  );
}

export function portalEnvironment(config, inherited = process.env) {
  const env = {
    ...platformEnvironment(inherited),
    ...LOCAL_SETTINGS,
    DATABASE_URL: config.databaseUrl,
    PORT: String(config.portalPort),
    PLUGIN_DIRS: path.join(config.root, "plugin"),
    SESSION_STATE_DIR: path.join(config.runtimeDir, "sessions"),
    ARTIFACT_DIR: path.join(config.runtimeDir, "artifacts"),
    PILOTSWARM_EXPORT_DIR: path.join(config.runtimeDir, "exports"),
    PILOTSWARM_LOG_DIR: path.join(config.runtimeDir, "logs"),
  };
  const token = config.values.GITHUB_TOKEN || config.values.GH_TOKEN
    || inherited.GITHUB_TOKEN || inherited.GH_TOKEN;
  if (token) env.GITHUB_TOKEN = token;
  return env;
}
