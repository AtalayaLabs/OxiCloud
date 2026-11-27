<script lang="ts">
	import { errorToast } from '$lib/utils/errors';
	import {
		getEditorUrl,
		getEditorUrlWithFallback,
		wopiHostPageUrl,
		type WopiAction
	} from '$lib/api/endpoints/wopi';
	import { useAsyncAction } from '$lib/composables/useAsyncAction.svelte';
	import Button from '$lib/components/Button.svelte';
	import Modal from '$lib/components/Modal.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';

	interface Props {
		open: boolean;
		fileId: string | null;
		fileName: string;
		action?: 'edit' | 'view';
		onclose?: () => void;
	}

	let { open = $bindable(false), fileId, fileName, action = 'edit', onclose }: Props = $props();

	/// `/wopi/edit/{id}?access_token=…` — OUR origin. The editor
	/// handshake (a cross-origin form POST carrying the access token) now
	/// happens inside that server-rendered page rather than here.
	///
	/// Why it moved: the POST target is operator config
	/// (`OXICLOUD_WOPI_DISCOVERY_URL`), unknowable when this bundle is
	/// built, and the SPA's CSP is a build-time `<meta>`. The only way
	/// for the SPA to post there was to allow a whole SCHEME
	/// (`form-action https:`) — which excluded a cleartext editor
	/// outright and let ANY https origin receive a token-bearing form.
	/// The host page emits its own CSP naming the one real origin, so
	/// `form-action` here is back to `'self'`.
	///
	/// The host page is also the stricter path on its own merits: it
	/// re-verifies permission and re-mints the token from the CALLER's
	/// live `Update` right, instead of trusting the `can_write` flag in
	/// the token it was handed.
	let hostPageUrl = $state('');
	/// Origin of the editor, from the URL the server resolved via
	/// discovery. Used to validate `postMessage` sources — see `onMessage`.
	let editorOrigin = $state('');
	let loading = $state(false);
	let resolvedAction = $state<WopiAction>('edit');
	let convertTargets = $state<string[]>([]);
	let convertModalOpen = $state(false);
	let selectedTarget = $state('');
	const convertAction = useAsyncAction();

	const sourceExt = $derived(
		fileName.includes('.') ? (fileName.split('.').pop()?.toLowerCase() ?? '') : ''
	);

	function close() {
		open = false;
		hostPageUrl = '';
		editorOrigin = '';
		resolvedAction = 'edit';
		convertTargets = [];
		convertModalOpen = false;
		selectedTarget = '';
		convertAction.clearError();
		onclose?.();
	}

	function openConvertModal() {
		selectedTarget = convertTargets[0] ?? '';
		convertAction.clearError();
		convertModalOpen = true;
	}

	function onConvertModalClose() {
		if (convertAction.busy) convertModalOpen = true;
	}

	async function confirmConvert() {
		if (!fileId || !selectedTarget) return;
		const ok = await convertAction.run(async () => {
			const data = await getEditorUrl(fileId, 'convert', selectedTarget);
			try {
				editorOrigin = new URL(data.editor_url).origin;
			} catch {
				editorOrigin = '';
			}
			resolvedAction = data.action ?? 'convert';
			convertTargets = [];
			hostPageUrl = wopiHostPageUrl(fileId, data.access_token);
		});
		if (ok) {
			convertModalOpen = false;
		}
	}

	function onKeydown(e: KeyboardEvent) {
		if (open && e.key === 'Escape' && !convertModalOpen) close();
	}

	// The editor iframe posts status messages (Collabora / OnlyOffice WOPI
	// protocol). We drop the spinner once it loads, and close the host modal
	// when the editor's own close button fires UI_Close / Document close.
	function onMessage(e: MessageEvent) {
		if (!open) return;
		// Validate the SOURCE before parsing. This handler used to act on
		// a message from any origin — `window` receives postMessage from
		// anything that can reach it (an opener, any frame), so any page
		// could close the editor mid-edit. Low blast radius, but an
		// unauthenticated message handler is worth closing while we know
		// what the legitimate origin is.
		//
		// The editor posts from its own origin, which the server resolved
		// from discovery; the host page itself never posts. An empty
		// `editorOrigin` means we have not loaded yet and nothing is
		// expected, so everything is dropped.
		if (!editorOrigin || e.origin !== editorOrigin) return;
		let data: Record<string, unknown>;
		try {
			data = JSON.parse(typeof e.data === 'string' ? e.data : '') as Record<string, unknown>;
		} catch {
			return; // not a JSON message — ignore
		}
		const msgId = String(data.MessageId ?? data.messageId ?? '');
		if (msgId === 'UI_Close' || msgId === 'close') {
			close();
		} else if (msgId === 'App_LoadingStatus') {
			const values = data.Values as { Status?: string } | undefined;
			const status = values?.Status;
			if (status === 'Document_Loaded' || status === 'Frame_Ready') {
				loading = false;
			}
		}
	}

	// When opened, fetch a WOPI access token and point the iframe at our
	// own host page, which performs the editor POST handshake itself.
	$effect(() => {
		if (!open || !fileId) return;
		loading = true;
		hostPageUrl = '';
		resolvedAction = action;
		convertTargets = [];
		convertModalOpen = false;
		getEditorUrlWithFallback(fileId, fileName, action)
			.then((data) => {
				// `editor_url` is still returned so we can learn the origin
				// to trust for postMessage; we no longer post to it.
				try {
					editorOrigin = new URL(data.editor_url).origin;
				} catch {
					editorOrigin = '';
				}
				resolvedAction = data.action ?? action;
				convertTargets = data.convert_targets ?? [];
				selectedTarget = convertTargets[0] ?? '';
				hostPageUrl = wopiHostPageUrl(fileId!, data.access_token);
			})
			.catch((e) => {
				errorToast(e);
				close();
			})
			.finally(() => (loading = false));
	});
