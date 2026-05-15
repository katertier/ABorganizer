<script lang="ts">
    import { HOST } from "./lib/shared/host.js";
    import Library from "./routes/Library.svelte";
    import Setup from "./routes/Setup.svelte";
    import Player from "./routes/Player.svelte";

    // Minimal hash-based router. Picked because zero deps + works
    // identically in browser + WKWebView modes (which both honor
    // `window.location.hash`). When the routing complexity outgrows
    // it, swap for `svelte-routing` or similar.
    let route = $state(window.location.hash.slice(1) || "/");

    window.addEventListener("hashchange", () => {
        route = window.location.hash.slice(1) || "/";
    });

    // First-use guidance (ADR-0040 § First-use guidance) — placeholder
    // wiring. A future slice will probe `/library_roots` + `/auth/tokens`
    // + `/books/count` and short-circuit to /setup when any is empty.
</script>

<main class:compact={HOST === "menubar"}>
    <nav>
        <a href="#/">Library</a>
        <a href="#/player">Player</a>
        <a href="#/setup">Setup</a>
        <span class="host-badge">host: {HOST}</span>
    </nav>

    {#if route === "/" || route === ""}
        <Library />
    {:else if route === "/player"}
        <Player />
    {:else if route === "/setup"}
        <Setup />
    {:else}
        <p>Unknown route: {route}</p>
    {/if}
</main>

<style>
    main {
        max-width: 1200px;
        margin: 0 auto;
        padding: 1rem;
    }
    main.compact {
        max-width: 480px;
        padding: 0.5rem;
    }
    nav {
        display: flex;
        gap: 1rem;
        align-items: center;
        border-bottom: 1px solid #ddd;
        padding-bottom: 0.5rem;
        margin-bottom: 1rem;
    }
    .host-badge {
        margin-left: auto;
        font-size: 0.75rem;
        opacity: 0.6;
    }
</style>
