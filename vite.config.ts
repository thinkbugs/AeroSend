import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // Cargo rewrites src-tauri/target while `tauri dev` is running. Watching
      // it makes vite crash with EBUSY on the half-written exe, which kills the
      // whole dev loop.
      ignored: ["**/src-tauri/target/**", "**/dist/**"],
    },
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: ["es2021", "chrome105", "safari15"],
    minify: !process.env.TAURI_DEBUG ? "esbuild" : false,
    sourcemap: !!process.env.TAURI_DEBUG,
    rollupOptions: {
      input: {
        main: path.join(import.meta.dirname, "index.html"),
        // The annotation overlay is its own document: a transparent window must
        // not inherit the app's opaque stylesheet.
        annotate: path.join(import.meta.dirname, "annotate.html"),
      },
    },
  },
});
