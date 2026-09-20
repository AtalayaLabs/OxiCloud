import { describe, expect, it } from 'vitest';

import {
	KIND_AWARENESS,
	KIND_SYNC,
	KIND_UPDATE,
	bytesToUuid,
	decodeFrame,
	encodeFrame,
	uuidToBytes
} from './wireCodec';

describe('wireCodec', () => {
	it('uuidToBytes round-trips through bytesToUuid', () => {
		const uuid = 'f47ac10b-58cc-4372-a567-0e02b2c3d479';
		expect(bytesToUuid(uuidToBytes(uuid))).toBe(uuid);
	});

	it('uuidToBytes rejects malformed input', () => {
		expect(() => uuidToBytes('not-a-uuid')).toThrow();
		expect(() => uuidToBytes('f47ac10b58cc4372a5670e02b2c3d47')).toThrow(); // 31 hex
	});

	it('encodeFrame lays out kind + id + payload', () => {
		const uuid = '00112233-4455-6677-8899-aabbccddeeff';
		const payload = new Uint8Array([0xaa, 0xbb, 0xcc]);
		const frame = encodeFrame(KIND_UPDATE, uuid, payload);
		expect(frame.length).toBe(17 + payload.length);
		expect(frame[0]).toBe(KIND_UPDATE);
		expect(Array.from(frame.subarray(1, 17))).toEqual([
			0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff
		]);
		expect(Array.from(frame.subarray(17))).toEqual([0xaa, 0xbb, 0xcc]);
	});

	it('decodeFrame is the inverse of encodeFrame', () => {
		const uuid = 'f47ac10b-58cc-4372-a567-0e02b2c3d479';
		const payload = new Uint8Array([0x01, 0x02, 0x03, 0x04]);
		const frame = encodeFrame(KIND_SYNC, uuid, payload);
		const decoded = decodeFrame(frame);
		expect(decoded).not.toBeNull();
		expect(decoded!.kind).toBe(KIND_SYNC);
		expect(decoded!.fileId).toBe(uuid);
		expect(Array.from(decoded!.payload)).toEqual([0x01, 0x02, 0x03, 0x04]);
	});

	it('decodeFrame accepts empty payloads (header-only, 17 bytes)', () => {
		const uuid = '00000000-0000-0000-0000-000000000000';
		const frame = encodeFrame(KIND_AWARENESS, uuid, new Uint8Array(0));
		expect(frame.length).toBe(17);
		const decoded = decodeFrame(frame);
		expect(decoded).not.toBeNull();
		expect(decoded!.payload.length).toBe(0);
	});

	it('decodeFrame rejects frames shorter than the header', () => {
		expect(decodeFrame(new Uint8Array(16))).toBeNull();
		expect(decodeFrame(new Uint8Array(0))).toBeNull();
	});

	it('decodeFrame rejects unknown kind bytes', () => {
		const uuid = '00000000-0000-0000-0000-000000000000';
		const bad = encodeFrame(0x01, uuid, new Uint8Array(0));
		bad[0] = 0x77; // not any of UPDATE/AWARENESS/SYNC
		expect(decodeFrame(bad)).toBeNull();
	});
});
