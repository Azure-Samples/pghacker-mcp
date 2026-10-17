// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The home page's server in the Azure container, which the supervisor
// (scripts/container.mjs) runs as pgh-home on loopback, behind oauth2-proxy.
// It reads only the built page, and asks GitHub's API, without a token, for
// the stars of the repository the page links to (scripts/lib/github-stars.mjs).
// Its settings are its port, the page's directory, and the portal's origin
// (PGHACKER_PORTAL_URL), which the portal's share links go on to; the
// deployment's privacy statement (PGHACKER_PRIVACY_URL) and copyright link
// (PGHACKER_COPYRIGHT_URL and PGHACKER_COPYRIGHT_TEXT), if it has them, which
// the footer shows, and without which it has no such links; and, when the app
// has Application Insights, usage telemetry's (scripts/lib/usage.mjs), whose
// users' key is its one secret.
import { readFileSync } from "node:fs";
import http from "node:http";
import path from "node:path";
import { APP_ROOT } from "./lib/config.mjs";
import { HOME_DIR, createHomeHandler } from "./lib/edge.mjs";
import { createStarCounts } from "./lib/github-stars.mjs";
import { createUsage, readUsageSettings } from "./lib/usage.mjs";

const port = Number(process.env.PORT);
if (!Number.isInteger(port) || port < 1 || port > 65535) {
  throw new Error("PORT must be the home page's port.");
}
const root = process.env.PGHACKER_HOME_DIR || HOME_DIR;
const portal = process.env.PGHACKER_PORTAL_URL || "";
const privacy = process.env.PGHACKER_PRIVACY_URL || "";
const copyright = { url: process.env.PGHACKER_COPYRIGHT_URL || "", text: process.env.PGHACKER_COPYRIGHT_TEXT || "" };
const { version } = JSON.parse(readFileSync(path.join(APP_ROOT, "package.json"), "utf8"));
const usage = createUsage(readUsageSettings(), { role: "pghacker-home", version });

const server = http.createServer(
  createHomeHandler({ root, portal, privacy, copyright, onPage: usage.pageView, stars: createStarCounts() }),
);
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, () => {
    // The last page views go before the process does.
    server.close(() => usage.close().finally(() => process.exit(0)));
    server.closeIdleConnections();
    setTimeout(() => process.exit(0), 8000).unref();
  });
}
server.listen(port, "127.0.0.1", () => {
  console.log(`[home] The home page from ${root} on 127.0.0.1:${port}.`);
});
