// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The Azure deployment's configuration, on AKS (deploy/README.md). Terraform's
// `kubernetes` output carries the app's settings (app_settings: the
// environment the deployment's README describes, without its secrets). The
// renderer (scripts/lib/aks.mjs) reads them with readAzureConfig and gives
// each component the environment that componentEnvironments makes: only the
// settings it uses, and its secrets as references to the Kubernetes Secret
// that its SecretProviderClass syncs from Key Vault, never as values. A
// database URL names its password as $(VAR), which Kubernetes expands from
// that Secret. No component but the token proxy (scripts/token-proxy.mjs) has
// an Azure identity. The ops jobs (scripts/probe.mjs, corpus-restore.mjs,
// history-build.mjs) read the same settings, with their secrets, from their
// own environment.

import path from "node:path";
import { APP_ROOT, extractionEnvironment } from "./config.mjs";
import { parseCopyright, parsePrivacyUrl } from "./edge.mjs";
import { PLACEHOLDER_KEY } from "./foundry.mjs";
import { parseConnectionString } from "./monitor.mjs";
import { oauth2ProxyRouteArgs } from "./routes.mjs";

/** Each pod's writable directory, an emptyDir; its HOME too. */
export const DATA_DIR = "/var/lib/pghacker";

/** Where the model catalog's ConfigMap is mounted. */
export const CATALOG_DIR = "/etc/pghacker";

export const PORTS = {
  ingress: 8080,
  health: 8081,
  portal: 3001,
  indexer: 3101,
  mcp: 3102,
  mcpInternal: 3103,
  home: 3104,
  foundry: 3110,
  monitor: 3111,
};

/** The token proxy on loopback: its default origin, as before the cluster. */
export const FOUNDRY_PROXY = `http://127.0.0.1:${PORTS.foundry}`;

/** The users the Dockerfile creates, which the pods run as. */
export const USERS = {
  // PilotSwarm's portal and worker, the crawl scheduler, the egress check.
  agent: 10001,
  // pghacker-indexer, pghacker-mcp, the token proxy and the ops jobs.
  services: 10002,
  // oauth2-proxy.
  proxy: 10003,
  // The home page's server.
  home: 10004,
};

/** The Services that components reach each other by. */
export const SERVICES = {
  edge: "edge",
  home: "home",
  mcp: "mcp",
  indexer: "indexer",
  tokenProxy: "token-proxy",
  portal: "portal",
};

/**
 * Each secret's name in Key Vault, by the environment variable that holds it.
 * Terraform grants each workload identity its secrets one by one
 * (deploy/terraform/identity.tf), as componentSecrets lists them.
 */
export const VAULT_SECRETS = {
  PGHACKER_DB_ADMIN_PASSWORD: "db-admin-password",
  PGH_INDEXER_PASSWORD: "pgh-indexer-password",
  PGH_READER_PASSWORD: "pgh-reader-password",
  PGH_SWARM_PASSWORD: "pgh-swarm-password",
  OAUTH2_PROXY_CLIENT_ID: "github-client-id",
  OAUTH2_PROXY_CLIENT_SECRET: "github-client-secret",
  OAUTH2_PROXY_COOKIE_SECRET: "oauth2-cookie-secret",
  PGHACKER_USAGE_ID_KEY: "usage-id-key",
};

/** The Kubernetes Secret that an identity's SecretProviderClass syncs. */
export const secretName = (identity) => `${identity}-secrets`;

/** The embedding model's dimensions, the schema's. */
const EMBED_DIM = "1536";
const EMBED_API_VERSION = "2024-10-21";

/** The settings every reader needs. */
const REQUIRED = [
  "PGHACKER_DB_HOST", "PGHACKER_DB_NAME", "PGHACKER_DB_ADMIN_USER",
  "PGHACKER_FOUNDRY_ENDPOINT", "PGHACKER_FOUNDRY_SOL", "PGHACKER_FOUNDRY_LUNA", "PGHACKER_FOUNDRY_EMBEDDING",
];

/** The secrets the jobs need. */
const REQUIRED_SECRETS = [
  "PGHACKER_DB_ADMIN_PASSWORD", "PGH_INDEXER_PASSWORD", "PGH_READER_PASSWORD", "PGH_SWARM_PASSWORD",
];

