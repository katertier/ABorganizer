// Player-engine adapter (ADR-0040 § Player engine adapter).
//
// Two concrete engines share one interface so components above
// the engine layer don't branch on host. The factory picks the
// right one at boot.
//
// `HtmlAudioEngine` and `AVPlayerBridgeEngine` are scaffolds —
// real `load` / `play` / `seek` wiring lands in the player
// feature slice (Phase D cluster 3). The interface is fixed
// now so component code can compile against it without
// coordinating with the engine implementer.

import { HOST, type Host } from "../host.js";

export type PlayerEvent = "play" | "pause" | "ended" | "timeupdate" | "error";

export interface PlayerEngine {
    load(url: string): Promise<void>;
    play(): Promise<void>;
    pause(): Promise<void>;
    seek(ms: number): Promise<void>;
    on(event: PlayerEvent, handler: () => void): void;
}

class HtmlAudioEngine implements PlayerEngine {
    private audio: HTMLAudioElement = new Audio();

    async load(url: string): Promise<void> {
        this.audio.src = url;
        await this.audio.load();
    }

    async play(): Promise<void> {
        await this.audio.play();
    }

    async pause(): Promise<void> {
        this.audio.pause();
    }

    async seek(ms: number): Promise<void> {
        this.audio.currentTime = ms / 1000;
    }

    on(event: PlayerEvent, handler: () => void): void {
        this.audio.addEventListener(event, handler);
    }
}

class AVPlayerBridgeEngine implements PlayerEngine {
    private handlers: Map<PlayerEvent, Array<() => void>> = new Map();

    async load(url: string): Promise<void> {
        this.postToHost({ type: "load", url });
    }

    async play(): Promise<void> {
        this.postToHost({ type: "play" });
    }

    async pause(): Promise<void> {
        this.postToHost({ type: "pause" });
    }

    async seek(ms: number): Promise<void> {
        this.postToHost({ type: "seek", ms });
    }

    on(event: PlayerEvent, handler: () => void): void {
        const list = this.handlers.get(event) ?? [];
        list.push(handler);
        this.handlers.set(event, list);
    }

    private postToHost(payload: Record<string, unknown>): void {
        const player = window.webkit?.messageHandlers?.player;
        if (player && typeof (player as { postMessage?: (m: unknown) => void }).postMessage === "function") {
            (player as { postMessage: (m: unknown) => void }).postMessage(payload);
        } else {
            console.warn("aborg.player_bridge_unavailable", payload);
        }
    }
}

export function makePlayerEngine(host: Host = HOST): PlayerEngine {
    return host === "menubar" ? new AVPlayerBridgeEngine() : new HtmlAudioEngine();
}
