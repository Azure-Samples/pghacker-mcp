// Extraction sessions: running them, and the scheduler that keeps them busy
// for as long as threads await extraction (npm run crawl -- --watch, which
// the Azure container runs). The scheduler has no caps of its own: its
// limits are Foundry's rate limit, found like TCP finds a link's capacity
// (additive increase, multiplicative decrease), and the container's memory.
import { setTimeout as delay } from "node:timers/promises";

/**
 * How many sessions may run: starts at `start`, gains one each
 * `increaseEveryMs` while every slot is busy and more work waits, halves on
 * congestion (Foundry's 429), and doesn't grow for `holdMs` after one (or
 * longer, as Foundry's Retry-After asks).
 */
export class Concurrency {
  constructor({ start = 8, max = 16, min = 1, increaseEveryMs = 60_000, now = Date.now() } = {}) {
    this.min = min;
    this.max = Math.max(min, max);
    this.limit = Math.min(this.max, Math.max(min, start));
    this.increaseEveryMs = increaseEveryMs;
    this.changedAt = now;
    this.holdUntil = now;
  }

  /** Foundry refused: halve, and hold. */
  congestion(now, retryAfterMs = 0) {
    if (now < this.holdUntil && now - this.changedAt < this.increaseEveryMs) {
      // One congestion episode halves once.
      this.holdUntil = Math.max(this.holdUntil, now + retryAfterMs);
      return this.limit;
    }
    this.limit = Math.max(this.min, Math.floor(this.limit / 2));
    this.changedAt = now;
    this.holdUntil = now + Math.max(retryAfterMs, this.increaseEveryMs);
    return this.limit;
  }

  /** A minute passed: grow by one if every slot was busy with more work waiting. */
  tick(now, saturated) {
    if (saturated && now >= this.holdUntil && now - this.changedAt >= this.increaseEveryMs && this.limit < this.max) {
      this.limit += 1;
      this.changedAt = now;
    }
    return this.limit;
  }

  /** Memory is short: no more than `n` sessions. */
  cap(now, n) {
    const capped = Math.max(this.min, Math.min(this.limit, n));
    if (capped < this.limit) {
      this.limit = capped;
      this.changedAt = now;
    }
    return this.limit;
  }
}

/** Sessions to start now: free slots, but no more than the work waiting needs. */
export function launches({ limit, running, available, perSession }) {
  if (available <= 0) return 0;
  return Math.max(0, Math.min(limit - running, Math.ceil(available / perSession)));
}

/** Whether an error is Foundry's rate limit. */
export function isRateLimited(error) {
  return /\b429\b|rate.?limit|too many requests/i.test(String(error?.message ?? error ?? ""));
}

/** Retry-After from response headers, in milliseconds (retry-after-ms first). */
export function retryAfterMs(headers) {
  const ms = Number(headers.get("retry-after-ms"));
  if (Number.isFinite(ms) && ms > 0) return ms;
  const seconds = Number(headers.get("retry-after"));
  return Number.isFinite(seconds) && seconds > 0 ? seconds * 1000 : 0;
}

/**
 * Asks Foundry's `deployment` for a few tokens: congested when it answers
 * 429, or reports less than 5% of its token rate left.
 */
export async function probeFoundry({ endpoint, deployment, key, fetchImpl = fetch }) {
  const response = await fetchImpl(`${endpoint}/openai/v1/responses`, {
    method: "POST",
    headers: { "content-type": "application/json", "api-key": key },
    body: JSON.stringify({ model: deployment, input: "Reply with OK.", max_output_tokens: 16 }),
    signal: AbortSignal.timeout(60_000),
  });
  await response.arrayBuffer();
  const remaining = Number(response.headers.get("x-ratelimit-remaining-tokens"));
  const limit = Number(response.headers.get("x-ratelimit-limit-tokens"));
  const low = Number.isFinite(remaining) && Number.isFinite(limit) && limit > 0 && remaining / limit < 0.05;
  return {
    status: response.status,
    congested: response.status === 429 || low,
    retryAfterMs: retryAfterMs(response.headers),
  };
}

/** The share of the container's memory in use, or null where it isn't known. */
export function memoryUsed(proc = process) {
  const limit = proc.constrainedMemory?.() ?? 0;
  const available = proc.availableMemory?.() ?? 0;
  if (!limit || !available || limit > 2 ** 52) return null;
  return Math.min(1, Math.max(0, 1 - available / limit));
}

