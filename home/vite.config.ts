// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";

// The page's files go to static/, not Vite's assets/: the PilotSwarm portal
// owns /assets on the same origin, and its page polls "/" for assets/*.js
// names to see whether it was redeployed (scripts/lib/routes.mjs).
// `npm run dev` proxies the rest of the site to a running `npm start`.
const site = "http://127.0.0.1:3001";

/**
 * src/theme-boot.js, which shows the kept theme before the first paint: a
 * classic script in <head>, which runs before the page draws, unlike a
 * module; a file, since the page's CSP allows no inline script; and named by
 * its hash, since /static/ files are cached for good.
 */
function themeBoot(): Plugin {
  const source = readFileSync(new URL("./src/theme-boot.js", import.meta.url), "utf8");
  const fileName = `static/theme-boot-${createHash("sha256").update(source).digest("hex").slice(0, 10)}.js`;
  return {
    name: "pghacker-theme-boot",
    configureServer(server) {
      server.middlewares.use(`/${fileName}`, (_request, response) => {
        response.setHeader("content-type", "text/javascript; charset=utf-8");
        response.end(source);
      });
    },
    generateBundle() {
      this.emitFile({ type: "asset", fileName, source });
    },
    transformIndexHtml() {
      return [{ tag: "script", attrs: { src: `/${fileName}` }, injectTo: "head" }];
    },
  };
}

export default defineConfig({
  plugins: [react(), themeBoot()],
  build: { assetsDir: "static" },
  server: {
    proxy: {
      "/mcp": site,
      "/portal": { target: site, ws: true },
      "/api": { target: site, ws: true },
      "/assets": site,
    },
  },
});
