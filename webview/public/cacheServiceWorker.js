// Force service worker to update immediately
self.addEventListener('install', (event) => {
  self.skipWaiting();
});

self.addEventListener('activate', (event) => {
  event.waitUntil(self.clients.claim());
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);

  // Cache foldit.js and foldit.wasm together (emscripten requires they stay in sync)
  if (url.pathname === "/foldit.wasm" || url.pathname === "/foldit.js") {
    event.respondWith(
      caches.match(event.request).then(cached => cached || fetch(event.request))
    );
  }

  // Never cache .data files
  if (url.pathname.endsWith('.data')) {
    event.respondWith(fetch(event.request));
  }
});
