// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// npm run load's plan and what it shows (scripts/load.mjs runs it): its
// flags, the steps it skips, the test slices it loads, the progress of each
// step as ingest_status and the history build's log report it, and the
// summary that ends it.

import { readdirSync, statSync } from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";
import { bar, formatBytes, formatDuration, formatNumber, stripAnsi } from "./progress.mjs";

export const STEPS = 5;
export const REPO_URL = "https://git.postgresql.org/git/postgresql.git";
export const QUICK_PATH = "src/backend/parser";
/** The steps' lines below their headers. */
export const INDENT = "      ";

/** npm run load's flags. */
export function parseLoadArgs(argv) {
  const { values } = parseArgs({
    args: argv,
    options: {
      quick: { type: "boolean", default: false },
      "mail-only": { type: "boolean", default: false },
      "code-only": { type: "boolean", default: false },
      force: { type: "boolean", default: false },
    },
    strict: true,
    allowPositionals: false,
  });
  const options = { quick: values.quick, mailOnly: values["mail-only"], codeOnly: values["code-only"], force: values.force };
  if (options.mailOnly && options.codeOnly) {
    throw new Error("--mail-only and --code-only exclude each other: without either, npm run load loads both");
  }
  if (options.quick && options.mailOnly) throw new Error("--quick narrows the code, which --mail-only skips");
  return options;
}

/**
 * The indexer's clone of PostgreSQL's repository, in its git directory
 * (PGHACKER_GIT_DIR): the hourly history sync fetches new commits into the
 * same clone.
 */
export function historyGitDir(config) {
  return path.join(config.runtimeDir, "git", "postgres-history.git");
}

/** The history build's arguments. */
export function historyArgs(gitDir, { quick = false } = {}) {
  return [
    "history", "build", "--git-dir", gitDir, "--url", REPO_URL, "--fetch", "--install",
    ...(quick ? ["--path", QUICK_PATH] : []),
  ];
}

/**
 * What each step does: `{ run: true }`, or `{ run: false, reason }`. The
 * mail always runs (add_source is idempotent, and a re-run retries failed
 * months); the code skips an installed history (`history`, from
 * list_sources) unless `force`.
 */
export function planSteps(options, { history = null } = {}) {
  const codeOnly = { run: false, reason: "skipped (--code-only)" };
  let code;
  if (options.mailOnly) {
    code = { run: false, reason: "skipped (--mail-only)" };
  } else if (history?.head && !options.force) {
    code = { run: false, reason: "installed", history };
  } else {
    code = { run: true, rebuild: Boolean(history?.head), paths: options.quick ? [QUICK_PATH] : [] };
  }
  return {
    mail: options.codeOnly ? codeOnly : { run: true },
    code,
    embeddings: options.codeOnly ? codeOnly : { run: true },
    graph: { run: true },
  };
}

/** A step's header: [2/5] Mail: ... */
export function stepLine(n, text) {
  return `[${n}/${STEPS}] ${text}`;
}

