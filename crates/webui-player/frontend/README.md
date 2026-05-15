# aborg-frontend (Svelte 5 + Vite + TS)

Single Svelte 5 codebase serving two hosts (ADR-0040):

- Browser SPA at `https://localhost:8429/`
- macOS menubar app's WKWebView embed (`?host=menubar` or via
  the `window.webkit.messageHandlers` probe)

The bundle builds to `../static/` so `ab-webui-player` (the Rust
crate) serves it as the daemon's static-asset surface.

## Setup

```bash
# Install Bun (one-time per machine)
curl -fsSL https://bun.sh/install | bash

cd crates/webui-player/frontend
bun install
bun run build
```

## Development

```bash
bun run dev
```

Dev server runs on `http://localhost:5173`. `/api/*` requests are
proxied to the daemon at `:8429`. The daemon's CORS allow-list
includes the Vite default port for this flow.

## Layout

```
frontend/
├── index.html            # Vite entry
├── package.json          # Bun manifest + dev/build scripts
├── svelte.config.js      # Svelte preprocessor (TS in script blocks)
├── tsconfig.json         # Strict TS + Svelte plugin
├── vite.config.ts        # Vite + Svelte plugin + dev proxy
├── src/
│   ├── main.ts           # Svelte 5 mount entry point
│   ├── App.svelte        # Top-level component + hash router
│   ├── app.css           # Global resets + base typography
│   ├── lib/
│   │   ├── shared/       # CROSS-APP — host-detection + API client.
│   │   │   ├── host.ts   # Host-detection contract (ADR-0040).
│   │   │   └── api.ts    # `fetch` helpers with bearer-token glue.
│   │   └── player/       # PLAYER-ONLY — engine adapter.
│   │       └── engine.ts # PlayerEngine adapter (browser vs. menubar)
│   └── routes/
│       ├── Library.svelte # Book list (calls GET /books)
│       ├── Player.svelte  # Player engine scaffold
│       └── Setup.svelte   # First-use guidance placeholder
└── .gitignore
```

## Slice cadence (ADR-0040 build order)

1. **Layout + routes + data flow** (this slice) — placeholder
   styling, hash-based routing, host detection, player-engine
   adapter shape. Real CRUD round-trips with the daemon API
   wired in.
2. **Modular extension** — per-host branches, bundle-only
   features in menubar embed, Now Playing integration, AirPlay
   button, Siri registration.
3. **Polish** — icons, logo, custom design language, animation,
   colour palette. Land last.

Each step is its own slice; this scaffolding is step 1's
foundation only.

## Shared frontend modules (`src/lib/shared/`)

`src/lib/shared/` is the **cross-application** TypeScript module
shared with any future Svelte frontend in this workspace (the
planned `webui-config` rewrite from Askama, plus any sibling
apps that ship later).

The factor exists today even with only one consumer (the player
SPA) per the schema-as-if-planned-from-day-one rule (2026-05-15
retrospective, item #6): when a second Svelte app lands, it
imports from this location via a relative path — never copies.

If a third consumer appears, promote `shared/` to a top-level
`frontend/shared/` Bun workspace package + introduce a Vite
path alias. Defer that move until the third consumer is real.

## Not part of the Rust build

This directory is a separate JavaScript project. CI runs
`bun run build` before `cargo build` to populate `../static/`;
for local dev, run it once manually (see Setup above).
`node_modules/`, `dist/`, and Bun lockfiles are gitignored.