/** What serving the app needs on top of what the jobs need. */
const REQUIRED_TO_SERVE = ["PGHACKER_PUBLIC_URL", "PGHACKER_PORTAL_URL"];

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

/** A number from 0 to 1, such as a chance; `fallback` when unset. */
function fraction(env, name, fallback) {
  const raw = value(env, name);
  if (raw === undefined) return fallback;
  const n = Number(raw);
  if (!Number.isFinite(n) || n < 0 || n > 1) throw new Error(`${name} must be a number from 0 to 1, such as 0.1.`);
  return n;
}

/** true or false; `fallback` when unset. */
function flag(env, name, fallback) {
  const raw = value(env, name);
  if (raw === undefined) return fallback;
  if (raw !== "true" && raw !== "false") throw new Error(`${name} must be true or false.`);
  return raw === "true";
}

function list(env, name) {
  return (value(env, name) ?? "").split(",").map((item) => item.trim()).filter(Boolean);
}

/**
 * Feedback's settings: the chance that an address's hour of use brings a
 * request for it, whether the form takes a comment, the pauses (seconds) over
 * which a call starts a question and under which a session stays active, the
 * questions before the first ask, the apps asked, pghacker-mcp's own list
 * when unset, and debug mode, which asks at every question after a client's
 * first.
 */
function feedbackSettings(env) {
  const questionGap = integer(env, "PGHACKER_FEEDBACK_QUESTION_GAP", 180, { min: 1, max: 3600 });
  const sessionGap = integer(env, "PGHACKER_FEEDBACK_SESSION_GAP", 600, { min: 2, max: 86_400 });
  if (sessionGap <= questionGap) {
    throw new Error("PGHACKER_FEEDBACK_SESSION_GAP must be more than PGHACKER_FEEDBACK_QUESTION_GAP.");
  }
  const clients = list(env, "PGHACKER_FEEDBACK_CLIENTS");
  return {
    rate: fraction(env, "PGHACKER_FEEDBACK_RATE", 0),
    comments: flag(env, "PGHACKER_FEEDBACK_COMMENTS", false),
    questionGap,
    sessionGap,
    minQuestions: integer(env, "PGHACKER_FEEDBACK_MIN_QUESTIONS", 3, { min: 0, max: 100 }),
    ...(clients.length > 0 ? { clients } : {}),
    debug: flag(env, "PGHACKER_FEEDBACK_DEBUG", false),
  };
}

/** The URL of the deployment's privacy statement; undefined when it has none. */
function privacyUrl(env, name) {
  const raw = value(env, name);
  if (raw === undefined) return undefined;
  const url = parsePrivacyUrl(raw);
  if (url === undefined) throw new Error(`${name} must be an https URL with a host, such as https://example.com/privacy.`);
  return url;
}

/**
 * The deployment's copyright link, as { url, text }: PGHACKER_COPYRIGHT_URL
 * and PGHACKER_COPYRIGHT_TEXT, both or neither; undefined when it has none.
 */
function copyrightLink(env) {
  const url = value(env, "PGHACKER_COPYRIGHT_URL");
  const text = value(env, "PGHACKER_COPYRIGHT_TEXT");
  if (url === undefined && text === undefined) return undefined;
  const link = parseCopyright({ url, text });
  if (link === undefined) {
    throw new Error(
      "PGHACKER_COPYRIGHT_URL and PGHACKER_COPYRIGHT_TEXT must be set together: an https URL, such as https://example.com/, and the link's text, such as © 2026 Example, of at most 100 characters without control characters.",
    );
  }
  return link;
}

function httpsOrigin(env, name) {
  const url = new URL(value(env, name));
  if (url.protocol !== "https:" || url.pathname !== "/" || url.search) {
    throw new Error(`${name} must be an https origin, such as https://name.azurefd.net.`);
  }
  return url;
}

/** An http(s) origin, such as a Service's; `fallback` when unset. */
function origin(env, name, fallback) {
  const raw = value(env, name);
  if (raw === undefined) return fallback;
  const url = new URL(raw);
  if (!["http:", "https:"].includes(url.protocol) || url.pathname !== "/" || url.search) {
    throw new Error(`${name} must be an http origin, such as http://token-proxy.pghacker.svc.cluster.local:3110.`);
  }
  return url.origin;
}

