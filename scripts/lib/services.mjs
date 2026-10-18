// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { execFileSync, spawn } from "node:child_process";
import { createWriteStream, existsSync, mkdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { LOCAL_HOST, platformEnvironment, serviceEnvironments, serviceUrl } from "./config.mjs";
import { skillToolName, verifySkill } from "./evalcheck.mjs";

export const SERVICES = {
  indexer: { binary: "pghacker-indexer", args: ["serve"], portKey: "indexerPort" },
  mcp: { binary: "pghacker-mcp", args: [], portKey: "mcpPort" },
};

/** The tools each MCP server must list. */
export const EXPECTED_TOOLS = {
  indexer: ["add_source", "ingest_status", "next_extraction_batch", "match_extracts", "submit_extracts", "merge_extracts"],
  mcp: ["search_threads", "get_thread", "search_extracts", "author_perspective", "get_ontology", "query", "search", "grep", "read_file", "list_files", "load_skill_review_as_author"],
};

export function binaryPath(config, name) {
  return path.join(config.root, "target", "release", name);
}

function recordPath(config) {
  return path.join(config.runtimeDir, "services.json");
}

function run(command, args, options) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: "inherit", ...options });
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (code === 0) resolve();
      else reject(new Error(`${path.basename(command)} ${args[0] ?? ""} failed (${signal || `exit ${code}`}).`));
    });
  });
}

/** Builds both binaries in release mode; quick when they're up to date. */
export async function buildServices(config) {
  await run("cargo", ["build", "--release", "--locked", "-p", "pghacker-indexer", "-p", "pghacker-mcp"], {
    cwd: config.root,
    env: { ...platformEnvironment(), CARGO_TERM_COLOR: "never" },
  });
}

/** Builds the home page into home/dist, with the dependencies npm run setup installed. */
export async function buildHome(config) {
  const home = path.join(config.root, "home");
  if (!existsSync(path.join(home, "node_modules"))) {
    throw new Error("The home page's dependencies are missing. Run npm run setup first.");
  }
  await run("npm", ["run", "build", "--silent"], { cwd: home, env: platformEnvironment() });
}

/** Creates or updates the roles, extensions, schema and graph, then migrates. */
export async function bootstrapDatabase(config) {
  await run(binaryPath(config, SERVICES.indexer.binary), ["bootstrap"], {
    cwd: config.root,
    env: serviceEnvironments(config).bootstrap,
  });
}

/** Runs the services as children, restarting one that exits unexpectedly. */
export class ServiceSupervisor {
  constructor(config) {
    this.config = config;
    this.children = new Map();
    this.stopping = false;
    this.restarts = new Map();
  }

  start() {
    mkdirSync(path.join(this.config.runtimeDir, "logs"), { recursive: true });
    for (const name of Object.keys(SERVICES)) this.#spawn(name);
    this.#record();
  }

  #spawn(name) {
    const service = SERVICES[name];
    const env = serviceEnvironments(this.config)[name];
    const log = createWriteStream(path.join(this.config.runtimeDir, "logs", `${service.binary}.log`), { flags: "a" });
    const child = spawn(binaryPath(this.config, service.binary), service.args, {
      cwd: this.config.root, env, stdio: ["ignore", "pipe", "pipe"],
    });
    child.stdout.pipe(log);
    child.stderr.pipe(log);
    child.once("exit", (code, signal) => {
      log.end();
      this.children.delete(name);
      if (this.stopping) return;
      const recent = (this.restarts.get(name) ?? []).filter((at) => Date.now() - at < 60_000);
      if (recent.length >= 5) {
        console.error(`[pghacker-mcp] ${service.binary} keeps exiting (${signal || `exit ${code}`}); see .runtime/logs/${service.binary}.log.`);
        return;
      }
      this.restarts.set(name, [...recent, Date.now()]);
      console.error(`[pghacker-mcp] ${service.binary} exited (${signal || `exit ${code}`}); restarting.`);
      setTimeout(() => {
        if (!this.stopping) {
          this.#spawn(name);
          this.#record();
        }
      }, 1000 * (recent.length + 1));
    });
    this.children.set(name, child);
  }

  #record() {
    const pids = Object.fromEntries([...this.children].map(([name, child]) => [name, child.pid]));
    writeFileSync(recordPath(this.config), JSON.stringify({ project: this.config.project, pids }), { mode: 0o600 });
  }

  async stop() {
    this.stopping = true;
    const exits = [...this.children.values()].map((child) => new Promise((resolve) => {
      if (child.exitCode !== null) return resolve();
      child.once("exit", resolve);
      child.kill("SIGTERM");
      setTimeout(() => child.kill("SIGKILL"), 25_000).unref();
    }));
    await Promise.all(exits);
    try {
      unlinkSync(recordPath(this.config));
    } catch {
      // Already gone.
    }
  }
}

