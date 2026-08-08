// The three API dialects the asale gateway answers in, and working code for
// each — the desktop app's copy.
//
// The web console keeps a hand-synced twin at `asale-web/src/lib/api-compat.ts`
// rather than importing this one. `@shared/*` carries **types only** by design
// (see the header of `api-types.ts`): types are erased before bundling, while
// runtime values are not, and Turbopack roots itself at `asale-web/` and cannot
// resolve a module above it. The client also ships as its own open-source
// repository, so the dependency could not point the other way either.
//
// If you change a base URL, an auth header or a sample here, change it there
// too. Kept dependency-free and framework-free so that stays cheap: plain data
// and string templates, no imports, nothing to configure.
//
// The samples are meant to be pasted, not read. Every one of them is a complete
// program against the vendor's own SDK with two lines changed — the base URL and
// the key — because "point your existing client at us" is the entire pitch and
// a sample that needs editing before it runs does not make it.

/** The vendor whose wire format a request speaks. */
export type CompatMode = "openai" | "anthropic" | "gemini";

export const COMPAT_MODES: CompatMode[] = ["openai", "anthropic", "gemini"];

/** The languages every mode has a sample in. */
export type CompatLang = "curl" | "python" | "node";

export const COMPAT_LANGS: CompatLang[] = ["curl", "python", "node"];

export const COMPAT_LANG_LABELS: Record<CompatLang, string> = {
  curl: "cURL",
  python: "Python",
  node: "Node.js",
};

/**
 * Where the gateway lives. Overridable so a dev build documents the endpoint it
 * is actually talking to rather than production's.
 */
export const DEFAULT_GATEWAY = "https://gw.asale.ai";

export interface CompatSpec {
  mode: CompatMode;
  /** Tab label. A product name, not a translated string — these are proper nouns. */
  name: string;
  /** The vendor SDK a reader already has installed. */
  sdk: string;
  /** What goes in the client's `base_url` / `baseURL` field. */
  baseUrl: (gateway: string) => string;
  /** The path a request actually lands on, for the reference table. */
  endpoint: string;
  /** Every path this mode serves, so the table is not a half-truth. */
  paths: string[];
  /** The header the key travels in — one per vendor, see `gateway::authenticate`. */
  authHeader: string;
  /** The environment variable that vendor's SDK reads by default. */
  envVar: string;
  /** A model that trades on the platform, used in the samples. */
  sampleModel: string;
}

export const COMPAT: Record<CompatMode, CompatSpec> = {
  openai: {
    mode: "openai",
    name: "OpenAI",
    sdk: "openai",
    // The `/v1` belongs to the base URL in OpenAI's clients: they append
    // `/chat/completions` to whatever they are given.
    baseUrl: (gw) => `${gw}/v1`,
    endpoint: "POST /v1/chat/completions",
    paths: [
      "POST /v1/chat/completions",
      "POST /v1/responses",
      "POST /v1/completions",
      "GET /v1/models",
    ],
    authHeader: "Authorization: Bearer <key>",
    envVar: "OPENAI_API_KEY",
    sampleModel: "gpt-5",
  },
  anthropic: {
    mode: "anthropic",
    name: "Anthropic",
    sdk: "anthropic",
    // Anthropic's clients append `/v1/messages` themselves, so the base URL
    // stops at the host. Giving them `…/v1` produces `/v1/v1/messages`.
    baseUrl: (gw) => gw,
    endpoint: "POST /v1/messages",
    paths: ["POST /v1/messages", "POST /v1/messages/count_tokens"],
    authHeader: "x-api-key: <key>",
    envVar: "ANTHROPIC_API_KEY",
    sampleModel: "claude-sonnet-4-5",
  },
  gemini: {
    mode: "gemini",
    name: "Gemini",
    sdk: "google-genai",
    // Origin only. Google's clients append `/v1beta/models/…` themselves, and
    // `GOOGLE_GEMINI_BASE_URL` (what gemini-cli reads, and what asale's own
    // buy switch writes into `~/.gemini/.env`) is an origin too.
    baseUrl: (gw) => gw,
    endpoint: "POST /v1beta/models/{model}:generateContent",
    paths: [
      "POST /v1beta/models/{model}:generateContent",
      "POST /v1beta/models/{model}:streamGenerateContent",
      "GET /v1beta/models",
    ],
    authHeader: "x-goog-api-key: <key>",
    envVar: "GEMINI_API_KEY",
    sampleModel: "gemini-2.5-pro",
  },
};

