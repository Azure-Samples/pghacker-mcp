// The Azure deployment's configuration. deploy/terraform gives the container
// the environment described in deploy/README.md; this turns it into the
// environment, user and arguments of each process the supervisor
// (scripts/container.mjs) runs. Each process gets only what it needs: no
// other process sees the administrator's password, the roles' passwords or
// oauth2-proxy's secrets, and none gets the managed identity's endpoint:
// the supervisor's Foundry proxy (scripts/lib/foundry.mjs) uses it for them.

import { readFileSync } from "node:fs";
import path from "node:path";
import { APP_ROOT, extractionEnvironment } from "./config.mjs";
import { PLACEHOLDER_KEY } from "./foundry.mjs";

export const DATA_DIR = "/var/lib/pghacker";

export const PORTS = {
  ingress: 8080,
  health: 8081,
  portal: 3001,
  indexer: 3101,
  mcp: 3102,
  mcpInternal: 3103,
  foundry: 3110,
};

/** The supervisor's Foundry proxy, which adds the managed identity's token. */
export const FOUNDRY_PROXY = `http://127.0.0.1:${PORTS.foundry}`;

/** The Unix users the processes run as (the Dockerfile creates them). */
export const USERS = {
  agent: "pgh-agent",
  services: "pgh-svc",
  proxy: "pgh-proxy",
};

/** The embedding model's dimensions, the schema's. */
const EMBED_DIM = "1536";
const EMBED_API_VERSION = "2024-10-21";

const REQUIRED = [
  "PGHACKER_DB_HOST", "PGHACKER_DB_NAME", "PGHACKER_DB_ADMIN_USER", "PGHACKER_DB_ADMIN_PASSWORD",
  "PGH_INDEXER_PASSWORD", "PGH_READER_PASSWORD", "PGH_SWARM_PASSWORD",
  "AZURE_CLIENT_ID", "PGHACKER_FOUNDRY_ENDPOINT", "PGHACKER_FOUNDRY_SOL", "PGHACKER_FOUNDRY_LUNA",
  "PGHACKER_FOUNDRY_EMBEDDING",
];

/** What serving the app needs on top of what the jobs need. */
const REQUIRED_TO_SERVE = [
  "PGHACKER_PUBLIC_URL", "OAUTH2_PROXY_CLIENT_ID", "OAUTH2_PROXY_CLIENT_SECRET", "OAUTH2_PROXY_COOKIE_SECRET",
];

/** Settings that pass through to every process when present. */
const PASS_THROUGH = ["RUST_LOG", "TZ", "LANG", "LC_ALL", "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS"];

function value(env, name) {
  const raw = env[name];
  return typeof raw === "string" && raw.trim() !== "" ? raw.trim() : undefined;
}

function integer(env, name, fallback, { min = 1, max = 1_000_000 } = {}) {
  const raw = value(env, name);
  if (raw === undefined) return fallback;
  const n = Number(raw);
  if (!Number.isInteger(n) || n < min || n > max) throw new Error(`${name} must be an integer from ${min} to ${max}.`);
  return n;
}

function list(env, name) {
  return (value(env, name) ?? "").split(",").map((item) => item.trim()).filter(Boolean);
}

/**
 * The deployment's settings from the container's environment. `serve`
 * requires what the app needs beyond the jobs: the public URL and
 * oauth2-proxy's secrets.
 */
