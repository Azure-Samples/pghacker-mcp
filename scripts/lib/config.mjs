import { createHash, randomBytes } from "node:crypto";
import { appendFileSync, readFileSync, writeFileSync } from "node:fs";
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

const secret = () => randomBytes(32).toString("hex");

export function initialEnv(password = secret(), rolePasswords = { indexer: secret(), reader: secret() }) {
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
    ...roleLines(rolePasswords),
    "",
  ].join("\n");
}

function roleLines({ indexer, reader }) {
  return [
    "# pghacker roles: pgh_indexer writes schema pghacker and the graph; pgh_reader serves pghacker-mcp.",
    `PGH_INDEXER_PASSWORD=${indexer}`,
    `PGH_READER_PASSWORD=${reader}`,
  ];
}

/** Adds role passwords to an .env from before they existed; returns whether it did. */
export function ensureRolePasswords(root = APP_ROOT) {
  const file = path.join(root, ".env");
  const values = parseEnv(readFileSync(file, "utf8"));
  if (values.PGH_INDEXER_PASSWORD && values.PGH_READER_PASSWORD) return false;
  appendFileSync(file, `\n${roleLines({ indexer: secret(), reader: secret() }).join("\n")}\n`, { mode: 0o600 });
  return true;
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
  const indexerPort = values.PGHACKER_INDEXER_PORT ? portNumber(values, "PGHACKER_INDEXER_PORT") : 3101;
  const mcpPort = values.PGHACKER_MCP_PORT ? portNumber(values, "PGHACKER_MCP_PORT") : 3102;
  // PORTAL_PORT is the site's: the home page, the portal and /mcp, through
  // the edge (scripts/lib/edge.mjs). PilotSwarm's own listener is behind it.
  const portalInternalPort = values.PGHACKER_PORTAL_INTERNAL_PORT
    ? portNumber(values, "PGHACKER_PORTAL_INTERNAL_PORT") : 3002;
  if (new Set([postgresPort, portalPort, portalInternalPort, indexerPort, mcpPort]).size !== 5) {
    throw new Error("PostgreSQL, the site, PilotSwarm's portal, pghacker-indexer and pghacker-mcp need different ports.");
  }
  for (const name of ["PGH_INDEXER_PASSWORD", "PGH_READER_PASSWORD"]) {
    const value = required(values, name);
    if (value.length < 16) throw new Error(`${name} must be a real local password of at least 16 characters.`);
  }

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
    portalInternalPort,
    indexerPort,
    mcpPort,
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
    PORT: String(config.portalInternalPort),
    PLUGIN_DIRS: path.join(config.root, "plugin"),
    SESSION_STATE_DIR: path.join(config.runtimeDir, "sessions"),
    ARTIFACT_DIR: path.join(config.runtimeDir, "artifacts"),
    PILOTSWARM_EXPORT_DIR: path.join(config.runtimeDir, "exports"),
    PILOTSWARM_LOG_DIR: path.join(config.runtimeDir, "logs"),
    PS_MODEL_PROVIDERS_PATH: path.join(config.root, "config", "model_providers.json"),
    // PilotSwarm runs 2 session turns at a time by default; crawls run 8 extractors.
    PILOTSWARM_WORKER_CONCURRENCY: concurrency(config.values.PILOTSWARM_WORKER_CONCURRENCY),
    PILOTSWARM_ORCHESTRATION_CONCURRENCY: concurrency(config.values.PILOTSWARM_ORCHESTRATION_CONCURRENCY),
  };
  const token = config.values.GITHUB_TOKEN || config.values.GH_TOKEN
    || inherited.GITHUB_TOKEN || inherited.GH_TOKEN;
  if (token) env.GITHUB_TOKEN = token;
  env.PGHACKER_INDEXER_MCP_URL = serviceUrl(config.indexerPort);
  // PGHACKER_AGENT_MCP_URL points the local agents at another pghacker-mcp,
  // such as a deployment's public endpoint, for an evaluation of it.
  env.PGHACKER_MCP_URL = config.values.PGHACKER_AGENT_MCP_URL || inherited.PGHACKER_AGENT_MCP_URL
    || serviceUrl(config.mcpPort);
  return env;
}

function concurrency(value) {
  const n = Number(value);
  return Number.isInteger(n) && n >= 1 && n <= 64 ? String(n) : "8";
}

export function serviceUrl(port) {
  return `http://${LOCAL_HOST}:${port}/mcp`;
}

/** A role's URL on this app's database. */
export function roleDatabaseUrl(config, role, password) {
  const url = new URL(config.databaseUrl);
  url.username = role;
  url.password = password;
  return url.toString();
}

const EMBED_KEYS = [
  "HORIZON_EMBED_URL", "HORIZON_EMBED_MODEL", "HORIZON_EMBED_DIM",
  "HORIZON_EMBED_API_KEY", "HORIZON_EMBED_API_KEY_HEADER", "HORIZON_EMBED_BEARER",
];

