// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { Moon, Sun } from "lucide-react";
import { useEffect, useLayoutEffect, useState } from "react";
import { THEME_STORAGE_KEY, applyTheme, browserStorage, otherTheme, readTheme, writeTheme } from "../theme.ts";

/** The theme: the kept one, applied to <html>, and switched in other tabs too. */
function useTheme() {
  const [storage] = useState(browserStorage);
  const [theme, setTheme] = useState(() => readTheme(storage));

  useLayoutEffect(() => applyTheme(document.documentElement, theme), [theme]);
  useEffect(() => {
    const onStorage = (event: StorageEvent) => {
      if (event.key === THEME_STORAGE_KEY || event.key === null) setTheme(readTheme(storage));
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, [storage]);

  function toggle() {
    const next = otherTheme(theme);
    writeTheme(storage, next);
    setTheme(next);
  }
  return { theme, toggle };
}

/** The top bar's theme toggle: a sun or a moon, for the theme shown; a click switches to the other. */
export function ThemeToggle() {
  const { theme, toggle } = useTheme();
  const label = `Switch to the ${otherTheme(theme)} theme`;
  return (
    <button type="button" className="btn btn-ghost btn-icon" aria-label={label} title={label} onClick={toggle}>
      {theme === "dark" ? <Moon aria-hidden="true" /> : <Sun aria-hidden="true" />}
    </button>
  );
}