export function readAzureConfig(env = process.env, { serve = true, root = APP_ROOT, dataDir = DATA_DIR } = {}) {
  const missing = [...REQUIRED, ...(serve ? REQUIRED_TO_SERVE : [])].filter((name) => value(env, name) === undefined);
  if (missing.length) throw new Error(`The container's environment lacks ${missing.join(", ")}.`);
  const sslmode = value(env, "PGHACKER_DB_SSLMODE") ?? "require";
  if (!["require", "prefer", "disable"].includes(sslmode)) {
    throw new Error("PGHACKER_DB_SSLMODE must be require, prefer or disable.");
  }
  const endpoint = value(env, "PGHACKER_FOUNDRY_ENDPOINT").replace(/\/+$/, "");
  if (!/^https?:\/\/[^/]+$/.test(endpoint)) {
    throw new Error("PGHACKER_FOUNDRY_ENDPOINT must be the account's origin, such as https://name.openai.azure.com.");
  }
  let publicUrl;
  if (serve) {
    publicUrl = new URL(value(env, "PGHACKER_PUBLIC_URL"));
    if (publicUrl.protocol !== "https:" || publicUrl.pathname !== "/" || publicUrl.search) {
      throw new Error("PGHACKER_PUBLIC_URL must be an https origin, such as https://name.azurefd.net.");
    }
  }
  const start = integer(env, "PGHACKER_CRAWL_START_SESSIONS", 8, { max: 64 });
  const max = integer(env, "PGHACKER_CRAWL_MAX_SESSIONS", 16, { max: 64 });
  if (start > max) throw new Error("PGHACKER_CRAWL_START_SESSIONS must not exceed PGHACKER_CRAWL_MAX_SESSIONS.");
  const role = value(env, "PGHACKER_PORTAL_ROLE") ?? "user";
  if (!["user", "admin"].includes(role)) throw new Error("PGHACKER_PORTAL_ROLE must be user or admin.");
  const github = {
    org: value(env, "PGHACKER_GITHUB_ORG"),
    team: value(env, "PGHACKER_GITHUB_TEAM"),
    users: list(env, "PGHACKER_GITHUB_USERS"),
  };
  if (serve && !github.org && !github.team && github.users.length === 0) {
    throw new Error("Set PGHACKER_GITHUB_ORG and PGHACKER_GITHUB_TEAM, or PGHACKER_GITHUB_USERS: the portal must not admit every GitHub user.");
  }
  const history = value(env, "PGHACKER_CODE_HISTORY");
  if (history !== undefined && !/^[a-z_][a-z0-9_]{0,62}$/.test(history)) {
    throw new Error("PGHACKER_CODE_HISTORY must name a schema, such as pghacker.");
  }
  return {
    root,
    dataDir,
    db: {
      host: value(env, "PGHACKER_DB_HOST"),
      port: integer(env, "PGHACKER_DB_PORT", 5432, { max: 65535 }),
      name: value(env, "PGHACKER_DB_NAME"),
      sslmode,
      admin: { user: value(env, "PGHACKER_DB_ADMIN_USER"), password: env.PGHACKER_DB_ADMIN_PASSWORD },
      passwords: { indexer: env.PGH_INDEXER_PASSWORD, reader: env.PGH_READER_PASSWORD, swarm: env.PGH_SWARM_PASSWORD },
    },
    foundry: {
      endpoint,
      // The user-assigned identity whose tokens the proxy requests.
      clientId: value(env, "AZURE_CLIENT_ID"),
      sol: value(env, "PGHACKER_FOUNDRY_SOL"),
      luna: value(env, "PGHACKER_FOUNDRY_LUNA"),
      embedding: value(env, "PGHACKER_FOUNDRY_EMBEDDING"),
    },
    public: publicUrl
      ? {
        url: publicUrl.origin,
        hosts: [...new Set([publicUrl.hostname, ...list(env, "PGHACKER_PUBLIC_HOSTS")])],
        frontDoorId: value(env, "PGHACKER_FRONT_DOOR_ID"),
      }
      : undefined,
    rate: {
      perMinute: integer(env, "PGHACKER_RATE_PER_MINUTE", 60),
      burst: integer(env, "PGHACKER_RATE_BURST", 30),
    },
    crawl: {
      enabled: value(env, "PGHACKER_CRAWL") !== "off",
      start,
      max,
      perSession: integer(env, "PGHACKER_CRAWL_PER_SESSION", 8, { max: 50 }),
    },
    // The schema whose commit history the code tools read; unset, pghacker.
    code: { history },
    // The extraction order's settings for the indexer; unset, its defaults.
    extraction: extractionEnvironment(env),
    github: {
      ...github,
      clientId: env.OAUTH2_PROXY_CLIENT_ID,
      clientSecret: env.OAUTH2_PROXY_CLIENT_SECRET,
      cookieSecret: env.OAUTH2_PROXY_COOKIE_SECRET,
    },
    portalRole: role,
    pass: Object.fromEntries(PASS_THROUGH.filter((name) => value(env, name) !== undefined).map((name) => [name, env[name]])),
  };
}

/** A role's URL on the deployment's database (or `database`). */
export function databaseUrl(config, user, password, database = config.db.name) {
  const url = new URL(`postgresql://${config.db.host}:${config.db.port}/`);
  url.username = user;
  url.password = password;
  url.pathname = `/${encodeURIComponent(database)}`;
  url.searchParams.set("sslmode", config.db.sslmode);
  return url.toString();
}

export function databaseUrls(config) {
  const { admin, passwords } = config.db;
  return {
    admin: databaseUrl(config, admin.user, admin.password),
    indexer: databaseUrl(config, "pgh_indexer", passwords.indexer),
    reader: databaseUrl(config, "pgh_reader", passwords.reader),
    swarm: databaseUrl(config, "pgh_swarm", passwords.swarm),
  };
}

/**
 * PilotSwarm's model catalog: Foundry's Sol, the default, and Luna, through
 * the proxy, which replaces the placeholder key with a token.
 */
