import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { LOCAL_HOST, platformEnvironment } from "./config.mjs";

function recordPath(config) {
  return path.join(config.runtimeDir, "portal.json");
}

export function readPortalRecord(config) {
  let record;
  try {
    record = JSON.parse(readFileSync(recordPath(config), "utf8"));
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw new Error("Cannot read the portal process record.", { cause: error });
  }
  if (!Number.isSafeInteger(record.pid) || record.pid <= 0
    || record.project !== config.project || record.root !== config.root
    || !Number.isInteger(record.port) || record.port < 1 || record.port > 65535) {
    throw new Error("The portal process record does not belong to this app.");
  }
  return record;
}

export function processIsAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    if (error.code === "ESRCH") return false;
    throw error;
  }
}

export function preparePortalRecord(config) {
  mkdirSync(config.runtimeDir, { recursive: true });
  const previous = readPortalRecord(config);
  if (previous && processIsAlive(previous.pid)) {
    throw new Error(`A portal process is already recorded (PID ${previous.pid}). Run npm run stop first.`);
  }
  if (previous) unlinkSync(recordPath(config));
}

export function writePortalRecord(config) {
  writeFileSync(recordPath(config), JSON.stringify({
    pid: process.pid,
    project: config.project,
    root: config.root,
    port: config.portalPort,
  }), { flag: "wx", mode: 0o600 });
}

export function clearPortalRecord(config) {
  const record = readPortalRecord(config);
  if (record?.pid === process.pid) unlinkSync(recordPath(config));
}

export async function assertPortAvailable(port) {
  const probe = createServer();
  await new Promise((resolve, reject) => {
    probe.once("error", (error) => reject(new Error(`Cannot use ${LOCAL_HOST}:${port}: ${error.code}.`, { cause: error })));
    probe.listen(port, LOCAL_HOST, () => probe.close((error) => error ? reject(error) : resolve()));
  });
}

function dockerOptions(config) {
  const env = {
    ...platformEnvironment(),
    POSTGRES_USER: config.values.POSTGRES_USER,
    POSTGRES_DB: config.values.POSTGRES_DB,
    POSTGRES_PASSWORD: config.values.POSTGRES_PASSWORD,
    POSTGRES_PORT: String(config.postgresPort),
  };
  const options = { cwd: config.root, env, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] };
  const context = execFileSync("docker", ["context", "show"], options).trim();
  const endpoint = execFileSync("docker", ["context", "inspect", context, "--format", "{{.Endpoints.docker.Host}}"], options).trim();
  if (!endpoint.startsWith("unix://")) {
    throw new Error("This starter requires a local Docker Unix socket, not a remote Docker endpoint.");
  }
  return {
    env,
    args: [
      "--context", context, "compose",
      "--project-directory", config.root,
      "--env-file", path.join(config.root, ".env"),
      "--file", path.join(config.root, "compose.yaml"),
      "--project-name", config.project,
    ],
  };
}

export async function runDatabase(action, config) {
  const options = dockerOptions(config);
  const command = action === "up"
    ? ["up", "--detach", "--build", "--wait", "--wait-timeout", "90", "postgres"]
    : action === "stop" ? ["stop", "postgres"] : null;
  if (!command) throw new Error(`Unsupported database action: ${action}.`);
  await new Promise((resolve, reject) => {
    const child = spawn("docker", [...options.args, ...command], {
      cwd: config.root, env: options.env, stdio: "inherit",
    });
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (code === 0) resolve();
      else reject(new Error(`Docker Compose ${action} failed (${signal || `exit ${code}`}).`));
    });
  });
  if (action === "up") {
    const address = execFileSync("docker", [...options.args, "port", "postgres", "5432"], {
      cwd: config.root, env: options.env, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"],
    }).trim();
    if (address !== `${LOCAL_HOST}:${config.postgresPort}`) {
      throw new Error("PostgreSQL is not published only at the configured localhost port.");
    }
  }
}

export async function stopPortal(config) {
  const record = readPortalRecord(config);
  if (!record) return false;
  if (!processIsAlive(record.pid)) {
    unlinkSync(recordPath(config));
    return false;
  }
  let command;
  try {
    command = execFileSync("ps", ["-p", String(record.pid), "-o", "command="], {
      encoding: "utf8", stdio: ["ignore", "pipe", "pipe"],
    }).trim();
  } catch (error) {
    if (!processIsAlive(record.pid)) {
      unlinkSync(recordPath(config));
      return false;
    }
    throw new Error("Cannot verify the recorded portal process; it was not stopped.", { cause: error });
  }
  if (command !== config.project) {
    throw new Error("The recorded PID no longer identifies this portal; refusing to stop another process.");
  }
  process.kill(record.pid, "SIGTERM");
  const deadline = Date.now() + 45_000;
  while (processIsAlive(record.pid)) {
    if (Date.now() >= deadline) throw new Error("Portal shutdown timed out; PostgreSQL was left running.");
    await delay(100);
  }
  if (readPortalRecord(config)?.pid === record.pid) unlinkSync(recordPath(config));
  return true;
}
