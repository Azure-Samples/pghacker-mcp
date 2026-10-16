// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The theme, before the first paint: a classic script that vite.config.ts
// puts in <head> as a /static/ file named by its hash, since the page's CSP
// allows no inline script. theme.ts has the same key and rules: light unless
// "dark" or "system" is kept, and "system" follows the system's scheme.
(() => {
  let preference = null;
  try {
    preference = localStorage.getItem("pghacker-mcp.theme");
  } catch {
    // Storage is off: the page is light.
  }
  const dark =
    preference === "dark" ||
    (preference === "system" &&
      typeof matchMedia === "function" &&
      matchMedia("(prefers-color-scheme: dark)").matches);
  const root = document.documentElement;
  root.classList.toggle("dark", dark);
  root.style.colorScheme = dark ? "dark" : "light";
})();
