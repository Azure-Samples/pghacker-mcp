// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { Concurrency, isRateLimited, lanesLine, launches, retryAfterMs, watch } from "../scripts/lib/crawl.mjs";

const MINUTE = 60_000;

test("concurrency grows by one a minute while saturated, halves on congestion, and holds after it", () => {
  const control = new Concurrency({ start: 8, max: 16, now: 0 });
  assert.equal(control.tick(30_000, true), 8, "not before a minute");
  assert.equal(control.tick(MINUTE, false), 8, "not while slots are free");
  assert.equal(control.tick(MINUTE, true), 9);
  assert.equal(control.tick(MINUTE + 1, true), 9, "one step a minute");
  assert.equal(control.tick(2 * MINUTE, true), 10);
  assert.equal(control.congestion(2 * MINUTE + 5_000), 5);
  assert.equal(control.congestion(2 * MINUTE + 10_000), 5, "one congestion episode halves once");
  assert.equal(control.tick(3 * MINUTE, true), 5, "held for a minute after congestion");
  assert.equal(control.tick(3 * MINUTE + 10_000, true), 6);
  assert.equal(control.congestion(4 * MINUTE + 20_000, 5 * MINUTE), 3);
  assert.equal(control.tick(8 * MINUTE, true), 3, "held as long as Retry-After asks");
  assert.equal(control.tick(9 * MINUTE + 20_001, true), 4);
  assert.equal(control.cap(10 * MINUTE, 2), 2);
  assert.equal(control.cap(10 * MINUTE, 0), 1, "never below one");
  const top = new Concurrency({ start: 16, max: 16, now: 0 });
  assert.equal(top.tick(MINUTE, true), 16, "never above max");
});

test("sessions start for free slots, but no more than the waiting threads need", () => {
  assert.equal(launches({ limit: 8, running: 0, available: 100, perSession: 8 }), 8);
  assert.equal(launches({ limit: 8, running: 6, available: 100, perSession: 8 }), 2);
  assert.equal(launches({ limit: 8, running: 0, available: 9, perSession: 8 }), 2);
  assert.equal(launches({ limit: 8, running: 9, available: 50, perSession: 8 }), 0);
  assert.equal(launches({ limit: 8, running: 0, available: 0, perSession: 8 }), 0);
});

test("rate limits are recognized, with Retry-After in either form", () => {
  assert.ok(isRateLimited(new Error("Request failed with status 429")));
  assert.ok(isRateLimited("Rate limit reached for gpt-6-luna"));
  assert.ok(!isRateLimited(new Error("connection reset")));
  assert.equal(retryAfterMs(new Headers({ "retry-after-ms": "1500" })), 1500);
  assert.equal(retryAfterMs(new Headers({ "retry-after": "7" })), 7000);
  assert.equal(retryAfterMs(new Headers()), 0);
});

/** A clock that each sleep advances, and an abort after `ticks` ticks. */
function harness(ticks, tickMs = 30_000) {
  let clock = 0;
  let count = 0;
  const controller = new AbortController();
  return {
    now: () => clock,
    signal: controller.signal,
    sleep: async (ms) => {
      // Let the sessions' promises settle, as real time would.
      await new Promise((resolve) => setImmediate(resolve));
      clock += ms ?? tickMs;
      count += 1;
      if (count >= ticks) controller.abort();
    },
  };
}

