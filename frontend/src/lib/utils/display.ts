/** Display helpers shared across list views. */

import { TEXTY_EXT_RE } from './textyExt';

/**
 * Icon registry name for a file, from its MIME type and filename.
 *
 * This is a port of the server's `display_helpers.rs::icon_class_with_ext`,
 * resolving straight to a registry key instead of going through a CSS class
 * string. The server used to send `icon_class` ("fas fa-file-pdf"); the only
 * thing the SPA ever did with it was parse the FA token back out, so the API
 * was serialising a CSS class that was never used as a CSS class. Step 3 of
 * `docs/plan/retire-precomputed-display-fields.md` moved the classification
 * here and step 4 dropped the field.
 *
 * Resolution order matches the server exactly — specific MIME, then MIME
 * prefix, then extension — because the extension arms are the fallback for
 * rows whose stored MIME is still generic (`application/octet-stream`). Once
 * content-sniffing has normalised those (`docs/plan/blob-resniff.md`), the
 * extension half can go, here and in the Rust.
 *
 * Folders do not come through here: callers pass `'folder'` directly, as the
 * server did with its `"fas fa-folder"` constant.
 */
export function iconNameFor(name: string | undefined | null, mime: string | undefined | null) {
	const m = mime ?? '';

	// 1. Specific MIME matches.
	switch (m) {
		case 'application/pdf':
			return 'file-pdf';
		case 'application/msword':
		case 'application/vnd.openxmlformats-officedocument.wordprocessingml.document':
		case 'application/vnd.oasis.opendocument.text':
			return 'file-word';
		case 'application/vnd.ms-excel':
		case 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet':
		case 'application/vnd.oasis.opendocument.spreadsheet':
		case 'text/csv':
			return 'file-excel';
		case 'application/vnd.ms-powerpoint':
		case 'application/vnd.openxmlformats-officedocument.presentationml.presentation':
		case 'application/vnd.oasis.opendocument.presentation':
			return 'file-powerpoint';
		case 'application/zip':
		case 'application/x-rar-compressed':
		case 'application/vnd.rar':
		case 'application/x-7z-compressed':
		case 'application/gzip':
		case 'application/x-gzip':
		case 'application/x-tar':
		case 'application/x-bzip2':
		case 'application/x-xz':
		case 'application/x-compress':
			return 'file-archive';
		case 'application/json':
		case 'application/ld+json':
		case 'application/javascript':
		case 'application/typescript':
		case 'application/x-httpd-php':
		case 'application/xml':
		case 'application/xhtml+xml':
		case 'application/sql':
		case 'application/x-yaml':
		case 'application/toml':
		case 'application/x-sh':
		case 'application/x-shellscript':
		case 'application/x-csh':
			return 'file-code';
		case 'application/x-apple-diskimage':
		case 'application/x-ms-dos-executable':
		case 'application/x-msdownload':
		case 'application/x-msi':
		case 'application/vnd.debian.binary-package':
		case 'application/x-rpm':
		case 'application/vnd.appimage':
			return 'hdd';
	}

	// 2. MIME prefix matches.
	if (m.startsWith('image/')) return 'file-image';
	if (m.startsWith('video/')) return 'file-video';
	if (m.startsWith('audio/')) return 'file-audio';
	if (
		m.startsWith('text/x-script') ||
		m.startsWith('text/x-python') ||
		m.startsWith('text/x-java') ||
		m.startsWith('text/x-c') ||
		m.startsWith('text/x-rust') ||
		m.startsWith('text/x-go') ||
		m.startsWith('text/x-ruby') ||
		m.startsWith('text/x-shellscript') ||
		m.startsWith('text/x-php') ||
		m.includes('javascript') ||
		m.includes('typescript')
	) {
		return 'file-code';
	}
	if (m.startsWith('text/')) return 'file-alt';

	// 3. Extension fallback, for generic or empty MIME.
	const ext = extensionOf(name);
	if (ext === null) return 'file';
	return ICON_BY_EXT[ext] ?? 'file';
}

/**
 * Lowercased final extension of a filename, or `null` when there isn't one.
 *
 * Mirrors the server's `ext_of`: splits on the last `.` of the last path
 * segment, and treats a leading dot as "no extension" so `.bashrc` is a
 * dotfile rather than a file of type `bashrc`.
 */
