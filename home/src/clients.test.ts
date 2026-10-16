// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import assert from "node:assert/strict";
import test from "node:test";
import { CLIENTS, SERVER_NAME, configPrefix } from "./clients.ts";

const URL = "https://pghacker-example.z01.azurefd.net/mcp";
const byId = (id: string) => {
  const client = CLIENTS.find((candidate) => candidate.id === id);
  assert.ok(client, `no client ${id}`);
  return client;
};

test("each agent has its own tab, most used first, and Other has nothing to paste", () => {
  assert.deepEqual(
    CLIENTS.map((client) => client.id),
    ["claude-code", "vscode", "codex", "cursor", "copilot-cli", "copilot-app", "other"],
  );
  assert.equal(new Set(CLIENTS.map((client) => client.label)).size, CLIENTS.length);
  for (const client of CLIENTS) assert.ok(client.action.length > 0, client.id);
  const other = byId("other");
  assert.equal(other.command, undefined);
  assert.equal(other.config, undefined);
});

test("the JSON configurations are each client's shape, for the page's URL", () => {
  const parse = (id: string) => JSON.parse(byId(id).config?.text(URL) ?? "null");
  assert.deepEqual(parse("vscode"), { servers: { [SERVER_NAME]: { type: "http", url: URL } } });
  assert.deepEqual(parse("copilot-cli"), { mcpServers: { [SERVER_NAME]: { type: "http", url: URL, tools: ["*"] } } });
  assert.deepEqual(parse("copilot-app"), parse("copilot-cli"));
  assert.deepEqual(parse("claude-code"), { mcpServers: { [SERVER_NAME]: { type: "http", url: URL } } });
  assert.deepEqual(parse("cursor"), { mcpServers: { [SERVER_NAME]: { url: URL } } });
});

test("Codex's configuration is a TOML table with the URL", () => {
  assert.equal(byId("codex").config?.text(URL), `[mcp_servers.${SERVER_NAME}]\nurl = "${URL}"`);
});

test("the commands add the server by name with the URL", () => {
  assert.equal(byId("copilot-cli").command?.(URL), `copilot mcp add --transport http ${SERVER_NAME} ${URL}`);
  assert.equal(byId("claude-code").command?.(URL), `claude mcp add --transport http ${SERVER_NAME} ${URL}`);
  assert.equal(byId("codex").command?.(URL), `codex mcp add ${SERVER_NAME} --url ${URL}`);
});

test("every configuration file sits behind a disclosure that names it", () => {
  for (const client of CLIENTS) {
    if (!client.config) continue;
    assert.ok(client.config.file.length > 0, client.id);
    assert.ok(configPrefix(client.config).length > 0, client.id);
  }
  assert.equal(configPrefix({ file: "x", language: "json", text: () => "" }), "Or add it to");
});

test("the GitHub Copilot app adds the server in its UI, or from Copilot CLI's file, which it loads", () => {
  const app = byId("copilot-app");
  assert.equal(app.command, undefined);
  assert.match(app.action, /Customize.*MCP/);
  assert.equal(app.config?.file, "~/.copilot/mcp-config.json");
  assert.equal(app.config?.file, byId("copilot-cli").config?.file);
});
