// Theme handling: "light" | "dark" | "system", persisted via the settings
// store (key "theme"). The resolved theme lands on <html data-theme="…">.
import { useEffect, useState } from "react";
import { invoke, inTauri } from "./lib";

export type Theme = "light" | "dark" | "system";
export const THEMES: Theme[] = ["light", "dark", "system"];

const media = window.matchMedia("(prefers-color-scheme: dark)");
let current: Theme = "system";
const listeners = new Set<(t: Theme) => void>();

function apply(theme: Theme) {
  const resolved = theme === "system" ? (media.matches ? "dark" : "light") : theme;
  document.documentElement.dataset.theme = resolved;
}

media.addEventListener("change", () => {
  if (current === "system") apply("system");
});

/** Load the persisted theme (default "system") and apply it before render. */
export async function initTheme(): Promise<void> {
  if (inTauri) {
    try {
      const saved = await invoke<string | null>("get_setting", { key: "theme" });
      if (saved === "light" || saved === "dark" || saved === "system") current = saved;
    } catch {
      // keep the default
    }
  }
  apply(current);
}

/** Current theme + setter (persists and applies immediately). */
export function useTheme(): [Theme, (t: Theme) => void] {
  const [theme, setLocal] = useState<Theme>(current);
  useEffect(() => {
    listeners.add(setLocal);
    return () => {
      listeners.delete(setLocal);
    };
  }, []);
  const setTheme = (t: Theme) => {
    current = t;
    apply(t);
    listeners.forEach((l) => l(t));
    if (inTauri) invoke("set_setting", { key: "theme", value: t }).catch(() => {});
  };
  return [theme, setTheme];
}