/** The mail of config/test-slices.toml: its list, months and seed threads, `{ list, months, seeds }`. */
export function parseSlices(text) {
  const slices = { list: "pgsql-hackers", months: [], seeds: [] };
  let section = "";
  let seed = null;
  const value = (raw) => {
    const trimmed = raw.replace(/\s+#[^"\]]*$/, "").trim();
    try {
      return JSON.parse(trimmed);
    } catch {
      return trimmed.replace(/^"|"$/g, "");
    }
  };
  for (const raw of String(text).split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    if (line.startsWith("[[")) {
      section = line.replace(/^\[\[\s*|\s*\]\]$/g, "");
      seed = section === "mail.seed_threads" ? {} : null;
      if (seed) slices.seeds.push(seed);
      continue;
    }
    if (line.startsWith("[")) {
      section = line.replace(/^\[\s*|\s*\]$/g, "");
      seed = null;
      continue;
    }
    const match = /^([A-Za-z0-9_]+)\s*=\s*(.+)$/.exec(line);
    if (!match) continue;
    const [, key, text] = match;
    if (section === "mail" && key === "list") slices.list = String(value(text));
    else if (section === "mail" && key === "months") slices.months = [].concat(value(text)).map(String);
    else if (seed && (key === "topic" || key === "message_id")) seed[key] = String(value(text));
  }
  slices.seeds = slices.seeds.filter((s) => s.message_id);
  return slices;
}

const MONTH_NAME = new Intl.DateTimeFormat("en-US", { month: "long", timeZone: "UTC" });

function monthParts(month) {
  const [year, number] = month.split("-").map(Number);
  return { year, name: MONTH_NAME.format(new Date(Date.UTC(year, number - 1, 1))) };
}

/** Months as people say them: June 2026, June–July 2026, December 2025–January 2026. */
export function monthRange(months) {
  const sorted = [...months].filter((month) => /^\d{4}-\d{2}$/.test(month)).sort();
  if (!sorted.length) return "";
  const first = monthParts(sorted[0]);
  const last = monthParts(sorted.at(-1));
  if (sorted.length === 1) return `${first.name} ${first.year}`;
  if (first.year === last.year) return `${first.name}–${last.name} ${last.year}`;
  return `${first.name} ${first.year}–${last.name} ${last.year}`;
}

/** The mail step's header. */
export function mailHeader(slices) {
  const parts = [monthRange(slices.months), slices.seeds.length ? `${slices.seeds.length} seed threads` : ""].filter(Boolean);
  return `Mail: ${slices.list}${parts.length ? `, ${parts.join(" and ")}` : ""}`;
}

const TERMINAL = new Set(["done", "failed", "cancelled"]);

/**
 * The mail's progress in `status` (ingest_status): the slices' months and
 * seed threads, each done when its job is; their failures; and whether
 * they're all finished.
 */
export function mailProgress(status, slices) {
  const wanted = new Set(slices.months);
  const months = (status?.mail?.months ?? [])
    .filter((month) => month.list === slices.list && (wanted.size === 0 || wanted.has(month.month)));
  const seeds = status?.mail?.seed_threads ?? [];
  const latest = (list, key) => list.find((item) => item.state === "running")
    ?? [...list].filter((item) => item.state === "done").sort((a, b) => String(b[key]).localeCompare(String(a[key])))[0];
  const failures = [
    ...months.filter((month) => month.state === "failed")
      .map((month) => ({ key: `month ${month.month}`, what: month.month, error: month.error })),
    ...seeds.filter((seed) => seed.state === "failed")
      .map((seed) => ({ key: `seed ${seed.message_id}`, what: `"${seed.topic ?? seed.message_id}"`, error: seed.error })),
  ];
  const runningSeed = seeds.find((seed) => seed.state === "running") ?? [...seeds].reverse().find((seed) => seed.state === "done");
  return {
    months: {
      done: months.filter((month) => TERMINAL.has(month.state)).length,
      total: Math.max(slices.months.length, months.length),
      current: latest(months, "month"),
    },
    seeds: {
      done: seeds.filter((seed) => TERMINAL.has(seed.state)).length,
      total: Math.max(slices.seeds.length, seeds.length),
      current: runningSeed,
    },
    failures,
    finished: months.length >= slices.months.length && months.every((month) => TERMINAL.has(month.state))
      && seeds.length >= slices.seeds.length && seeds.every((seed) => TERMINAL.has(seed.state)),
    totals: {
      messages: status?.mail?.messages ?? 0,
      threads: status?.mail?.threads ?? 0,
      authors: status?.mail?.authors ?? 0,
    },
  };
}

function counter(done, total, width) {
  return `${String(done).padStart(width)}/${String(total).padEnd(width)}`;
}

/** The mail's bars, then what it has so far: the spinner marks what's running. */
export function mailLines(mail, indent = INDENT, { live = true } = {}) {
  const lines = [];
  const width = String(Math.max(mail.months.total, mail.seeds.total)).length;
  if (mail.months.total) {
    const current = mail.months.current;
    let detail = "";
    if (current?.state === "running") {
      // A month downloads whole (about 100 MB) before any of it is stored.
      detail = `{spin} ${current.month}: ${current.messages ? `${formatNumber(current.messages)} messages` : "downloading"}`;
    } else if (current) {
      detail = `${current.month}: ${formatNumber(current.messages)} messages`;
    } else if (mail.months.done < mail.months.total) {
      detail = "queued";
    }
    lines.push(`${indent}months   ${bar(mail.months.done, mail.months.total)} ${counter(mail.months.done, mail.months.total, width)}  ${detail}`.trimEnd());
  }
  if (mail.seeds.total) {
    const current = mail.seeds.current;
    const spin = current?.state === "running" ? "{spin} " : "";
    const detail = current ? `${spin}"${current.topic ?? current.message_id}"` : mail.seeds.done < mail.seeds.total ? "queued" : "";
    lines.push(`${indent}threads  ${bar(mail.seeds.done, mail.seeds.total)} ${counter(mail.seeds.done, mail.seeds.total, width)}  ${detail}`.trimEnd());
  }
  const { messages, threads, authors } = mail.totals;
  if (live) lines.push(`${indent}so far   ${formatNumber(messages)} messages in ${formatNumber(threads)} threads from ${formatNumber(authors)} authors`);
  return lines;
}

/** What changes the mail's state, without its counters: a plain stream prints the bars again when it changes. */
export function mailKey(mail) {
  return [mail.months.done, mail.seeds.done, mail.months.current?.month, mail.months.current?.state, mail.seeds.current?.message_id].join("|");
}

/** The mail's totals. */
export function mailDone(mail, ms) {
  const { messages, threads, authors } = mail.totals;
  return `✔ ${formatNumber(messages)} messages in ${formatNumber(threads)} threads from ${formatNumber(authors)} authors (${formatDuration(ms)})`;
}

/** A log field's value: name=value, or name="quoted value". */
function field(line, name) {
  const match = new RegExp(`(?:^|\\s)${name}=(?:"((?:[^"\\\\]|\\\\.)*)"|(\\S+))`).exec(line);
  return match ? (match[1] ?? match[2]) : undefined;
}

function number(line, name) {
  const value = field(line, name);
  return value === undefined || !/^\d+$/.test(value) ? undefined : Number(value);
}

/**
 * A line of the history build's log (pghacker-core's telemetry: tracing's
 * text format, its message then its fields, with or without colors), as an
 * event: fetched, graph, walking, walked, built, installed, or a warning or
 * error; null for any other line.
 */
export function parseHistoryLine(raw) {
  const line = stripAnsi(raw).trim();
  if (!line) return null;
  if (line.includes("fetched the history's clone")) return { type: "fetched", refs: number(line, "refs"), secs: number(line, "secs") };
  if (line.includes("read the commit graph")) {
    return { type: "graph", commits: number(line, "commits"), lanes: number(line, "lanes"), refs: number(line, "refs") };
  }
  if (line.includes("walking the history")) {
    return {
      type: "walking", commits: number(line, "commits"), of: number(line, "of"),
      rate: number(line, "commits_per_sec"), secs: number(line, "secs"),
    };
  }
  if (line.includes("walked the history")) return { type: "walked", secs: number(line, "secs") };
  if (line.includes("installed the history")) return { type: "installed" };
  if (/\bbuilt\b/.test(line)) {
    const step = field(line, "step");
    if (step !== undefined) return { type: "built", what: "step", name: step, secs: number(line, "secs") };
    const constraint = field(line, "constraint");
    if (constraint !== undefined) return { type: "built", what: "constraint", name: constraint, secs: number(line, "secs") };
    // An index's field is its CREATE INDEX statement's head, unquoted: its name is the last word.
    const index = /(?:^|\s)index=(.+?)(?:\s+secs=\d+|$)/.exec(line);
    if (index) return { type: "built", what: "index", name: index[1].trim().split(/\s+/).pop(), secs: number(line, "secs") };
  }
  const level = /\b(ERROR|WARN)\b\s+(.*)$/.exec(line);
  if (level) return { type: level[1] === "ERROR" ? "error" : "warning", message: level[2] };
  return null;
}

/** The build steps after the walk, as the log's "built" lines mark their ends. */
export const BUILD_STEPS = ["writing the commits and lines", "file versions", "constraints and indexes"];

/** The history build's progress, from its log's events. */
export class HistoryProgress {
  constructor({ fetching = false, now = Date.now } = {}) {
    this.now = now;
    this.fetching = fetching;
    this.phase = "cloning";
    this.started = now();
    this.at = { cloning: this.started };
    this.commits = 0;
    this.total = 0;
    this.rate = 0;
    this.step = 0;
    this.built = 0;
    this.lastBuilt = "";
    this.cloneBytes = 0;
  }

  #enter(phase) {
    if (this.phase !== phase) this.at[phase] = this.now();
    this.phase = phase;
  }

  apply(event) {
    if (!event) return;
    switch (event.type) {
      case "fetched":
        this.#enter("reading");
        break;
      case "graph":
        this.#enter("walking");
        this.total = event.commits ?? this.total;
        break;
      case "walking":
        this.#enter("walking");
        this.commits = event.commits ?? this.commits;
        this.total = event.of ?? this.total;
        this.rate = event.rate ?? this.rate;
        break;
      case "walked":
        this.commits = this.total || this.commits;
        this.#enter("building");
        this.step = 1;
        break;
      case "built":
        this.#enter("building");
        if (event.what === "step") this.step = Math.max(this.step, /line/.test(event.name) ? 2 : 3);
        else {
          this.step = 3;
          this.built += 1;
          this.lastBuilt = event.name;
        }
        break;
      case "installed":
        this.#enter("installed");
        break;
      default:
    }
  }

  #since(phase, until) {
    return formatDuration((until ?? this.now()) - (this.at[phase] ?? this.now()));
  }

  /** The live lines: the clone, the walk and the build, each once it has started. */
  lines(indent = INDENT) {
    const order = ["cloning", "reading", "walking", "building", "installed"];
    const reached = (phase) => order.indexOf(this.phase) >= order.indexOf(phase);
    const cloneLabel = this.fetching ? "fetching" : "cloning ";
    const lines = [];
    const cloneEnd = this.at.reading;
    if (this.phase === "cloning") {
      lines.push(`${indent}${cloneLabel} {spin} ${formatBytes(this.cloneBytes)} ${this.fetching ? "in the clone" : "received"} (${this.#since("cloning")})`);
    } else {
      lines.push(`${indent}${cloneLabel} ✔ ${formatBytes(this.cloneBytes)} (${this.#since("cloning", cloneEnd)})`);
    }
    if (this.phase === "reading") lines.push(`${indent}walking  {spin} reading the commit graph`);
    if (this.phase === "walking") {
      const left = this.rate > 0 && this.total > this.commits ? `, ~${formatDuration(((this.total - this.commits) / this.rate) * 1000)} left` : "";
      const rate = this.rate > 0 ? ` (${formatNumber(this.rate)}/s${left})` : "";
      lines.push(`${indent}walking  ${bar(this.commits, this.total)} ${formatNumber(this.commits)}/${formatNumber(this.total)} commits${rate}`);
    } else if (reached("building")) {
      lines.push(`${indent}walking  ✔ ${formatNumber(this.total || this.commits)} commits (${this.#since("walking", this.at.building)})`);
    }
    if (this.phase === "building") {
      const extra = this.step === 3 && this.built ? ` (${formatNumber(this.built)} built: ${this.lastBuilt})` : "";
      lines.push(`${indent}building {spin} step ${this.step}/${BUILD_STEPS.length}: ${BUILD_STEPS[this.step - 1]}${extra}`);
    } else if (this.phase === "installed") {
      lines.push(`${indent}building ✔ installed in schema pghacker (${this.#since("building", this.at.installed)})`);
    }
    return lines;
  }

  /** What changes the build's state, without its counters. */
  key() {
    return `${this.phase}|${this.step}`;
  }
}

