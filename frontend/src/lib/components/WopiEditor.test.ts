import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/svelte';

vi.mock('$lib/api/endpoints/wopi', () => ({
	getEditorUrlWithFallback: vi.fn(),
	// Real shape, not a stub returning undefined: the component feeds this
	// straight into the iframe `src`, so a bad value fails as a missing
	// editor rather than a type error.
	wopiHostPageUrl: (fileId: string, token: string) => `/wopi/edit/${fileId}?access_token=${token}`
}));
vi.mock('$lib/utils/errors', () => ({ errorToast: vi.fn() }));

import { getEditorUrlWithFallback } from '$lib/api/endpoints/wopi';
import { errorToast } from '$lib/utils/errors';
import WopiEditor from './WopiEditor.svelte';

const g = getEditorUrlWithFallback as unknown as ReturnType<typeof vi.fn>;
const et = errorToast as unknown as ReturnType<typeof vi.fn>;

/** The editor the server resolved via discovery, and its origin. */
const EDITOR_URL = 'https://collabora.test:9980/cool/word/edit?WOPISrc=x';
const EDITOR_ORIGIN = 'https://collabora.test:9980';

beforeEach(() => vi.clearAllMocks());

it('opens, loads the editor URL, and frames OUR host page', async () => {
	g.mockResolvedValue({
		editor_url: EDITOR_URL,
		access_token: 'tok',
		access_token_ttl: 9999
	});
	render(WopiEditor, { props: { open: true, fileId: 'f1', fileName: 'doc.docx', action: 'edit' } });
	await waitFor(() => expect(g).toHaveBeenCalledWith('f1', 'doc.docx', 'edit'));
	expect(screen.getByTestId('wopi-editor-dialog')).toBeTruthy();

	// The iframe must point at our own `/wopi/edit/{id}` host page, NOT at
	// the editor. The SPA used to POST the token form straight to
	// `editor_url`, which forced the shipped CSP to allow `form-action
	// https:` — any https origin could receive a token-bearing form, and a
	// cleartext editor was excluded outright. If someone reinstates the
	// direct POST, this assertion is what notices.
	const frame = await waitFor(() => screen.getByTestId('wopi-editor-frame'));
	expect(frame.getAttribute('src')).toBe('/wopi/edit/f1?access_token=tok');
	expect(frame.getAttribute('src')).not.toContain('collabora.test');

	// MUST NOT be named `wopi_frame`. The host page's form targets that
	// name, and a navigable is named after the iframe containing it — so
	// if this element claims it, the host page's own context matches the
	// target and the editor loads HERE, under the SPA's
	// `frame-src 'self' blob:`, which blocks the cross-origin editor.
	// Symptom in production was Chrome's "this content is blocked" with
	// the host page's (correct) policy never consulted. Caught by hand on
	// a real deployment, not by any test — hence this pin.
	expect(frame.getAttribute('name')).not.toBe('wopi_frame');
});

it('drops the spinner on an App_LoadingStatus message and closes on UI_Close', async () => {
	g.mockResolvedValue({ editor_url: EDITOR_URL, access_token: 't', access_token_ttl: 1 });
	const onclose = vi.fn();
	render(WopiEditor, { props: { open: true, fileId: 'f1', fileName: 'd.docx', onclose } });
	await waitFor(() => screen.getByTestId('wopi-editor-dialog'));
	await waitFor(() => screen.getByTestId('wopi-editor-frame'));

	window.dispatchEvent(
		new MessageEvent('message', {
			origin: EDITOR_ORIGIN,
			data: JSON.stringify({
				MessageId: 'App_LoadingStatus',
				Values: { Status: 'Document_Loaded' }
			})
		})
	);
	window.dispatchEvent(
		new MessageEvent('message', {
			origin: EDITOR_ORIGIN,
			data: JSON.stringify({ MessageId: 'UI_Close' })
		})
	);
	await waitFor(() => expect(onclose).toHaveBeenCalled());
});

it('ignores a postMessage from any origin but the editor', async () => {
	// `onMessage` used to act on a message from ANY origin — `window`
	// receives postMessage from anything that can reach it (an opener, any
	// frame), so a hostile page could close the editor out from under the
	// user mid-edit. Low blast radius, but an unauthenticated message
	// handler, and the legitimate origin is known.
	//
	// Pins the guard, not just "the handler runs": the UI_Close below is
	// byte-identical to the one the previous test acts on — only `origin`
	// differs. Drop the check and this test goes green for the wrong
	// reason, which is why both tests exist.
	g.mockResolvedValue({ editor_url: EDITOR_URL, access_token: 't', access_token_ttl: 1 });
	const onclose = vi.fn();
	render(WopiEditor, { props: { open: true, fileId: 'f1', fileName: 'd.docx', onclose } });
	await waitFor(() => screen.getByTestId('wopi-editor-frame'));

	window.dispatchEvent(
		new MessageEvent('message', {
			origin: 'https://evil.example',
			data: JSON.stringify({ MessageId: 'UI_Close' })
		})
	);
	// Give the handler a turn; it must have done nothing.
	await new Promise((r) => setTimeout(r, 10));
	expect(onclose).not.toHaveBeenCalled();
	expect(screen.getByTestId('wopi-editor-dialog')).toBeTruthy();
});

it('reports a load failure via errorToast', async () => {
	g.mockRejectedValue(new Error('no wopi host'));
	render(WopiEditor, { props: { open: true, fileId: 'f1', fileName: 'd.docx' } });
	await waitFor(() => expect(et).toHaveBeenCalled());
});

it('does not load when closed', () => {
	render(WopiEditor, { props: { open: false, fileId: 'f1', fileName: 'd.docx' } });
	expect(g).not.toHaveBeenCalled();
});
