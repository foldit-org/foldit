/*! coi-serviceworker v0.1.7 - Guido Zuidhof and contributors, licensed under MIT */
/*
 * This service worker enables Cross-Origin Isolation by adding the required
 * COOP and COEP headers to all responses. This is necessary for
 * SharedArrayBuffer to work (used by wasm-bindgen-rayon for threading).
 *
 * Include this script in your HTML *before* any other scripts:
 *   <script src="coi-serviceworker.js"></script>
 *
 * On first load, it registers itself as a service worker and reloads the page.
 * On subsequent loads, the service worker intercepts all fetches and adds the
 * required headers.
 */
if (typeof window === 'undefined') {
    // Service worker context
    self.addEventListener("install", () => self.skipWaiting());
    self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));

    self.addEventListener("fetch", function (e) {
        if (e.request.cache === "only-if-cached" && e.request.mode !== "same-origin") {
            return;
        }

        e.respondWith(
            fetch(e.request)
                .then(function (response) {
                    if (response.status === 0) {
                        return response;
                    }

                    const newHeaders = new Headers(response.headers);
                    newHeaders.set("Cross-Origin-Embedder-Policy", "require-corp");
                    newHeaders.set("Cross-Origin-Opener-Policy", "same-origin");

                    return new Response(response.body, {
                        status: response.status,
                        statusText: response.statusText,
                        headers: newHeaders,
                    });
                })
                .catch(function (e) {
                    console.error(e);
                })
        );
    });
} else {
    // Window context -- register the service worker
    (async function () {
        if (window.crossOriginIsolated) return;

        // If a controller already exists, the service worker is active but
        // the browser still reports crossOriginIsolated as false (Safari).
        // Do not reload again — it would loop forever.
        if (navigator.serviceWorker.controller) {
            console.warn(
                "COOP/COEP Service Worker is active but crossOriginIsolated is still false. " +
                "SharedArrayBuffer may not be available in this browser."
            );
            return;
        }

        const registration = await navigator.serviceWorker
            .register(window.document.currentScript.src)
            .catch((e) =>
                console.error("COOP/COEP Service Worker failed to register:", e)
            );
        if (registration) {
            console.log("COOP/COEP Service Worker registered, reloading page to enable Cross-Origin Isolation.");
            window.location.reload();
        }
    })();
}
