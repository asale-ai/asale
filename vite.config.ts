import { fileURLToPath, URL } from "node:url";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri expects a fixed dev port and builds into ./dist.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  // `@shared/*` is declared in tsconfig for the type-checker, but that is
  // invisible to the bundler. Until now everything under shared/ was imported
  // `import type` and erased before it reached rollup; the country list is real
  // code, so the alias has to exist here too.
  resolve: {
    alias: {
      "@shared": fileURLToPath(new URL("./shared", import.meta.url)),
    },
  },
  server: { port: 9173, strictPort: true },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      output: {
        manualChunks: {
          react: ["react", "react-dom", "react-i18next", "i18next"],
          tauri: [
            "@tauri-apps/api",
            "@tauri-apps/plugin-autostart",
            "@tauri-apps/plugin-process",
            "@tauri-apps/plugin-updater",
          ],
        },
      },
    },
  },
});
