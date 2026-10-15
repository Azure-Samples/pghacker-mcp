import assert from "node:assert/strict";
import test from "node:test";
import { MCP_PATH, mcpUrl } from "./mcp.ts";

test("the endpoint is /mcp on the page's own origin", () => {
  assert.equal(MCP_PATH, "/mcp");
  assert.equal(mcpUrl("https://pghacker-example.z01.azurefd.net"), "https://pghacker-example.z01.azurefd.net/mcp");
  assert.equal(mcpUrl("https://pghacker.example.org"), "https://pghacker.example.org/mcp");
  assert.equal(mcpUrl("http://127.0.0.1:3001"), "http://127.0.0.1:3001/mcp");
});
