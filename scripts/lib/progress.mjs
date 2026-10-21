// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Console progress for the launcher's long steps: npm start in the
// background, and npm run load. On a terminal, a block of live lines
// redraws in place at most 4 times a second, with a spinner where a line
// says {spin}. Otherwise, such as in CI or through `| tee`, the block prints
// as plain lines without ANSI codes: the lines that changed, when its
// state changes, and every 10 seconds while it doesn't.

export const SPINNER = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
const REDRAW_MS = 250;
const PLAIN_MS = 10_000;
const ANSI = /\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07/g;

/** `text` without ANSI escape codes. */
export function stripAnsi(text) {
  return String(text).replace(ANSI, "");
}

/** `n` with thousands separators: 62,114. */
export function formatNumber(n) {
  return Number(n ?? 0).toLocaleString("en-US");
}

/** A duration as people read it: 12 s, 3 m 10 s, 1 h 05 m. */
export function formatDuration(ms) {
  const total = Math.max(0, Math.round(ms / 1000));
  const pad = (n) => String(n).padStart(2, "0");
  if (total < 60) return `${total} s`;
  if (total < 3600) return `${Math.floor(total / 60)} m ${pad(total % 60)} s`;
  return `${Math.floor(total / 3600)} h ${pad(Math.floor((total % 3600) / 60))} m`;
}

/** A size in decimal units: 412 MB. */
export function formatBytes(bytes) {
  const n = Number(bytes ?? 0);
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${Math.round(n / 1e6)} MB`;
  if (n >= 1e3) return `${Math.round(n / 1e3)} KB`;
  return `${n} B`;
}

/** A bar of `width` cells, full only when `done` reaches `total`. */
export function bar(done, total, width = 15) {
  let filled = total > 0 ? Math.round((width * Math.min(done, total)) / total) : 0;
  if (done < total) filled = Math.min(filled, width - 1);
  return "█".repeat(filled) + "░".repeat(width - filled);
}

/** `line` cut to `columns` characters, so that a redraw never wraps. */
function fit(line, columns) {
  const characters = [...line];
  return characters.length > columns ? `${characters.slice(0, Math.max(1, columns - 1)).join("")}…` : line;
}

/**
 * Prints permanent lines, and a block of live lines below them. `live(lines,
 * key)` sets the block: lines, or a function that returns them, evaluated
 * at each redraw, so that elapsed times keep moving; `key`, the state they
 * show without their counters (by default the lines themselves), decides
 * when a plain stream prints them again. `end(lines)` replaces the block
 * with permanent lines.
 */
export class Progress {
  constructor({ stream = process.stdout, tty, now = Date.now, timer = true } = {}) {
    this.stream = stream;
    this.tty = tty ?? (Boolean(stream.isTTY) && process.env.TERM !== "dumb");
    this.now = now;
    this.timer = timer;
    this.render = null;
    this.key = undefined;
    this.drawn = 0;
    this.frame = 0;
    this.lastDraw = 0;
    this.lastPlain = 0;
    this.plainKey = undefined;
    this.plainLines = [];
    this.interval = null;
  }

  #lines() {
    const lines = typeof this.render === "function" ? this.render() : this.render;
    return (Array.isArray(lines) ? lines : [lines]).filter((line) => line !== undefined && line !== null);
  }

  #erase() {
    if (!this.drawn) return "";
    const up = this.drawn > 1 ? `\x1b[${this.drawn - 1}A` : "";
    this.drawn = 0;
    return `\r${up}\x1b[J`;
  }

  #draw() {
    const columns = Math.max(20, (this.stream.columns || 80) - 1);
    const spin = SPINNER[this.frame++ % SPINNER.length];
    const lines = this.#lines().map((line) => fit(String(line).replaceAll("{spin}", spin), columns));
    this.stream.write(this.#erase() + lines.join("\n"));
    this.drawn = lines.length;
    this.lastDraw = this.now();
  }

  /** Prints the block's lines that changed since it last printed them. */
  #plain(force) {
    const lines = this.#lines().map((line) => plainLine(line));
    const key = this.key === undefined ? lines.join("\n") : String(typeof this.key === "function" ? this.key() : this.key);
    if (!force && key === this.plainKey && this.now() - this.lastPlain < PLAIN_MS) return;
    this.plainKey = key;
    this.lastPlain = this.now();
    const changed = lines.filter((line, i) => line !== this.plainLines[i]);
    this.plainLines = lines;
    if (changed.length) this.stream.write(`${changed.join("\n")}\n`);
  }

  /** Redraws the block now, or prints it if it's due; for the timer, and for tests. */
  tick() {
    if (this.render === null) return;
    if (this.tty) this.#draw();
    else this.#plain(false);
  }

  live(lines, key) {
    const first = this.render === null;
    this.render = lines;
    this.key = key;
    if (this.tty) {
      if (first || this.now() - this.lastDraw >= REDRAW_MS) this.#draw();
    } else {
      this.#plain(first);
    }
    if (this.timer && !this.interval) {
      this.interval = setInterval(() => this.tick(), REDRAW_MS);
      this.interval.unref();
    }
  }

  /** Prints permanent lines above the live block. */
  print(...lines) {
    const text = lines.map((line) => (this.tty ? line : plainLine(line))).join("\n");
    if (this.tty) {
      this.stream.write(`${this.#erase()}${text}\n`);
      if (this.render !== null) this.#draw();
    } else {
      this.stream.write(`${text}\n`);
    }
  }

  /** Ends the live block, leaving `lines` in its place. */
  end(...lines) {
    if (this.interval) clearInterval(this.interval);
    this.interval = null;
    if (this.tty) this.stream.write(this.#erase());
    this.render = null;
    this.key = undefined;
    this.plainKey = undefined;
    this.plainLines = [];
    if (lines.length) this.print(...lines);
  }
}

/** A live line as a plain stream gets it: without the spinner or ANSI codes. */
function plainLine(line) {
  return stripAnsi(String(line).replace(/\{spin\} ?/g, ""));
}
