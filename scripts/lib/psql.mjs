// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// psql, pg_restore and other commands for the Azure jobs (probe, restore,
// egress check), whose image has postgresql-client-17.
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";

/**
 * `url` verifying the server as the services do: libpq's `require` only
 * encrypts, so require becomes verify-full against the system's CAs.
 */
export function verifiedUrl(url) {
  const parsed = new URL(url);
  if (parsed.searchParams.get("sslmode") === "require") {
    parsed.searchParams.set("sslmode", "verify-full");
    parsed.searchParams.set("sslrootcert", "system");
  }
  // libpq URIs percent-decode query values, but do not decode form-style '+'.
  parsed.search = [...parsed.searchParams]
    .map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(value)}`).join("&");
  return parsed.toString();
}

/** Runs a command; resolves with its exit code and output, never rejects on failure. */
export function run(command, args, { env = process.env, timeoutMs = 0 } = {}) {
  return new Promise((resolve) => {
    const child = spawn(command, args, { env, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    const timer = timeoutMs ? setTimeout(() => child.kill("SIGKILL"), timeoutMs) : null;
    child.once("error", (error) => resolve({ code: -1, stdout, stderr: `${stderr}${error.message}` }));
    child.once("close", (code, signal) => {
      if (timer) clearTimeout(timer);
      resolve({ code: code ?? -1, signal, stdout, stderr });
    });
  });
}

/** One SQL script through psql, stopping at the first error; its output as text. */
export async function psql(url, sql, { timeoutMs = 600_000 } = {}) {
  const result = await run("psql", [
    "--no-psqlrc", "--quiet", "--tuples-only", "--no-align", "--set", "ON_ERROR_STOP=1",
    "--dbname", verifiedUrl(url), "--command", sql,
  ], { env: { ...process.env, PGCONNECT_TIMEOUT: "30" }, timeoutMs });
  if (result.code !== 0) throw new Error(`psql failed: ${result.stderr.trim().slice(0, 2000)}`);
  return result.stdout.trim();
}

/** A query whose one value is JSON. */
export async function psqlJson(url, sql) {
  const text = await psql(url, sql);
  return text ? JSON.parse(text) : null;
}

/** A dedicated autocommit session; every query is sent as a separate statement. */
export function psqlSession(url, { env = process.env } = {}) {
  const child = spawn("psql", [
    "--no-psqlrc", "--quiet", "--tuples-only", "--no-align",
    "--set", "ON_ERROR_STOP=1", "--set", "AUTOCOMMIT=on",
    "--set", "VERBOSITY=verbose", "--dbname", verifiedUrl(url),
  ], { env: { ...env, PGCONNECT_TIMEOUT: "30" }, stdio: ["pipe", "pipe", "pipe"] });
  let pending = null;
  let buffer = "";
  let stderr = "";
  let failure = null;
  let ended = false;
  let resolveClosed;
  const closed = new Promise((resolve) => { resolveClosed = resolve; });

  function error(message) {
    const parsed = new URL(url);
    let detail = stderr.replaceAll(url, "<database URL>").replaceAll(verifiedUrl(url), "<database URL>");
    if (parsed.password) {
      detail = detail.replaceAll(parsed.password, "***").replaceAll(decodeURIComponent(parsed.password), "***");
    }
    const result = new Error(`${message}${detail ? `: ${detail.trim().slice(-4000)}` : ""}`);
    result.sqlstate = /(?:ERROR|FATAL):\s+([0-9A-Z]{5}):/.exec(detail)?.[1] ?? null;
    return result;
  }

  function fail(reason) {
    failure = reason;
    if (pending) {
      clearTimeout(pending.timer);
      pending.reject(reason);
      pending = null;
    }
  }

  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (text) => { stderr = (stderr + text).slice(-16000); });
  child.stdout.on("data", (text) => {
    buffer += text;
    let newline;
    while ((newline = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, newline).replace(/\r$/, "");
      buffer = buffer.slice(newline + 1);
      if (!pending) continue;
      if (line === pending.marker) {
        clearTimeout(pending.timer);
        const { resolve, lines } = pending;
        pending = null;
        resolve(lines.join("\n").trim());
      } else {
        pending.lines.push(line);
      }
    }
  });
  child.once("error", (cause) => {
    fail(new Error(`cannot start psql: ${cause.message}`, { cause }));
  });
  child.once("close", (code, signal) => {
    ended = true;
    if (code !== 0 || pending) fail(error(`psql session ended (code ${code}, signal ${signal})`));
    resolveClosed();
  });
  child.stdin.on("error", (cause) => {
    fail(new Error(`cannot write to psql: ${cause.message}`, { cause }));
  });

  return {
    async query(sql, { timeoutMs = 0 } = {}) {
      if (failure) throw failure;
      if (ended) throw new Error("psql session is closed");
      if (pending) throw new Error("psql session queries must be sequential");
      if (!sql.trim()) throw new Error("psql statement is empty");
      stderr = "";
      return new Promise((resolve, reject) => {
        const marker = `pgh_statement_${randomUUID().replaceAll("-", "")}`;
        const timer = timeoutMs ? setTimeout(() => child.kill("SIGINT"), timeoutMs) : null;
        pending = { marker, resolve, reject, lines: [], timer };
        child.stdin.write(`${sql.trimEnd()}\n;\n\\echo ${marker}\n`);
      });
    },
    async json(sql, options) {
      return JSON.parse(await this.query(sql, options));
    },
    cancel() {
      if (!ended) child.kill("SIGINT");
    },
    async close() {
      if (!ended) {
        if (pending) child.kill("SIGINT");
        else child.stdin.end("\\q\n");
      }
      await closed;
      if (failure) throw failure;
    },
  };
}