test("the scheduler keeps sessions busy, grows while saturated, backs off on Foundry's 429 and deduplicates new extracts", async () => {
  const time = harness(8);
  const pending = 400;
  let extracted = 0;
  const started = [];
  let dedups = 0;
  let probes = 0;
  const logs = [];
  const result = await watch({
    status: async () => ({ extraction: { threads_pending: pending, threads_changed_since_extraction: 0, threads_extracted: extracted } }),
    // Sessions never end in this test: the running count shows the limit.
    extract: (quota) => {
      started.push(quota);
      return new Promise(() => {});
    },
    dedup: async () => {
      dedups += 1;
      return { ok: true };
    },
    probe: async () => {
      probes += 1;
      extracted += 150;
      return probes === 4 ? { status: 429, congested: true, retryAfterMs: 0 } : { status: 200, congested: false };
    },
    log: (line) => logs.push(line),
    signal: time.signal,
    now: time.now,
    sleep: time.sleep,
    options: { start: 4, max: 6, perSession: 8, tickMs: 30_000 },
  });
  // 4 at once, then one more at each saturated minute up to 6; the 429 at
  // minute 3 halves the limit, which then grows again, below the 6 running.
  assert.equal(started.length, 6);
  assert.ok(started.every((quota) => quota === 8));
  assert.equal(result.sessions.size, 6);
  assert.ok(logs.some((line) => line.includes("rate limit (HTTP 429): up to 3 sessions")), logs.join("\n"));
  assert.equal(dedups, 4, "each time 100 or more threads were extracted since the last");
  assert.equal(probes, 4, "once a minute");
});

test("the scheduler pauses after failures in a row and starts only what the backlog needs", async () => {
  const time = harness(2);
  const started = [];
  const logs = [];
  await watch({
    status: async () => ({ extraction: { threads_pending: 20, threads_changed_since_extraction: 0, threads_extracted: 0 } }),
    extract: async (quota) => {
      started.push(quota);
      return { ok: false, error: "the model returned nothing" };
    },
    dedup: async () => ({ ok: true }),
    log: (line) => logs.push(line),
    signal: time.signal,
    now: time.now,
    sleep: time.sleep,
    options: { start: 8, max: 8, perSession: 8, tickMs: 30_000 },
  });
  // 20 threads need three sessions; all three fail, which pauses the
  // scheduler for a minute: nothing starts at the next tick.
  assert.deepEqual(started, [8, 8, 4]);
  assert.ok(logs.some((line) => line.includes("pausing")), logs.join("\n"));
});

const LANES = {
  order: "lanes",
  new_mail: {
    days: 7, share: 0.5, threads: 195, leased: 4, messages: 3609,
    oldest_activity_at: "2026-10-09T04:28:48Z", leased_since_start: 12,
  },
  backfill: {
    threads: 44640, leased: 9, messages: 321528,
    newest_activity_at: "2026-10-07T04:53:38.123Z", leased_since_start: 13,
  },
};

test("the lanes read in a line, and nothing without them", () => {
  assert.equal(
    lanesLine(LANES),
    "new mail 195 threads (4 leased, oldest activity 2026-10-09 04:28), backfill 44640 threads " +
      "(9 leased, newest activity 2026-10-07 04:53), leased since start 12 new mail and 13 backfill",
  );
  assert.equal(lanesLine(undefined), "");
  const empty = { ...LANES, new_mail: { ...LANES.new_mail, threads: 0, oldest_activity_at: null } };
  assert.match(lanesLine(empty), /new mail 0 threads \(4 leased, oldest activity none\)/);
});

test("the scheduler logs the backlog and its lanes every 10 minutes", async () => {
  const time = harness(25);
  const logs = [];
  await watch({
    status: async () => ({ extraction: { threads_pending: 300, threads_changed_since_extraction: 20, threads_extracted: 0, lanes: LANES } }),
    extract: () => new Promise(() => {}),
    dedup: async () => ({ ok: true }),
    log: (line) => logs.push(line),
    signal: time.signal,
    now: time.now,
    sleep: time.sleep,
    options: { start: 2, max: 2, perSession: 8, tickMs: 30_000 },
  });
  // 25 ticks of 30 seconds: at 0 and at 10 minutes.
  const reports = logs.filter((line) => line.startsWith("Backlog:"));
  assert.equal(reports.length, 2, logs.join("\n"));
  assert.equal(reports[0], `Backlog: 320 threads, 0 sessions running; ${lanesLine(LANES)}.`);
  assert.ok(reports[1].startsWith("Backlog: 320 threads, 2 sessions running; new mail 195 threads"), reports[1]);
});
