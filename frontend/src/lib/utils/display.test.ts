import { describe, it, expect } from 'vitest';
import { iconNameFor, textyExtensionsWithoutIcon, formatDate } from './display';
import { OxiIcons } from '$lib/icons/registry';

/**
 * `iconNameFor` replaced `iconNameFromClass` in step 3 of
 * `docs/plan/retire-precomputed-display-fields.md`.
 *
 * The old function parsed a glyph name back out of the server's `icon_class`
 * ("fas fa-file-pdf" → "file-pdf"), which meant the API serialised a CSS
 * class string that was never used as a CSS class. The classification now
 * happens here, from `mime_type` + filename.
 *
 * These cases are the port of the deleted Rust
 * `test_icon_class_for_with_extension_fallback` — same corpus, same
 * resolution order, asserting registry names rather than CSS classes — plus
 * the cases that only matter client-side.
 */
describe('iconNameFor', () => {
	it('resolves specific MIME types', () => {
		expect(iconNameFor('doc.pdf', 'application/pdf')).toBe('file-pdf');
		expect(
			iconNameFor(
				'file.docx',
				'application/vnd.openxmlformats-officedocument.wordprocessingml.document'
			)
		).toBe('file-word');
		expect(iconNameFor('data.csv', 'text/csv')).toBe('file-excel');
		expect(iconNameFor('pack.zip', 'application/zip')).toBe('file-archive');
	});

	it('resolves MIME prefixes', () => {
		expect(iconNameFor('a.jpg', 'image/jpeg')).toBe('file-image');
		expect(iconNameFor('a.mkv', 'video/x-matroska')).toBe('file-video');
		expect(iconNameFor('a.mp3', 'audio/mpeg')).toBe('file-audio');
		expect(iconNameFor('a.py', 'text/x-python')).toBe('file-code');
		expect(iconNameFor('readme', 'text/plain')).toBe('file-alt');
	});

	// The ordering that matters: a specific MIME wins over the extension, so a
	// mislabelled extension cannot override a known type. This is also the
	// property that lets the extension arms be deleted later without changing
	// any row whose MIME is accurate.
	it('prefers MIME over extension when both are known', () => {
		expect(iconNameFor('actually-an-image.zip', 'image/png')).toBe('file-image');
	});

	it('falls back to the extension when MIME is generic', () => {
		expect(iconNameFor('script.py', 'application/octet-stream')).toBe('file-code');
		expect(iconNameFor('app.dmg', 'application/octet-stream')).toBe('hdd');
		expect(iconNameFor('archive.zip', 'application/octet-stream')).toBe('file-archive');
		expect(iconNameFor('data.xlsx', 'application/octet-stream')).toBe('file-excel');
		expect(iconNameFor('run.sh', 'application/octet-stream')).toBe('terminal');
	});

	it('lowercases the extension and ignores directories in the path', () => {
		expect(iconNameFor('IMG_2026.JPG', '')).toBe('file-image');
		expect(iconNameFor('song.FLAC', 'application/octet-stream')).toBe('file-audio');
		// Only the final segment's final extension counts — a dot in a parent
		// directory must not be read as the file's type.
		expect(iconNameFor('/my.photos/notes.md', '')).toBe('file-alt');
	});

	it('returns the generic glyph when nothing identifies the file', () => {
		expect(iconNameFor('no_extension', 'application/octet-stream')).toBe('file');
		expect(iconNameFor('blob.xyzzy', 'application/octet-stream')).toBe('file');
		expect(iconNameFor(null, null)).toBe('file');
		expect(iconNameFor('', '')).toBe('file');
	});

	// A dotfile has no extension — `.bashrc` is not a file of type "bashrc".
	// Mirrors the server's `ext_of`, which rejects a leading dot.
	it('treats a leading dot as a dotfile, not an extension', () => {
		expect(iconNameFor('.bashrc', '')).toBe('file');
		// …but MIME still decides when it is known, as it did server-side.
		expect(iconNameFor('.bashrc', 'text/plain')).toBe('file-alt');
	});

	// Every glyph the classifier can name must exist in the icon registry, or
	// the row renders blank. This is the check the Rust side never needed —
	// there the value was a CSS class, so a typo degraded silently; here it is
	// a registry key, so a typo is a missing glyph.
	it('only returns names present in the icon registry', () => {
		const samples: Array<[string, string]> = [
			['a.pdf', 'application/pdf'],
			['a.docx', 'application/msword'],
			['a.xlsx', 'application/vnd.ms-excel'],
			['a.pptx', 'application/vnd.ms-powerpoint'],
			['a.zip', 'application/zip'],
			['a.json', 'application/json'],
			['a.dmg', 'application/x-apple-diskimage'],
			['a.png', 'image/png'],
			['a.mp4', 'video/mp4'],
			['a.mp3', 'audio/mpeg'],
			['a.txt', 'text/plain'],
			['a.sh', 'application/octet-stream'],
			['a.unknown', '']
		];
		for (const [name, mime] of samples) {
			const glyph = iconNameFor(name, mime);
			expect(OxiIcons, `glyph "${glyph}" for ${name} (${mime})`).toHaveProperty(glyph);
		}
	});

	// `TEXTY_EXT_RE` gates whether the collab editor mounts; the icon table
	// decides which glyph is drawn. They are two spellings of the same
	// judgement and used to be kept aligned with the Rust classifier by
	// comment alone. Now that both live in the FE, drift is a test failure.
	it('has a glyph for every extension the texty gate accepts', () => {
		expect(textyExtensionsWithoutIcon()).toEqual([]);
	});
});

describe('formatDate', () => {
	it('formats epoch seconds and milliseconds', () => {
		expect(formatDate(1_700_000_000)).toMatch(/\d{4}/); // seconds
		expect(formatDate(1_700_000_000_000)).toMatch(/\d{4}/); // ms
	});
	it('formats ISO-8601 strings', () => {
		expect(formatDate('2024-01-15')).toMatch(/2024/);
	});
	it('returns empty string for null/undefined/invalid', () => {
		expect(formatDate(null)).toBe('');
		expect(formatDate(undefined)).toBe('');
		expect(formatDate('definitely not a date')).toBe('');
	});
});
