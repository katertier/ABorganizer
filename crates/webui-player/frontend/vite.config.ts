import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Vite + Svelte 5 + TypeScript configuration.
//
// Build output lands in `../static/` so `ab-webui-player` serves
// the bundle as the daemon's static-asset surface. The CI build
// step runs `bun run build` before `cargo build` so the embedded
// bundle is always in sync with the Rust release.
//
// Dev server proxies `/api/*` to the daemon at the default port
// (8429). The daemon's CORS allow-list includes
// `http://localhost:5173` (the Vite default) for this flow.
export default defineConfig({
    plugins: [svelte()],
    build: {
        outDir: "../static",
        emptyOutDir: true,
        sourcemap: true,
    },
    server: {
        port: 5173,
        proxy: {
            "/api": {
                target: "http://localhost:8429",
                changeOrigin: false,
            },
        },
    },
});
