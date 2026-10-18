// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/*
 * The colour theme: light or dark, kept in localStorage. The page is light
 * unless someone switches it to dark with the toggle in the top bar.
 * theme-boot.js shows the kept theme before the first paint, with the same
 * key and rule; the toggle changes it.
 *
 * The theme is the `dark` class on <html>, which tokens.css gives the dark
 * tokens, and <html>'s color-scheme.
 */

/** The theme that the page shows. */
export type Theme = "light" | "dark";

/** Where the theme is kept; theme-boot.js reads the same key. */
export const THEME_STORAGE_KEY = "pghacker-mcp.theme";

/** The kept theme: dark if "dark" is kept, light otherwise, "system" from before included. */
export function parseTheme(value: string | null): Theme {
  return value === "dark" ? "dark" : "light";
}

/** The other theme, which the toggle switches to. */
export function otherTheme(theme: Theme): Theme {
  return theme === "dark" ? "light" : "dark";
}

/** What {@link applyTheme} changes: <html>. */
export interface ThemeRoot {
  readonly classList: Pick<DOMTokenList, "toggle">;
  readonly style: Pick<CSSStyleDeclaration, "colorScheme">;
}

/** Shows `theme` on `root`. */
export function applyTheme(root: ThemeRoot, theme: Theme): void {
  root.classList.toggle("dark", theme === "dark");
  root.style.colorScheme = theme;
}

/** What the page uses of localStorage. */
export type KeyValueStore = Pick<Storage, "getItem" | "setItem">;

/** The browser's localStorage, or undefined where the page can't reach it. */
export function browserStorage(): KeyValueStore | undefined {
  try {
    return globalThis.localStorage ?? undefined;
  } catch {
    return undefined;
  }
}

/**
 * The kept theme. The browser can refuse storage, in a private window or
 * when it's full: the page is light then, and a switch lasts until it reloads.
 */
export function readTheme(store: KeyValueStore | undefined): Theme {
  try {
    return parseTheme(store?.getItem(THEME_STORAGE_KEY) ?? null);
  } catch {
    return "light";
  }
}

/** Keeps `theme`, unless storage fails. */
export function writeTheme(store: KeyValueStore | undefined, theme: Theme): void {
  try {
    store?.setItem(THEME_STORAGE_KEY, theme);
  } catch {
    // The theme lasts until the page reloads.
  }
}
