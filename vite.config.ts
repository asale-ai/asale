import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri expects a fixed dev port and builds into ./dist.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 9173, strictPort: true },
  build: { outDir: "dist", emptyOutDir: true },
});
