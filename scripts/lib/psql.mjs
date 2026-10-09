// psql, pg_restore and other commands for the Azure jobs (probe, restore),
// whose image has postgresql-client-17.
import { spawn } from "node:child_process";

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
