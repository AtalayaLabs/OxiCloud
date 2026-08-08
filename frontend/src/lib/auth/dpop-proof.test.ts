import { describe, it, expect, beforeEach, vi } from 'vitest';

// Mock the keypair source: jsdom has no IndexedDB, so the real
// `ensureKeypair()` throws. We generate a real P-256 pair via
// SubtleCrypto (Node 20+ ships it natively as `crypto.webcrypto`,
// exposed on `globalThis.crypto` in the test env) — same shape the
// browser sees, real signing bytes exercised end-to-end.
const KEYPAIR_PROMISE = crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, true, [
	'sign',
	'verify'
]);
vi.mock('./dpop', () => ({
	ensureKeypair: () => KEYPAIR_PROMISE
}));

import {
	buildDpopProof,
	canonicalHtu,
	clearNonce,
	isDpopNonceChallenge,
	updateNonceFromResponse
} from './dpop-proof';

/** Base64URL decode → bytes. Only used for test assertions. */
function b64uDecode(s: string): Uint8Array {
	const pad = s.length % 4 === 0 ? '' : '='.repeat(4 - (s.length % 4));
	const b64 = s.replace(/-/g, '+').replace(/_/g, '/') + pad;
	const bin = atob(b64);
	const out = new Uint8Array(bin.length);
	for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
	return out;
}

function b64uDecodeJson(s: string): Record<string, unknown> {
	return JSON.parse(new TextDecoder().decode(b64uDecode(s)));
}

beforeEach(() => {
	// Nonce state is module-level — reset between tests so cross-file
	// order doesn't leak nonces from one test into another.
	clearNonce();
});

describe('canonicalHtu', () => {
	it('strips query and fragment', () => {
		expect(canonicalHtu('https://oxi.example/api/x?y=1&z=2#frag')).toBe(
			'https://oxi.example/api/x'
		);
	});
	it('resolves relative against location.origin', () => {
		expect(canonicalHtu('/api/auth/me')).toMatch(/^https?:\/\/.+\/api\/auth\/me$/);
	});
});

describe('isDpopNonceChallenge', () => {
	function res(status: number, wwwAuth?: string): Response {
		return new Response(null, {
			status,
			headers: wwwAuth ? { 'WWW-Authenticate': wwwAuth } : {}
		});
	}

	it('matches DPoP scheme + use_dpop_nonce error', () => {
		expect(isDpopNonceChallenge(res(401, 'DPoP error="use_dpop_nonce"'))).toBe(true);
		expect(isDpopNonceChallenge(res(401, 'dpop error="use_dpop_nonce"'))).toBe(true);
		expect(isDpopNonceChallenge(res(401, 'DPoP error=use_dpop_nonce'))).toBe(true);
	});
	it('rejects other errors', () => {
		expect(isDpopNonceChallenge(res(401, 'DPoP error="invalid_dpop_proof"'))).toBe(false);
		expect(isDpopNonceChallenge(res(401, 'Bearer error="invalid_token"'))).toBe(false);
	});
	it('requires status 401', () => {
		expect(isDpopNonceChallenge(res(200, 'DPoP error="use_dpop_nonce"'))).toBe(false);
	});
	it('handles missing header', () => {
		expect(isDpopNonceChallenge(res(401))).toBe(false);
	});
});

describe('buildDpopProof', () => {
	it('produces a compact JWS with the expected header + claims', async () => {
		const proof = await buildDpopProof('POST', 'https://oxi.example/api/foo?bar=1');
		expect(proof).not.toBeNull();
		const parts = proof!.split('.');
		expect(parts).toHaveLength(3);

		const header = b64uDecodeJson(parts[0]);
		expect(header.typ).toBe('dpop+jwt');
		expect(header.alg).toBe('ES256');
		const jwk = header.jwk as Record<string, string>;
		expect(jwk.kty).toBe('EC');
		expect(jwk.crv).toBe('P-256');
		expect(jwk.x).toMatch(/^[A-Za-z0-9_-]+$/);
		expect(jwk.y).toMatch(/^[A-Za-z0-9_-]+$/);
		// Only the RFC 7638 members — no `use`, `alg`, `kid` etc leaking in.
		expect(Object.keys(jwk).sort()).toEqual(['crv', 'kty', 'x', 'y']);

		const claims = b64uDecodeJson(parts[1]);
		expect(claims.htm).toBe('POST');
		// htu MUST NOT carry the query string
		expect(claims.htu).toBe('https://oxi.example/api/foo');
		expect(typeof claims.iat).toBe('number');
		expect(typeof claims.jti).toBe('string');
		expect((claims.jti as string).length).toBeGreaterThan(10);
		// No nonce sent when none has been received yet — bootstrap branch.
		expect(claims.nonce).toBeUndefined();

		// Signature bytes are 64 for P-256 raw (R || S)
		expect(b64uDecode(parts[2]).length).toBe(64);
	});

	it('includes the current nonce claim once one has been received', async () => {
		updateNonceFromResponse(
			new Response(null, { status: 200, headers: { 'DPoP-Nonce': 'srv-nonce-abc' } })
		);
		const proof = await buildDpopProof('GET', '/api/auth/me');
		const claims = b64uDecodeJson(proof!.split('.')[1]);
		expect(claims.nonce).toBe('srv-nonce-abc');
	});

	it('mints a fresh jti per call so replay-cache can distinguish', async () => {
		const a = await buildDpopProof('GET', '/api/foo');
		const b = await buildDpopProof('GET', '/api/foo');
		const jtiA = b64uDecodeJson(a!.split('.')[1]).jti as string;
		const jtiB = b64uDecodeJson(b!.split('.')[1]).jti as string;
		expect(jtiA).not.toBe(jtiB);
	});

	it('uppercases the method in the htm claim', async () => {
		const proof = await buildDpopProof('post', '/api/x');
		const claims = b64uDecodeJson(proof!.split('.')[1]);
		expect(claims.htm).toBe('POST');
	});
});

describe('updateNonceFromResponse', () => {
	it('extracts and stores DPoP-Nonce', async () => {
		updateNonceFromResponse(
			new Response(null, { status: 200, headers: { 'DPoP-Nonce': 'nonce-1' } })
		);
		const proof = await buildDpopProof('GET', '/api/x');
		expect(b64uDecodeJson(proof!.split('.')[1]).nonce).toBe('nonce-1');
	});
	it('is a no-op when the header is absent', async () => {
		updateNonceFromResponse(new Response(null, { status: 200 }));
		const proof = await buildDpopProof('GET', '/api/x');
		expect(b64uDecodeJson(proof!.split('.')[1]).nonce).toBeUndefined();
	});
	it('overwrites when a new nonce arrives', async () => {
		updateNonceFromResponse(new Response(null, { status: 200, headers: { 'DPoP-Nonce': 'v1' } }));
		updateNonceFromResponse(new Response(null, { status: 200, headers: { 'DPoP-Nonce': 'v2' } }));
		const proof = await buildDpopProof('GET', '/api/x');
		expect(b64uDecodeJson(proof!.split('.')[1]).nonce).toBe('v2');
	});
});