/**
 * The deployment's settings, from the app's settings (`secrets: false`, as
 * the renderer reads Terraform's output) or a job's environment, with its
 * secrets. `serve` requires what the app needs beyond the jobs: the public
 * and portal URLs.
 */
export function readAzureConfig(env = process.env, { serve = true, secrets = true, root = APP_ROOT, dataDir = DATA_DIR } = {}) {
  const required = [...REQUIRED, ...(secrets ? REQUIRED_SECRETS : []), ...(serve ? REQUIRED_TO_SERVE : [])];
  const missing = required.filter((name) => value(env, name) === undefined);
  if (missing.length) throw new Error(`The settings lack ${missing.join(", ")}.`);
  const sslmode = value(env, "PGHACKER_DB_SSLMODE") ?? "require";
  if (!["require", "prefer", "disable"].includes(sslmode)) {
    throw new Error("PGHACKER_DB_SSLMODE must be require, prefer or disable.");
  }
  const endpoint = value(env, "PGHACKER_FOUNDRY_ENDPOINT").replace(/\/+$/, "");
  if (!/^https?:\/\/[^/]+$/.test(endpoint)) {
    throw new Error("PGHACKER_FOUNDRY_ENDPOINT must be the account's origin, such as https://name.openai.azure.com.");
  }
  let publicUrl;
  let portalUrl;
  if (serve) {
    publicUrl = httpsOrigin(env, "PGHACKER_PUBLIC_URL");
    portalUrl = httpsOrigin(env, "PGHACKER_PORTAL_URL");
  }
  const start = integer(env, "PGHACKER_CRAWL_START_SESSIONS", 8, { max: 64 });
  const max = integer(env, "PGHACKER_CRAWL_MAX_SESSIONS", 16, { max: 64 });
  if (start > max) throw new Error("PGHACKER_CRAWL_START_SESSIONS must not exceed PGHACKER_CRAWL_MAX_SESSIONS.");
  const role = value(env, "PGHACKER_PORTAL_ROLE") ?? "user";
  if (!["user", "admin"].includes(role)) throw new Error("PGHACKER_PORTAL_ROLE must be user or admin.");
  // Who may sign in to the portal, whose agents have a shell. Terraform
  // requires github_org and github_team and refuses their placeholders; here,
  // at least one of the three must be set.
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
  // Usage telemetry, when the app has Application Insights: its resource,
  // the key of the users' keyed hashes (a secret: only a component's own
  // environment has it), and whether pghacker-mcp records what users type
  // into tools.
  const connectionString = value(env, "APPLICATIONINSIGHTS_CONNECTION_STRING");
  let usage;
  if (serve && connectionString !== undefined) {
    const idKey = value(env, "PGHACKER_USAGE_ID_KEY");
    if (secrets && (idKey === undefined || [...idKey].length < 32)) {
      throw new Error("APPLICATIONINSIGHTS_CONNECTION_STRING needs PGHACKER_USAGE_ID_KEY, of at least 32 characters.");
    }
    usage = {
      ...parseConnectionString(connectionString),
      connectionString,
      ...(secrets ? { idKey } : {}),
      revision: value(env, "CONTAINER_APP_REVISION"),
      details: flag(env, "PGHACKER_USAGE_DETAILS", false),
    };
  }
  // Feedback, which records with usage telemetry, so only with it.
  const feedback = usage ? feedbackSettings(env) : undefined;
  return {
    root,
    dataDir,
    db: {
      host: value(env, "PGHACKER_DB_HOST"),
      port: integer(env, "PGHACKER_DB_PORT", 5432, { max: 65535 }),
      name: value(env, "PGHACKER_DB_NAME"),
      sslmode,
      admin: { user: value(env, "PGHACKER_DB_ADMIN_USER"), ...(secrets ? { password: env.PGHACKER_DB_ADMIN_PASSWORD } : {}) },
      ...(secrets
        ? { passwords: { indexer: env.PGH_INDEXER_PASSWORD, reader: env.PGH_READER_PASSWORD, swarm: env.PGH_SWARM_PASSWORD } }
        : {}),
    },
    foundry: {
      endpoint,
      sol: value(env, "PGHACKER_FOUNDRY_SOL"),
      luna: value(env, "PGHACKER_FOUNDRY_LUNA"),
      embedding: value(env, "PGHACKER_FOUNDRY_EMBEDDING"),
      // The token proxy, which adds the identity's token to each request.
      proxy: origin(env, "PGHACKER_FOUNDRY_PROXY_URL", FOUNDRY_PROXY),
    },
    public: publicUrl
      ? {
        url: publicUrl.origin,
        // Front Door's own host, the only one that serves the portal.
        portalUrl: portalUrl.origin,
        hosts: [...new Set([publicUrl.hostname, portalUrl.hostname, ...list(env, "PGHACKER_PUBLIC_HOSTS")])],
        frontDoorId: value(env, "PGHACKER_FRONT_DOOR_ID"),
        // The home page's footer shows these; without them, it has no such links.
        privacyUrl: privacyUrl(env, "PGHACKER_PRIVACY_URL"),
        copyright: copyrightLink(env),
      }
      : undefined,
    rate: {
      perMinute: integer(env, "PGHACKER_RATE_PER_MINUTE", 60),
      burst: integer(env, "PGHACKER_RATE_BURST", 30),
    },
    // The calls a client address may run at once on the public listener.
    callsPerClient: integer(env, "PGHACKER_MCP_CALLS_PER_CLIENT", 10, { max: 1000 }),
    // The internal listener's, which the agents share: they all connect from
    // one address.
    internalRate: {
      perMinute: integer(env, "PGHACKER_INTERNAL_RATE_PER_MINUTE", 300),
      burst: integer(env, "PGHACKER_INTERNAL_RATE_BURST", 60),
    },
    // pgh_reader connections: every tool but query, and query.
    pools: {
      mcp: integer(env, "PGHACKER_MCP_POOL", 16, { max: 128 }),
      query: integer(env, "PGHACKER_QUERY_POOL", 8, { max: 128 }),
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
    github,
    portalRole: role,
    usage,
    feedback,
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

/** Every role's URL: the jobs', which have the secrets. */
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
 * A role's URL whose password is the environment variable `variable`, which
 * Kubernetes expands from the Secret it references. Terraform generates the
 * passwords alphanumeric, so they need no escaping in a URL.
 */
function databaseUrlFrom(config, user, variable, database = config.db.name) {
  const { host, port, sslmode } = config.db;
  return `postgresql://${user}:$(${variable})@${host}:${port}/${encodeURIComponent(database)}?sslmode=${sslmode}`;
}

/**
 * The environment of pghacker-indexer's bootstrap and history commands, which
 * the jobs run: the administrator's URL and the roles' passwords, with the
 * job's own PATH and HOME.
 */
export function bootstrapEnvironment(config, env = process.env) {
  const urls = databaseUrls(config);
  return {
    PATH: env.PATH ?? "/usr/local/bin:/usr/bin:/bin",
    ...(env.HOME ? { HOME: env.HOME } : {}),
    ...config.pass,
    RUST_LOG: config.pass.RUST_LOG ?? "info,tokio_postgres=warn,rmcp=warn",
    PGHACKER_ADMIN_DATABASE_URL: urls.admin,
    PGHACKER_INDEXER_DATABASE_URL: urls.indexer,
    PGH_INDEXER_PASSWORD: config.db.passwords.indexer,
    PGH_READER_PASSWORD: config.db.passwords.reader,
    PGH_SWARM_PASSWORD: config.db.passwords.swarm,
  };
}

/**
 * PilotSwarm's model catalog: Foundry's Sol, the default, and Luna, through
 * the token proxy at `proxy`, which replaces the placeholder key with a token.
 */
export function foundryCatalog(foundry, proxy = foundry.proxy) {
  return {
    defaultModel: `azure-foundry:${foundry.sol}`,
    providers: [{
      id: "azure-foundry",
      type: "openai",
      baseUrl: `${proxy}/openai/v1`,
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

/** The embeddings settings both Rust services read: the Foundry account's, through the token proxy. */
export function embedEnvironment(foundry, proxy = foundry.proxy) {
  return {
    HORIZON_EMBED_URL: `${proxy}/openai/deployments/${encodeURIComponent(foundry.embedding)}/embeddings?api-version=${EMBED_API_VERSION}`,
    HORIZON_EMBED_MODEL: foundry.embedding,
    HORIZON_EMBED_DIM: EMBED_DIM,
    HORIZON_EMBED_API_KEY: PLACEHOLDER_KEY,
  };
}

/** The names a Service answers to in-cluster, which listeners check Host against. */
export function serviceHosts(name, namespace) {
  return [name, `${name}.${namespace}`, `${name}.${namespace}.svc`, `${name}.${namespace}.svc.cluster.local`];
}

/** The http origin of `service` on `port`. */
export function serviceOrigin(service, port, namespace) {
  return `http://${service}.${namespace}.svc.cluster.local:${port}`;
}

/**
 * The secrets each workload identity reads, by environment variable: what its
 * SecretProviderClass syncs and Terraform grants it (deploy/terraform/identity.tf).
 * Only the ops jobs read the administrator's password.
 */
export function componentSecrets(config) {
  const usage = config.usage ? ["PGHACKER_USAGE_ID_KEY"] : [];
  return {
    indexer: ["PGH_INDEXER_PASSWORD"],
    mcp: ["PGH_READER_PASSWORD", ...usage],
    swarm: ["PGH_SWARM_PASSWORD"],
    edge: ["OAUTH2_PROXY_CLIENT_ID", "OAUTH2_PROXY_CLIENT_SECRET", "OAUTH2_PROXY_COOKIE_SECRET"],
    home: usage,
    proxy: [],
    ops: ["PGHACKER_DB_ADMIN_PASSWORD", "PGH_INDEXER_PASSWORD", "PGH_READER_PASSWORD", "PGH_SWARM_PASSWORD"],
  };
}

/** A plain setting, with any $( escaped, since Kubernetes would expand it. */
function plain(name, setting) {
  return { name, value: String(setting).replaceAll("$(", () => "$$(") };
}

/** A setting from the identity's Kubernetes Secret. */
function secret(identity, name) {
  return { name, valueFrom: { secretKeyRef: { name: secretName(identity), key: name } } };
}

/** A setting that names secrets as $(VAR), defined before it. */
function expanded(name, setting) {
  return { name, value: setting };
}

const entries = (map) => Object.entries(map).map(([name, setting]) => plain(name, setting));

/**
 * Each component's environment, as Kubernetes env entries: plain settings,
 * and secrets from its identity's Kubernetes Secret. `cluster` gives the
 * namespace and the revision that usage telemetry names its events with.
 * Components:
 *   indexer, mcp, home, edge (oauth2-proxy), tokenProxy, on the services pool;
 *   portal and worker (with the crawl scheduler), on the PilotSwarm pool;
 *   ops, the jobs; egressCheck, the job that checks the PilotSwarm pool's
 *   egress.
 */
export function componentEnvironments(config, { namespace, revision }) {
  const origin = (service, port) => serviceOrigin(service, port, namespace);
  const tokenProxy = origin(SERVICES.tokenProxy, PORTS.foundry);
  const monitor = origin(SERVICES.tokenProxy, PORTS.monitor);
  const indexerMcp = `${origin(SERVICES.indexer, PORTS.indexer)}/mcp`;
  const mcpInternal = `${origin(SERVICES.mcp, PORTS.mcpInternal)}/mcp`;
  const rust = { RUST_LOG: config.pass.RUST_LOG ?? "info,tokio_postgres=warn,rmcp=warn" };
  const base = { HOME: DATA_DIR, ...config.pass };
  const embed = embedEnvironment(config.foundry, tokenProxy);
  const history = config.code?.history ? { PGHACKER_CODE_HISTORY: config.code.history } : {};
  const catalog = path.posix.join(CATALOG_DIR, "model_providers.json");
  const agentFiles = {
    PLUGIN_DIRS: path.posix.join(config.root, "plugin"),
    SESSION_STATE_DIR: path.posix.join(DATA_DIR, "sessions"),
    ARTIFACT_DIR: path.posix.join(DATA_DIR, "artifacts"),
    PILOTSWARM_EXPORT_DIR: path.posix.join(DATA_DIR, "exports"),
    PILOTSWARM_LOG_DIR: path.posix.join(DATA_DIR, "logs"),
    PS_MODEL_PROVIDERS_PATH: catalog,
  };
  const swarmDatabase = [
    secret("swarm", "PGH_SWARM_PASSWORD"),
    expanded("DATABASE_URL", databaseUrlFrom(config, "pgh_swarm", "PGH_SWARM_PASSWORD")),
  ];
  // Interactive sessions get a few slots beyond the crawl's.
  const concurrency = String(config.crawl.max + 4);
  const environments = {
    indexer: [
      ...entries({ ...base, ...rust }),
      secret("indexer", "PGH_INDEXER_PASSWORD"),
      expanded("PGHACKER_INDEXER_DATABASE_URL", databaseUrlFrom(config, "pgh_indexer", "PGH_INDEXER_PASSWORD")),
      ...entries({
        ...embed,
        ...history,
        ...config.extraction,
        PGHACKER_INDEXER_ADDR: `0.0.0.0:${PORTS.indexer}`,
        PGHACKER_ALLOWED_HOSTS: serviceHosts(SERVICES.indexer, namespace).join(","),
        PGHACKER_SLICES: path.posix.join(config.root, "config", "test-slices.toml"),
        PGHACKER_CACHE_DIR: path.posix.join(DATA_DIR, "cache"),
        PGHACKER_GIT_DIR: path.posix.join(DATA_DIR, "git"),
        PGHACKER_SOURCES: "open",
        PGHACKER_EXTRACTION: config.crawl.enabled ? "automatic" : "manual",
        PGHACKER_KEEP_DOWNLOADS: "false",
      }),
    ],
    portal: [
      ...entries(base),
      ...swarmDatabase,
      ...entries({
        ...agentFiles,
        PORT: String(PORTS.portal),
        PORTAL_HOST: "0.0.0.0",
        PORTAL_AUTH_PROVIDER: "proxy",
        PORTAL_AUTH_PROXY_MODE: "header",
        PORTAL_AUTH_PROXY_TRUST_HEADERS: "true",
        PORTAL_AUTHZ_DEFAULT_ROLE: config.portalRole,
        PORTAL_AUTH_ALLOW_UNAUTHENTICATED: "false",
        WORKERS: "0",
        PORTAL_TUI_MODE: "local",
        AZURE_OAI_KEY: PLACEHOLDER_KEY,
        // oauth2-proxy forwards the user's GitHub groups in a header.
        NODE_OPTIONS: "--max-http-header-size=65536",
      }),
    ],
    worker: [
      ...entries(base),
      ...swarmDatabase,
      ...entries({
        ...agentFiles,
        PILOTSWARM_WORKER_CONCURRENCY: concurrency,
        PILOTSWARM_ORCHESTRATION_CONCURRENCY: concurrency,
        // The drain on SIGTERM, within the pod's grace period.
        PILOTSWARM_WORKER_SHUTDOWN_TIMEOUT_MS: "30000",
        AZURE_OAI_KEY: PLACEHOLDER_KEY,
        PGHACKER_INDEXER_MCP_URL: indexerMcp,
        // Agents use pghacker-mcp's internal listener: no Front Door check,
        // and a rate limit of its own, which they share.
        PGHACKER_MCP_URL: mcpInternal,
        // The crawl scheduler's.
        PGHACKER_DEPLOYMENT: "azure",
        PGHACKER_FOUNDRY_ENDPOINT: tokenProxy,
        PGHACKER_FOUNDRY_LUNA: config.foundry.luna,
        PGHACKER_CRAWL_MODEL: `azure-foundry:${config.foundry.luna}`,
        PGHACKER_CRAWL_START_SESSIONS: String(config.crawl.start),
        PGHACKER_CRAWL_MAX_SESSIONS: String(config.crawl.max),
        PGHACKER_CRAWL_PER_SESSION: String(config.crawl.perSession),
      }),
    ],
    tokenProxy: entries({
      ...config.pass,
      PGHACKER_FOUNDRY_ENDPOINT: config.foundry.endpoint,
      ...(config.usage ? { APPLICATIONINSIGHTS_CONNECTION_STRING: config.usage.connectionString } : {}),
    }),
    ops: [
      ...entries({ ...base, ...rust }),
      secret("ops", "PGHACKER_DB_ADMIN_PASSWORD"),
      secret("ops", "PGH_INDEXER_PASSWORD"),
      secret("ops", "PGH_READER_PASSWORD"),
      secret("ops", "PGH_SWARM_PASSWORD"),
      expanded(
        "PGHACKER_ADMIN_DATABASE_URL",
        databaseUrlFrom(config, config.db.admin.user, "PGHACKER_DB_ADMIN_PASSWORD"),
      ),
      expanded("PGHACKER_INDEXER_DATABASE_URL", databaseUrlFrom(config, "pgh_indexer", "PGH_INDEXER_PASSWORD")),
      ...entries({
        PGHACKER_DEPLOYMENT: "azure",
        PGHACKER_DB_HOST: config.db.host,
        PGHACKER_DB_PORT: String(config.db.port),
        PGHACKER_DB_NAME: config.db.name,
        PGHACKER_DB_SSLMODE: config.db.sslmode,
        PGHACKER_DB_ADMIN_USER: config.db.admin.user,
        PGHACKER_FOUNDRY_ENDPOINT: config.foundry.endpoint,
        PGHACKER_FOUNDRY_SOL: config.foundry.sol,
        PGHACKER_FOUNDRY_LUNA: config.foundry.luna,
        PGHACKER_FOUNDRY_EMBEDDING: config.foundry.embedding,
        PGHACKER_FOUNDRY_PROXY_URL: tokenProxy,
      }),
    ],
    egressCheck: [
      ...entries(base),
      ...swarmDatabase,
      ...entries({
        PGHACKER_INDEXER_MCP_URL: indexerMcp,
        PGHACKER_MCP_URL: mcpInternal,
        PGHACKER_FOUNDRY_PROXY_URL: tokenProxy,
        // The components the agents mustn't reach: the portal, which trusts
        // oauth2-proxy's identity headers; pghacker-mcp's public listener;
        // the token proxy's telemetry port; the home page and the edge.
        PGHACKER_EGRESS_DENIED: [
          `${origin(SERVICES.portal, PORTS.portal)}/api/health`,
          `${origin(SERVICES.mcp, PORTS.mcp)}/healthz`,
          `${origin(SERVICES.tokenProxy, PORTS.monitor)}/`,
          `${origin(SERVICES.home, PORTS.home)}/healthz`,
          `${origin(SERVICES.edge, 80)}/ping`,
        ].join(","),
      }),
    ],
  };
  if (config.public) {
    // Usage telemetry's settings, for pghacker-mcp and the home page: their
    // events go through the token proxy, which adds the identity's token.
    const usage = (identity) => (config.usage
      ? [
        ...entries({
          PGHACKER_USAGE_ENDPOINT: monitor,
          PGHACKER_USAGE_IKEY: config.usage.instrumentationKey,
          ...(revision ? { CONTAINER_APP_REVISION: revision } : {}),
        }),
        secret(identity, "PGHACKER_USAGE_ID_KEY"),
      ]
      : []);
    environments.mcp = [
      ...entries({ ...base, ...rust }),
      secret("mcp", "PGH_READER_PASSWORD"),
      expanded("PGHACKER_READER_DATABASE_URL", databaseUrlFrom(config, "pgh_reader", "PGH_READER_PASSWORD")),
      ...entries({
        ...embed,
        ...history,
        PGHACKER_MCP_ADDR: `0.0.0.0:${PORTS.mcp}`,
        PGHACKER_MCP_INTERNAL_ADDR: `0.0.0.0:${PORTS.mcpInternal}`,
        PGHACKER_ALLOWED_HOSTS: config.public.hosts.join(","),
        // Agents reach the internal listener by the Service's names.
        PGHACKER_INTERNAL_ALLOWED_HOSTS: serviceHosts(SERVICES.mcp, namespace).join(","),
        // Front Door's X-Azure-SocketIP: its X-Azure-ClientIP follows a
        // client's X-Forwarded-For, which anyone can send.
        PGHACKER_CLIENT_IP_HEADER: "x-azure-socketip",
        PGHACKER_RATE_PER_MINUTE: String(config.rate.perMinute),
        PGHACKER_RATE_BURST: String(config.rate.burst),
        PGHACKER_MCP_CALLS_PER_CLIENT: String(config.callsPerClient),
        PGHACKER_INTERNAL_RATE_PER_MINUTE: String(config.internalRate.perMinute),
        PGHACKER_INTERNAL_RATE_BURST: String(config.internalRate.burst),
        PGHACKER_MCP_POOL: String(config.pools.mcp),
        PGHACKER_QUERY_POOL: String(config.pools.query),
        ...(config.public.frontDoorId ? { PGHACKER_FRONT_DOOR_ID: config.public.frontDoorId } : {}),
        ...(config.usage ? { PGHACKER_USAGE_DETAILS: String(config.usage.details) } : {}),
        ...(config.feedback
          ? {
            PGHACKER_FEEDBACK_RATE: String(config.feedback.rate),
            PGHACKER_FEEDBACK_COMMENTS: String(config.feedback.comments),
            PGHACKER_FEEDBACK_QUESTION_GAP: String(config.feedback.questionGap),
            PGHACKER_FEEDBACK_SESSION_GAP: String(config.feedback.sessionGap),
            PGHACKER_FEEDBACK_MIN_QUESTIONS: String(config.feedback.minQuestions),
            ...(config.feedback.clients ? { PGHACKER_FEEDBACK_CLIENTS: config.feedback.clients.join(",") } : {}),
            PGHACKER_FEEDBACK_DEBUG: String(config.feedback.debug),
          }
          : {}),
      }),
      ...usage("mcp"),
    ];
    // oauth2-proxy: its GitHub OAuth App's credentials and cookie secret,
    // nothing else; its routes and settings are its arguments.
    environments.edge = [
      ...entries(config.pass),
      secret("edge", "OAUTH2_PROXY_CLIENT_ID"),
      secret("edge", "OAUTH2_PROXY_CLIENT_SECRET"),
      secret("edge", "OAUTH2_PROXY_COOKIE_SECRET"),
    ];
    // The home page's server: the built page, its port and the portal's
    // origin, Front Door's host name, which its share links go on to; the
    // privacy statement and copyright link its footer shows, if the
    // deployment has them; and usage telemetry's settings, with the Front
    // Door profile whose requests name their client.
    environments.home = [
      ...entries({
        ...base,
        PORT: String(PORTS.home),
        HOST: "0.0.0.0",
        PGHACKER_HOME_DIR: path.posix.join(config.root, "home", "dist"),
        PGHACKER_PORTAL_URL: config.public.portalUrl,
        ...(config.public.privacyUrl ? { PGHACKER_PRIVACY_URL: config.public.privacyUrl } : {}),
        ...(config.public.copyright
          ? { PGHACKER_COPYRIGHT_URL: config.public.copyright.url, PGHACKER_COPYRIGHT_TEXT: config.public.copyright.text }
          : {}),
        ...(config.usage && config.public.frontDoorId ? { PGHACKER_FRONT_DOOR_ID: config.public.frontDoorId } : {}),
      }),
      ...usage("home"),
    ];
  }
  return environments;
}

/**
 * oauth2-proxy's arguments: GitHub sign-in for the portal, restricted to
 * the team (or the listed users), on the portal's host alone; /mcp and the
 * home page pass through without it. Secrets come from its environment,
 * never its arguments. `origins` names each target's http origin, as
 * scripts/lib/routes.mjs names them: in the cluster, their Services'.
 */
export function oauth2ProxyArgs(config, {
  listen = `0.0.0.0:${PORTS.ingress}`,
  origins = {
    mcp: `http://127.0.0.1:${PORTS.mcp}`,
    portal: `http://127.0.0.1:${PORTS.portal}`,
    home: `http://127.0.0.1:${PORTS.home}`,
  },
} = {}) {
  const { github } = config;
  return [
    `--http-address=${listen}`,
    "--provider=github",
    ...(github.org ? [`--github-org=${github.org}`] : []),
    ...(github.team ? [`--github-team=${github.team}`] : []),
    ...(github.users.length ? [`--github-user=${github.users.join(",")}`] : []),
    "--email-domain=*",
    // /mcp and the home page's files without sign-in; the portal, at /portal
    // with its /api and /assets, after it (scripts/lib/routes.mjs).
    ...oauth2ProxyRouteArgs(origins),
    // Sign-in happens on the portal's host alone, so its callback is fixed
    // and nothing needs X-Forwarded-Host. No forwarded header is trusted:
    // Front Door and the ingress pass on a client's, and with --reverse-proxy
    // oauth2-proxy matches its skip-auth routes against X-Forwarded-Uri.
    `--redirect-url=${config.public.portalUrl}/oauth2/callback`,
    `--whitelist-domain=${new URL(config.public.portalUrl).hostname}`,
    "--reverse-proxy=false",
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
