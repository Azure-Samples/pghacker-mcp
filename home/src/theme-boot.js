// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// The theme, before the first paint: a classic script that vite.config.ts
// puts in <head> as a /static/ file named by its hash, since the page's CSP
// allows no inline script. theme.ts has the same key and rule: dark if "dark"
// is kept, light otherwise.
(() => {
  let theme = null;
  try {
    theme = localStorage.getItem("pghacker-mcp.theme");
  } catch {
    // Storage is off: the page is light.
  }
  const dark = theme === "dark";
  const root = document.documentElement;
  root.classList.toggle("dark", dark);
  root.style.colorScheme = dark ? "dark" : "light";
})();
