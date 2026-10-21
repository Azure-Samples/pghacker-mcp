// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { spawn } from "node:child_process";
import { createWriteStream, existsSync, mkdirSync, readFileSync } from "node:fs";
import path from "node:path";
import { createInterface } from "node:readline";
import { setTimeout as delay } from "node:timers/promises";
import { LOCAL_HOST, portalEnvironment, readLocalConfig, serviceEnvironments, serviceUrl } from "./lib/config.mjs";
import { latestLogLine, needsBuild, startInBackground, startLogPath, tailLines } from "./lib/background.mjs";
import {
  HistoryProgress, INDENT, directorySize, graphCaughtUp, graphLabels, graphWaiting, historyArgs, historyGitDir,
  FEEDBACK_LINE, historyLine, mailDone, mailHeader, mailKey, mailLines, mailProgress, parseHistoryLine, parseLoadArgs, parseSlices,
  planSteps, stepLine, summaryLines,
} from "./lib/load.mjs";
import { runningPortal } from "./lib/processes.mjs";
import { Progress, bar, formatDuration, formatNumber } from "./lib/progress.mjs";
import { SERVICES, binaryPath, callTool, listTools } from "./lib/services.mjs";

// Loads the local data, starting the app in the background first if it
// isn't running:
//   npm run load                  the mail's test slices and PostgreSQL's whole commit history
//   npm run load -- --quick       the history of src/backend/parser alone
//   npm run load -- --mail-only   the mail alone; --code-only, the code alone
//   npm run load -- --force       rebuild an installed history
// Each step shows its progress, and a re-run skips what's done. It ends
// with what works now and what extraction (npm run crawl) adds.

const POLL_MS = 2000;
/** How long a step waits without progress before it leaves the rest to the background. */
const MAIL_STALL_MS = 20 * 60_000;
const EMBED_STALL_MS = 3 * 60_000;
const GRAPH_STALL_MS = 10 * 60_000;

class Interrupted extends Error {}

let options;
try {
  options = parseLoadArgs(process.argv.slice(2));
} catch (error) {
  console.error(`[load] ${error.message}.`);
  console.error("Usage: npm run load [-- --quick | --mail-only | --code-only | --force]");
  process.exit(2);
}

const progress = new Progress();
const controller = new AbortController();
const { signal } = controller;
/** What to say after Ctrl+C, about the step it stopped. */
let resumeNote = "";
let interrupts = 0;
process.on("SIGINT", () => {
  interrupts += 1;
  if (interrupts > 1) process.exit(130);
  controller.abort();
});

function relative(file) {
  return path.relative(process.cwd(), file) || file;
}

async function sleep(ms) {
  try {
    await delay(ms, undefined, { signal });
  } catch {
    throw new Interrupted();
  }
}

function checkInterrupted() {
  if (signal.aborted) throw new Interrupted();
}

let config;
let indexer;
const failures = [];
/** Whether a step loaded anything, or only found it done. */
let worked = false;

async function ingestStatus() {
  const status = await callTool(indexer, "ingest_status", {}, { timeout: 60_000 });
  return typeof status === "string" ? JSON.parse(status) : status;
}

async function indexerTool(name, args = {}) {
  const result = await callTool(indexer, name, args, { timeout: 120_000 });
  return typeof result === "string" ? JSON.parse(result) : result;
}

async function healthy(port) {
  try {
    return (await fetch(`http://${LOCAL_HOST}:${port}/healthz`, { signal: AbortSignal.timeout(2000) })).ok;
  } catch {
    return false;
  }
}

async function servicesHealthy() {
  const results = await Promise.all(Object.values(SERVICES).map((service) => healthy(config[service.portKey])));
  return results.every(Boolean);
}

