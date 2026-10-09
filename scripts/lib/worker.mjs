import path from "node:path";

/** The app's model catalog: GitHub Copilot models, including Luna for the crawler. */
export function modelProvidersPath(config) {
  return path.join(config.root, "config", "model_providers.json");
}

export function workerOptions(config, env) {
  return {
    store: config.databaseUrl,
    githubToken: env.GITHUB_TOKEN,
    workerNodeId: config.project,
    sessionStateDir: path.join(config.runtimeDir, "sessions"),
    pluginDirs: [path.join(config.root, "plugin")],
    modelProvidersPath: modelProvidersPath(config),
    disableManagementAgents: true,
    logLevel: "error",
  };
}

/** The app's agents, which a standalone client may create sessions for. */
export async function appAgentNames(config) {
  const { loadAgentFiles } = await import("pilotswarm-sdk");
  return loadAgentFiles(path.join(config.root, "plugin", "agents")).map((agent) => agent.name);
}
