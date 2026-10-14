import { mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { processEnvironments, readAzureConfig } from "./lib/azure.mjs";
import { run } from "./lib/psql.mjs";

// Builds the commit history in the Azure deployment (deploy/README.md,
// "Commit history"), as a one-off execution of the probe job at 4 vCPU and
// 8 GiB, once an image with migration 0011 is live. It fetches every branch
// and tag of PostgreSQL's repository into a bare clone, builds the history
// into a schema of its own and swaps it into pghacker, then rebuilds every
// file of the branch heads and of random commits and compares them with git.
// Reports in one JSON line; exits 1 when the build or the check failed.
// Arguments: --sample N (random commits to check, default 40).
const REPO = "https://git.postgresql.org/git/postgresql.git";
const INDEXER = "/usr/local/bin/pghacker-indexer";
const started = Date.now();
const report = { ok: false };

const at = process.argv.indexOf("--sample");
const sample = at > 0 ? Number(process.argv[at + 1]) : 40;

async function indexer(args, env, timeoutMs) {
  const result = await run(INDEXER, args, { env, timeoutMs });
  if (result.code !== 0) {
    throw new Error(`${args.slice(0, 2).join(" ")}: ${result.stderr.trim().split("\n").slice(-8).join(" | ")}`);
  }
  return JSON.parse(result.stdout);
}

let dir;
try {
  if (!Number.isInteger(sample) || sample < 0 || sample > 1000) throw new Error("--sample must be 0 to 1000");
  const config = readAzureConfig(process.env, { serve: false });
  const env = processEnvironments(config).bootstrap;
  dir = await mkdtemp(path.join(os.tmpdir(), "postgres-history-"));
  const git = path.join(dir, "postgres.git");
  let step = Date.now();
  const built = await indexer(
    ["history", "build", "--fetch", "--git-dir", git, "--url", REPO, "--install"],
    env,
    40 * 60_000,
  );
  report.build = {
    ms: Date.now() - step,
    commits: built.walk?.commits,
    stored: built.stored,
    lanes: built.lanes,
    refs: built.refs,
    walk_secs: built.walk_secs,
    write_secs: built.write_secs,
    finish_secs: built.finish_secs,
  };
  step = Date.now();
  const verified = await indexer(
    ["history", "verify", "--git-dir", git, "--schema", "pghacker", "--heads", "--sample", String(sample)],
    env,
    15 * 60_000,
  );
  report.verify = { ms: Date.now() - step, ...verified };
  report.ok = verified.mismatch_count === 0 && verified.scan_disagreements === 0;
} catch (error) {
  report.error = String(error.message).slice(0, 1000);
} finally {
  if (dir) await rm(dir, { recursive: true, force: true });
}
report.ms = Date.now() - started;
console.log(JSON.stringify(report));
process.exit(report.ok ? 0 : 1);
