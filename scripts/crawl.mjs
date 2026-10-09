import { parseArgs } from "node:util";
import { portalEnvironment, readLocalConfig, serviceUrl } from "./lib/config.mjs";
import { callTool } from "./lib/services.mjs";
import { appAgentNames, modelProvidersPath } from "./lib/worker.mjs";

// Runs extraction on the local runtime (npm start must be running):
//   npm run crawl -- --pilot                10 threads, one extractor session
//   npm run crawl -- --threads 200 --concurrency 4
//   npm run crawl -- --supervisor           the pghacker-crawler agent drives its own pilot
// Extractor sessions are pghacker-extract agents on the configured model
// (default github-copilot:gpt-6-luna). Progress comes from ingest_status.
const { values: options } = parseArgs({
  options: {
    pilot: { type: "boolean", default: false },
    supervisor: { type: "boolean", default: false },
    dedup: { type: "boolean", default: false },
    threads: { type: "string", default: "0" },
    "per-session": { type: "string", default: "8" },
    concurrency: { type: "string", default: "4" },
    model: { type: "string", default: "github-copilot:gpt-6-luna" },
    "timeout-minutes": { type: "string", default: "90" },
  },
});

const config = readLocalConfig();
const indexer = serviceUrl(config.indexerPort);
const timeout = Number(options["timeout-minutes"]) * 60_000;

async function status() {
  return callTool(indexer, "ingest_status");
}

function summary(s) {
  const e = s.extraction;
  return `pending ${e.threads_pending}, extracted ${e.threads_extracted}, skipped ${e.threads_skipped}, ` +
    `features ${e.features}, decisions ${e.decisions}, positions ${e.positions}, citations ${e.citations}`;
}

async function runSession(client, agent, task, title) {
  const started = Date.now();
  const session = await client.createSessionForAgent(agent, { model: options.model, title });
  console.log(`[crawl] ${title}: session ${session.sessionId} started.`);
  try {
    const reply = await session.sendAndWait(task, timeout);
    const seconds = Math.round((Date.now() - started) / 1000);
    console.log(`[crawl] ${title} finished in ${seconds}s:\n${(reply ?? "(no reply)").trim().slice(0, 1500)}\n`);
    return { sessionId: session.sessionId, ok: true, seconds, reply };
  } catch (error) {
    console.error(`[crawl] ${title} failed: ${error.message}`);
    return { sessionId: session.sessionId, ok: false, error: error.message };
  }
}

// The client checks that the provider's credential resolves, as the worker does.
const token = portalEnvironment(config).GITHUB_TOKEN;
if (token) process.env.GITHUB_TOKEN = token;
const { PilotSwarmClient } = await import("pilotswarm-sdk");
const client = new PilotSwarmClient({
  store: config.databaseUrl,
  modelProvidersPath: modelProvidersPath(config),
  allowedAgentNames: await appAgentNames(config),
});
await client.start();
try {
  let current = await status();
  console.log(`[crawl] Before: ${summary(current)}.`);
  if (options.supervisor) {
    await runSession(client, "pghacker-crawler",
      "Check ingest_status. Then run the pilot: spawn one pghacker-extract sub-agent with the task " +
      "\"Process 10 threads, then report.\", wait for it, and summarize what it saved for my review.",
      "pghacker crawler pilot");
  } else if (options.dedup) {
    await runSession(client, "pghacker-dedup",
      "Review duplicate features, decisions and authors and merge true duplicates.", "pghacker dedup");
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
        const result = await runSession(client, "pghacker-extract", `Process ${quota} threads, then report.`,
          `pghacker extract ${sequence}`);
        reserved -= quota;
        failedInARow = result.ok ? 0 : failedInARow + 1;
        console.log(`[crawl] Progress: ${summary(await status())}.`);
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
  console.log(`[crawl] After: ${summary(await status())}.`);
} finally {
  await client.stop();
}
