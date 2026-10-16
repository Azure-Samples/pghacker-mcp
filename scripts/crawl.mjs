// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { parseArgs } from "node:util";
import { APP_ROOT, portalEnvironment, readLocalConfig, serviceUrl } from "./lib/config.mjs";
import { lanesLine, memoryUsed, probeFoundry, runSession, watch } from "./lib/crawl.mjs";
import { callTool } from "./lib/services.mjs";
import { appAgentNames, modelProvidersPath } from "./lib/worker.mjs";

// Runs extraction on the local runtime (npm start must be running):
//   npm run crawl -- --pilot                10 threads, one extractor session
//   npm run crawl -- --threads 200 --concurrency 4
//   npm run crawl -- --supervisor           the pghacker-crawler agent drives its own pilot
//   npm run crawl -- --dedup                one deduplication session
//   npm run crawl -- --watch                keep extracting as threads arrive (the Azure container runs this)
// Extractor sessions are pghacker-extract agents on the configured model
// (default github-copilot:gpt-6-luna, or PGHACKER_CRAWL_MODEL). Progress
// comes from ingest_status. In Azure (PGHACKER_DEPLOYMENT=azure) the
// settings come from the environment the container's supervisor gives it.
const { values: options } = parseArgs({
  options: {
    pilot: { type: "boolean", default: false },
    supervisor: { type: "boolean", default: false },
    dedup: { type: "boolean", default: false },
    watch: { type: "boolean", default: false },
    threads: { type: "string", default: "0" },
    "per-session": { type: "string", default: process.env.PGHACKER_CRAWL_PER_SESSION || "8" },
    concurrency: { type: "string", default: "4" },
    model: { type: "string", default: process.env.PGHACKER_CRAWL_MODEL || "github-copilot:gpt-6-luna" },
    "timeout-minutes": { type: "string", default: "90" },
    "keep-sessions": { type: "boolean", default: false },
  },
});

const azure = process.env.PGHACKER_DEPLOYMENT === "azure";
const config = azure ? null : readLocalConfig();
const indexer = azure ? process.env.PGHACKER_INDEXER_MCP_URL : serviceUrl(config.indexerPort);
const timeout = Number(options["timeout-minutes"]) * 60_000;
const log = (message) => console.log(`[crawl] ${message}`);

async function status() {
  return callTool(indexer, "ingest_status");
}

function summary(s) {
  const e = s.extraction;
  const lanes = lanesLine(e.lanes);
  return `pending ${e.threads_pending}, extracted ${e.threads_extracted}, skipped ${e.threads_skipped}, ` +
    `features ${e.features}, decisions ${e.decisions}, positions ${e.positions}, citations ${e.citations}` +
    (lanes ? `; ${lanes}` : "");
}

function session(agent, task, title) {
  return runSession(client, agent, task, { model: options.model, title, timeoutMs: timeout, log });
}

const DEDUP_TASK = "Review duplicate features, decisions and authors and merge true duplicates.";

// The client checks that the provider's credential resolves, as the worker does.
if (!azure) {
  const token = portalEnvironment(config).GITHUB_TOKEN;
  if (token) process.env.GITHUB_TOKEN = token;
}
const { PilotSwarmClient } = await import("pilotswarm-sdk");
const client = new PilotSwarmClient({
  store: azure ? process.env.DATABASE_URL : config.databaseUrl,
  modelProvidersPath: azure ? process.env.PS_MODEL_PROVIDERS_PATH : modelProvidersPath(config),
  allowedAgentNames: await appAgentNames(config ?? { root: APP_ROOT }),
});
await client.start();

/** Deletes a finished session, which the scheduler's thousands would otherwise pile up in the portal. */
async function forget(result) {
  if (!result.ok || options["keep-sessions"] || !result.sessionId) return;
  try {
    await client.deleteSession(result.sessionId);
  } catch (error) {
    log(`Could not delete session ${result.sessionId}: ${error.message}`);
  }
}

