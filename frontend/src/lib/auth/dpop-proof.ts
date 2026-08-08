/**
 * Build a DPoP proof JWT (RFC 9449) signed with the browser's persistent
 * P-256 keypair (see `./dpop.ts`), and manage the `DPoP-Nonce` state
 * that the server issues in response headers.
 *
 * A proof is minted PER request and carries the exact `htm` (method) +
 * `htu` (target URL, no query) it's bound to. Server rejects any
 * mismatch — a stolen proof cannot be replayed against a different URL
 * or method, and the `jti` (unique per proof) prevents even same-URL
 * replay within the nonce's lifetime.
 *
 * Nonce handling: at Gate 5b the server issues a `DPoP-Nonce` response
 * header rotating every ~2min. This module holds the current nonce in
 * module-level state (mirrored to `sessionStorage` so it survives a
 * `/api/auth/me` remount but is per-tab per the plan — each tab does
 * its own bootstrap challenge). At Gate 4 the server does not yet emit
 * nonces; the client sends proofs without the `nonce` claim, which
 * server-side (Gate 5) accepts on the bootstrap-only clock branch.
 */

import { ensureKeypair } from './dpop';

const NONCE_STORAGE_KEY = 'oxicloud-dpop-nonce';

let currentNonce: string | null = null;

/** Initialise nonce state from sessionStorage on first import. */
function loadNonceOnce(): void {
	if (currentNonce !== null) return;
	try {
		const stored = sessionStorage.getItem(NONCE_STORAGE_KEY);
		if (stored) currentNonce = stored;
	} catch {
		/* sessionStorage may be absent (SSR / privacy mode) — ignore */
	}
}

/** Update the nonce state from a fresh `DPoP-Nonce` response header. */
export function updateNonceFromResponse(response: Response): void {
	const fresh = response.headers.get('DPoP-Nonce');
	if (!fresh || fresh === currentNonce) return;
	currentNonce = fresh;
	try {
		sessionStorage.setItem(NONCE_STORAGE_KEY, fresh);
	} catch {
		/* sessionStorage full / disabled — keep in-memory copy */
	}
}

/** Wipe the current nonce — called on logout so a new session bootstraps fresh. */
export function clearNonce(): void {
	currentNonce = null;
	try {
		sessionStorage.removeItem(NONCE_STORAGE_KEY);
	} catch {
		/* ignore */
	}
}

/** Base64URL encode, no padding — RFC 7515 §2. */
function b64u(bytes: Uint8Array): string {
	let s = '';
	for (const b of bytes) s += String.fromCharCode(b);
	return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function b64uJson(value: unknown): string {
	return b64u(new TextEncoder().encode(JSON.stringify(value)));
}

/**
 * Canonicalise a URL for the `htu` claim (RFC 9449 §4.2). Rules:
 *   * scheme + authority (host + port) + path
 *   * NO query string, NO fragment
 *   * lowercase scheme + host per URI normalisation
 *
 * Accepts absolute (`https://oxi.example/api/x`) or relative
 * (`/api/x`) URLs; relative resolves against `location.origin`.
 */
export function canonicalHtu(url: string): string {
	const base = typeof location !== 'undefined' ? location.origin : 'http://localhost';
	const u = new URL(url, base);
	return `${u.protocol}//${u.host}${u.pathname}`;
}

/**
 * Build a signed DPoP proof for the given method+URL. Returns the
 * compact JWS string ready to drop into a `DPoP` HTTP header, or
 * `null` when the keypair is unavailable (SubtleCrypto absent,
 * IndexedDB blocked, etc.) — caller MUST proceed without the header
 * in that case (fail-open contract, matches `docs/plan/dpop.md`).
 *
 * Each call mints a fresh `jti` (random UUID) and stamps a current
 * `iat`, so two calls made in the same tick still produce different
 * proofs — replay-cache friendly by construction.
 */
export async function buildDpopProof(method: string, url: string): Promise<string | null> {
	loadNonceOnce();

	let keypair: CryptoKeyPair;
	try {
		keypair = await ensureKeypair();
	} catch (err) {
		console.debug('dpop-proof: keypair unavailable', err);
		return null;
	}

	// Export the public key JWK — becomes the `jwk` header member.
	// Extractable is a property of the PUBLIC key (we only ever set
	// `extractable: false` on generation for the private half); the
	// public half is always extractable in ECDSA/`P-256` regardless.
	const jwk = await crypto.subtle.exportKey('jwk', keypair.publicKey);

	const header = {
		typ: 'dpop+jwt',
		alg: 'ES256',
		jwk: {
			crv: jwk.crv,
			kty: jwk.kty,
			x: jwk.x,
			y: jwk.y
		}
	};

	const claims: Record<string, unknown> = {
		htm: method.toUpperCase(),
		htu: canonicalHtu(url),
		iat: Math.floor(Date.now() / 1000),
		jti: crypto.randomUUID()
	};
	if (currentNonce) claims.nonce = currentNonce;

	const signingInput = `${b64uJson(header)}.${b64uJson(claims)}`;
	const signatureBytes = new Uint8Array(
		await crypto.subtle.sign(
			// ES256: raw R || S concatenation (64 bytes for P-256) — RFC
			// 7515 A.3. SubtleCrypto emits exactly this format; no DER
			// unwrapping needed.
			{ name: 'ECDSA', hash: 'SHA-256' },
			keypair.privateKey,
			new TextEncoder().encode(signingInput)
		)
	);
	return `${signingInput}.${b64u(signatureBytes)}`;
}

/**
 * True when the current 401 response is the server's DPoP-Nonce
 * challenge: `WWW-Authenticate: DPoP error="use_dpop_nonce"`. The
 * fetch interceptor uses this to decide whether to retry the original
 * request once with a freshly-received nonce (which the same response
 * carries in its `DPoP-Nonce` header).
 */
export function isDpopNonceChallenge(response: Response): boolean {
	if (response.status !== 401) return false;
	const auth = response.headers.get('WWW-Authenticate') ?? '';
	// Header syntax per RFC 6750 §3 / RFC 9449 §7.1: whitespace-tolerant
	// scheme + comma-separated key="value" pairs. Case-insensitive
	// scheme + key match; strict "use_dpop_nonce" for the error value.
	if (!/^\s*DPoP(\s|,|$)/i.test(auth)) return false;
	return /error\s*=\s*"?use_dpop_nonce"?/i.test(auth);
}
