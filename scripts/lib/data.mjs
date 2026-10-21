// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Whether the local database has data: the mail (pghacker.messages) and the
// commit history (pghacker.code_commits), asked of pghacker-mcp's query tool
// as any client would. npm start says when either is missing, and the local
// home page shows it (scripts/lib/edge.mjs, home/src/data-status.ts).

import { serviceUrl } from "./config.mjs";
import { parseCsv } from "./evalcheck.mjs";
import { callTool } from "./services.mjs";

/** One row: whether each table has a row at all, as 0 or 1. */
export const DATA_SQL = "SELECT (SELECT count(*) FROM (SELECT FROM messages LIMIT 1) m) AS mail, "
  + "(SELECT count(*) FROM (SELECT FROM code_commits LIMIT 1) c) AS code";

/** `{ mail, code }`, whether each is loaded, from the query tool's CSV. */
export function parseLoaded(csv) {
  const [header, row] = parseCsv(String(csv));
  const at = (name) => header?.indexOf(name) ?? -1;
  if (!row || at("mail") < 0 || at("code") < 0) throw new Error(`Unexpected answer from query: ${String(csv).slice(0, 200)}`);
  return { mail: Number(row[at("mail")]) > 0, code: Number(row[at("code")]) > 0 };
}

/** What's loaded, from the running pghacker-mcp. */
export async function probeData(config, { timeout = 5000, fetchImpl = fetch } = {}) {
  return parseLoaded(await callTool(serviceUrl(config.mcpPort), "query", { sql: DATA_SQL }, { timeout, fetchImpl }));
}

/**
 * The home page's data-status: what's loaded, when not everything is.
 * "empty", neither the mail nor the code; "mail", only the mail (the code
 * is missing); "code", only the code (the mail is missing); "ready", both.
 */
export function dataStatus({ mail, code }) {
  if (mail && code) return "ready";
  if (mail) return "mail";
  if (code) return "code";
  return "empty";
}

/** What npm start says when data is missing, and how to load it; null when both are loaded. */
export function dataNotice(loaded) {
  switch (dataStatus(loaded)) {
    case "empty":
      return "No data loaded yet: run npm run load to load the mail and the code.";
    case "mail":
      return "No code loaded yet: run npm run load -- --code-only to load PostgreSQL's commit history.";
    case "code":
      return "No mail loaded yet: run npm run load -- --mail-only to load pgsql-hackers.";
    default:
      return null;
  }
}
