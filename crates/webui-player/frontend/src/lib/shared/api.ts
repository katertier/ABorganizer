// Tiny `fetch` wrapper that prefixes the API base + carries the
// bearer token. Per ADR-0040 the menubar host pre-injects the
// token via `localStorage`, so the same code path covers both
// modes.

const API_BASE = "/api/v1";

function authHeaders(): Record<string, string> {
    const token = localStorage.getItem("aborg.bearer_token");
    return token ? { Authorization: `Bearer ${token}` } : {};
}

export async function apiGet<T>(path: string): Promise<T> {
    const response = await fetch(`${API_BASE}${path}`, {
        headers: { ...authHeaders() },
    });
    if (!response.ok) {
        throw new Error(`GET ${path}: ${response.status}`);
    }
    return (await response.json()) as T;
}

export async function apiPost<T, B = unknown>(path: string, body: B): Promise<T> {
    const response = await fetch(`${API_BASE}${path}`, {
        method: "POST",
        headers: {
            "Content-Type": "application/json",
            ...authHeaders(),
        },
        body: JSON.stringify(body),
    });
    if (!response.ok) {
        throw new Error(`POST ${path}: ${response.status}`);
    }
    return (await response.json()) as T;
}
