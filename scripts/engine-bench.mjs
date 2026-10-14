import { mkdtemp, readdir, readFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { databaseUrls, readAzureConfig } from "./lib/azure.mjs";
import { run } from "./lib/psql.mjs";

// The engine benchmark on the deployment's own data (docs/search-design.md,
// "Rollout"), as a one-off execution of the probe job with the bench image
// (deploy/bench.Dockerfile) at 4 vCPU and 8 GiB: deploy/scripts/run-job.sh
// bench. It runs `pghacker-replay engines` as pgh_reader on the app's
// database, with the history in schema pghacker, at every snapshot. It logs
// each difference where both engines completed, which is a wrong answer, and
// each call only the history left incomplete. Then it reports in one JSON
// line: ok when there are none, and the summary. The per-call records stay
// on the job's disk. Arguments go to pghacker-replay engines, replacing the
// defaults: --pg19 2000 --jobs 8 --passes 1 --database <the app's>
// --schema pghacker.
const REPLAY = process.env.PGHACKER_REPLAY ?? "/usr/local/bin/pghacker-replay";
// The probe job's replica timeout (deploy/terraform/containerapps.tf), less a
// margin for the container's start and the report.
const JOB_MS = 3600_000;
const MARGIN_MS = 5 * 60_000;
// Calls of each kind logged one per line; the report counts them all.
const LISTED = 500;
const started = Date.now();
const report = { ok: false };

const given = (args, name) => args.some((a) => a === name || a.startsWith(`${name}=`));
const clip = (text, n = 300) => (typeof text === "string" && text.length > n ? `${text.slice(0, n)}…` : text);
const tail = (text) => (text || "").trim().split("\n").slice(-8).join(" | ");

try {
  const config = readAzureConfig(process.env, { serve: false });
  const extra = process.argv.slice(2);
  const defaults = [
    ["--pg19", "2000"], ["--jobs", "8"], ["--passes", "1"],
    ["--database", config.db.name], ["--schema", "pghacker"],
  ];
  const args = ["engines", ...defaults.filter(([name]) => !given(extra, name)).flat(), ...extra];
  report.args = args.slice(1);
  const root = await mkdtemp(path.join(os.tmpdir(), "engine-bench-"));
  // Only what the replay needs: the reader's URL and a root for its files.
  const env = {
    PATH: process.env.PATH,
    HOME: root,
    PGHACKER_ROOT: root,
    PGHACKER_READER_DATABASE_URL: databaseUrls(config).reader,
    RUST_LOG: process.env.RUST_LOG ?? "warn",
  };
  for (const name of ["TZ", "LANG", "LC_ALL", "SSL_CERT_FILE", "SSL_CERT_DIR"]) {
    if (process.env[name]) env[name] = process.env[name];
  }
  const timeoutMs = JOB_MS - (Date.now() - started) - MARGIN_MS;
  const t = Date.now();
  const result = await run(REPLAY, args, { env, timeoutMs });
  if (result.signal) {
    const limit = Date.now() - t >= timeoutMs ? ` at its limit of ${Math.round(timeoutMs / 1000)} s` : "";
    throw new Error(`pghacker-replay engines: killed by ${result.signal}${limit}`);
  }
  if (result.code !== 0) throw new Error(`pghacker-replay engines: ${tail(result.stderr)}`);

  const runtime = path.join(root, ".runtime", "replay");
  const summaries = JSON.parse(await readFile(path.join(runtime, "engines-summary.json"), "utf8"));
  const logs = (await readdir(runtime)).filter((name) => /^engines-.*\.jsonl$/.test(name)).sort();
  const records = (await readFile(path.join(runtime, logs.at(-1)), "utf8"))
    .split("\n").filter(Boolean).map((line) => JSON.parse(line));
  const wrong = records.filter((r) => !r.same && !r.snapshot_incomplete && !r.history_incomplete);
  const historyOnly = records.filter((r) => r.history_incomplete && !r.snapshot_incomplete);
  for (const r of wrong.slice(0, LISTED)) {
    const { pass, head, workload, family, call, detail } = r;
    console.log(JSON.stringify({ wrong: { pass, head, workload, family, call, detail } }));
  }
  for (const r of historyOnly.slice(0, LISTED)) {
    const { pass, head, workload, family, call, snapshot_ms, history_ms } = r;
    console.log(JSON.stringify({ history_incomplete: { pass, head, workload, family, call, snapshot_ms, history_ms } }));
  }
  const last = summaries.at(-1);
  Object.assign(report, {
    passes: summaries.length,
    seconds: last.seconds,
    calls: records.length,
    wrong: wrong.length,
    history_only_incomplete: historyOnly.length,
    snapshot_only_incomplete: records.filter((r) => r.snapshot_incomplete && !r.history_incomplete).length,
    both_incomplete: records.filter((r) => r.snapshot_incomplete && r.history_incomplete).length,
    all: last.all,
    by_workload: last.by_workload,
    by_head: last.by_head,
    first_differences: (last.first_differences ?? []).map((d) => ({ head: d.head, call: clip(d.call), detail: clip(d.detail) })),
  });
  report.ok = wrong.length === 0 && historyOnly.length === 0;
} catch (error) {
  report.error = String(error.message).slice(0, 1000);
}
report.ms = Date.now() - started;
console.log(JSON.stringify(report));
process.exit(report.ok ? 0 : 1);
