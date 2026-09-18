/**
 * WOPI (Collabora / OnlyOffice) integration — ported from features/files/wopiEditor.js.
 * `getEditorUrl` returns the iframe action URL + access token; the office editor
 * is launched by POST-ing the token to that URL (see WopiEditor.svelte).
 */
import { apiFetch, withBase } from '$lib/api/client';

export interface WopiEditorData {
	editor_url: string;
	access_token: string;
	access_token_ttl: string | number;
}

const FALLBACK_EXTS = [
	'docx',
	'doc',
	'odt',
	'rtf',
	'txt',
	'xlsx',
	'xls',
	'ods',
	'csv',
	'pptx',
	'ppt',
	'odp'
];

let cachedExts: string[] | null = null;

export async function getSupportedExtensions(): Promise<string[]> {
	if (cachedExts) return cachedExts;
	try {
		const res = await fetch(withBase('/wopi/supported-extensions'));
		if (res.ok) {
			const exts = (await res.json()) as string[];
			if (Array.isArray(exts) && exts.length > 0) {
				cachedExts = exts;
				return exts;
			}
		}
	} catch {
		/* fall through to the hardcoded list */
	}
	cachedExts = FALLBACK_EXTS;
	return cachedExts;
}

/**
 * What the configured editor actually advertises, or `null` when discovery
 * answers with nothing — no WOPI client configured, or one that is down.
 *
 * `getSupportedExtensions()` hides that case behind [`FALLBACK_EXTS`], which is
 * the right default for *opening* a file the user already has: offering the
 * action and failing loudly beats hiding it because one probe timed out.
 * Creating a document is the opposite call — there is no point offering to make
 * a file nothing can open — so that path asks here instead.
 */
export async function advertisedExtensions(): Promise<string[] | null> {
	try {
		const res = await fetch(withBase('/wopi/supported-extensions'));
		if (!res.ok) return null;
		const exts = (await res.json()) as string[];
		return Array.isArray(exts) && exts.length > 0 ? exts : null;
	} catch {
		return null;
	}
}

export async function canEditWithWopi(filename: string): Promise<boolean> {
	const ext = filename.split('.').pop()?.toLowerCase() ?? '';
	return (await getSupportedExtensions()).includes(ext);
}

export async function getEditorUrl(
	fileId: string,
	action: 'edit' | 'view' = 'edit'
): Promise<WopiEditorData> {
	const res = await apiFetch(
		`/api/wopi/editor-url?file_id=${encodeURIComponent(fileId)}&action=${encodeURIComponent(action)}`,
		{ credentials: 'same-origin' }
	);
	if (!res.ok) {
		const text = await res.text().catch(() => '');
		throw new Error(`Editor URL request failed: ${res.status} ${text}`);
	}
	return (await res.json()) as WopiEditorData;
}

/** PDFs are view-only in WOPI: an edit request returns 422 → retry as view. */
export async function getEditorUrlWithFallback(
	fileId: string,
	filename: string,
	action: 'edit' | 'view' = 'edit'
): Promise<WopiEditorData> {
	try {
		return await getEditorUrl(fileId, action);
	} catch (e) {
		const ext = filename.split('.').pop()?.toLowerCase() ?? '';
		const msg = e instanceof Error ? e.message : '';
		if (action === 'edit' && ext === 'pdf' && msg.includes('422')) {
			return getEditorUrl(fileId, 'view');
		}
		throw e;
	}
}
