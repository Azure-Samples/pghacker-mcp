// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { Progress, bar, formatBytes, formatDuration, formatNumber, stripAnsi } from "../scripts/lib/progress.mjs";

/** A stream that keeps what's written, as a terminal or a pipe. */
function stream(isTTY, columns = 80) {
  return { isTTY, columns, text: "", write(chunk) { this.text += chunk; } };
}

test("numbers have thousands separators, durations read as people say them", () => {
  assert.equal(formatNumber(62114), "62,114");
  assert.equal(formatNumber(879), "879");
  assert.equal(formatNumber(undefined), "0");
  assert.equal(formatDuration(0), "0 s");
  assert.equal(formatDuration(12_400), "12 s");
  assert.equal(formatDuration(65_000), "1 m 05 s");
  assert.equal(formatDuration(190_000), "3 m 10 s");
  assert.equal(formatDuration(432_000), "7 m 12 s");
  assert.equal(formatDuration(3_900_000), "1 h 05 m");
  assert.equal(formatBytes(412_345_678), "412 MB");
  assert.equal(formatBytes(1_234_567_890), "1.2 GB");
  assert.equal(formatBytes(2048), "2 KB");
});

test("a bar fills in proportion, and is full only when done", () => {
  assert.equal(bar(0, 2), "░".repeat(15));
  assert.equal(bar(1, 2).length, 15);
  assert.equal(bar(13, 14), `${"█".repeat(14)}░`);
  assert.equal(bar(14, 14), "█".repeat(15));
  assert.equal(bar(5, 0), "░".repeat(15));
  assert.equal(bar(35_000, 62_114, 15), `${"█".repeat(8)}${"░".repeat(7)}`);
});

test("ANSI codes are stripped", () => {
  assert.equal(stripAnsi("\x1b[2m2026\x1b[0m \x1b[32m INFO\x1b[0m walked"), "2026  INFO walked");
});

test("on a terminal, the live block redraws in place, at most 4 times a second, with a spinner", () => {
  let now = 1000;
  const out = stream(true);
  const progress = new Progress({ stream: out, now: () => now, timer: false });
  progress.print("[2/5] Mail");
  progress.live(["a {spin} 1", "b"]);
  assert.match(out.text, /^\[2\/5\] Mail\na ⠋ 1\nb$/);
  const before = out.text;
  now += 100;
  progress.live(["a {spin} 2", "b"]);
  assert.equal(out.text, before, "no redraw within 250 ms");
  now += 200;
  progress.live(["a {spin} 3", "b"]);
  // Up one line, to its start, cleared, then the block again.
  assert.ok(out.text.endsWith("\r\x1b[1A\x1b[Ja ⠙ 3\nb"), JSON.stringify(out.text));
  progress.print("✖ failed");
  assert.ok(out.text.endsWith("\r\x1b[1A\x1b[J✖ failed\na ⠹ 3\nb"), JSON.stringify(out.text));
  progress.end("✔ done");
  assert.ok(out.text.endsWith("\r\x1b[1A\x1b[J✔ done\n"), JSON.stringify(out.text));
});

test("a live line longer than the terminal is cut, so that it never wraps", () => {
  const out = stream(true, 30);
  const progress = new Progress({ stream: out, timer: false });
  progress.live(["x".repeat(100)]);
  assert.equal(out.text, `${"x".repeat(28)}…`);
});

test("without a terminal, live lines print plainly: on each change of state, and every 10 s", () => {
  let now = 0;
  const out = stream(false);
  const progress = new Progress({ stream: out, now: () => now, timer: false });
  progress.live(["walking {spin} 5,000/62,114"], "walking");
  progress.live(["walking {spin} 10,000/62,114"], "walking");
  now += 9_000;
  progress.tick();
  assert.equal(out.text, "walking 5,000/62,114\n");
  now += 2_000;
  progress.live(["walking {spin} 15,000/62,114"], "walking");
  assert.equal(out.text, "walking 5,000/62,114\nwalking 15,000/62,114\n");
  progress.live(["building {spin} step 1/3"], "building");
  assert.match(out.text, /building step 1\/3\n$/);
  progress.end("\x1b[32m✔\x1b[0m built");
  assert.ok(out.text.endsWith("✔ built\n"));
  assert.doesNotMatch(out.text, /\x1b/);
});

test("without a terminal, only the lines that changed print again", () => {
  const out = stream(false);
  const progress = new Progress({ stream: out, timer: false });
  progress.live(["cloning  {spin} 0 B received", "walking"], "cloning");
  progress.live(["cloning  ✔ 412 MB", "walking  {spin} reading the commit graph"], "reading");
  progress.live(["cloning  ✔ 412 MB", "walking  ✔ 62,114 commits", "building {spin} step 1/3"], "building");
  assert.equal(out.text, [
    "cloning  0 B received", "walking",
    "cloning  ✔ 412 MB", "walking  reading the commit graph",
    "walking  ✔ 62,114 commits", "building step 1/3", "",
  ].join("\n"));
});

test("live lines may be a function, evaluated at each redraw", () => {
  let now = 0;
  const out = stream(true);
  const progress = new Progress({ stream: out, now: () => now, timer: false });
  progress.live(() => `${now / 1000} s`);
  now = 2000;
  progress.tick();
  assert.ok(out.text.endsWith("2 s"));
});