/** Runs one task in a new session of `agent`; never throws. */
export async function runSession(client, agent, task, { model, title, timeoutMs, log = console.log }) {
  const started = Date.now();
  let session;
  try {
    session = await client.createSessionForAgent(agent, { model, title });
    log(`${title}: session ${session.sessionId} started.`);
    const reply = await session.sendAndWait(task, timeoutMs);
    const seconds = Math.round((Date.now() - started) / 1000);
    log(`${title} finished in ${seconds}s:\n${(reply ?? "(no reply)").trim().slice(0, 1500)}\n`);
    return { sessionId: session.sessionId, ok: true, seconds, reply };
  } catch (error) {
    log(`${title} failed: ${error.message}`);
    return { sessionId: session?.sessionId, ok: false, error: error.message };
  }
}

/**
 * The scheduler: every `tickMs`, reads the extraction backlog, probes
 * Foundry once a minute, and starts extraction sessions of `perSession`
 * threads while the concurrency limit and memory allow; starts a
 * deduplication session after `dedupEvery` newly extracted threads, or when
 * the backlog empties. Runs until `signal` aborts.
 */
export async function watch({
  status,
  extract,
  dedup,
  probe = null,
  memory = () => null,
  log = console.log,
  signal,
  now = Date.now,
  sleep = (ms) => delay(ms, undefined, { signal }).catch(() => {}),
  options = {},
}) {
  const {
    start = 8, max = 16, perSession = 8, tickMs = 30_000, probeEveryMs = 60_000, increaseEveryMs = 60_000,
    dedupEvery = 100, memoryHold = 0.85, memoryShed = 0.92,
  } = options;
  const control = new Concurrency({ start, max, increaseEveryMs, now: now() });
  let running = 0;
  let reserved = 0;
  let sequence = 0;
  let failures = 0;
  let pauseUntil = 0;
  let lastProbe = -Infinity;
  let deduplicating = null;
  let extractedAtDedup = null;
  const sessions = new Set();
  log(`Watching: ${control.limit} sessions to start, up to ${max}, ${perSession} threads each.`);

  while (!signal?.aborted) {
    let backlog;
    try {
      backlog = (await status()).extraction;
    } catch (error) {
      log(`ingest_status failed: ${error.message}`);
      await sleep(tickMs);
      continue;
    }
    const pending = backlog.threads_pending + backlog.threads_changed_since_extraction;
    const at = now();
    if (probe && at - lastProbe >= probeEveryMs) {
      lastProbe = at;
      try {
        const answer = await probe();
        if (answer?.congested) {
          const limit = control.congestion(at, answer.retryAfterMs);
          log(`Foundry is at its rate limit (HTTP ${answer.status}): up to ${limit} sessions.`);
        }
      } catch (error) {
        log(`The Foundry probe failed: ${error.message}`);
      }
    }
    const used = memory();
    if (used !== null && used >= memoryShed) {
      const limit = control.cap(at, running - 1);
      log(`Memory is ${Math.round(used * 100)}% used: up to ${limit} sessions.`);
    }
    control.tick(at, running >= control.limit && pending - reserved > 0);

    const memoryOk = used === null || used < memoryHold;
    const count = at >= pauseUntil && memoryOk
      ? launches({ limit: control.limit, running, available: pending - reserved, perSession })
      : 0;
    for (let i = 0; i < count; i++) {
      const quota = Math.min(perSession, pending - reserved);
      reserved += quota;
      running += 1;
      sequence += 1;
      const session = Promise.resolve()
        .then(() => extract(quota, sequence))
        .then((result) => {
          if (result?.ok) {
            failures = 0;
            return;
          }
          if (isRateLimited(result?.error)) control.congestion(now());
          failures += 1;
          if (failures >= 3) {
            const wait = Math.min(30 * 60_000, 60_000 * 2 ** Math.min(failures - 3, 5));
            pauseUntil = now() + wait;
            log(`${failures} sessions in a row failed; pausing ${Math.round(wait / 60_000)} min.`);
          }
        })
        .catch((error) => log(`An extraction session failed: ${error.message}`))
        .finally(() => {
          reserved -= quota;
          running -= 1;
          sessions.delete(session);
        });
      sessions.add(session);
    }

    extractedAtDedup ??= backlog.threads_extracted;
    const fresh = backlog.threads_extracted - extractedAtDedup;
    if (!deduplicating && fresh > 0 && (fresh >= dedupEvery || (pending === 0 && running === 0))) {
      extractedAtDedup = backlog.threads_extracted;
      deduplicating = Promise.resolve()
        .then(() => dedup())
        .catch((error) => log(`The deduplication session failed: ${error.message}`))
        .finally(() => {
          deduplicating = null;
        });
    }
    await sleep(tickMs);
  }
  return { sessions, deduplicating };
}
