import path from "node:path";

export function workerOptions(config, env) {
  return {
    store: config.databaseUrl,
    githubToken: env.GITHUB_TOKEN,
    workerNodeId: config.project,
    sessionStateDir: path.join(config.runtimeDir, "sessions"),
    pluginDirs: [path.join(config.root, "plugin")],
    disableManagementAgents: true,
    logLevel: "error",
  };
}
