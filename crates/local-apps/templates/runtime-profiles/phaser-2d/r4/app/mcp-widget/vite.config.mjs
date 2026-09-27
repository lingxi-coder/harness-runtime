import react from "@vitejs/plugin-react";
import { viteSingleFile } from "vite-plugin-singlefile";

export default {
  base: "./",
  plugins: [react(), viteSingleFile()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    reportCompressedSize: false,
    sourcemap: false,
    // Match the app template's own vite.config.mjs (react-dom/r1): this
    // widget is seeded into the same on-device workspace
    // (local_app_runtime_profiles.rs's mcp-widget files) and built by the
    // same jitless-V8/iSH runtime, which has no WebAssembly. Vite's
    // `vite:build-import-analysis` only skips its WASM-backed es-module-lexer
    // init for a non-"es" output format, so an "es" bundle (the default)
    // fails on-device the same way an unset `format` did for the app
    // template — see that file's comment for the verified failure mode.
    target: "safari17",
    rollupOptions: {
      output: {
        format: "iife",
        inlineDynamicImports: true,
      },
    },
  },
};