/** Deletes the scheduler's sessions that a previous run left behind; their leases lapse. */
async function clearLeftovers() {
  const ours = /^pghacker (extract|dedup)\b/;
  for (const info of await client.listSessions()) {
    if (info.parentSessionId || !ours.test(info.title ?? "")) continue;
    try {
      await client.deleteSession(info.sessionId);
      log(`Deleted ${info.title} (${info.status}) from a previous run.`);
    } catch (error) {
      log(`Could not delete session ${info.sessionId}: ${error.message}`);
    }
  }
}

try {
  if (options.watch) {
    const controller = new AbortController();
    for (const signal of ["SIGINT", "SIGTERM"]) {
      process.once(signal, () => {
        log(`${signal}: no more sessions.`);
        controller.abort();
      });
    }
    await clearLeftovers();
    const endpoint = process.env.PGHACKER_FOUNDRY_ENDPOINT;
    const deployment = process.env.PGHACKER_FOUNDRY_LUNA;
    await watch({
      status,
      extract: async (quota, sequence) => {
        const result = await session("pghacker-extract", `Process ${quota} threads, then report.`, `pghacker extract ${sequence}`);
        await forget(result);
        return result;
      },
      dedup: async () => {
        const result = await session("pghacker-dedup", DEDUP_TASK, "pghacker dedup");
        await forget(result);
        return result;
      },
      probe: endpoint && deployment && process.env.AZURE_OAI_KEY
        ? () => probeFoundry({ endpoint, deployment, key: process.env.AZURE_OAI_KEY })
        : null,
      memory: memoryUsed,
      log,
      signal: controller.signal,
      options: {
        start: Number(process.env.PGHACKER_CRAWL_START_SESSIONS) || Number(options.concurrency) || 8,
        max: Number(process.env.PGHACKER_CRAWL_MAX_SESSIONS) || 16,
        perSession: Math.max(1, Number(options["per-session"])),
      },
    });
  } else {
    let current = await status();
    log(`Before: ${summary(current)}.`);
    if (options.supervisor) {
      await session("pghacker-crawler",
        "Check ingest_status. Then run the pilot: spawn one pghacker-extract sub-agent with the task " +
        "\"Process 10 threads, then report.\", wait for it, and summarize what it saved for my review.",
        "pghacker crawler pilot");
    } else if (options.dedup) {
      await session("pghacker-dedup", DEDUP_TASK, "pghacker dedup");
    } else {
      const perSession = Math.max(1, Number(options["per-session"]));
      const limit = options.pilot ? 10 : Number(options.threads) || Infinity;
      const concurrency = options.pilot ? 1 : Math.max(1, Number(options.concurrency));
      const pendingOf = (s) => s.extraction.threads_pending + s.extraction.threads_changed_since_extraction;
      let launched = 0;
      let reserved = 0;
      let sequence = 0;
      let failedInARow = 0;
      // Each lane starts a session as soon as its previous one ends, while
      // pending threads exceed those already promised to running sessions.
      async function lane() {
        while (launched < limit && failedInARow < concurrency) {
          const available = pendingOf(await status()) - reserved;
          if (available <= 0) return;
          const quota = Math.min(perSession, limit - launched, available);
          launched += quota;
          reserved += quota;
          sequence += 1;
          const result = await session("pghacker-extract", `Process ${quota} threads, then report.`,
            `pghacker extract ${sequence}`);
          reserved -= quota;
          failedInARow = result.ok ? 0 : failedInARow + 1;
          log(`Progress: ${summary(await status())}.`);
        }
      }
      // A pass ends when the lanes have promised every pending thread; threads
      // that a session left behind get another pass while passes make progress.
      let before = pendingOf(current);
      while (before > 0 && launched < limit && failedInARow < concurrency) {
        await Promise.all(Array.from({ length: concurrency }, lane));
        const after = pendingOf(await status());
        if (after >= before) break;
        before = after;
      }
      if (failedInARow >= concurrency) {
        console.error(`[crawl] ${failedInARow} sessions in a row failed; stopping.`);
        process.exitCode = 1;
      }
    }
    log(`After: ${summary(await status())}.`);
  }
} finally {
  await client.stop();
}
if (options.watch) process.exit(0);