export function foundryCatalog(foundry) {
  return {
    defaultModel: `azure-foundry:${foundry.sol}`,
    providers: [{
      id: "azure-foundry",
      type: "openai",
      baseUrl: `${FOUNDRY_PROXY}/openai/v1`,
      apiKey: "env:AZURE_OAI_KEY",
      wireApi: "responses",
      models: [
        {
          name: foundry.sol,
          description: "GPT-6.1 Sol via Azure AI Foundry: the default for conversations.",
          cost: "high",
        },
        {
          name: foundry.luna,
          description: "GPT-6 Luna via Azure AI Foundry: extraction, deduplication and evaluation.",
          cost: "medium",
        },
      ],
    }],
  };
}

/** The embeddings settings both Rust services read: the Foundry account's, through the proxy. */
export function embedEnvironment(foundry) {
  return {
    HORIZON_EMBED_URL: `${FOUNDRY_PROXY}/openai/deployments/${encodeURIComponent(foundry.embedding)}/embeddings?api-version=${EMBED_API_VERSION}`,
    HORIZON_EMBED_MODEL: foundry.embedding,
    HORIZON_EMBED_DIM: EMBED_DIM,
    HORIZON_EMBED_API_KEY: PLACEHOLDER_KEY,
  };
}

/** Where each user's files go. */
export function layout(config) {
  const data = config.dataDir;
  return {
    config: path.join(data, "config"),
    modelProviders: path.join(data, "config", "model_providers.json"),
    agent: {
      home: path.join(data, "agent", "home"),
      sessions: path.join(data, "agent", "sessions"),
      artifacts: path.join(data, "agent", "artifacts"),
      exports: path.join(data, "agent", "exports"),
      logs: path.join(data, "agent", "logs"),
    },
    services: {
      home: path.join(data, "svc", "home"),
      cache: path.join(data, "svc", "cache"),
      git: path.join(data, "svc", "git"),
    },
    proxy: { home: path.join(data, "proxy", "home") },
  };
}

function base(config, home, user) {
  return {
    PATH: "/usr/local/bin:/usr/bin:/bin",
    HOME: home,
    USER: user,
    LOGNAME: user,
    ...config.pass,
  };
}

const local = (port) => `http://127.0.0.1:${port}/mcp`;

