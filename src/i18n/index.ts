// i18n bootstrap: four locales bundled directly; the chosen language persists
// via the Tauri settings store (key "language").
import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import { invoke, inTauri } from "../lib";
import en from "./locales/en.json";
import zh from "./locales/zh.json";
import zhTW from "./locales/zh-TW.json";
import ja from "./locales/ja.json";

export const LANGUAGES = [
  { id: "zh", label: "简体中文" },
  { id: "zh-TW", label: "繁體中文" },
  { id: "en", label: "English" },
  { id: "ja", label: "日本語" },
] as const;
export type Language = (typeof LANGUAGES)[number]["id"];

function isLanguage(v: unknown): v is Language {
  return LANGUAGES.some((l) => l.id === v);
}

/** Best match for the browser locale; falls back to English. */
function detect(): Language {
  const nav = (navigator.language || "en").toLowerCase();
  if (nav.startsWith("zh")) {
    const traditional = ["tw", "hk", "mo", "hant"].some((s) => nav.includes(s));
    return traditional ? "zh-TW" : "zh";
  }
  if (nav.startsWith("ja")) return "ja";
  return "en";
}

i18n.use(initReactI18next).init({
  resources: {
    en: { translation: en },
    zh: { translation: zh },
    "zh-TW": { translation: zhTW },
    ja: { translation: ja },
  },
  lng: detect(),
  fallbackLng: "en",
  interpolation: { escapeValue: false },
});

/** Apply the persisted language (if any) before first render. */
export async function initLanguage(): Promise<void> {
  if (!inTauri) return;
  try {
    const saved = await invoke<string | null>("get_setting", { key: "language" });
    if (isLanguage(saved)) await i18n.changeLanguage(saved);
  } catch {
    // keep the detected language
  }
}

/** Switch and persist the language. */
export async function setLanguage(lang: Language): Promise<void> {
  await i18n.changeLanguage(lang);
  if (inTauri) {
    try {
      await invoke("set_setting", { key: "language", value: lang });
    } catch {
      // non-fatal: the language still applies for this session
    }
  }
}

export default i18n;
