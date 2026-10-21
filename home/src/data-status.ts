// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/*
 * Whether the local database has its data. Locally, npm start's server adds
 * a data-status meta tag to the page when the mail or the code is missing
 * (scripts/lib/edge.mjs, withDataStatus): "empty", neither is loaded;
 * "mail", only the mail is (the code is missing); "code", only the code is
 * (the mail is missing). With both loaded, and always in Azure, there's no
 * tag, and the MCP server's panel says it's available.
 */

export type DataStatus = "empty" | "mail" | "code";

/** What the page reads of the data-status meta tag. */
export interface DataStatusMeta {
  readonly content: string;
}

/** What the panel says instead of "Available": a pill, and a line with the command that loads what's missing. */
export interface DataNotice {
  readonly pill: string;
  readonly text: string;
  readonly command: string;
}

const NOTICES: Record<DataStatus, DataNotice> = {
  empty: {
    pill: "No data loaded",
    text: "The database is empty, so the tools find nothing. Load the mail and the code with",
    command: "npm run load",
  },
  mail: {
    pill: "No code loaded",
    text: "PostgreSQL's commit history isn't loaded, so the code tools find nothing. Load it with",
    command: "npm run load -- --code-only",
  },
  code: {
    pill: "No mail loaded",
    text: "The pgsql-hackers mail isn't loaded, so the mail tools find nothing. Load it with",
    command: "npm run load -- --mail-only",
  },
};

/** The status the meta tag gives, if it's one of the three; otherwise undefined, and the panel says "Available". */
export function readDataStatus(meta: DataStatusMeta | null): DataStatus | undefined {
  const content = meta?.content;
  return content === "empty" || content === "mail" || content === "code" ? content : undefined;
}

/** What the panel says for `status`. */
export function dataNotice(status: DataStatus): DataNotice {
  return NOTICES[status];
}