/** The bytes under `dir`, such as a clone as it grows; files that vanish meanwhile count as none. */
export function directorySize(dir) {
  let total = 0;
  let entries;
  try {
    entries = readdirSync(dir, { recursive: true, withFileTypes: true });
  } catch {
    return 0;
  }
  for (const entry of entries) {
    if (!entry.isFile()) continue;
    try {
      total += statSync(path.join(entry.parentPath ?? entry.path, entry.name)).size;
    } catch {
      // Gone already, such as a temporary pack.
    }
  }
  return total;
}

/** The installed history in brief: 62,114 commits on 40 branches and 400 tags, at master 4b97fd1. */
export function historyLine(history) {
  if (!history) return "no history";
  const refs = history.branches || history.tags
    ? ` on ${formatNumber(history.branches)} branches and ${formatNumber(history.tags)} tags` : "";
  const head = history.head ? `, at ${history.master ?? "master"} ${String(history.head).slice(0, 7)}` : "";
  const paths = history.paths?.length ? ` (only ${history.paths.join(", ")})` : "";
  return `${formatNumber(history.commits)} commits${refs}${head}${paths}`;
}

/** The order the graph's labels are listed in; any others follow, largest first. */
const LABEL_ORDER = ["Author", "Message", "Thread", "Patch", "Commit", "File", "Feature", "Decision", "Position", "Release"];

