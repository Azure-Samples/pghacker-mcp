// The Azure container's supervisor, run as root by tini (Dockerfile). It
// prepares each user's directories and PilotSwarm's model catalog, runs the
// Foundry proxy, which signs the services' model requests with the managed
// identity's tokens (scripts/lib/foundry.mjs), bootstraps the database, then
// runs every service as its own user with only its own environment
// (scripts/lib/azure.mjs), restarts those that exit, and serves the probes'
// health checks on PORTS.health:
//   /healthz  the supervisor is working: 503 once bootstrap has failed for
//             15 minutes or a service keeps exiting, so the platform
//             restarts the container
//   /readyz   bootstrap is done and the services that serve requests
//             answer: oauth2-proxy, pghacker-mcp and the portal. The body
//             also reports the home page, the indexer and the crawl
//             scheduler, whose outages don't stop the endpoint (a crash loop
//             fails /healthz).
import { spawn } from "node:child_process";
import { chmodSync, chownSync, mkdirSync, writeFileSync } from "node:fs";
import http from "node:http";
import path from "node:path";
import readline from "node:readline";
import { setTimeout as delay } from "node:timers/promises";
import {
  PORTS, USERS, foundryCatalog, layout, lookupUser, oauth2ProxyArgs, processEnvironments, readAzureConfig,
} from "./lib/azure.mjs";
import { managedIdentityTokens, startFoundryProxy } from "./lib/foundry.mjs";

const BOOTSTRAP_GIVE_UP_MS = 15 * 60_000;
const RESTART_WINDOW_MS = 10 * 60_000;
const CRASH_LOOP_RESTARTS = 10;
const STOP_GRACE_MS = 25_000;

const log = (message) => process.stdout.write(`[supervisor] ${message}\n`);

const config = readAzureConfig();
const files = layout(config);
const environments = processEnvironments(config);
const users = {
  agent: lookupUser(USERS.agent),
  services: lookupUser(USERS.services),
  proxy: lookupUser(USERS.proxy),
  home: lookupUser(USERS.home),
};

/** Each user's private directories, and the catalog everyone reads. */
function prepareFiles() {
  const owned = [
    [users.agent, Object.values(files.agent)],
    [users.services, Object.values(files.services)],
    [users.proxy, Object.values(files.proxy)],
    [users.home, Object.values(files.home)],
  ];
  for (const [user, directories] of owned) {
    for (const directory of directories) {
      mkdirSync(directory, { recursive: true });
      for (const dir of [directory, path.dirname(directory)]) {
        chownSync(dir, user.uid, user.gid);
        chmodSync(dir, 0o700);
      }
    }
  }
  mkdirSync(files.config, { recursive: true });
  chmodSync(files.config, 0o755);
  writeFileSync(files.modelProviders, `${JSON.stringify(foundryCatalog(config.foundry), null, 2)}\n`, { mode: 0o644 });
}

const state = {
  stopping: false,
  bootstrapped: false,
  bootstrapFailingSince: null,
};

/** A service: started, its output prefixed, restarted with backoff when it exits. */
class Service {
  constructor(name, { user, command, args, env, health }) {
    Object.assign(this, { name, user, command, args, env, health });
    this.child = null;
    this.restarts = [];
  }

  start() {
    if (state.stopping) return;
    const child = spawn(this.command, this.args, {
      cwd: config.root,
      env: this.env,
      uid: this.user.uid,
      gid: this.user.gid,
      stdio: ["ignore", "pipe", "pipe"],
    });
    this.child = child;
    for (const stream of [child.stdout, child.stderr]) {
      readline.createInterface({ input: stream }).on("line", (line) => {
        process.stdout.write(`[${this.name}] ${line}\n`);
      });
    }
    child.once("error", (error) => log(`${this.name} failed to start: ${error.message}`));
    child.once("exit", (code, signal) => {
      this.child = null;
      if (state.stopping) return;
      const now = Date.now();
      this.restarts = this.restarts.filter((at) => now - at < RESTART_WINDOW_MS);
      this.restarts.push(now);
      const wait = Math.min(60_000, 1000 * 2 ** (this.restarts.length - 1));
      log(`${this.name} exited (${signal || `exit ${code}`}); restarting in ${wait / 1000} s.`);
      setTimeout(() => this.start(), wait).unref();
    });
  }

  get crashLooping() {
    const now = Date.now();
    return this.restarts.filter((at) => now - at < RESTART_WINDOW_MS).length >= CRASH_LOOP_RESTARTS;
  }

  async stop() {
    const child = this.child;
    if (!child || child.exitCode !== null) return;
    const exited = new Promise((resolve) => child.once("exit", resolve));
    child.kill("SIGTERM");
    const timer = setTimeout(() => child.kill("SIGKILL"), STOP_GRACE_MS);
    await exited;
    clearTimeout(timer);
  }
}

/** The services that requests go to: readiness waits for these. */
const SERVING = new Set(["oauth2-proxy", "mcp", "swarm"]);

