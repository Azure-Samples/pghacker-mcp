// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/*
 * How to add the MCP server to each agent: the first thing to do, a command
 * to run when there is one, and a configuration file, which the page shows
 * collapsed. Each follows the client's documentation: `claude mcp add` and
 * .mcp.json, `codex mcp add --url` and ~/.codex/config.toml, Copilot CLI's
 * `copilot mcp add` and ~/.copilot/mcp-config.json, the GitHub Copilot app's
 * Customize > MCP, which also loads Copilot CLI's servers, VS Code's MCP: Add
 * Server and .vscode/mcp.json, and Cursor's mcp.json.
 *
 * The agents that run on their own come first: Claude Code and OpenAI Codex,
 * in order of use by the Stack Overflow Developer Survey 2026 (66% and 30%),
 * then GitHub Copilot's CLI and app. The editors follow, VS Code and Cursor.
 */

/** The name each agent knows the server by. */
export const SERVER_NAME = "pghacker";

export interface ClientConfig {
  /** Where it goes, such as ".vscode/mcp.json". */
  readonly file: string;
  /** The disclosure's words before the file, when not "Or add it to". */
  readonly prefix?: string;
  readonly language: "json" | "toml";
  readonly text: (url: string) => string;
}

export interface Client {
  readonly id: string;
  readonly label: string;
  /** The first thing to do, on its own line. */
  readonly action: string;
  /** A command to run, shown. */
  readonly command?: (url: string) => string;
  /** A configuration file, collapsed until asked for. */
  readonly config?: ClientConfig;
}

const json = (value: unknown): string => JSON.stringify(value, null, 2);

/** Copilot CLI's configuration, which the GitHub Copilot app loads too. */
const copilotConfig: ClientConfig = {
  file: "~/.copilot/mcp-config.json",
  language: "json",
  text: (url) => json({ mcpServers: { [SERVER_NAME]: { type: "http", url, tools: ["*"] } } }),
};

export const CLIENTS: readonly Client[] = [
  {
    id: "claude-code",
    label: "Claude Code",
    action: "Run in a terminal:",
    command: (url) => `claude mcp add --transport http ${SERVER_NAME} ${url}`,
    config: {
      file: ".mcp.json",
      prefix: "Or add it to a project's",
      language: "json",
      text: (url) => json({ mcpServers: { [SERVER_NAME]: { type: "http", url } } }),
    },
  },
  {
    id: "codex",
    label: "Codex",
    action: "Run in a terminal:",
    command: (url) => `codex mcp add ${SERVER_NAME} --url ${url}`,
    config: {
      file: "~/.codex/config.toml",
      language: "toml",
      text: (url) => `[mcp_servers.${SERVER_NAME}]\nurl = ${JSON.stringify(url)}`,
    },
  },
  {
    id: "copilot-cli",
    label: "Copilot CLI",
    action: "Run in a terminal, or use /mcp add in the CLI:",
    command: (url) => `copilot mcp add --transport http ${SERVER_NAME} ${url}`,
    config: copilotConfig,
  },
  {
    id: "copilot-app",
    label: "Copilot app",
    action: "In the GitHub Copilot app, click Customize in the sidebar, then MCP, and add a custom server with this URL. Use HTTP transport.",
    config: copilotConfig,
  },
  {
    id: "vscode",
    label: "VS Code",
    action: "Run MCP: Add Server… from the Command Palette, choose HTTP and paste the URL.",
    config: {
      file: ".vscode/mcp.json",
      language: "json",
      text: (url) => json({ servers: { [SERVER_NAME]: { type: "http", url } } }),
    },
  },
  {
    id: "cursor",
    label: "Cursor",
    action: "Add the server to ~/.cursor/mcp.json for all your projects, or to a project's .cursor/mcp.json.",
    config: {
      file: "mcp.json",
      prefix: "Show",
      language: "json",
      text: (url) => json({ mcpServers: { [SERVER_NAME]: { url } } }),
    },
  },
  {
    id: "other",
    label: "Other",
    action: "Add a remote MCP server with this URL. It speaks Streamable HTTP and needs no sign-in, key or headers.",
  },
];

/** The words of the disclosure that opens `config`, before its file name. */
export function configPrefix(config: ClientConfig): string {
  return config.prefix ?? "Or add it to";
}