/** Stops services a crashed launcher left behind, after checking each PID is ours. */
export function stopRecordedServices(config) {
  if (!existsSync(recordPath(config))) return 0;
  let stopped = 0;
  const record = JSON.parse(readFileSync(recordPath(config), "utf8"));
  if (record.project === config.project) {
    for (const [name, pid] of Object.entries(record.pids ?? {})) {
      const binary = binaryPath(config, SERVICES[name]?.binary ?? "");
      let command = "";
      try {
        command = execFileSync("ps", ["-p", String(pid), "-o", "command="], { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim();
      } catch {
        continue;
      }
      if (Number.isSafeInteger(pid) && pid > 0 && command.startsWith(binary)) {
        process.kill(pid, "SIGTERM");
        stopped += 1;
      }
    }
  }
  unlinkSync(recordPath(config));
  return stopped;
}

async function postJson(url, body, timeout, fetchImpl) {
  const deadline = Date.now() + timeout;
  for (let attempt = 1; ; attempt++) {
    const response = await fetchImpl(url, {
      method: "POST",
      headers: { "content-type": "application/json", accept: "application/json, text/event-stream" },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(Math.max(1, deadline - Date.now())),
    });
    const text = await response.text();
    // pghacker-mcp rate-limits each client address and says when to retry.
    const retryAfter = response.headers.has("retry-after") ? Number(response.headers.get("retry-after")) * 1000 : NaN;
    if (response.status === 429 && retryAfter >= 0 && attempt < 6 && Date.now() + retryAfter < deadline) {
      await delay(retryAfter);
      continue;
    }
    if (!response.ok) throw new Error(`${url} answered HTTP ${response.status}: ${text.slice(0, 200)}`);
    const json = text.startsWith("{") ? text : text.split("\n").find((line) => line.startsWith("data:"))?.slice(5);
    return JSON.parse(json);
  }
}

/** The tool names an MCP server lists, through a client's initialize handshake. */
export async function listTools(url, { timeout = 10_000, fetchImpl = fetch } = {}) {
  const init = await postJson(url, {
    jsonrpc: "2.0", id: 1, method: "initialize",
    params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "pghacker-smoke", version: "1" } },
  }, timeout, fetchImpl);
  if (!init.result?.serverInfo) throw new Error(`${url} did not initialize.`);
  const listed = await postJson(url, { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} }, timeout, fetchImpl);
  return (listed.result?.tools ?? []).map((tool) => tool.name);
}

/** Waits for both services' /healthz, then checks their tools. */
export async function checkServices(config, { timeout = 60_000, fetchImpl = fetch } = {}) {
  const deadline = Date.now() + timeout;
  const results = {};
  for (const [name, service] of Object.entries(SERVICES)) {
    const port = config[service.portKey];
    const health = `http://${LOCAL_HOST}:${port}/healthz`;
    for (;;) {
      try {
        const response = await fetchImpl(health, { signal: AbortSignal.timeout(2000) });
        if (response.ok) break;
      } catch {
        // Not listening yet.
      }
      if (Date.now() > deadline) throw new Error(`${service.binary} did not become healthy at ${health}.`);
      await delay(500);
    }
    const tools = await listTools(serviceUrl(port), { fetchImpl });
    const missing = EXPECTED_TOOLS[name].filter((tool) => !tools.includes(tool));
    if (missing.length) throw new Error(`${service.binary} does not list ${missing.join(", ")}.`);
    results[name] = tools;
  }
  return results;
}

/** Calls an MCP tool through the stateless JSON-RPC endpoint; returns its JSON or text. */
export async function callTool(url, name, args = {}, { timeout = 120_000, fetchImpl = fetch } = {}) {
  const reply = await mcpRequest(url, "tools/call", { name, arguments: args }, { timeout, fetchImpl, raw: true });
  if (reply.error) throw new Error(`${name}: ${reply.error.message}`);
  const result = reply.result ?? {};
  const text = (result.content ?? []).filter((block) => block.type === "text").map((block) => block.text).join("\n");
  if (result.isError) throw new Error(`${name}: ${text}`);
  return result.structuredContent ?? text;
}

/**
 * Sends one MCP request, such as skills/get or resources/read, after a
 * client's initialize; returns its result, or with `raw` the whole reply.
 */
export async function mcpRequest(url, method, params = {}, { timeout = 120_000, fetchImpl = fetch, raw = false } = {}) {
  await postJson(url, {
    jsonrpc: "2.0", id: 1, method: "initialize",
    params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "pghacker-scripts", version: "1" } },
  }, timeout, fetchImpl);
  const reply = await postJson(url, { jsonrpc: "2.0", id: 2, method, params }, timeout, fetchImpl);
  if (raw) return reply;
  if (reply.error) throw new Error(`${method}: ${reply.error.message}`);
  return reply.result;
}

/**
 * The names of the skills an MCP server lists, after reading each of their
 * files as a host would and checking it against the listing's digest, and
 * checking that the tool that loads a skill, if the server lists it to this
 * client (which declares no skills extension), returns the same SKILL.md.
 */
export async function checkSkills(url, { timeout = 30_000, fetchImpl = fetch } = {}) {
  const { skills = [] } = await mcpRequest(url, "skills/list", {}, { timeout, fetchImpl });
  const tools = skills.length ? await listTools(url, { timeout, fetchImpl }) : [];
  const checked = [];
  for (const skill of skills) {
    const name = skill.frontmatter?.name ?? skill.uri;
    const texts = new Map();
    for (const { uri } of Array.isArray(skill.resources) ? skill.resources : []) {
      const { contents } = await mcpRequest(url, "resources/read", { uri }, { timeout, fetchImpl });
      texts.set(uri, contents?.[0]?.text);
    }
    const problems = verifySkill(skill, texts);
    if (problems.length) throw new Error(`${skill.uri} doesn't match its listing: ${problems.join("; ")}`);
    const tool = skillToolName(skill.frontmatter?.name);
    if (tools.includes(tool)) {
      const loaded = await callTool(url, tool, {}, { timeout, fetchImpl });
      if (loaded !== texts.get(skill.uri)) throw new Error(`${tool} doesn't return ${skill.uri} as resources/read does.`);
      checked.push(`${name} (${tool} returns it too)`);
    } else {
      checked.push(name);
    }
  }
  return checked;
}
