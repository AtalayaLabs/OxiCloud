/// <reference types="@sveltejs/kit" />
/// <reference lib="webworker" />

/**
 * DPoP-signing Service Worker (RFC 9449 companion to the page-context
 * `apiFetch` interceptor).
 *
 * WHY this exists. Under `OXICLOUD_DPOP_MODE=required` the server rejects
 * bound-session requests that carry no DPoP proof. The page-context
 * `apiFetch` interceptor covers everything the SPA drives through
 * `fetch()` — but the browser itself makes requests JS can never touch:
 * `<img src>` (thumbnails, photo previews), `<a href download>` /
 * `<a href>` (file downloads / inline previews), and `EventSource`
 * (admin log tail). Those had lived behind a middleware allowlist
 * (Gate C) — this SW replaces that allowlist entirely by attaching a
 * proof to every same-origin `/api/*` request the browser makes,
 * regardless of who initiated it.
 *
 * Why NOT a Web Worker. Dedicated workers can't see network requests
 * the page initiates. Only Service Workers register a `fetch` handler
 * for their scope. This IS the browser-side hook for browser-driven
 * requests.
 *
 * Shared state with the page. Both scopes share the same-origin
 * IndexedDB (where the persistent P-256 keypair lives) and SubtleCrypto
 * (also available in SW context). The nonce cache is per-scope — the
 * page module holds its own in-memory nonce, the SW holds its own; on
 * first request each scope pays a one-round-trip nonce challenge, then
 * catches up via `DPoP-Nonce` response headers.
 *
 * Skip conditions:
 *   * cross-origin (privacy — never leak the user's keypair thumbprint
 *     to third parties);
 *   * anything outside `/api/*` (static assets don't hit the DPoP
 *     middleware, no need to burn crypto per request);
 *   * requests that already carry a `DPoP` header — the page context
 *     signed them via `apiFetch` (or the XHR upload path), don't
 *     double-sign;
 *   * proof unavailable (keypair inaccessible) — pass through
 *     unsigned so unbound sessions still function (fail-open contract,
 *     matches `docs/plan/dpop.md`).
 */

import { buildDpopProof, isDpopNonceChallenge, updateNonceFromResponse } from '$lib/auth/dpop-proof';

// eslint-disable-next-line @typescript-eslint/consistent-type-declarations
declare const self: ServiceWorkerGlobalScope;

const ORIGIN = self.location.origin;

// Fast-forward the SW lifecycle so open tabs pick up the new version
// on the next navigation without waiting for every existing tab to
// close (default lifecycle stalls activation until then).
self.addEventListener('install', () => {
	void self.skipWaiting();
});

self.addEventListener('activate', (event) => {
	event.waitUntil(self.clients.claim());
});

self.addEventListener('fetch', (event) => {
	const req = event.request;
	const url = new URL(req.url);
	// Same-origin only — never leak a DPoP proof (which carries the
	// user's public-key JWK) to a third-party host.
	if (url.origin !== ORIGIN) return;
	// Only DPoP-protected paths need signing. Static assets, locales,
	// vendors are outside the middleware and don't need the crypto tax.
	if (!url.pathname.startsWith('/api/')) return;
	// Don't double-sign — page-context `apiFetch`, `fetchMe`, `tryRefresh`,
	// and `uploadFileWithProgress` already attach a proof themselves.
	if (req.headers.has('DPoP')) return;
	event.respondWith(signAndFetch(req));
});

async function signAndFetch(req: Request): Promise<Response> {
	const firstProof = await buildDpopProof(req.method, req.url).catch(() => null);
	// No keypair (IndexedDB blocked, SubtleCrypto missing, etc.) — pass
	// through unsigned. Unbound sessions still work; bound sessions in
	// `required` mode will 401, matching the fail-open contract.
	if (!firstProof) return fetch(req);

	// Body-preservation contract: `new Request(existing, ...)` transfers
	// ownership of `existing.body` (a `ReadableStream` — read once). To
	// keep a retry option open for POST / PUT bodies we tee ahead of the
	// first attempt via `req.clone()`. Cheap on GET (no body), one
	// stream tee on state-changing calls.
	const retryReq = req.clone();
	const first = await fetch(signWith(req, firstProof));
	updateNonceFromResponse(first);
	if (!isDpopNonceChallenge(first)) return first;

	// Fresh proof — the current call to `buildDpopProof` picks up the
	// nonce we just harvested from `first`'s `DPoP-Nonce` header.
	const secondProof = await buildDpopProof(retryReq.method, retryReq.url).catch(() => null);
	if (!secondProof) return first; // couldn't sign — surface the challenge
	return fetch(signWith(retryReq, secondProof));
}

/**
 * Build the signed outbound `Request` from an intercepted one.
 *
 * `mode: 'same-origin'` is load-bearing. Browser-initiated `<img src>`
 * / `<a href>` requests default to `mode: 'no-cors'`, and in no-cors
 * mode the browser silently strips any header not on the CORS-safelist
 * (`Accept`, `Accept-Language`, `Content-Language`, `Content-Type`)
 * BEFORE sending — so `DPoP` would never reach the wire even though
 * `Headers.set('DPoP', …)` succeeds in JS. `same-origin` (or `cors`)
 * lets custom headers through. Legal for our targets: every path we
 * intercept starts with `/api/` on the same origin as the SW itself.
 */
function signWith(req: Request, proof: string): Request {
	return new Request(req, {
		headers: withDpopHeader(req.headers, proof),
		mode: 'same-origin'
	});
}

function withDpopHeader(existing: Headers, proof: string): Headers {
	const h = new Headers(existing);
	h.set('DPoP', proof);
	return h;
}
