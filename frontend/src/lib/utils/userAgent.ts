/**
 * Parse a raw HTTP `User-Agent` string into a compact human label like
 * "Chrome on Mac" for admin/session UIs. Not a general-purpose UA
 * parser — pragmatic regex-based buckets covering the browsers +
 * operating systems that make up ~99% of real-world traffic, plus the
 * OxiCloud-specific device-auth prefix.
 *
 * Order of detection matters:
 *   * Edge / Opera / Firefox before Chrome (they all include `Chrome/…`)
 *   * Chrome before Safari (Chrome includes `Safari/…`)
 *   * Version check guards Safari against matching a WebKit-based crawler
 *
 * `null` / `undefined` / empty → `"—"` so the admin table renders a
 * consistent placeholder without every callsite writing `?? '—'`.
 */
export function shortUserAgent(ua: string | null | undefined): string {
	if (!ua) return '—';

	// Device-authorization grant sessions carry a bespoke marker
	// (`device:<client_name>`) instead of a browser UA. Pass through.
	if (ua.startsWith('device:')) return ua;

	// Browser detection — order matters.
	let browser: string | null = null;
	if (/\bEdg[eA]?\//.test(ua)) browser = 'Edge';
	else if (/\bOPR\/|Opera\//.test(ua)) browser = 'Opera';
	else if (/\bFirefox\/|FxiOS\//.test(ua)) browser = 'Firefox';
	else if (/\bChrome\//.test(ua)) browser = 'Chrome';
	else if (/\bSafari\//.test(ua) && /\bVersion\//.test(ua)) browser = 'Safari';
	else if (/\bcurl\//.test(ua)) browser = 'curl';
	else if (/\bwget/i.test(ua)) browser = 'wget';
	else if (/\bNextcloud\b/i.test(ua)) browser = 'Nextcloud client';
	else if (/\bnode\b/i.test(ua)) browser = 'Node';

	// OS detection — iOS/iPad before Mac (iPad UAs include "Macintosh" on
	// modern iPadOS "desktop mode"; without the iPad check first they'd
	// be miscategorised as Mac).
	let os: string | null = null;
	if (/Windows/i.test(ua)) os = 'Windows';
	else if (/iPhone|iPad|iPod/i.test(ua)) os = 'iOS';
	else if (/Android/i.test(ua)) os = 'Android';
	else if (/Mac OS X|Macintosh/i.test(ua)) os = 'Mac';
	else if (/CrOS/i.test(ua)) os = 'ChromeOS';
	else if (/Linux/i.test(ua)) os = 'Linux';
	else if (/FreeBSD|OpenBSD|NetBSD/i.test(ua)) os = 'BSD';

	if (browser && os) return `${browser} on ${os}`;
	if (browser) return browser;
	if (os) return os;

	// Unknown shape — truncate the raw string so a huge UA doesn't
	// blow up the table column width. Full string still available in
	// the row's `title=` tooltip.
	return ua.length > 40 ? ua.slice(0, 40) + '…' : ua;
}
