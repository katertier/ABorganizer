import { mount } from "svelte";
import App from "./App.svelte";
import "./app.css";

// Svelte 5 mount API. The `target` query selector must match
// `index.html`'s root div.
const target = document.getElementById("app");
if (!target) {
    throw new Error("aborg-frontend: #app root not found in index.html");
}

mount(App, { target });
