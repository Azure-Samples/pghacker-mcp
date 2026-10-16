// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { Check, Monitor, Moon, Sun, type LucideIcon } from "lucide-react";
import {
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type MouseEvent,
  type PointerEvent,
} from "react";
import {
  DARK_SCHEME_QUERY,
  THEME_STORAGE_KEY,
  applyTheme,
  browserStorage,
  readPreference,
  resolveTheme,
  writePreference,
  type ThemePreference,
} from "../theme.ts";

const CHOICES: readonly { value: ThemePreference; label: string; Icon: LucideIcon }[] = [
  { value: "light", label: "Light", Icon: Sun },
  { value: "dark", label: "Dark", Icon: Moon },
  { value: "system", label: "System", Icon: Monitor },
];

/**
 * The theme: the kept choice and the theme it shows, applied to <html>. It
 * follows the system's scheme while the choice is "system", and choices made
 * in other tabs.
 */
function useTheme() {
  const [storage] = useState(browserStorage);
  const [media] = useState(() => (typeof matchMedia === "function" ? matchMedia(DARK_SCHEME_QUERY) : undefined));
  const [preference, setPreference] = useState(() => readPreference(storage));
  const [systemDark, setSystemDark] = useState(() => media?.matches ?? false);
  const theme = resolveTheme(preference, systemDark);

  useLayoutEffect(() => applyTheme(document.documentElement, theme), [theme]);
  useEffect(() => {
    if (!media) return;
    const onChange = () => setSystemDark(media.matches);
    media.addEventListener("change", onChange);
    return () => media.removeEventListener("change", onChange);
  }, [media]);
  useEffect(() => {
    const onStorage = (event: StorageEvent) => {
      if (event.key === THEME_STORAGE_KEY || event.key === null) setPreference(readPreference(storage));
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, [storage]);

  const choose = useCallback(
    (next: ThemePreference) => {
      writePreference(storage, next);
      setPreference(next);
    },
    [storage],
  );
  return { preference, theme, choose };
}

/**
 * The theme menu, in the top bar: a sun or moon button that opens light,
 * dark and the system's, with the choice checked. A menu button: arrow keys,
 * Home and End move through it, Escape closes it.
 */
export function ThemeMenu() {
  const { preference, theme, choose } = useTheme();
  const [open, setOpen] = useState(false);
  const wrapper = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  // Where focus goes as the menu opens: the menu after a click, a choice from the keyboard.
  const initialFocus = useRef<"menu" | "first" | "last">("menu");
  const menuId = useId();
  const current = CHOICES.find((choice) => choice.value === preference) ?? CHOICES[0];

  const items = () => Array.from(menu.current?.querySelectorAll<HTMLElement>('[role="menuitemradio"]') ?? []);

  useEffect(() => {
    if (!open) return;
    const choices = items();
    const target = { menu: menu.current, first: choices[0], last: choices.at(-1) }[initialFocus.current];
    target?.focus();
    const onPointerDown = (event: globalThis.PointerEvent) => {
      if (!wrapper.current?.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [open]);

  function show(focus: "menu" | "first" | "last") {
    initialFocus.current = focus;
    setOpen(true);
  }

  function close({ returnFocus }: { returnFocus: boolean }) {
    setOpen(false);
    if (returnFocus) button.current?.focus();
  }

  function onButtonClick(event: MouseEvent<HTMLButtonElement>) {
    // A click from Enter or Space has no detail: focus the first choice then.
    if (open) close({ returnFocus: false });
    else show(event.detail === 0 ? "first" : "menu");
  }

  function onButtonKeyDown(event: KeyboardEvent<HTMLButtonElement>) {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      show(event.key === "ArrowDown" ? "first" : "last");
    } else if (event.key === "Escape" && open) {
      event.preventDefault();
      close({ returnFocus: false });
    }
  }

  function onMenuKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const choices = items();
    const index = choices.indexOf(document.activeElement as HTMLElement);
    const focus = (next: number) => {
      event.preventDefault();
      choices[(next + choices.length) % choices.length]?.focus();
    };
    if (event.key === "ArrowDown") focus(index + 1);
    else if (event.key === "ArrowUp") focus(index < 0 ? choices.length - 1 : index - 1);
    else if (event.key === "Home") focus(0);
    else if (event.key === "End") focus(choices.length - 1);
    else if (event.key === "Escape") {
      event.preventDefault();
      close({ returnFocus: true });
    } else if (event.key === "Tab") close({ returnFocus: false });
  }

  // The choice under the pointer is the one highlighted.
  function onItemPointerMove(event: PointerEvent<HTMLButtonElement>) {
    if (document.activeElement !== event.currentTarget) event.currentTarget.focus({ preventScroll: true });
  }

  return (
    <div className="theme-menu" ref={wrapper}>
      <button
        ref={button}
        type="button"
        className="btn btn-ghost btn-icon"
        aria-label={`Theme: ${current.label}`}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        data-state={open ? "open" : "closed"}
        onClick={onButtonClick}
        onKeyDown={onButtonKeyDown}
      >
        {theme === "dark" ? <Moon aria-hidden="true" /> : <Sun aria-hidden="true" />}
      </button>
      {open && (
        <div
          ref={menu}
          id={menuId}
          className="menu"
          role="menu"
          aria-label="Theme"
          tabIndex={-1}
          onKeyDown={onMenuKeyDown}
          onPointerLeave={() => menu.current?.focus({ preventScroll: true })}
        >
          <div className="menu-heading" aria-hidden="true">
            Theme
          </div>
          {CHOICES.map(({ value, label, Icon }) => (
            <button
              key={value}
              type="button"
              className="menu-item"
              role="menuitemradio"
              aria-checked={value === preference}
              tabIndex={-1}
              onClick={() => {
                choose(value);
                close({ returnFocus: true });
              }}
              onPointerMove={onItemPointerMove}
            >
              <Icon aria-hidden="true" />
              {label}
              <span className="menu-check">{value === preference && <Check aria-hidden="true" />}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