/** [1/5]: the app, started in the background unless it's running. */
async function ensureApp() {
  if (await servicesHealthy()) {
    progress.print(stepLine(1, "App: ✔ already running"));
    return;
  }
  const started = Date.now();
  const recorded = runningPortal(config);
  if (recorded) {
    // Another npm start is under way: wait for its services.
    progress.print(stepLine(1, `The app is starting (PID ${recorded.pid})`));
    const log = startLogPath(config);
    while (!(await servicesHealthy())) {
      if (!runningPortal(config)) throw new Error(`The app stopped while starting; see ${relative(log)}.`);
      progress.live(() => [`${INDENT}{spin} ${formatDuration(Date.now() - started)}  ${latestLogLine(config) ?? ""}`.trimEnd()], "starting");
      await sleep(1000);
    }
    progress.end(`${INDENT}✔ ready (${formatDuration(Date.now() - started)})`);
    return;
  }
  progress.print(stepLine(1, "Starting the app in the background"));
  if (needsBuild(config)) progress.print(`${INDENT}The first start builds the Rust services: this takes several minutes.`);
  resumeNote = "The app's start was stopped.";
  try {
    const result = await startInBackground(config, { progress, indent: INDENT, signal });
    worked = true;
    progress.end(`${INDENT}✔ ${result.already ? "already running" : `ready (${formatDuration(result.ms)})`}; its log is ${relative(startLogPath(config))}`);
  } catch (error) {
    progress.end();
    if (error.interrupted) throw new Interrupted();
    const lines = [`${INDENT}✖ ${error.message}`];
    if (error.logTail?.length) {
      lines.push(`${INDENT}The end of ${relative(error.log)}:`, ...error.logTail.map((line) => `${INDENT}  ${line}`));
    }
    progress.print(...lines);
    throw Object.assign(new Error("The app didn't start, so nothing was loaded."), { reported: true });
  }
}

/** [2/5]: the mail's test slices, through add_source, until their jobs are done. */
async function loadMail(step, sources) {
  if (!step.run) {
    progress.print(stepLine(2, `Mail: ${step.reason}`));
    return;
  }
  const slices = parseSlices(readFileSync(path.join(config.root, "config", "test-slices.toml"), "utf8"));
  progress.print(stepLine(2, mailHeader(slices)));
  const started = Date.now();
  const args = { kind: "mailing_list", name: slices.list };
  // An indexer that takes any source (PGHACKER_SOURCES=open) would take the
  // whole archive without months: give it the slices'.
  if (sources?.available?.mode === "open") {
    Object.assign(args, { months: slices.months, seed_threads: slices.seeds.map(({ message_id, topic }) => ({ message_id, topic })) });
  }
  try {
    const added = await indexerTool("add_source", args);
    if (Number(added?.jobs_queued ?? 1) > 0) worked = true;
  } catch (error) {
    progress.print(`${INDENT}✖ add_source: ${error.message}`);
    failures.push("the mail");
    return;
  }
  resumeNote = "The indexer keeps loading the mail in the background.";
  const reported = new Set();
  let mail;
  let signature = "";
  let changedAt = Date.now();
  for (;;) {
    checkInterrupted();
    mail = mailProgress(await ingestStatus(), slices);
    for (const failure of mail.failures) {
      if (reported.has(failure.key)) continue;
      reported.add(failure.key);
      progress.print(`${INDENT}✖ ${failure.what}: ${failure.error ?? "failed"}`);
    }
    if (mail.finished) break;
    const now = `${mailKey(mail)}|${mail.totals.messages}`;
    if (now !== signature) {
      signature = now;
      changedAt = Date.now();
    } else if (Date.now() - changedAt > MAIL_STALL_MS) {
      progress.end(`${INDENT}… no progress in ${formatDuration(MAIL_STALL_MS)}: the indexer goes on in the background (ingest_status shows it).`);
      return;
    }
    const current = mail;
    progress.live(() => mailLines(current), mailKey(current));
    await sleep(POLL_MS);
  }
  progress.end(...(progress.tty ? mailLines(mail, INDENT, { live: false }) : []), `${INDENT}${mailDone(mail, Date.now() - started)}`);
  if (mail.failures.length) {
    const jobs = mail.months.total + mail.seeds.total;
    progress.print(`${INDENT}✖ ${mail.failures.length} of ${jobs} failed: npm run load tries them again.`);
    failures.push("the mail");
  }
}

