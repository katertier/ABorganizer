<script lang="ts">
    import { onMount } from "svelte";
    import { apiGet } from "../lib/shared/api.js";

    type Book = {
        book_id: number;
        title: string | null;
        author: string | null;
    };

    type BooksResponse = {
        items: Book[];
        total: number;
    };

    let books = $state<Book[]>([]);
    let total = $state(0);
    let loading = $state(true);
    let error = $state<string | null>(null);

    onMount(async () => {
        try {
            const response = await apiGet<BooksResponse>("/books?limit=50");
            books = response.items;
            total = response.total;
        } catch (e) {
            error = String(e);
        } finally {
            loading = false;
        }
    });
</script>

<h1>Library</h1>

{#if loading}
    <p>Loading…</p>
{:else if error}
    <p class="error">Failed to load books: {error}</p>
    <p class="hint">Auth token missing or daemon not running? Check <a href="#/setup">Setup</a>.</p>
{:else}
    <p>{total} books total — showing first {books.length}.</p>
    <ul>
        {#each books as book (book.book_id)}
            <li>
                <strong>{book.title ?? "(untitled)"}</strong>
                {#if book.author}— {book.author}{/if}
            </li>
        {/each}
    </ul>
{/if}

<style>
    .error {
        color: #c00;
    }
    .hint {
        opacity: 0.7;
        font-size: 0.9rem;
    }
    ul {
        list-style: none;
        padding: 0;
    }
    li {
        padding: 0.5rem 0;
        border-bottom: 1px solid #eee;
    }
</style>
