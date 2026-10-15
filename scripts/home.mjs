// The home page's server in the Azure container, which the supervisor
// (scripts/container.mjs) runs as pgh-home on loopback, behind oauth2-proxy.
// It reads only the built page and has no other settings or secrets.
import http from "node:http";
import { HOME_DIR, createHomeHandler } from "./lib/edge.mjs";

const port = Number(process.env.PORT);
if (!Number.isInteger(port) || port < 1 || port > 65535) {
  throw new Error("PORT must be the home page's port.");
}
const root = process.env.PGHACKER_HOME_DIR || HOME_DIR;

const server = http.createServer(createHomeHandler({ root }));
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, () => {
    server.close(() => process.exit(0));
    server.closeIdleConnections();
    setTimeout(() => process.exit(0), 5000).unref();
  });
}
server.listen(port, "127.0.0.1", () => {
  console.log(`[home] The home page from ${root} on 127.0.0.1:${port}.`);
});
