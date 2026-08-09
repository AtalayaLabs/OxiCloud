import { describe, it, expect } from 'vitest';
import { shortUserAgent } from './userAgent';

describe('shortUserAgent', () => {
	it('placeholder for missing input', () => {
		expect(shortUserAgent(null)).toBe('—');
		expect(shortUserAgent(undefined)).toBe('—');
		expect(shortUserAgent('')).toBe('—');
	});

	it('device-auth marker passes through unchanged', () => {
		expect(shortUserAgent('device:my-tv-42')).toBe('device:my-tv-42');
	});

	it.each([
		[
			'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36',
			'Chrome on Mac'
		],
		[
			'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0 Safari/537.36',
			'Chrome on Windows'
		],
		[
			'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0 Safari/537.36',
			'Chrome on Linux'
		],
		[
			'Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:126.0) Gecko/20100101 Firefox/126.0',
			'Firefox on Windows'
		],
		[
			'Mozilla/5.0 (X11; Ubuntu; Linux x86_64; rv:126.0) Gecko/20100101 Firefox/126.0',
			'Firefox on Linux'
		],
		[
			'Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15',
			'Safari on Mac'
		],
		[
			'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1',
			'Safari on iOS'
		],
		[
			// iPad on iPadOS 13+ ships a UA with "Macintosh" — must NOT
			// mis-detect as Mac. Guarded by iOS-first ordering.
			'Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1',
			'Safari on iOS'
		],
		[
			'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Mobile Safari/537.36',
			'Chrome on Android'
		],
		[
			'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36 Edg/125.0.2535.51',
			'Edge on Windows'
		],
		[
			'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36 OPR/110.0.0.0',
			'Opera on Linux'
		],
		['curl/8.7.1', 'curl'],
		['Wget/1.21.4', 'wget'],
		['Mozilla/5.0 (Nextcloud desktop client 3.14.2 stable-x86_64)', 'Nextcloud client'],
		['node-fetch/1.0 (+https://github.com/bitinn/node-fetch)', 'Node']
	])('%s → %s', (ua, expected) => {
		expect(shortUserAgent(ua)).toBe(expected);
	});

	it('truncates unknown UA shapes past 40 chars', () => {
		const long = 'SomeWeirdCrawler/1.0 with a very long description of its capabilities';
		const result = shortUserAgent(long);
		expect(result.endsWith('…')).toBe(true);
		expect(result.length).toBeLessThanOrEqual(41);
	});

	it('returns short unknown UA verbatim', () => {
		expect(shortUserAgent('MyBot/1.0')).toBe('MyBot/1.0');
	});
});
