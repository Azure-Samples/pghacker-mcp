// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/**
 * The colour theme: light, dark, or the system's, kept in localStorage. The
 * page is light unless someone chooses otherwise. theme-boot.js shows the
 * choice before the first paint, with the same key and rules; the theme menu
 * changes it.
 *
 * The theme is the `dark` class on <html>, which tokens.css gives the dark
 * tokens, and <html>'s color-scheme.
 */

/** The theme that someone chose. */
export type ThemePreference = "light" | "dark" | "system";

/** The theme that the page shows. */
export type Theme = "light" | "dark";

/** Where the choice is kept; theme-boot.js reads the same key. */
export const THEME_STORAGE_KEY = "pghacker-mcp.theme";

/** The media query for the system's dark scheme. */
export const DARK_SCHEME_QUERY = "(prefers-color-scheme: dark)";

/** The kept choice: light unless it's "dark" or "system". */
export function parseThemePreference(value: string | null): ThemePreference {
  return value === "dark" || value === "system" ? value : "light";
}

/** The theme that `preference` shows, given whether the system's scheme is dark. */
export function resolveTheme(preference: ThemePreference, systemDark: boolean): Theme {
  if (preference === "system") return systemDark ? "dark" : "light";
  return preference;
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
 * The kept choice. The browser can refuse storage, in a private window or
 * when it's full: the page is light then, and a choice lasts until it reloads.
 */
export function readPreference(store: KeyValueStore | undefined): ThemePreference {
  try {
    return parseThemePreference(store?.getItem(THEME_STORAGE_KEY) ?? null);
  } catch {
    return "light";
  }
}

/** Keeps `preference`, unless storage fails. */
export function writePreference(store: KeyValueStore | undefined, preference: ThemePreference): void {
  try {
    store?.setItem(THEME_STORAGE_KEY, preference);
  } catch {
    // The choice lasts until the page reloads.
  }
}
