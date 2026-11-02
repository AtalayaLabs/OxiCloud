/**
 * WOPI (Collabora / OnlyOffice) integration — ported from features/files/wopiEditor.js.
 * `getEditorUrl` returns the iframe action URL + access token; the office editor
 * is launched by POST-ing the token to that URL (see WopiEditor.svelte).
 */
import { ApiError, apiErrorFromResponse, apiFetch } from '$lib/api/client';

export interface WopiEditorData {
	editor_url: string;
	access_token: string;
	access_token_ttl: string | number;
}

// Fallback for when `/wopi/supported-extensions` discovery fails or
// hasn't landed yet. Restricted to Office-shaped formats that a
// WOPI editor uniquely handles — anything text-shaped (`.txt`,
// `.md`, code, config) belongs in the collab editor instead (see
// `$lib/utils/textyExt`). Including `.txt` here surfaced a WOPI
// "Edit" action on plain-text files even when no editor could
// handle them — the collab editor is the right home for those.
const FALLBACK_EXTS = [
	'docx',
	'doc',
	'odt',
	'rtf',
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
		const res = await apiFetch('/wopi/supported-extensions', { retryUnauthorized: false });
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
		const res = await apiFetch('/wopi/supported-extensions', { retryUnauthorized: false });
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
	if (!res.ok) throw await apiErrorFromResponse(res);
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
		// Branch on the ApiError's structural `status`, not a substring of
		// `.message`. Pre-sweep the thrown Error's message was
		// "Editor URL request failed: 422 not editable" (status in the
		// string), so `msg.includes('422')` sufficed; post-sweep
		// `apiErrorFromResponse` surfaces the server's own `message`
		// (just "not editable") which no longer carries the status. See
		// `project_error_type_contract` — the FE must switch on
		// structured fields, not message text.
		const ext = filename.split('.').pop()?.toLowerCase() ?? '';
		if (action === 'edit' && ext === 'pdf' && e instanceof ApiError && e.status === 422) {
			return getEditorUrl(fileId, 'view');
		}
		throw e;
	}
}
