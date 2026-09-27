import path from "node:path";
import { fileURLToPath } from "node:url";
import react from "@vitejs/plugin-react";

const projectRoot = path.dirname(fileURLToPath(import.meta.url));

export default {
  base: "./",
  plugins: [react()],
  resolve: {
    alias: {
      "@": projectRoot,
    },
  },
  build: {
    // The host materializes the verified dependency tree at local
    // `node_modules/`, matching Node and Vite's normal project layout.
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
    target: "safari17",
    reportCompressedSize: false,
    rollupOptions: {
      output: {
        // NOT a stylistic choice: this is what keeps the build off WebAssembly.
        //
        // iOS forbids JIT, so iSH runs V8 `--jitless`, and jitless V8 has no
        // WebAssembly. iSH injects `/lib/wasm-polyfill.js` in its place, which
        // is a single-purpose llhttp shim installed as the global
        // `WebAssembly`: `compile()` throws the bytes away and `instantiate()`
        // always returns llhttp's exports. Vite's `vite:build-import-analysis`
        // runs its bundled es-module-lexer over every output chunk and reads
        // `exports.__heap_base.value`; it receives llhttp, so each on-device
        // build failed with "Cannot read properties of undefined (reading
        // 'value')" — after transforming every module successfully, which is
        // why it read as a random bundler bug rather than a missing feature.
        //
        // That plugin's `generateBundle` opens with `if (format !== "es")
        // return;`, and that return happens BEFORE it awaits the lexer's WASM
        // init. Emitting a non-ES bundle is the only lever in this repo that
        // avoids the shim entirely. Verified against the real polyfill: `es`
        // fails, `iife` builds. The app's CSS is then injected at runtime via
        // a `<style>` element, which the local-app CSP permits
        // (`style-src 'self' 'unsafe-inline'`).
        //
        // This also fixes the only Ionic import style that works here.
        // `@ionic/core/components/ion-*.js` (the tree-shakeable per-component
        // entry points) dynamically import one another, and rolldown rejects
        // that under `iife` with "UMD and IIFE are not supported for
        // code-splitting builds" — `output.codeSplitting: false` does not lift
        // it. Import from the `@ionic/react` barrel instead; measured, that is
        // a flat ~1.4 MB whether the app uses one component or twelve.
        format: "iife",
        // An iife bundle is a single scope and cannot code-split, so any
        // dynamic import has to be folded in rather than emitted as a chunk.
        inlineDynamicImports: true,
      },
    },
  },
};