/** Runs the history build, showing its progress; returns its stats, from its output. */
async function buildHistory(step) {
  const gitDir = historyGitDir(config);
  mkdirSync(path.dirname(gitDir), { recursive: true });
  const logFile = path.join(config.runtimeDir, "logs", "load-history.log");
  mkdirSync(path.dirname(logFile), { recursive: true });
  const log = createWriteStream(logFile, { flags: "w", mode: 0o600 });
  const args = historyArgs(gitDir, { quick: step.paths.length > 0 });
  log.write(`pghacker-indexer ${args.join(" ")}\n`);
  const history = new HistoryProgress({ fetching: existsSync(path.join(gitDir, "HEAD")) });
  history.cloneBytes = directorySize(gitDir);
  const child = spawn(binaryPath(config, "pghacker-indexer"), args, {
    cwd: config.root,
    env: { ...serviceEnvironments(config).bootstrap, RUST_LOG: "info,tokio_postgres=warn", NO_COLOR: "1" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "";
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
    log.write(chunk);
  });
  const lines = createInterface({ input: child.stderr });
  // Once the build has closed, its last buffered lines must not draw the
  // block again, nor write to the closed log.
  let closed = false;
  lines.on("line", (line) => {
    if (closed) return;
    log.write(`${line}\n`);
    const event = parseHistoryLine(line);
    if (event?.type === "fetched") history.cloneBytes = directorySize(gitDir);
    history.apply(event);
    progress.live(() => history.lines(), history.key());
  });
  const sizer = setInterval(() => {
    if (history.phase === "cloning") history.cloneBytes = directorySize(gitDir);
    progress.live(() => history.lines(), history.key());
  }, 1000);
  progress.live(() => history.lines(), history.key());
  const stop = () => child.kill("SIGTERM");
  signal.addEventListener("abort", stop, { once: true });
  const [code, exitSignal] = await new Promise((resolve) => {
    child.once("error", (error) => {
      log.write(`${error.message}\n`);
      resolve([null, error.message]);
    });
    child.once("close", (exitCode, killed) => resolve([exitCode, killed]));
  });
  closed = true;
  lines.close();
  signal.removeEventListener("abort", stop);
  clearInterval(sizer);
  await new Promise((resolve) => log.end(resolve));
  if (signal.aborted) {
    progress.end(...(progress.tty ? history.lines().map((line) => line.replace("{spin} ", "")) : []));
    throw new Interrupted();
  }
  if (code !== 0) {
    progress.end(...(progress.tty ? history.lines().map((line) => line.replace("{spin}", "✖")) : []));
    const reason = exitSignal ? `stopped by ${exitSignal}` : `exit ${code}`;
    progress.print(
      `${INDENT}✖ The history build failed (${reason}). The end of ${relative(logFile)}:`,
      ...tailLines(readFileSync(logFile, "utf8"), 15).map((line) => `${INDENT}  ${line}`),
    );
    return null;
  }
  progress.end(...(progress.tty ? history.lines() : []));
  try {
    return JSON.parse(stdout);
  } catch {
    return {};
  }
}

/** [3/5]: PostgreSQL's commit history, built and installed unless it is already. */
async function loadCode(step) {
  if (step.reason === "installed") {
    const whole = !step.history.paths?.length;
    const note = whole || options.quick ? "--force rebuilds it" : "npm run load -- --code-only --force builds the whole tree";
    progress.print(stepLine(3, `Code: ✔ already installed: ${historyLine(step.history)} (${note})`));
    return;
  }
  if (!step.run) {
    progress.print(stepLine(3, `Code: ${step.reason}`));
    return;
  }
  const scope = step.paths.length ? `${step.paths.join(", ")} only` : "whole tree";
  progress.print(stepLine(3, `Code: PostgreSQL's commit history (${scope})${step.rebuild ? ", rebuilding it" : ""}`));
  resumeNote = "The history's build was stopped; its clone is kept, so the next build fetches only what's new.";
  const started = Date.now();
  worked = true;
  const built = await buildHistory(step);
  if (!built) {
    failures.push("the code");
    return;
  }
  const status = await ingestStatus();
  progress.print(`${INDENT}✔ ${historyLine(status.code?.history)} (${formatDuration(Date.now() - started)})`);
}

/** [4/5]: the mail's embeddings, while they make progress. */
async function waitForEmbeddings(step) {
  if (!step.run) {
    progress.print(stepLine(4, `Embeddings: ${step.reason}`));
    return;
  }
  let { embeddings } = await ingestStatus();
  if (!embeddings?.enabled) {
    progress.print(stepLine(4, "Embeddings: skipped, HORIZON_EMBED_* isn't set: full-text search only"));
    return;
  }
  resumeNote = "The indexer keeps embedding the mail in the background.";
  const line = (e, mark) => stepLine(4, `Embeddings: ${mark} ${bar(e.embedded, e.chunks)} ${formatNumber(e.embedded)}/${formatNumber(e.chunks)} chunks`);
  let changedAt = Date.now();
  let last = embeddings.embedded;
  while (embeddings.embedded < embeddings.chunks) {
    if (embeddings.embedded !== last) {
      last = embeddings.embedded;
      changedAt = Date.now();
    } else if (Date.now() - changedAt > EMBED_STALL_MS) {
      progress.end(stepLine(4, `Embeddings: ${formatNumber(embeddings.embedded)}/${formatNumber(embeddings.chunks)} chunks; the rest continues in the background`));
      return;
    }
    const current = embeddings;
    progress.live(() => line(current, "{spin}"), String(Math.floor((current.embedded / Math.max(1, current.chunks)) * 10)));
    await sleep(POLL_MS);
    ({ embeddings } = await ingestStatus());
  }
  progress.end(stepLine(4, `Embeddings: ✔ ${formatNumber(embeddings.embedded)}/${formatNumber(embeddings.chunks)} chunks`));
}

/** [5/5]: the graph, once its sync has caught up; with its vertices by label. */
async function waitForGraph() {
  resumeNote = "The indexer keeps syncing the graph in the background.";
  const started = Date.now();
  let { graph } = await ingestStatus();
  let changedAt = Date.now();
  let last = "";
  while (!graphCaughtUp(graph)) {
    const now = `${graph.pending_changes}|${graph.last_sync?.at}`;
    if (now !== last) {
      last = now;
      changedAt = Date.now();
    } else if (Date.now() - changedAt > GRAPH_STALL_MS) {
      progress.end(stepLine(5, `Graph: ${graphWaiting(graph)}; the sync continues in the background (ingest_status shows it)`));
      return;
    }
    const current = graph;
    progress.live(() => stepLine(5, `Graph: {spin} ${graphWaiting(current)} (${formatDuration(Date.now() - started)})`), current.full_sync?.requested ? "full" : "changes");
    await sleep(POLL_MS);
    ({ graph } = await ingestStatus());
  }
  progress.end(stepLine(5, `Graph: ✔ ${graphLabels(graph.nodes)}`));
}

async function main() {
  const started = Date.now();
  config = readLocalConfig();
  indexer = serviceUrl(config.indexerPort);
  await ensureApp();
  const sources = await indexerTool("list_sources");
  const steps = planSteps(options, { history: sources.history });
  await loadMail(steps.mail, sources);
  await loadCode(steps.code);
  await waitForEmbeddings(steps.embeddings);
  await waitForGraph();
  resumeNote = "";

  const status = await ingestStatus();
  let toolCount = 0;
  try {
    toolCount = (await listTools(serviceUrl(config.mcpPort))).length;
  } catch {
    // The summary says "all the tools".
  }
  const loaded = { mail: Number(status.mail?.messages ?? 0) > 0, code: Boolean(status.code?.history?.head) };
  progress.print("", ...summaryLines({
    loaded, ms: worked ? Date.now() - started : null, toolCount, extraction: status.extraction,
    hasToken: Boolean(portalEnvironment(config).GITHUB_TOKEN),
  }));
  progress.print("", FEEDBACK_LINE);
  if (failures.length) {
    progress.print("", `✖ Loading ${failures.join(" and ")} failed: npm run load tries again, and skips what's done.`);
    process.exitCode = 1;
  }
}

try {
  await main();
} catch (error) {
  progress.end();
  if (error instanceof Interrupted || signal.aborted) {
    progress.print("", `Stopped. ${resumeNote} Run npm run load again to resume: it skips the steps that are done.`.replace(/\s+/g, " ").trim());
    process.exitCode = 130;
  } else {
    if (!error.reported) progress.print(`✖ ${error.message}`);
    else progress.print("", `✖ ${error.message}`);
    process.exitCode = 1;
  }
}
