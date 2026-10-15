import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The page's files go to static/, not Vite's assets/: the PilotSwarm portal
// owns /assets on the same origin, and its page polls "/" for assets/*.js
// names to see whether it was redeployed (scripts/lib/routes.mjs).
// `npm run dev` proxies the rest of the site to a running `npm start`.
const site = "http://127.0.0.1:3001";

export default defineConfig({
  plugins: [react()],
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