/** The graph's vertices by label: Author 363, Message 6,166, ... */
export function graphLabels(nodes) {
  const entries = Object.entries(nodes ?? {}).filter(([, count]) => Number(count) > 0);
  const rank = (label) => {
    const at = LABEL_ORDER.indexOf(label);
    return at < 0 ? LABEL_ORDER.length : at;
  };
  entries.sort(([a, x], [b, y]) => rank(a) - rank(b) || Number(y) - Number(x) || a.localeCompare(b));
  return entries.map(([label, count]) => `${label} ${formatNumber(count)}`).join(", ") || "empty";
}

/** Whether the graph has caught up: no changes waiting and no full sync requested. */
export function graphCaughtUp(graph) {
  return Number(graph?.pending_changes ?? 0) === 0 && !graph?.full_sync?.requested;
}

/** The graph step's live text. */
export function graphWaiting(graph) {
  const pending = Number(graph?.pending_changes ?? 0);
  if (graph?.full_sync?.requested) {
    const reason = graph.full_sync.reason ? ` (${graph.full_sync.reason})` : "";
    return pending ? `full sync${reason}, ${formatNumber(pending)} changes waiting` : `full sync${reason}`;
  }
  return `${formatNumber(pending)} changes waiting`;
}

/** The summary's last line: where to say how it went. */
export const FEEDBACK_LINE = "Tried it? Share what worked, what didn't and what you'd like next: https://github.com/Azure-Samples/pghacker-mcp/discussions";

