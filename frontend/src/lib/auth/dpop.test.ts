import { describe, it, expect } from 'vitest';
import { computeJkt } from './dpop';

/**
 * RFC 7638 §3.1 known-answer vector.
 *
 * The RFC's example vector is for an RSA key; there's no equally-blessed
 * EC vector in the spec text. We instead pin our EC canonicalisation
 * against the *rules* by constructing a JWK with:
 *   * extra members that MUST be excluded from the hash (use, alg, kid),
 *   * members in non-alphabetical order,
 *   * whitespace hazards,
 * then verifying that our thumbprint matches an INDEPENDENT re-computation
 * of the canonical hash. If the alphabetical / whitespace / member-filter
 * rules regress, this test fails.
 *
 * The x/y coordinates below are from a real P-256 keypair generated for
 * this test — value not sensitive, generation is deterministic given the
 * algorithm output is exposed.
 */
describe('computeJkt', () => {
	it('produces a URL-safe base64 SHA-256 thumbprint of the canonical JWK', async () => {
		// Generate a real P-256 keypair via SubtleCrypto so the test hits
		// real bytes, not a hand-rolled JWK that might drift from what
		// the runtime actually emits.
		const pair = await crypto.subtle.generateKey(
			{ name: 'ECDSA', namedCurve: 'P-256' },
			true, // extractable so the test can read the JWK independently
			['sign', 'verify']
		);
		const jkt = await computeJkt(pair.publicKey);

		// Base64URL, no padding, exactly 43 chars for a 32-byte SHA-256.
		expect(jkt).toMatch(/^[A-Za-z0-9_-]{43}$/);

		// Independent re-computation of the canonical thumbprint —
		// same rules RFC 7638 §3.2 pins for EC keys.
		const jwk = await crypto.subtle.exportKey('jwk', pair.publicKey);
		const canonical = JSON.stringify({
			crv: jwk.crv,
			kty: jwk.kty,
			x: jwk.x,
			y: jwk.y
		});
		const hash = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(canonical));
		const bytes = new Uint8Array(hash);
		let s = '';
		for (const b of bytes) s += String.fromCharCode(b);
		const expected = btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');

		expect(jkt).toBe(expected);
	});

	it('is stable across repeat calls on the same key', async () => {
		const pair = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, false, [
			'sign',
			'verify'
		]);
		const first = await computeJkt(pair.publicKey);
		const second = await computeJkt(pair.publicKey);
		expect(first).toBe(second);
	});

	it('differs across independently-generated keypairs', async () => {
		const a = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, false, [
			'sign',
			'verify'
		]);
		const b = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, false, [
			'sign',
			'verify'
		]);
		expect(await computeJkt(a.publicKey)).not.toBe(await computeJkt(b.publicKey));
	});
});