function extensionOf(name: string | undefined | null): string | null {
	if (!name) return null;
	const base = name.slice(name.lastIndexOf('/') + 1);
	const dot = base.lastIndexOf('.');
	if (dot <= 0 || dot === base.length - 1) return null;
	return base.slice(dot + 1).toLowerCase();
}

/**
 * Extension → registry name, flattened from the server's match arms.
 *
 * Built once at module scope from grouped lists so the table reads like the
 * Rust `match` it ports rather than as 60 repeated literals — and so adding a
 * format means touching one array.
 */
const ICON_BY_EXT: Record<string, string> = Object.fromEntries(
	(
		[
			[['pdf'], 'file-pdf'],
			[['doc', 'docx', 'odt', 'rtf'], 'file-word'],
			[['xls', 'xlsx', 'ods', 'csv'], 'file-excel'],
			[['ppt', 'pptx', 'odp', 'key'], 'file-powerpoint'],
			[
				[
					'jpg',
					'jpeg',
					'png',
					'gif',
					'bmp',
					'svg',
					'webp',
					'ico',
					'tiff',
					'tif',
					'heic',
					'heif',
					'avif'
				],
				'file-image'
			],
			[['mp4', 'avi', 'mkv', 'mov', 'wmv', 'flv', 'webm', 'm4v'], 'file-video'],
			[['mp3', 'wav', 'ogg', 'flac', 'aac', 'wma', 'm4a', 'opus'], 'file-audio'],
			[['zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz', 'zst', 'lz4'], 'file-archive'],
			[['exe', 'msi', 'dmg', 'deb', 'rpm', 'appimage', 'pkg', 'snap', 'flatpak'], 'hdd'],
			[
				[
					'js',
					'jsx',
					'mjs',
					'cjs',
					'ts',
					'tsx',
					'py',
					'pyw',
					'rs',
					'go',
					'java',
					'kt',
					'kts',
					'scala',
					'c',
					'h',
					'cpp',
					'hpp',
					'cc',
					'cxx',
					'cs',
					'rb',
					'php',
					'swift',
					'r',
					'lua',
					'pl',
					'pm',
					'html',
					'htm',
					'css',
					'scss',
					'sass',
					'less',
					'json',
					'xml',
					'yaml',
					'yml',
					'toml',
					'ini',
					'cfg',
					'conf',
					'sql',
					'graphql',
					'proto',
					'vue',
					'svelte'
				],
				'file-code'
			],
			[['sh', 'bash', 'zsh', 'fish', 'ps1', 'bat', 'cmd'], 'terminal'],
			// `log` is NOT in the Rust table this ports. The drift assertion
			// below (`textyExtensionsWithoutIcon`) caught it on its first run:
			// `.log` has always been in `TEXTY_EXT_RE`, so the SPA opens a log
			// file in the collab editor, while the server classified it to the
			// generic `fas fa-file`. Adding it makes the glyph agree with the
			// behaviour; a `.log` file is text-shaped by every other measure.
			[['md', 'markdown', 'rst', 'txt', 'log'], 'file-alt']
		] as const
	).flatMap(([exts, icon]) => exts.map((e) => [e, icon]))
);

/**
 * Every extension the text/code gate accepts must also resolve to a glyph.
 *
 * `TEXTY_EXT_RE` (`utils/textyExt.ts`) and the table above are two spellings
 * of the same "this is a text-shaped file" judgement — the regex decides
 * whether to mount the collab editor, the table decides which glyph to draw.
 * They were kept in step with the Rust classifier by comment alone; now that
 * both live here, this assertion makes a drift a test failure instead of a
 * mismatched icon nobody notices. Exported for `display.test.ts` rather than
 * run at import time, so a production bundle pays nothing for it.
 */
export function textyExtensionsWithoutIcon(): string[] {
	const source = TEXTY_EXT_RE.source;
	const inner = source.slice(source.indexOf('(') + 1, source.indexOf(')'));
	return inner.split('|').filter((ext) => !(ext in ICON_BY_EXT));
}

