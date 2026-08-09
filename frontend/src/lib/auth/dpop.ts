/**
 * DPoP (RFC 9449) browser-side keypair lifecycle.
 *
 * Every SPA session that supports Web Crypto generates a P-256 ECDSA keypair
 * with `extractable: false` at first login, persists the `CryptoKey` handles
 * in IndexedDB, and reuses them for every subsequent request's DPoP proof.
 * The raw key bytes live in the browser's crypto subsystem — JS can call
 * `sign()` on the handle but never `exportKey()`. That's what defeats
 * info-stealer replay: the cookie alone is useless without the private key,
 * and the private key never leaves the browser process's crypto boundary.
 *
 * Fail-open contract: any failure here (SubtleCrypto unavailable, IndexedDB
 * blocked by policy, HTTP-not-HTTPS context) must throw or return so the
 * caller can log in WITHOUT a `dpop_jkt`. The resulting session lives with
 * `session.dpop_jkt = NULL` and is exempted at the middleware. This mirrors
 * `docs/plan/dpop.md`'s explicit design — degradation is per-session and
 * immutable, so a bound session cannot be downgraded by a later request.
 *
 * Threat model boundary: same as any `SubtleCrypto` non-extractable
 * `CryptoKey`. Defeats today's commodity info-stealers (which target
 * cookies via SQLite + OS keyring, not IndexedDB CryptoKey blobs).
 * Doesn't defeat a browser process compromised at login time or a
 * malicious extension with `webRequest` — those are prerequisites the
 * whole SPA relies on being clean.
 */

const DB_NAME = 'oxicloud-dpop';
const DB_VERSION = 1;
const STORE = 'keypair';
const KEY = 'current';

/** Base64URL-encode raw bytes, no padding — RFC 7515 §2 (`base64url`). */
function b64u(bytes: Uint8Array): string {
	let s = '';
	for (const b of bytes) s += String.fromCharCode(b);
	return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/** Open (and create-if-missing) the DPoP object store. */
function openDb(): Promise<IDBDatabase> {
	return new Promise((resolve, reject) => {
		const req = indexedDB.open(DB_NAME, DB_VERSION);
		req.onupgradeneeded = () => {
			const db = req.result;
			if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE);
		};
		req.onsuccess = () => resolve(req.result);
		req.onerror = () => reject(req.error);
	});
}

async function readKeypair(): Promise<CryptoKeyPair | null> {
	const db = await openDb();
	try {
		return await new Promise((resolve, reject) => {
			const tx = db.transaction(STORE, 'readonly');
			const req = tx.objectStore(STORE).get(KEY);
			req.onsuccess = () => resolve((req.result as CryptoKeyPair | undefined) ?? null);
			req.onerror = () => reject(req.error);
		});
	} finally {
		db.close();
	}
}

async function writeKeypair(pair: CryptoKeyPair): Promise<void> {
	const db = await openDb();
	try {
		await new Promise<void>((resolve, reject) => {
			const tx = db.transaction(STORE, 'readwrite');
			tx.objectStore(STORE).put(pair, KEY);
			tx.oncomplete = () => resolve();
			tx.onerror = () => reject(tx.error);
		});
	} finally {
		db.close();
	}
}

async function generateKeypair(): Promise<CryptoKeyPair> {
	return crypto.subtle.generateKey(
		{ name: 'ECDSA', namedCurve: 'P-256' },
		// extractable: false — the private key handle cannot be exported.
		// SubtleCrypto stores raw bytes in the browser's crypto subsystem
		// (Keychain/DPAPI-encrypted at rest on disk). JS holds only an
		// opaque reference usable via sign().
		false,
		['sign', 'verify']
	);
}

/**
 * Return the browser's persistent DPoP keypair. On first call in a fresh
 * profile, generates a new P-256 keypair (non-extractable) and persists
 * it. Subsequent calls return the SAME handle across the same profile —
 * across tabs (shared IndexedDB), across reloads, across sessions until
 * `clearKeypair()` is called.
 *
 * Tab-race guard: two tabs opened simultaneously both call
 * `ensureKeypair()` before either has persisted. `navigator.locks`
 * serialises them; the second waiter finds the persisted keypair and
 * returns it, so both tabs converge on the same handle.
 *
 * When `navigator.locks` is unavailable (very old Safari), the race is
 * theoretically possible but statistically rare; the loser overwrites
 * the winner's keypair, which just means the earlier tab's next request
 * fails DPoP verification once and forces a re-login. Not catastrophic.
 */
export async function ensureKeypair(): Promise<CryptoKeyPair> {
	const doEnsure = async (): Promise<CryptoKeyPair> => {
		const existing = await readKeypair();
		if (existing) return existing;
		const fresh = await generateKeypair();
		await writeKeypair(fresh);
		return fresh;
	};
	if (typeof navigator !== 'undefined' && navigator.locks?.request) {
		return navigator.locks.request('oxicloud-dpop-keypair', doEnsure);
	}
	return doEnsure();
}

/**
 * Compute the RFC 7638 JWK thumbprint of the public key — the value we
 * send to the server as `dpop_jkt` at login. Base64URL-encoded SHA-256
 * of the CANONICAL JWK (member names alphabetical, no whitespace, only
 * the REQUIRED members for the key type — see §3.2 for EC keys).
 *
 * Canonicalisation is load-bearing: a rogue `{"kty":"EC","crv":"P-256",...}`
 * with any deviation (extra whitespace, non-alphabetical order, extra
 * members like `use` or `alg`) yields a DIFFERENT hash and thus a
 * different thumbprint — the server would reject the binding. RFC 7638
 * §3.1 pins the exact serialisation; we reproduce it here.
 */
export async function computeJkt(pubKey: CryptoKey): Promise<string> {
	const jwk = await crypto.subtle.exportKey('jwk', pubKey);
	// RFC 7638 §3.2 — for EC keys, the REQUIRED members are crv, kty,
	// x, y in ALPHABETICAL order. Any other members (use, alg, kid, …)
	// MUST be omitted from the hash input.
	const canonical = JSON.stringify({
		crv: jwk.crv,
		kty: jwk.kty,
		x: jwk.x,
		y: jwk.y
	});
	const hash = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(canonical));
	return b64u(new Uint8Array(hash));
}

/**
 * Drop the persistent keypair — called on logout so the next login
 * mints a fresh binding with no correlation across the boundary.
 *
 * Safe to call when no keypair exists (IndexedDB may be absent in
 * private mode after close-and-reopen).
 */
export async function clearKeypair(): Promise<void> {
	let db: IDBDatabase;
	try {
		db = await openDb();
	} catch {
		return; // IndexedDB unavailable — nothing to clear
	}
	try {
		await new Promise<void>((resolve, reject) => {
			const tx = db.transaction(STORE, 'readwrite');
			tx.objectStore(STORE).delete(KEY);
			tx.oncomplete = () => resolve();
			tx.onerror = () => reject(tx.error);
		});
	} finally {
		db.close();
	}
}
