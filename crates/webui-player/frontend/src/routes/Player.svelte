<script lang="ts">
    import { makePlayerEngine } from "../lib/player/engine.js";

    const engine = makePlayerEngine();

    // Real `load(...)` wiring lands when the player feature slice
    // ships (Phase D cluster 3). For now this is a scaffold so the
    // route exists + the engine factory is exercised.
    let playing = $state(false);

    async function togglePlay() {
        if (playing) {
            await engine.pause();
            playing = false;
        } else {
            await engine.play();
            playing = true;
        }
    }
</script>

<h1>Player</h1>

<p>Engine adapter scaffolded; real audio wiring lands in the player slice.</p>

<button onclick={togglePlay}>
    {playing ? "Pause" : "Play"}
</button>
