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
