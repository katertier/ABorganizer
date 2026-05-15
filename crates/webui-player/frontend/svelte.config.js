import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";

export default {
    // Svelte 5 uses runes by default; preprocessor handles TS in script blocks.
    preprocess: vitePreprocess(),
};
