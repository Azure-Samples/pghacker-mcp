import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import {
  DARK_SCHEME_QUERY,
  THEME_STORAGE_KEY,
  applyTheme,
  parseThemePreference,
  readPreference,
  resolveTheme,
  writePreference,
  type KeyValueStore,
} from "./theme.ts";

/** <html>, as far as the theme goes. */
function fakeRoot() {
  const classes = new Set<string>();
  return {
    classes,
    classList: {
      toggle(token: string, force?: boolean) {
        const on = force ?? !classes.has(token);
        if (on) classes.add(token);
        else classes.delete(token);
        return on;
      },
    },
    style: { colorScheme: "" },
  };
}

const shown = (root: ReturnType<typeof fakeRoot>) => ({
  theme: root.classes.has("dark") ? "dark" : "light",
  colorScheme: root.style.colorScheme,
});

const STORED = [null, "light", "dark", "system", "Dark", ""];

test("the page is light unless dark or the system's scheme is chosen", () => {
  assert.equal(parseThemePreference(null), "light");
  assert.equal(parseThemePreference("light"), "light");
  assert.equal(parseThemePreference("dark"), "dark");
  assert.equal(parseThemePreference("system"), "system");
  assert.equal(parseThemePreference("Dark"), "light");
  for (const systemDark of [false, true]) {
    assert.equal(resolveTheme("light", systemDark), "light");
    assert.equal(resolveTheme("dark", systemDark), "dark");
  }
  assert.equal(resolveTheme("system", false), "light");
  assert.equal(resolveTheme("system", true), "dark");
});

test("a theme is the dark class and the color scheme on <html>", () => {
  const root = fakeRoot();
  applyTheme(root, "dark");
  assert.deepEqual(shown(root), { theme: "dark", colorScheme: "dark" });
  applyTheme(root, "light");
  assert.deepEqual(shown(root), { theme: "light", colorScheme: "light" });
});

test("the choice is kept, and storage that fails leaves the page light", () => {
  const items = new Map<string, string>();
  const store: KeyValueStore = {
    getItem: (key) => items.get(key) ?? null,
    setItem: (key, value) => void items.set(key, value),
  };
  assert.equal(readPreference(store), "light");
  writePreference(store, "system");
  assert.equal(items.get(THEME_STORAGE_KEY), "system");
  assert.equal(readPreference(store), "system");

  const refusing: KeyValueStore = {
    getItem: () => {
      throw new Error("SecurityError");
    },
    setItem: () => {
      throw new Error("QuotaExceededError");
    },
  };
  assert.equal(readPreference(refusing), "light");
  assert.doesNotThrow(() => writePreference(refusing, "dark"));
  assert.equal(readPreference(undefined), "light");
});

test("theme-boot.js shows the theme that theme.ts would, before the page's script", () => {
  const source = readFileSync(new URL("./theme-boot.js", import.meta.url), "utf8");
  const boot = (globals: Record<string, unknown>, { refuseStorage = false } = {}) => {
    const root = fakeRoot();
    const context: Record<string, unknown> = { document: { documentElement: root }, ...globals };
    if (refuseStorage) {
      Object.defineProperty(context, "localStorage", {
        get() {
          throw new Error("SecurityError");
        },
      });
    }
    vm.runInNewContext(source, context);
    return shown(root);
  };
  const scheme = (systemDark: boolean) => (query: string) => ({ matches: query === DARK_SCHEME_QUERY && systemDark });

  for (const stored of STORED) {
    for (const systemDark of [false, true]) {
      const expected = resolveTheme(parseThemePreference(stored), systemDark);
      const localStorage = { getItem: (key: string) => (key === THEME_STORAGE_KEY ? stored : null) };
      assert.deepEqual(
        boot({ localStorage, matchMedia: scheme(systemDark) }),
        { theme: expected, colorScheme: expected },
        `kept ${JSON.stringify(stored)}, system ${systemDark ? "dark" : "light"}`,
      );
    }
  }

  // No storage, or no matchMedia: light, as theme.ts.
  assert.deepEqual(boot({ matchMedia: scheme(true) }, { refuseStorage: true }), { theme: "light", colorScheme: "light" });
  assert.deepEqual(boot({ localStorage: { getItem: () => "system" } }), { theme: "light", colorScheme: "light" });
});