const services = [
  new Service("indexer", {
    user: users.services,
    command: "/usr/local/bin/pghacker-indexer",
    args: ["serve"],
    env: environments.indexer,
    health: `http://127.0.0.1:${PORTS.indexer}/healthz`,
  }),
  new Service("mcp", {
    user: users.services,
    command: "/usr/local/bin/pghacker-mcp",
    args: [],
    env: environments.mcp,
    health: `http://127.0.0.1:${PORTS.mcp}/healthz`,
  }),
  new Service("swarm", {
    user: users.agent,
    command: process.execPath,
    args: [path.join(config.root, "scripts", "swarm.mjs")],
    env: environments.swarm,
    health: `http://127.0.0.1:${PORTS.portal}/api/health`,
  }),
  ...(config.crawl.enabled
    ? [new Service("crawl", {
      user: users.agent,
      command: process.execPath,
      args: [path.join(config.root, "scripts", "crawl.mjs"), "--watch"],
      env: environments.crawl,
    })]
    : []),
  // The public home page, which oauth2-proxy sends / and its files to.
  new Service("home", {
    user: users.home,
    command: process.execPath,
    args: [path.join(config.root, "scripts", "home.mjs")],
    env: environments.home,
    health: `http://127.0.0.1:${PORTS.home}/healthz`,
  }),
  new Service("oauth2-proxy", {
    user: users.proxy,
    command: "/usr/local/bin/oauth2-proxy",
    args: oauth2ProxyArgs(config),
    env: environments.proxy,
    health: `http://127.0.0.1:${PORTS.ingress}/ping`,
  }),
];
const byName = Object.fromEntries(services.map((service) => [service.name, service]));

async function healthy(url) {
  try {
    const response = await fetch(url, { signal: AbortSignal.timeout(2000) });
    await response.arrayBuffer();
    return response.ok;
  } catch {
    return false;
  }
}

async function waitHealthy(service, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (!state.stopping && Date.now() < deadline) {
    if (await healthy(service.health)) return true;
    await delay(1000);
  }
  if (!state.stopping) log(`${service.name} isn't healthy after ${timeoutMs / 1000} s; going on.`);
  return false;
}

/** pghacker-indexer bootstrap, as the services' user, until it succeeds. */
async function bootstrap() {
  for (let attempt = 1; !state.stopping; attempt++) {
    const code = await new Promise((resolve) => {
      const child = spawn("/usr/local/bin/pghacker-indexer", ["bootstrap"], {
        cwd: config.root,
        env: environments.bootstrap,
        uid: users.services.uid,
        gid: users.services.gid,
        stdio: ["ignore", "inherit", "inherit"],
      });
      child.once("error", () => resolve(-1));
      child.once("exit", (exitCode) => resolve(exitCode));
    });
    if (code === 0) {
      state.bootstrapped = true;
      state.bootstrapFailingSince = null;
      log("The database is bootstrapped and migrated.");
      return true;
    }
    state.bootstrapFailingSince ??= Date.now();
    const wait = Math.min(60_000, 5000 * 2 ** Math.min(attempt - 1, 4));
    log(`Bootstrap failed (attempt ${attempt}); retrying in ${wait / 1000} s.`);
    await delay(wait);
  }
  return false;
}

let readiness = { at: 0, ready: false, checks: {} };
async function ready() {
  if (Date.now() - readiness.at < 3000) return readiness;
  const checks = {};
  await Promise.all(services.filter((service) => service.health).map(async (service) => {
    checks[service.name] = await healthy(service.health);
  }));
  if (byName.crawl) checks.crawl = Boolean(byName.crawl.child);
  checks.bootstrap = state.bootstrapped;
  const ready = state.bootstrapped && [...SERVING].every((name) => checks[name]);
  readiness = { at: Date.now(), ready, checks };
  return readiness;
}

function alive() {
  const failing = state.bootstrapFailingSince && Date.now() - state.bootstrapFailingSince > BOOTSTRAP_GIVE_UP_MS;
  const looping = services.filter((service) => service.crashLooping).map((service) => service.name);
  return { alive: !failing && looping.length === 0, bootstrapFailing: Boolean(failing), crashLooping: looping };
}

const health = http.createServer(async (request, response) => {
  const reply = (status, body) => {
    response.writeHead(status, { "content-type": "application/json" });
    response.end(`${JSON.stringify(body)}\n`);
  };
  if (request.url === "/healthz") {
    const status = alive();
    reply(status.alive ? 200 : 503, status);
  } else if (request.url === "/readyz") {
    const status = await ready();
    reply(status.ready ? 200 : 503, status.checks);
  } else {
    reply(404, { error: "not found" });
  }
});

async function shutdown(signal) {
  if (state.stopping) return;
  state.stopping = true;
  log(`${signal}: stopping.`);
  // The proxy first, so that no new request arrives, then the rest together.
  await byName["oauth2-proxy"].stop();
  await Promise.allSettled(services.map((service) => service.stop()));
  health.close();
  process.exit(0);
}
for (const signal of ["SIGTERM", "SIGINT"]) process.once(signal, () => shutdown(signal));

prepareFiles();
health.listen(PORTS.health, "0.0.0.0");
const foundryToken = managedIdentityTokens({
  endpoint: process.env.IDENTITY_ENDPOINT,
  header: process.env.IDENTITY_HEADER,
  clientId: config.foundry.clientId,
});
await startFoundryProxy({
  upstream: config.foundry.endpoint,
  token: foundryToken,
  port: PORTS.foundry,
  log: (message) => log(`Foundry proxy: ${message}`),
});
foundryToken().then(
  () => log(`Foundry proxy on :${PORTS.foundry} for ${config.foundry.endpoint}, with the managed identity's token.`),
  (error) => log(`Foundry proxy on :${PORTS.foundry}, but no token yet: ${error.message}`),
);
log(`Health checks on :${PORTS.health}; bootstrapping ${config.db.name} on ${config.db.host}.`);
if (await bootstrap()) {
  byName.home.start();
  byName.indexer.start();
  byName.mcp.start();
  await Promise.all([waitHealthy(byName.indexer, 120_000), waitHealthy(byName.mcp, 120_000)]);
  byName.swarm.start();
  await waitHealthy(byName.swarm, 300_000);
  byName.crawl?.start();
  byName["oauth2-proxy"].start();
  await waitHealthy(byName["oauth2-proxy"], 60_000);
  log("Started.");
}