/** What the samples print instead of a real key. */
export const KEY_PLACEHOLDER = "sk-asale-xxxxxxxxxxxxxxxxxxxxxxxx";

export interface SampleOpts {
  /** Gateway origin, no trailing slash. */
  gateway?: string;
  /** A real key, when the page has one to hand. Falls back to the placeholder. */
  key?: string;
  /** Override the model, e.g. the one the reader was just looking at. */
  model?: string;
}

/**
 * A runnable sample for one (mode, language) pair.
 *
 * The key is interpolated when the caller has one — the whole point of showing
 * this on the API-keys page is that a reader can copy something that works —
 * and falls back to a placeholder everywhere else.
 */
export function compatSample(mode: CompatMode, lang: CompatLang, opts: SampleOpts = {}): string {
  const spec = COMPAT[mode];
  const gw = (opts.gateway || DEFAULT_GATEWAY).replace(/\/+$/, "");
  const base = spec.baseUrl(gw);
  const key = opts.key || KEY_PLACEHOLDER;
  const model = opts.model || spec.sampleModel;

  if (mode === "openai") {
    if (lang === "curl") {
      return `curl ${base}/chat/completions \\
  -H "Authorization: Bearer ${key}" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${model}",
    "messages": [{"role": "user", "content": "Hello"}],
    "stream": false
  }'`;
    }
    if (lang === "python") {
      return `# pip install openai
from openai import OpenAI

client = OpenAI(
    api_key="${key}",
    base_url="${base}",
)

resp = client.chat.completions.create(
    model="${model}",
    messages=[{"role": "user", "content": "Hello"}],
)
print(resp.choices[0].message.content)`;
    }
    return `// npm i openai
import OpenAI from "openai";

const client = new OpenAI({
  apiKey: "${key}",
  baseURL: "${base}",
});

const resp = await client.chat.completions.create({
  model: "${model}",
  messages: [{ role: "user", content: "Hello" }],
});
console.log(resp.choices[0].message.content);`;
  }

  if (mode === "anthropic") {
    if (lang === "curl") {
      return `curl ${base}/v1/messages \\
  -H "x-api-key: ${key}" \\
  -H "anthropic-version: 2023-06-01" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${model}",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "Hello"}]
  }'`;
    }
    if (lang === "python") {
      return `# pip install anthropic
from anthropic import Anthropic

client = Anthropic(
    api_key="${key}",
    base_url="${base}",
)

msg = client.messages.create(
    model="${model}",
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello"}],
)
print(msg.content[0].text)`;
    }
    return `// npm i @anthropic-ai/sdk
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic({
  apiKey: "${key}",
  baseURL: "${base}",
});

const msg = await client.messages.create({
  model: "${model}",
  max_tokens: 1024,
  messages: [{ role: "user", content: "Hello" }],
});
console.log(msg.content[0].text);`;
  }

  if (lang === "curl") {
    return `curl "${base}/v1beta/models/${model}:generateContent" \\
  -H "x-goog-api-key: ${key}" \\
  -H "Content-Type: application/json" \\
  -d '{
    "contents": [{"role": "user", "parts": [{"text": "Hello"}]}]
  }'`;
  }
  if (lang === "python") {
    return `# pip install google-genai
from google import genai
from google.genai import types

client = genai.Client(
    api_key="${key}",
    http_options=types.HttpOptions(base_url="${gw}"),
)

resp = client.models.generate_content(
    model="${model}",
    contents="Hello",
)
print(resp.text)`;
  }
  return `// npm i @google/genai
import { GoogleGenAI } from "@google/genai";

const ai = new GoogleGenAI({
  apiKey: "${key}",
  httpOptions: { baseUrl: "${gw}" },
});

const resp = await ai.models.generateContent({
  model: "${model}",
  contents: "Hello",
});
console.log(resp.text);`;
}