/**
 * The summary: what works now, then the next step, extraction, which
 * needs a model. `ms` is how long loading took, or null if nothing needed
 * loading; `loaded` says whether the mail and the code are in;
 * `extraction` is ingest_status's; `hasToken`, whether the app has a
 * GitHub token for its agents.
 */
export function summaryLines({ loaded, ms, toolCount, extraction, hasToken }) {
  // Without a duration, nothing needed loading.
  const took = ms === null || ms === undefined ? "✔ Already loaded." : `✔ Loaded in ${formatDuration(ms)}.`;
  const lines = [];
  if (loaded.mail && loaded.code) {
    const tools = toolCount ? `All ${toolCount} tools` : "All the tools";
    lines.push(`${took} ${tools} work now: mail, code, SQL and the graph of authors, threads and commits.`);
  } else if (loaded.mail) {
    lines.push(`${took} The mail tools, SQL and the graph of authors and threads work now.`);
    lines.push("  The code tools need the commit history: npm run load -- --code-only.");
  } else if (loaded.code) {
    lines.push(`${took} The code tools, SQL and the graph of commits and files work now.`);
    lines.push("  The mail tools need the mail: npm run load -- --mail-only.");
  } else {
    return ["✖ Nothing is loaded yet. Run npm run load again: it skips the steps that are done."];
  }
  if (!loaded.mail) return lines;

  const pending = Number(extraction?.threads_pending ?? 0);
  const extracted = Number(extraction?.threads_extracted ?? 0);
  const findings = `${formatNumber(extraction?.features ?? 0)} features and ${formatNumber(extraction?.decisions ?? 0)} decisions`;
  if (pending === 0 && extracted > 0) {
    lines.push("", `✔ Extraction is done: ${formatNumber(extracted)} threads, ${findings}.`);
    return lines;
  }
  if (pending === 0) return lines;

  const crawl = extracted > 0
    ? [
      `npm run crawl -- --concurrency 8     the remaining ${formatNumber(pending)} threads`,
      "npm run crawl -- --dedup             then merge duplicate features and authors",
    ]
    : [
      "npm run crawl -- --pilot             10 threads in one session: check the results first",
      `npm run crawl -- --concurrency 8     then the rest of the ${formatNumber(pending)} threads`,
      "npm run crawl -- --dedup             then merge duplicate features and authors",
    ];
  lines.push("");
  if (extracted > 0) {
    lines.push("Next: finish the graph of features, decisions and authors' positions.");
    lines.push(`  Extraction has read ${formatNumber(extracted)} threads so far, with ${findings}.`);
  } else {
    lines.push("Next: build the full graph of features, decisions and authors' positions.");
  }
  if (hasToken) {
    lines.push("  Extraction runs crawler agents on the app's language model:");
    lines.push(...crawl.map((command) => `    ${command}`));
  } else {
    lines.push("  Extraction runs crawler agents on a language model (GitHub Copilot's, with a GitHub token):");
    lines.push("    1. Give the app a token, then restart it so the agents get it:");
    lines.push("         echo \"GITHUB_TOKEN=$(gh auth token)\" >> .env      (or any token with Copilot access)");
    lines.push("         npm run stop && npm start");
    lines.push("    2. Extract:");
    lines.push(...crawl.map((command) => `         ${command}`));
  }
  if (extracted === 0) {
    lines.push("  Until then, search_extracts and author_perspective return nothing,");
    lines.push("  and the graph has no Feature, Decision or Position vertices.");
  }
  return lines;
}
