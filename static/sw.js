// OxiCloud Service Worker
// FIXME: generate cache name according build ?
const CACHE_NAME = 'oxicloud-cache-v21';

// Only cache static assets — NOT HTML files.
// HTML files are served network-first so browsers always get the latest
// script/link references. Caching HTML causes stale entry points.
const ASSETS_TO_CACHE = [
    '/js/core/icons.js',
    '/js/core/i18n.js',
    '/js/core/languageSelector.js',
    '/js/core/notifications.js',
    '/locales/en.json',
    '/locales/es.json',
    '/locales/fa.json',
    '/locales/de.json',
    '/locales/nl.json',
    '/favicon.ico'
];

// HTML paths that should always be fetched from the network
const HTML_PATHS = ['/', '/index.html', '/login', '/login.html', '/admin', '/admin.html', '/profile', '/profile.html'];

// Install event - cache assets
self.addEventListener('install', (event) => {
    event.waitUntil(
        caches
            .open(CACHE_NAME)
            .then((cache) => {
                console.log('Cache opened');
                return cache.addAll(ASSETS_TO_CACHE);
            })
            .then(() => self.skipWaiting()) // Activate immediately
    );
});

// Activate event - clean old caches
self.addEventListener('activate', (event) => {
    event.waitUntil(
        caches
            .keys()
            .then((cacheNames) => {
                return Promise.all(
                    cacheNames
                        .filter((cacheName) => {
                            return cacheName !== CACHE_NAME;
                        })
                        .map((cacheName) => {
                            return caches.delete(cacheName);
                        })
                );
            })
            .then(() => self.clients.claim()) // Take control of clients
    );
});

// Fetch event - serve from cache, update cache from network
self.addEventListener('fetch', (event) => {
    // Don't intercept API requests - let them go straight to the network
    if (event.request.url.includes('/api/')) {
        return;
    }

    // HTML pages: always network-first so entry points are never stale
    const pathname = new URL(event.request.url).pathname;
    const isHtml = HTML_PATHS.includes(pathname) || pathname.endsWith('.html') || event.request.headers.get('accept')?.includes('text/html');
    if (isHtml) {
        event.respondWith(fetch(event.request).catch(() => caches.match(event.request)));
        return;
    }

    event.respondWith(
        caches.match(event.request).then((response) => {
            // Cache hit - return the response from the cached version
            if (response) {
                // For non-core assets, still fetch from network for updates
                if (!ASSETS_TO_CACHE.includes(new URL(event.request.url).pathname)) {
                    fetch(event.request)
                        .then((networkResponse) => {
                            if (networkResponse && networkResponse.status === 200) {
                                const clonedResponse = networkResponse.clone();
                                caches.open(CACHE_NAME).then((cache) => {
                                    cache.put(event.request, clonedResponse);
                                });
                            }
                        })
                        .catch(() => {
                            // Ignore network fetch errors - we already have a cached version
                        });
                }
                return response;
            }

            // Not in cache - get from network and add to cache
            return fetch(event.request).then((response) => {
                if (!response || response.status !== 200 || response.type !== 'basic') {
                    return response;
                }

                // Clone the response as it's a stream and can only be consumed once
                const responseToCache = response.clone();

                caches.open(CACHE_NAME).then((cache) => {
                    cache.put(event.request, responseToCache);
                });

                return response;
            });
        })
    );
});

// Background sync for failed requests
self.addEventListener('sync', (event) => {
    if (event.tag === 'oxicloud-sync') {
        event.waitUntil(
            // Implement background sync for pending file operations
            Promise.resolve() // Placeholder for actual implementation
        );
    }
});
