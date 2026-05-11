# Player UI (Svelte 5 + Bun)

The player UI lives here. `bun install && bun run build` produces a
static bundle in `../static/` which `ab-webui-player` serves.

This directory is **not** part of the Rust build. It's a separate
JavaScript project. CI runs `bun run build` before `cargo build` to
populate `../static/`; for local dev, run it once manually.

## Setup

```bash
# Install Bun (one-time per machine)
curl -fsSL https://bun.sh/install | bash

# Install deps + build
cd crates/webui-player/frontend
bun install
bun run build
```

## Development

```bash
bun run dev     # hot-reload server on http://localhost:5173
```

The dev server proxies API requests to the daemon at the default
port (`8429`).

## Files in scope for this directory

- `package.json` — Bun manifest, dev/build scripts
- `svelte.config.js` — Svelte compiler config
- `vite.config.js` — Vite + Bun bundler config
- `src/` — Svelte components, TypeScript modules
- `index.html` — entry point template

`node_modules/` and `dist/` are gitignored.
