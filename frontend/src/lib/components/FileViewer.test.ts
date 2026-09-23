import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
vi.mock('$lib/api/client', () => ({ apiFetch: vi.fn(), withBase: (p: string) => p }));
vi.mock('$lib/api/endpoints/files', () => ({
	fileDownloadUrl: () => '/dl',
	fileInlineUrl: () => '/in'
}));
vi.mock('$lib/api/endpoints/wopi', () => ({
	canEditWithWopi: vi.fn(),
	// WopiEditor's $effect calls this on mount when an editor opens; provide it
	// so the async handshake resolves instead of throwing an unhandled error
	// (vitest 4 fails the whole run on unhandled errors).
	getEditorUrlWithFallback: vi.fn(async () => ({
		editor_url: 'about:blank',
		access_token: 't',
		access_token_ttl: 0
	}))
}));
// Collab is off for most of these tests — a text file then takes the plain
// preview path the older assertions describe. The one collab test flips it.
const { cfg } = vi.hoisted(() => ({
	cfg: { loaded: true, features: { markdown_collab: false } }
}));
vi.mock('$lib/stores/serverConfig.svelte', () => ({ serverConfig: cfg }));
// Collab also needs a signed-in caller: it runs over the message bus, whose
// WS ticket route is deliberately off the anonymous share allowlist. These
// tests describe the authenticated case, so say so explicitly — see the
// anonymous test at the bottom for the other side.
const { sess } = vi.hoisted(() => ({ sess: { isAuthenticated: true } }));
vi.mock('$lib/stores/session.svelte', () => ({ session: sess }));
// The real editor mounts CodeMirror and a message-bus session; the stub just
// renders the toolbar the viewer passes in.
vi.mock(
	'$lib/components/CollabEditor.svelte',
	async () => await import('./__mocks__/CollabEditor.svelte')
);
import { apiFetch } from '$lib/api/client';
import { canEditWithWopi } from '$lib/api/endpoints/wopi';
import FileViewer from './FileViewer.svelte';
const af = apiFetch as unknown as ReturnType<typeof vi.fn>;
const cw = canEditWithWopi as unknown as ReturnType<typeof vi.fn>;
function file(over: Record<string, unknown> = {}) {
	return {
		id: 'i',
		name: 'pic.png',
		mime_type: 'image/png',
		category: 'Image',
		folder_id: '',
		created_by: null,
		updated_by: null,
		path: '',
		size: 1,
		modified_at: 0,
		created_at: 0,
		sort_date: 0,
		icon_class: '',
		icon_special_class: '',
		size_formatted: '1 B',
		etag: '',
		content_hash: '',
		...over
	} as never;
}
beforeEach(() => {
	vi.clearAllMocks();
	cw.mockResolvedValue(false);
});
it('renders an image with working zoom controls and closes', async () => {
	render(FileViewer, { props: { open: true, file: file() } });
	expect(await screen.findByTestId('file-viewer-dialog')).toBeTruthy();
	await fireEvent.click(screen.getByTestId('file-viewer-zoom-in-btn'));
	await fireEvent.click(screen.getByTestId('file-viewer-zoom-out-btn'));
	await fireEvent.click(screen.getByTestId('file-viewer-zoom-reset-btn'));
	await fireEvent.click(screen.getByTestId('file-viewer-close-btn'));
});
it('fetches text content for a text file', async () => {
	af.mockResolvedValue({ ok: true, text: async () => 'hello world' });
	render(FileViewer, {
		props: { open: true, file: file({ name: 'n.txt', mime_type: 'text/plain', category: 'Text' }) }
	});
	expect(await screen.findByTestId('file-viewer-dialog')).toBeTruthy();
	await waitFor(() => expect(af).toHaveBeenCalled());
});
it('renders nothing when closed', () => {
	render(FileViewer, { props: { open: false, file: file() } });
	expect(screen.queryByTestId('file-viewer-dialog')).toBeNull();
});
it('shows an Edit button for a WOPI-editable document', async () => {
	cw.mockResolvedValue(true);
	render(FileViewer, {
		props: {
			open: true,
			file: file({
				name: 'report.docx',
				mime_type: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
				category: 'Document'
			})
		}
	});
	await screen.findByTestId('file-viewer-dialog');
	await waitFor(() => expect(screen.getByTestId('file-viewer-edit-btn')).toBeTruthy());
});
it('exposes download and open-in-new-tab links', async () => {
	render(FileViewer, { props: { open: true, file: file() } });
	await screen.findByTestId('file-viewer-dialog');
	expect(screen.getByTestId('file-viewer-download-link').getAttribute('href')).toBe('/dl');
	expect(screen.getByTestId('file-viewer-open-new-tab-link').getAttribute('href')).toBe('/in');
});
it('handles a failed text fetch without crashing', async () => {
	af.mockResolvedValue({ ok: false, status: 500, text: async () => '' });
	render(FileViewer, {
		props: { open: true, file: file({ name: 'n.txt', mime_type: 'text/plain', category: 'Text' }) }
	});
	await screen.findByTestId('file-viewer-dialog');
	await waitFor(() => expect(af).toHaveBeenCalled());
});