</script>

<svelte:window onkeydown={onKeydown} onmessage={onMessage} />

{#if open}
	<div
		class="wopi"
		role="dialog"
		data-testid="wopi-editor-dialog"
		aria-modal="true"
		aria-label={fileName}
	>
		<header class="wopi__bar">
			<div class="wopi__title-group">
				<span class="wopi__title">{fileName}</span>
				{#if resolvedAction === 'view'}
					<span class="wopi__badge" data-testid="wopi-editor-readonly-badge">
						<Icon name="eye" />
						{t('files.wopi_read_only', 'Read-only')}
					</span>
				{/if}
			</div>
			<div class="wopi__actions">
				{#if resolvedAction === 'view' && convertTargets.length > 0}
					<Button
						variant="primary"
						small
						icon="exchange-alt"
						data-testid="wopi-editor-convert-btn"
						onclick={openConvertModal}
					>
						{t('files.wopi_convert_and_edit', 'Convert & edit')}
					</Button>
				{/if}
				<button
					class="wopi__close"
					data-testid="wopi-editor-close-btn"
					aria-label={t('common.close', 'Close')}
					onclick={close}
				>
					<Icon name="times" />
				</button>
			</div>
		</header>
		<div class="wopi__frame-wrap">
			{#if loading}
				<p class="wopi__status">{t('common.loading', 'Loading…')}</p>
			{/if}
			{#if hostPageUrl}
				<!--
					DELIBERATELY UNNAMED. It used to be `name="wopi_frame"`,
					because the SPA itself posted the token form into it. The
					host page now does that POST, and its own form says
					`target="wopi_frame"` — so if this element carries that
					name too, the host page's browsing context IS "wopi_frame"
					(a navigable is named after the iframe containing it) and
					the target can resolve to the host page itself instead of
					its child. The editor then loads in THIS frame, where the
					SPA's `frame-src 'self' blob:` blocks it: "this content is
					blocked", with the host page's correct policy never
					consulted.
					Nothing targets this frame, so the fix is to have no name.
				-->
				<iframe
					src={hostPageUrl}
					title={t('files.editor', 'Document editor')}
					class="wopi__frame"
					data-testid="wopi-editor-frame"
					allow="clipboard-read; clipboard-write"
					allowfullscreen
					sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-top-navigation allow-popups-to-escape-sandbox"
				></iframe>
			{/if}
		</div>

		<Modal
			bind:open={convertModalOpen}
			title={t('files.wopi_convert_title', 'Convert file for editing')}
			onclose={onConvertModalClose}
		>
			{#if convertTargets.length === 1}
				<p class="wopi__convert-msg">
					{t(
						'files.wopi_convert_message',
						{ sourceExt, targetExt: selectedTarget },
						'Files in .{{sourceExt}} format open in read-only mode. Converting creates a new .{{targetExt}} copy in the same folder for editing while keeping the original file untouched.'
					)}
				</p>
			{:else if convertTargets.length > 1}
				<p class="wopi__convert-msg">
					{t(
						'files.wopi_convert_message_multi',
						{ sourceExt },
						'Files in .{{sourceExt}} format open in read-only mode. Choose a target format to create an editable copy in the same folder while keeping the original file untouched.'
					)}
				</p>
				<fieldset class="wopi__format-group">
					<legend class="wopi__format-legend">
						{t('files.wopi_convert_format_label', 'Target format')}
					</legend>
					<div class="wopi__format-options">
						{#each convertTargets as ext (ext)}
							<label
								class="wopi__format-option"
								class:wopi__format-option--selected={selectedTarget === ext}
							>
								<input
									type="radio"
									name="wopi-convert-target"
									value={ext}
									bind:group={selectedTarget}
									onchange={convertAction.clearError}
									disabled={convertAction.busy}
									data-testid={`wopi-convert-format-${ext}`}
								/>
								<span>.{ext}</span>
							</label>
						{/each}
					</div>
				</fieldset>
			{/if}

			{#if convertAction.error}
				<p class="form-error" role="alert">{convertAction.error}</p>
			{/if}

			{#snippet footer()}
				<Button
					variant="secondary"
					disabled={convertAction.busy}
					onclick={() => (convertModalOpen = false)}
				>
					{t('common.cancel', 'Cancel')}
				</Button>
				<Button
					variant="primary"
					icon="exchange-alt"
					disabled={convertAction.busy || !selectedTarget}
					data-testid="wopi-convert-confirm-btn"
					onclick={confirmConvert}
				>
					{t(
						'files.wopi_convert_confirm',
						{ targetExt: selectedTarget },
						'Convert to .{{targetExt}} & edit'
					)}
				</Button>
			{/snippet}
		</Modal>
	</div>
{/if}

<style>
	.wopi {
		position: fixed;
		inset: 0;
		z-index: 1100;
		display: flex;
		flex-direction: column;
		background: var(--color-bg-base, var(--color-bg-surface));
	}

	.wopi__bar {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		height: 44px;
		padding: 0 1rem;
		background: var(--color-bg-elevated, var(--color-bg-surface));
		border-bottom: 1px solid var(--color-border);
		color: var(--color-text-heading);
	}

	.wopi__title-group {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		min-width: 0;
	}

	.wopi__title {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.wopi__badge {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		flex-shrink: 0;
		padding: 0.15rem 0.55rem;
		border: 1px solid var(--color-info-border);
		border-radius: var(--radius-full);
		background: var(--color-info-bg);
		color: var(--color-info-text);
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
	}

	.wopi__actions {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		flex-shrink: 0;
	}

	.wopi__close {
		background: none;
		border: none;
		color: var(--color-text);
		cursor: pointer;
		font-size: 1.1rem;
	}

	.wopi__frame-wrap {
		position: relative;
		flex: 1;
	}

	.wopi__frame {
		width: 100%;
		height: 100%;
		border: none;
	}

	.wopi__status {
		position: absolute;
		inset: 0;
		display: grid;
		place-items: center;
		color: var(--color-text-muted);
	}

	.wopi__convert-msg {
		margin: 0 0 var(--space-3);
		color: var(--color-text);
		line-height: 1.5;
	}

	.wopi__format-group {
		margin: 0 0 var(--space-3);
		padding: var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
	}

	.wopi__format-legend {
		padding: 0 var(--space-1);
		color: var(--color-text-muted);
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
	}

	.wopi__format-options {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
	}

	.wopi__format-option {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		color: var(--color-text);
		font-weight: var(--weight-medium);
		cursor: pointer;
	}

	.wopi__format-option--selected {
		border-color: var(--color-accent);
		background: var(--color-accent-bg);
	}
</style>