function embedEnvironment(config, inherited) {
  return Object.fromEntries(EMBED_KEYS
    .map((key) => [key, config.values[key] ?? inherited[key]])
    .filter(([, value]) => value !== undefined && value !== ""));
}

/**
 * PGHACKER_CODE_HISTORY, from .env or the shell: the schema whose commit
 * history the code tools read, for the indexer and pghacker-mcp alike, such
 * as a build's before it is installed. Unset, they read pghacker's.
 */
export function codeHistoryEnvironment(config, inherited = process.env) {
  const schema = String(config.values.PGHACKER_CODE_HISTORY ?? inherited.PGHACKER_CODE_HISTORY ?? "").trim();
  if (!schema) return {};
  if (!/^[a-z_][a-z0-9_]{0,62}$/.test(schema)) {
    throw new Error("PGHACKER_CODE_HISTORY must name a schema, such as pghacker.");
  }
  return { PGHACKER_CODE_HISTORY: schema };
}

const EXTRACTION_SETTINGS = {
  PGHACKER_EXTRACTION_ORDER: (v) => ["lanes", "largest"].includes(v) || "must be lanes or largest",
  PGHACKER_EXTRACTION_NEW_MAIL_SHARE: (v) => (Number(v) > 0 && Number(v) <= 1) || "must be a number above 0, up to 1",
  PGHACKER_EXTRACTION_NEW_MAIL_DAYS: (v) => (/^\d+$/.test(v) && Number(v) >= 1 && Number(v) <= 3650) ||
    "must be a whole number of days from 1 to 3650",
};

/**
 * The extraction order's settings for the indexer, each from the first of
 * `sources` that sets it (.env, then the shell; the container's environment
 * in Azure): PGHACKER_EXTRACTION_ORDER, lanes or largest (the order before
 * the lanes); PGHACKER_EXTRACTION_NEW_MAIL_SHARE, new mail's share of the
 * threads leased while the backfill has threads too; and
 * PGHACKER_EXTRACTION_NEW_MAIL_DAYS, how recent a thread's latest message
 * must be for the thread to be new mail. Unset, the indexer's defaults
 * apply: lanes, 0.5 and 7.
 */
export function extractionEnvironment(...sources) {
  const environment = {};
  for (const [name, check] of Object.entries(EXTRACTION_SETTINGS)) {
    const value = sources.map((source) => String(source?.[name] ?? "").trim()).find((v) => v !== "");
    if (value === undefined) continue;
    const verdict = check(value);
    if (verdict !== true) throw new Error(`${name} ${verdict}.`);
    environment[name] = value;
  }
  return environment;
}

/**
 * The environments of the Rust services, each with only what it needs:
 * bootstrap gets the administrator's URL; the indexer pgh_indexer's; pghacker-mcp pgh_reader's.
 */
export function serviceEnvironments(config, inherited = process.env) {
  const base = {
    ...platformEnvironment(inherited),
    RUST_LOG: inherited.RUST_LOG || "info,tokio_postgres=warn,rmcp=warn",
  };
  const indexerUrl = roleDatabaseUrl(config, "pgh_indexer", config.values.PGH_INDEXER_PASSWORD);
  const readerUrl = roleDatabaseUrl(config, "pgh_reader", config.values.PGH_READER_PASSWORD);
  const embed = embedEnvironment(config, inherited);
  const history = codeHistoryEnvironment(config, inherited);
  const extraction = extractionEnvironment(config.values, inherited);
  const runtime = (name) => path.join(config.runtimeDir, name);
  return {
    bootstrap: {
      ...base,
      PGHACKER_ADMIN_DATABASE_URL: config.databaseUrl,
      PGHACKER_INDEXER_DATABASE_URL: indexerUrl,
      PGH_INDEXER_PASSWORD: config.values.PGH_INDEXER_PASSWORD,
      PGH_READER_PASSWORD: config.values.PGH_READER_PASSWORD,
    },
    indexer: {
      ...base,
      ...embed,
      ...history,
      ...extraction,
      PGHACKER_INDEXER_DATABASE_URL: indexerUrl,
      PGHACKER_INDEXER_ADDR: `${LOCAL_HOST}:${config.indexerPort}`,
      PGHACKER_SLICES: path.join(config.root, "config", "test-slices.toml"),
      PGHACKER_CACHE_DIR: runtime("cache"),
      PGHACKER_GIT_DIR: runtime("git"),
    },
    mcp: {
      ...base,
      ...embed,
      ...history,
      PGHACKER_READER_DATABASE_URL: readerUrl,
      PGHACKER_MCP_ADDR: `${LOCAL_HOST}:${config.mcpPort}`,
      // Every local agent and script reaches pghacker-mcp from 127.0.0.1 and
      // shares one rate-limit bucket, sized for the server's public clients.
      PGHACKER_RATE_PER_MINUTE: config.values.PGHACKER_RATE_PER_MINUTE || "3000",
      PGHACKER_RATE_BURST: config.values.PGHACKER_RATE_BURST || "500",
    },
  };
}
