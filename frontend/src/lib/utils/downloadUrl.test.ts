import { describe, expect, it } from 'vitest';
import { downloadUrlPayload } from './downloadUrl';

describe('downloadUrlPayload', () => {
	it('keeps a colon in the filename from shifting the URL delimiter', () => {
		expect(downloadUrlPayload('report 10:30.txt', 'https://example.test/oxi/api/files/1')).toBe(
			'application/octet-stream:report 10_30.txt:https://example.test/oxi/api/files/1'
		);
	});

	it('preserves percent characters in names and the actual URL', () => {
		expect(downloadUrlPayload('100% report.txt', 'https://example.test/a%2Fb')).toBe(
			'application/octet-stream:100% report.txt:https://example.test/a%2Fb'
		);
	});
});