/**
 * Coarse colour bucket for a resolved icon name (see {@link iconNameFor}).
 * One hue per broad file family so the grid/list glyphs render in type-specific
 * colours instead of a flat monochrome. Each bucket is backed by a
 * `--file-kind-*` token (variables.css) and consumed via the `.file-icon--*`
 * modifier classes (resourceList.css).
 */
export function fileIconKind(iconName: string): string {
	switch (iconName) {
		case 'folder':
		case 'folder-open':
			return 'folder';
		case 'file-pdf':
			return 'pdf';
		case 'file-word':
			return 'doc';
		case 'file-excel':
			return 'sheet';
		case 'file-powerpoint':
			return 'slides';
		case 'file-archive':
		case 'file-zipper':
			return 'archive';
		case 'file-code':
			return 'code';
		case 'file-image':
			return 'image';
		case 'file-video':
			return 'video';
		case 'file-audio':
			return 'audio';
		case 'file-alt':
		case 'file-lines':
			return 'text';
		default:
			return 'generic';
	}
}

/** `.file-icon` colour-bucket modifier class for a resolved icon name. */
export function fileIconKindClass(iconName: string): string {
	return `file-icon--${fileIconKind(iconName)}`;
}

/**
 * Module-scope cache of `Intl.DateTimeFormat` instances, keyed by
 * `(locale, options signature)`. Constructing a formatter runs the full ICU
 * locale/pattern resolution (~50–200µs) while a `format()` call is ~1µs, and
 * {@link formatDate} runs roughly twice per row as large file lists render
 * and scroll — so a construct-per-call implementation (what
 * `toLocaleDateString(locale, options)` does under the hood) dominated list
 * fill. Entries are keyed by the locale actually requested — never frozen at
 * first use — so a runtime locale change just resolves a different entry.
 */
const dateTimeFormatCache = new Map<string, Intl.DateTimeFormat>();

// Entries built with `locale === undefined` snapshot the environment default
// locale at construction time. `toLocaleDateString(undefined, …)` re-reads the
// default on every call, so drop the cache if the default changes to keep the
// cached path behaviourally identical.
if (typeof window !== 'undefined') {
	window.addEventListener('languagechange', () => dateTimeFormatCache.clear());
}

/**
 * Cached equivalent of `new Intl.DateTimeFormat(locale, options)`.
 *
 * `date.toLocaleDateString(locale, options)` / `toLocaleTimeString(…)` are
 * specified (ECMA-402) as building exactly this formatter per call — and
 * their component defaulting is a no-op once `options` names any date/time
 * component — so `dateTimeFormatFor(locale, options).format(date)` is
 * output-identical while paying construction once per (locale, options).
 *
 * The options signature uses `JSON.stringify`, so pass options as a hoisted
 * const or an inline literal (stable key order per callsite); a differently
 * ordered but equal object would only create a redundant entry, never a wrong
 * result.
 */
export function dateTimeFormatFor(
	locale: string | undefined,
	options?: Intl.DateTimeFormatOptions
): Intl.DateTimeFormat {
	const key = `${locale ?? ''}|${options ? JSON.stringify(options) : ''}`;
	let fmt = dateTimeFormatCache.get(key);
	if (!fmt) {
		fmt = new Intl.DateTimeFormat(locale, options);
		dateTimeFormatCache.set(key, fmt);
	}
	return fmt;
}

/** Options for {@link formatDate}, hoisted so every call shares one cache key. */
const FORMAT_DATE_OPTS: Intl.DateTimeFormatOptions = {
	year: 'numeric',
	month: 'short',
	day: 'numeric'
};

/** Format a timestamp (epoch seconds/ms or ISO-8601 string) as a local date. */
export function formatDate(value: number | string | null | undefined): string {
	if (value === null || value === undefined) return '';
	let d: Date;
	if (typeof value === 'number') {
		// Heuristic: seconds vs milliseconds.
		d = new Date(value < 1e12 ? value * 1000 : value);
	} else {
		d = new Date(value);
	}
	if (Number.isNaN(d.getTime())) return '';
	return dateTimeFormatFor(undefined, FORMAT_DATE_OPTS).format(d);
}
