// Host-detection contract (ADR-0040).
//
// At app boot we pick layout + feature flags based on which
// surface is hosting the SPA. Two cases:
//
//   1. Browser SPA at https://localhost:8429/ — full layout,
//      HTML5 <audio> engine, no bundle-only features.
//   2. Menubar embed inside WKWebView — compact layout, Swift
//      `AVPlayer` engine via `window.webkit.messageHandlers`,
//      Siri / Now Playing / AirPlay surface enabled.
//
// Detection precedence:
//   1. `?host=menubar` query param (explicit override; used by
//      the menubar app and by browser-side debugging).
//   2. `window.webkit.messageHandlers` probe — WKWebView injects
//      this object; vanilla browsers don't.
//   3. Default `"browser"`.

export type Host = "browser" | "menubar";

declare global {
    interface Window {
        webkit?: {
            messageHandlers?: Record<string, unknown>;
        };
    }
}

export function detectHost(): Host {
    const params = new URLSearchParams(window.location.search);
    if (params.get("host") === "menubar") {
        return "menubar";
    }
    if (typeof window.webkit?.messageHandlers !== "undefined") {
        return "menubar";
    }
    return "browser";
}

// Single shared `Host` value, evaluated once at module load.
// Components import this directly rather than re-running
// `detectHost()` per render.
export const HOST: Host = detectHost();