// A text-shaped file is edited in place, so the viewer hands its chrome to the
// editor: title + close stay on top, the actions move down next to the sync
// state — the shape the office editor already has.
it('moves the actions into the editor row for a collab-editable file', async () => {
	cfg.features.markdown_collab = true;
	render(FileViewer, {
		props: {
			open: true,
			file: file({ name: 'notes.md', mime_type: 'text/markdown', category: 'Document' })
		}
	});
	await screen.findByTestId('collab-editor-stub');
	expect(screen.getByTestId('file-viewer-collab-window-link')).toBeTruthy();
	expect(screen.getByTestId('file-viewer-collab-download-link')).toBeTruthy();
	// The top bar keeps only the title and the close button.
	expect(screen.queryByTestId('file-viewer-download-link')).toBeNull();
	expect(screen.queryByTestId('file-viewer-open-new-tab-link')).toBeNull();
	expect(screen.getByTestId('file-viewer-close-btn')).toBeTruthy();
	expect(screen.getByTestId('file-viewer-close-btn')).toBeTruthy();
	cfg.features.markdown_collab = false;
});

// A public-share visitor holds an anonymous session, and anonymous
// sessions never get a WebSocket — `POST /api/rt/ticket` is off their
// allowlist by design. Mounting the collaborative editor for them
// produced a 403 per attempt (`authz.denied`,
// `reason="anonymous_route_not_allowlisted"`) and an editor that could
// never sync. The plain preview is the correct surface: static, no
// realtime updates, which is all a share visitor can have.
it('shows the plain preview instead of the editor for an anonymous visitor', async () => {
	cfg.features.markdown_collab = true;
	sess.isAuthenticated = false;
	af.mockResolvedValue({ ok: true, text: async () => 'hello from the share' });

	render(FileViewer, {
		props: {
			open: true,
			file: file({ name: 'notes.md', mime_type: 'text/markdown', category: 'Document' })
		}
	});

	await screen.findByText('hello from the share');
	expect(screen.queryByTestId('collab-editor-stub')).toBeNull();

	sess.isAuthenticated = true;
	cfg.features.markdown_collab = false;
});

// `readOnly` is a separate gate from authentication: a host that opens
// the viewer read-only is asking for a preview, and that must hold even
// for a signed-in caller who could otherwise edit.
it('shows the plain preview instead of the editor when opened read-only', async () => {
	cfg.features.markdown_collab = true;
	af.mockResolvedValue({ ok: true, text: async () => 'read only body' });

	render(FileViewer, {
		props: {
			open: true,
			readOnly: true,
			file: file({ name: 'notes.md', mime_type: 'text/markdown', category: 'Document' })
		}
	});

	await screen.findByText('read only body');
	expect(screen.queryByTestId('collab-editor-stub')).toBeNull();

	cfg.features.markdown_collab = false;
});

// Fullscreen is the default, and the toggle sticks for the next file opened
// on this device.
it('remembers the editor fullscreen choice', async () => {
	cfg.features.markdown_collab = true;
	const md = file({ name: 'notes.md', mime_type: 'text/markdown', category: 'Document' });
	const first = render(FileViewer, { props: { open: true, file: md } });
	const toggle = await screen.findByTestId('file-viewer-collab-fullscreen-btn');
	expect(toggle.getAttribute('aria-pressed')).toBe('true');
	await fireEvent.click(toggle);
	expect(toggle.getAttribute('aria-pressed')).toBe('false');
	first.unmount();

	render(FileViewer, { props: { open: true, file: md } });
	const again = await screen.findByTestId('file-viewer-collab-fullscreen-btn');
	expect(again.getAttribute('aria-pressed')).toBe('false');
	localStorage.removeItem('oxi-collab-fullscreen');
	cfg.features.markdown_collab = false;
});