/** Each process's environment: an allowlist, nothing inherited. */
export function processEnvironments(config) {
  const urls = databaseUrls(config);
  const files = layout(config);
  const embed = embedEnvironment(config.foundry);
  const rust = { RUST_LOG: config.pass.RUST_LOG ?? "info,tokio_postgres=warn,rmcp=warn" };
  const history = config.code?.history ? { PGHACKER_CODE_HISTORY: config.code.history } : {};
  const services = base(config, files.services.home, USERS.services);
  const agent = base(config, files.agent.home, USERS.agent);
  const crawlModel = `azure-foundry:${config.foundry.luna}`;
  // Interactive sessions get a few slots beyond the crawl's.
  const concurrency = String(config.crawl.max + 4);
  const environments = {
    bootstrap: {
      ...services,
      ...rust,
      PGHACKER_ADMIN_DATABASE_URL: urls.admin,
      PGHACKER_INDEXER_DATABASE_URL: urls.indexer,
      PGH_INDEXER_PASSWORD: config.db.passwords.indexer,
      PGH_READER_PASSWORD: config.db.passwords.reader,
      PGH_SWARM_PASSWORD: config.db.passwords.swarm,
    },
    indexer: {
      ...services,
      ...rust,
      ...embed,
      ...history,
      ...config.extraction,
      PGHACKER_INDEXER_DATABASE_URL: urls.indexer,
      PGHACKER_INDEXER_ADDR: `127.0.0.1:${PORTS.indexer}`,
      PGHACKER_SLICES: path.join(config.root, "config", "test-slices.toml"),
      PGHACKER_CACHE_DIR: files.services.cache,
      PGHACKER_GIT_DIR: files.services.git,
      PGHACKER_SOURCES: "open",
      PGHACKER_EXTRACTION: config.crawl.enabled ? "automatic" : "manual",
      PGHACKER_KEEP_DOWNLOADS: "false",
    },
    swarm: {
      ...agent,
      DATABASE_URL: urls.swarm,
      PORT: String(PORTS.portal),
      PORTAL_HOST: "127.0.0.1",
      PORTAL_AUTH_PROVIDER: "proxy",
      PORTAL_AUTH_PROXY_MODE: "header",
      PORTAL_AUTH_PROXY_TRUST_HEADERS: "true",
      PORTAL_AUTHZ_DEFAULT_ROLE: config.portalRole,
      PORTAL_AUTH_ALLOW_UNAUTHENTICATED: "false",
      WORKERS: "0",
      PORTAL_TUI_MODE: "local",
      PLUGIN_DIRS: path.join(config.root, "plugin"),
      SESSION_STATE_DIR: files.agent.sessions,
      ARTIFACT_DIR: files.agent.artifacts,
      PILOTSWARM_EXPORT_DIR: files.agent.exports,
      PILOTSWARM_LOG_DIR: files.agent.logs,
      PS_MODEL_PROVIDERS_PATH: files.modelProviders,
      PILOTSWARM_WORKER_CONCURRENCY: concurrency,
      PILOTSWARM_ORCHESTRATION_CONCURRENCY: concurrency,
      AZURE_OAI_KEY: PLACEHOLDER_KEY,
      PGHACKER_INDEXER_MCP_URL: local(PORTS.indexer),
      // Agents use pghacker-mcp's listener without the public rate limit.
      PGHACKER_MCP_URL: local(PORTS.mcpInternal),
      // oauth2-proxy forwards the user's GitHub groups in a header.
      NODE_OPTIONS: "--max-http-header-size=65536",
    },
    crawl: {
      ...agent,
      PGHACKER_DEPLOYMENT: "azure",
      DATABASE_URL: urls.swarm,
      PS_MODEL_PROVIDERS_PATH: files.modelProviders,
      AZURE_OAI_KEY: PLACEHOLDER_KEY,
      PGHACKER_INDEXER_MCP_URL: local(PORTS.indexer),
      PGHACKER_FOUNDRY_ENDPOINT: FOUNDRY_PROXY,
      PGHACKER_FOUNDRY_LUNA: config.foundry.luna,
      PGHACKER_CRAWL_MODEL: crawlModel,
      PGHACKER_CRAWL_START_SESSIONS: String(config.crawl.start),
      PGHACKER_CRAWL_MAX_SESSIONS: String(config.crawl.max),
      PGHACKER_CRAWL_PER_SESSION: String(config.crawl.perSession),
    },
  };
  if (config.public) {
    environments.mcp = {
      ...services,
      ...rust,
      ...embed,
      ...history,
      PGHACKER_READER_DATABASE_URL: urls.reader,
      PGHACKER_MCP_ADDR: `127.0.0.1:${PORTS.mcp}`,
      PGHACKER_MCP_INTERNAL_ADDR: `127.0.0.1:${PORTS.mcpInternal}`,
      PGHACKER_ALLOWED_HOSTS: config.public.hosts.join(","),
      PGHACKER_CLIENT_IP_HEADER: "x-azure-clientip",
      PGHACKER_RATE_PER_MINUTE: String(config.rate.perMinute),
      PGHACKER_RATE_BURST: String(config.rate.burst),
      ...(config.public.frontDoorId ? { PGHACKER_FRONT_DOOR_ID: config.public.frontDoorId } : {}),
    };
    environments.proxy = {
      ...base(config, files.proxy.home, USERS.proxy),
      OAUTH2_PROXY_CLIENT_ID: config.github.clientId,
      OAUTH2_PROXY_CLIENT_SECRET: config.github.clientSecret,
      OAUTH2_PROXY_COOKIE_SECRET: config.github.cookieSecret,
    };
  }
  return environments;
}

/**
 * oauth2-proxy's arguments: GitHub sign-in for the portal, restricted to
 * the team (or the listed users); /mcp passes through without it. Secrets
 * come from its environment, never its arguments, which any user can read.
 */
export function oauth2ProxyArgs(config) {
  const { github } = config;
  return [
    `--http-address=0.0.0.0:${PORTS.ingress}`,
    "--provider=github",
    ...(github.org ? [`--github-org=${github.org}`] : []),
    ...(github.team ? [`--github-team=${github.team}`] : []),
    ...(github.users.length ? [`--github-user=${github.users.join(",")}`] : []),
    "--email-domain=*",
    `--upstream=http://127.0.0.1:${PORTS.mcp}/mcp`,
    `--upstream=http://127.0.0.1:${PORTS.portal}/`,
    "--skip-auth-route=^/mcp$",
    "--api-route=^/api/",
    `--redirect-url=${config.public.url}/oauth2/callback`,
    `--whitelist-domain=${new URL(config.public.url).hostname}`,
    "--reverse-proxy=true",
    "--cookie-name=_pghacker_auth",
    "--cookie-secure=true",
    "--cookie-samesite=lax",
    "--cookie-expire=12h",
    "--skip-provider-button=true",
    "--pass-user-headers=true",
    "--upstream-timeout=240s",
    "--silence-ping-logging=true",
    "--request-logging=false",
    "--footer=-",
  ];
}

/** A user's uid, gid and home from /etc/passwd. */
export function lookupUser(name, passwd = readFileSync("/etc/passwd", "utf8")) {
  for (const line of passwd.split("\n")) {
    const [user, , uid, gid, , home] = line.split(":");
    if (user === name) return { name, uid: Number(uid), gid: Number(gid), home };
  }
  throw new Error(`The image has no user ${name}.`);
}
