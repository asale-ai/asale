/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Override the daemon RPC base URL (e.g. http://127.0.0.1:9700). */
  readonly VITE_ASALE_DAEMON?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
