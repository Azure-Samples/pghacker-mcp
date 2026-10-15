/** The MCP endpoint's path, on the page's own origin. */
export const MCP_PATH = "/mcp";

/**
 * The MCP endpoint's URL for `origin`, the page's own: the host the browser
 * asked for, such as Front Door's host name or a custom domain in Azure, or
 * the local edge (http://127.0.0.1:3001).
 */
export function mcpUrl(origin: string): string {
  return new URL(MCP_PATH, origin).href;
}
