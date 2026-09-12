import { describe, it, expect } from 'vitest';
import { parseIncoming, pingFrame, subscribeFrame, unsubscribeFrame } from './frames';

describe('frame builders', () => {
	it('subscribeFrame produces a valid JSON-RPC 2.0 request', () => {
		expect(subscribeFrame(7, 'folder:abc')).toEqual({
			jsonrpc: '2.0',
			id: 7,
			method: 'rt.subscribe',
			params: { topic: 'folder:abc' }
		});
	});

	it('unsubscribeFrame mirrors the subscribe shape', () => {
		expect(unsubscribeFrame(8, 'folder:abc')).toEqual({
			jsonrpc: '2.0',
			id: 8,
			method: 'rt.unsubscribe',
			params: { topic: 'folder:abc' }
		});
	});

	it('pingFrame omits params entirely (matches the wire spec)', () => {
		const frame = pingFrame(9);
		expect(frame).toEqual({ jsonrpc: '2.0', id: 9, method: 'rt.ping' });
		expect('params' in frame).toBe(false);
	});
});

describe('parseIncoming', () => {
	it('recognises an `rt.event` notification', () => {
		const raw = JSON.stringify({
			jsonrpc: '2.0',
			method: 'rt.event',
			params: {
				topic: 'folder:abc',
				event: 'file_created',
				data: { file_id: 'x', name: 'a.txt', parent_id: 'abc', actor: 'me' }
			}
		});
		const result = parseIncoming(raw);
		expect(result.kind).toBe('event');
		if (result.kind === 'event') {
			expect(result.params.topic).toBe('folder:abc');
			expect(result.params.event).toBe('file_created');
		}
	});

	it('recognises an `rt.revoked` notification', () => {
		const raw = JSON.stringify({
			jsonrpc: '2.0',
			method: 'rt.revoked',
			params: { topic: 'folder:abc', reason: 'grant_revoked' }
		});
		const result = parseIncoming(raw);
		expect(result.kind).toBe('revoked');
		if (result.kind === 'revoked') expect(result.params.topic).toBe('folder:abc');
	});

	it('recognises a success response', () => {
		const raw = JSON.stringify({
			jsonrpc: '2.0',
			id: 42,
			result: { subscribed: 'folder:abc' }
		});
		const result = parseIncoming(raw);
		expect(result.kind).toBe('success');
		if (result.kind === 'success') {
			expect(result.id).toBe(42);
			expect(result.result).toEqual({ subscribed: 'folder:abc' });
		}
	});

	it('recognises an error response and preserves the code', () => {
		const raw = JSON.stringify({
			jsonrpc: '2.0',
			id: 42,
			error: { code: -32001, message: 'no_read', data: { topic: 'folder:xyz' } }
		});
		const result = parseIncoming(raw);
		expect(result.kind).toBe('error');
		if (result.kind === 'error') {
			expect(result.id).toBe(42);
			expect(result.error.code).toBe(-32001);
			expect(result.error.message).toBe('no_read');
		}
	});

	it('collapses malformed frames to `ignore` with a stable reason key', () => {
		expect(parseIncoming('not-json').kind).toBe('ignore');
		expect(parseIncoming('[]').kind).toBe('ignore');
		expect(parseIncoming(JSON.stringify({ jsonrpc: '1.0', method: 'rt.event' })).kind).toBe(
			'ignore'
		);
		expect(parseIncoming(JSON.stringify({ jsonrpc: '2.0', method: 'rt.unknown' })).kind).toBe(
			'ignore'
		);
	});

	it('never throws — always returns a discriminated result', () => {
		// Random shapes that used to trigger throws in earlier drafts.
		const cases: string[] = ['', 'null', '42', '{}', '{"jsonrpc":"2.0"}'];
		for (const c of cases) {
			expect(() => parseIncoming(c)).not.toThrow();
		}
	});
});
