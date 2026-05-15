<script lang="ts">
    // First-use setup flow — scaffolded shape only.
    // The real flow lands as part of the discovery-surfaces slice
    // (Phase D cluster 3) and probes:
    //
    //   GET /library_roots   → empty?  → step 1
    //   GET /auth/tokens     → empty?  → step 2 (browser-cert trust)
    //   GET /books/count     → zero?   → step 3 (initial scan)
    //
    // Today this is just a static checklist.
    let steps = [
        { title: "Configure library root", status: "pending" },
        { title: "Trust browser cert", status: "pending" },
        { title: "Trigger initial scan", status: "pending" },
    ];
</script>

<h1>Setup</h1>

<p>One-time setup flow — currently a placeholder.</p>

<ol>
    {#each steps as step}
        <li>
            <span class="status">[{step.status}]</span>
            {step.title}
        </li>
    {/each}
</ol>

<style>
    .status {
        font-family: monospace;
        opacity: 0.6;
        margin-right: 0.5rem;
    }
</style>
