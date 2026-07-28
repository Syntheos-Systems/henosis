/** Vite and Vitest configuration for the Henosis desktop webview. */
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

/** Shared development, build, and component-test configuration. */
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
  envPrefix: ["VITE_", "TAURI_"],
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    css: true,
  },
});
