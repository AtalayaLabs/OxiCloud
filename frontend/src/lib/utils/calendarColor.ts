/** Calendar colour helpers.
 *
 * Colours arrive from the server already validated as `#RRGGBB` or
 * `#RRGGBBAA`, but the UI still treats a malformed value as "use the
 * accent token" rather than injecting an arbitrary string into a style
 * attribute.
 */

const HEX_RE = /^#([0-9a-f]{6})([0-9a-f]{2})?$/i;

/** Stable fallbacks for calendars created without a colour (the
 * server-side default calendar has `color: null`). Hashing the
 * calendar id keeps every calendar a consistent colour across
 * reloads without a per-calendar migration. */
const FALLBACK_PALETTE = [
	'#3366cc',
	'#dc3912',
	'#109618',
	'#990099',
	'#ff9900',
	'#0099c6',
	'#dd4477',
	'#66aa00'
];

export interface ReadableColor {
	background: string;
	foreground: string;
}

function hashString(value: string): number {
	let hash = 0;
	for (let i = 0; i < value.length; i++) {
		hash = (hash * 31 + value.charCodeAt(i)) | 0;
	}
	return Math.abs(hash);
}

export function readableEventColor(color: string | null | undefined, seed = ''): ReadableColor {
	const match = color ? HEX_RE.exec(color) : null;
	if (!match) {
		if (!seed) {
			return { background: 'var(--color-accent)', foreground: 'var(--color-on-accent)' };
		}
		const fallback = FALLBACK_PALETTE[hashString(seed) % FALLBACK_PALETTE.length];
		return {
			background: fallback,
			foreground: readableEventColor(fallback).foreground
		};
	}
	const hex = match[1];
	const r = parseInt(hex.slice(0, 2), 16);
	const g = parseInt(hex.slice(2, 4), 16);
	const b = parseInt(hex.slice(4, 6), 16);
	// WCAG relative luminance; threshold 0.45 keeps mid yellows dark-text.
	const luminance = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
	return {
		background: color as string,
		foreground: luminance > 0.45 ? '#111827' : '#ffffff'
	};
}
