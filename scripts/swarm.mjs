// The Azure deployment's PilotSwarm worker and portal, which the supervisor
// (scripts/container.mjs) runs as pgh-agent with only their environment.
// The portal listens on 127.0.0.1 behind oauth2-proxy, which signs users in
// with GitHub and passes who they are in headers (PORTAL_AUTH_PROVIDER=proxy).
import { once } from "node:events";
import os from "node:os";
import { setTimeout as delay } from "node:timers/promises";

const env = process.env;
const log = (message) => console.log(`[pghacker-mcp] ${message}`);

const { PilotSwarmWorker } = await import("pilotswarm-sdk");
const worker = new PilotSwarmWorker({
  store: env.DATABASE_URL,
  workerNodeId: `pghacker-${os.hostname()}`,
  sessionStateDir: env.SESSION_STATE_DIR,
  pluginDirs: [env.PLUGIN_DIRS],
  modelProvidersPath: env.PS_MODEL_PROVIDERS_PATH,
  disableManagementAgents: true,
  logLevel: "error",
});
if (worker.systemAgents.length !== 0) throw new Error("The app must not load automatic system agents.");

let server;
let stopping;
function stop(signal) {
  stopping ??= (async () => {
    log(`${signal}: stopping.`);
    const portalClosed = server ? once(server, "close").catch(() => {}) : Promise.resolve();
    try {
      await worker.stop();
    } catch (error) {
      console.error(`[pghacker-mcp] The worker didn't stop cleanly: ${error.message}`);
    }
    // The SDK's database pools outlive a graceful stop: exit once the portal closed.
    await Promise.race([portalClosed, delay(10_000)]);
    process.exit(0);
  })();
}
for (const signal of ["SIGINT", "SIGTERM"]) process.once(signal, () => stop(signal));

await worker.start();
const { startServer } = await import("pilotswarm/web");
server = await startServer({ port: Number(env.PORT), host: "127.0.0.1", workers: 0 });
if (server.address()?.address !== "127.0.0.1") throw new Error("The portal must listen on 127.0.0.1 only.");
log(`Portal on 127.0.0.1:${env.PORT}, signed in through oauth2-proxy.`);
