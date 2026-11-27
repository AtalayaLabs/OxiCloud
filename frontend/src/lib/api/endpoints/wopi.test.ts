import { it, expect, vi, beforeEach } from 'vitest';
// Keep the real `ApiError` + `apiErrorFromResponse` so wopi's
// post-sweep `throw await apiErrorFromResponse(res)` wires correctly;
// the test's error-mock fakes the Response surface the helper reads.
vi.mock('$lib/api/client', async () => {
	const actual = await vi.importActual<typeof import('$lib/api/client')>('$lib/api/client');
	return {
		...actual,
		apiFetch: vi.fn(),
		apiJson: vi.fn(),
		withBase: (p: string) => p
	};
});
import { apiFetch, apiJson } from '$lib/api/client';
import {
	getSupportedExtensions,
	canEditWithWopi,
	getEditorUrl,
	getEditorUrlWithFallback
} from './wopi';
const af = apiFetch as unknown as ReturnType<typeof vi.fn>;
const j = apiJson as unknown as ReturnType<typeof vi.fn>;
beforeEach(() => {
	vi.clearAllMocks();
	j.mockResolvedValue({ extensions: ['docx', 'xlsx'] });
});
it('reports WOPI edit support based on extension', async () => {
	await getSupportedExtensions().catch(() => {});
	const editable = await canEditWithWopi('report.docx').catch(() => false);
	const notEditable = await canEditWithWopi('photo.png').catch(() => false);
	expect(typeof editable).toBe('boolean');
	expect(typeof notEditable).toBe('boolean');
});

it('fetches an editor URL for a file', async () => {
	af.mockResolvedValue({
		ok: true,
		json: async () => ({
			editor_url: 'https://wopi/edit',
			access_token: 'tok',
			access_token_ttl: 9
		})
	});
	const data = await getEditorUrl('f1', 'edit');
	expect(data.editor_url).toBe('https://wopi/edit');
	expect(af).toHaveBeenCalledWith(
		expect.stringContaining('file_id=f1'),
		expect.objectContaining({ credentials: 'same-origin' })
	);
});

// Mock a non-ok Response in the shape `apiErrorFromResponse` reads:
// needs `clone()` returning something with the same `json()`. Post-sweep
// wopi throws `ApiError` via the shared helper, so pure `{ok, status,
// text}` mocks no longer suffice.
const errRes = (status: number, body: unknown = {}) => {
	const base = { ok: false, status, statusText: '', url: '/mock', json: async () => body };
	return { ...base, clone: () => base } as unknown as Response;
};

it('throws when the editor URL request fails', async () => {
	af.mockResolvedValue(errRes(500, { message: 'boom' }));
	await expect(getEditorUrl('f1')).rejects.toMatchObject({ name: 'ApiError', status: 500 });
});

it('falls back to view mode when an edit request 422s on a PDF', async () => {
	af.mockResolvedValueOnce(errRes(422, { message: 'not editable' })).mockResolvedValueOnce({
		ok: true,
		json: async () => ({ editor_url: 'https://wopi/view', access_token: 't', access_token_ttl: 1 })
	});
	const data = await getEditorUrlWithFallback('f1', 'doc.pdf', 'edit');
	expect(data.editor_url).toBe('https://wopi/view');
	expect(af).toHaveBeenCalledTimes(2);
});

// Regression pin: OnlyOffice advertises only `view` + `convert` (no `edit`)
// for legacy `.doc`; getEditorUrlWithFallback used to limit its 422 -> view
// retry to `ext === 'pdf'`, surfacing an API 422 error on `.doc` open.
it('falls back to view mode when an edit request 422s on a legacy .doc', async () => {
	af.mockResolvedValueOnce(errRes(422, { message: 'not editable' })).mockResolvedValueOnce({
		ok: true,
		json: async () => ({
			editor_url: 'https://wopi/word/view',
			access_token: 't',
			access_token_ttl: 1,
			action: 'view',
			convert_targets: ['docx']
		})
	});
	const data = await getEditorUrlWithFallback('f1', 'MODELE TESTAMENTS.doc', 'edit');
	expect(data.editor_url).toBe('https://wopi/word/view');
	expect(af).toHaveBeenCalledTimes(2);
	expect(af).toHaveBeenLastCalledWith(
		expect.stringContaining('action=view'),
		expect.objectContaining({ credentials: 'same-origin' })
	);
});

it('rethrows 422 when neither edit nor view is supported', async () => {
	af.mockResolvedValue(errRes(422, { message: 'No editor available for .zip files' }));
	await expect(getEditorUrlWithFallback('f1', 'archive.zip', 'edit')).rejects.toMatchObject({
		name: 'ApiError',
		status: 422
	});
	expect(af).toHaveBeenCalledTimes(2);
});

it('passes action=convert and target_ext when requesting conversion', async () => {
	af.mockResolvedValue({
		ok: true,
		json: async () => ({
			editor_url: 'https://wopi/convert-and-edit/doc/docx',
			access_token: 'ctok',
			access_token_ttl: 9,
			action: 'convert'
		})
	});
	const data = await getEditorUrl('f1', 'convert', 'docx');
	expect(data.action).toBe('convert');
	expect(af).toHaveBeenCalledWith(
		'/api/wopi/editor-url?file_id=f1&action=convert&target_ext=docx',
		expect.objectContaining({ credentials: 'same-origin' })
	);
});

it('does not fall back for non-422 edit failures', async () => {
	af.mockResolvedValue(errRes(500, { message: 'server error' }));
	await expect(getEditorUrlWithFallback('f1', 'sheet.xlsx', 'edit')).rejects.toMatchObject({
		name: 'ApiError',
		status: 500
	});
	expect(af).toHaveBeenCalledTimes(1);
});
