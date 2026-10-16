// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@fontsource-variable/inter";
import "./styles/tokens.css";
import "./styles/base.css";
import "./styles/page.css";
import { App } from "./App.tsx";
import { mcpUrl } from "./mcp.ts";

const root = document.getElementById("root");
if (root === null) throw new Error("The page has no #root.");
createRoot(root).render(
  <StrictMode>
    <App url={mcpUrl(window.location.origin)} />
  </StrictMode>,
);
