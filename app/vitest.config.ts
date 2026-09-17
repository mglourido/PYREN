import { sveltekit } from "@sveltejs/kit/vite";
import { defineConfig } from "vitest/config";

// Deliberately its own config, not a `test` block bolted onto
// `vite.config.js`: that file's `daemonBridge()` plugin (`apply: "serve"`)
// forwards requests to the real daemon's Unix socket, and unit tests must
// never depend on a running daemon. `sveltekit()` alone is enough to
// resolve the `$lib` alias and to compile `.svelte.ts` rune syntax - both
// of which `telemetry.svelte.ts` needs.
export default defineConfig({
  plugins: [sveltekit()],
  test: {
    include: ["tests/**/*.test.ts"],
  },
});
