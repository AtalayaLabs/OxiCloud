<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { errorMessage } from '$lib/utils/errors';
	import { t } from '$lib/i18n/index.svelte';
	import { formatDate } from '$lib/utils/display';
	import {
		createContact,
		listAddressBooks,
		listContacts,
		updateContact,
		type AddressBook,
		type Contact,
		type ContactCreate,
		type ContactUpdate
	} from '$lib/api/endpoints/contacts';
	import type { AddressDto, EmailDto, PhoneDto } from '$lib/api/generated/openapi/types.gen';

	const PAGE_SIZE = 200;
	const SEARCH_DEBOUNCE_MS = 250;

	interface FormEmail {
		email: string;
		type: string;
	}
	interface FormPhone {
		number: string;
		type: string;
	}
	interface FormAddress {
		street: string;
		city: string;
		state: string;
		postal_code: string;
		country: string;
		type: string;
	}
	interface ContactForm {
		full_name: string;
		first_name: string;
		last_name: string;
		nickname: string;
		organization: string;
		title: string;
		notes: string;
		email: FormEmail[];
		phone: FormPhone[];
		address: FormAddress[];
	}

	let books = $state<AddressBook[]>([]);
	let mode = $state<'contacts' | 'directory'>('contacts');
	let bookId = $state('');
	/** Last real address book the user viewed, restored when leaving the directory. */
	let lastAddressBookId = $state('');
	let contacts = $state<Contact[]>([]);
	let selectedId = $state<string | null>(null);
	let query = $state('');
	let loadingBooks = $state(true);
	let loadingContacts = $state(false);
	let loadingMore = $state(false);
	let hasMore = $state(false);
	let error = $state<string | null>(null);
	let editing = $state(false);
	let creating = $state(false);
	let saving = $state(false);
	let saveError = $state<string | null>(null);
	let form = $state<ContactForm | null>(null);

	const realBooks = $derived(books.filter((b) => !b.is_system));
	const directoryBook = $derived(books.find((b) => b.is_system) ?? null);
	const selectedBook = $derived(books.find((b) => b.id === bookId) ?? null);
	const selected = $derived(contacts.find((c) => c.id === selectedId) ?? null);
	const canEdit = $derived(
		mode === 'contacts' && !!selectedBook && !selectedBook.is_readonly && !selectedBook.is_system
	);
	const searching = $derived(query.trim().length > 0);

	let searchTimer: ReturnType<typeof setTimeout> | undefined;
	let requestSeq = 0;

	function displayName(contact: Contact): string {
		const full = contact.full_name?.trim();
		if (full) return full;
		const parts = [contact.first_name, contact.last_name]
			.map((p) => p?.trim())
			.filter((p): p is string => !!p);
		return parts.join(' ') || contact.nickname?.trim() || primaryEmail(contact) || contact.uid;
	}

	/** Explicit "First Last" from the structured name fields. */
	function nameParts(contact: Contact): string {
		return [contact.first_name, contact.last_name]
			.map((p) => p?.trim())
			.filter((p): p is string => !!p)
			.join(' ');
	}

	function primaryEmail(contact: Contact): string {
		return contact.email.find((e) => e.is_primary)?.email ?? contact.email[0]?.email ?? '';
	}

	function primaryPhone(contact: Contact): string {
		return contact.phone.find((p) => p.is_primary)?.number ?? contact.phone[0]?.number ?? '';
	}

	function initials(contact: Contact): string {
		const words = displayName(contact).split(/\s+/).filter(Boolean);
		return words
			.slice(0, 2)
			.map((w) => w[0]?.toUpperCase() ?? '')
			.join('');
	}

	async function loadBooks() {
		loadingBooks = true;
		error = null;
		try {
			books = await listAddressBooks();
			const real = books.filter((b) => !b.is_system);
			const target = real.find((b) => !b.is_readonly) ?? real[0] ?? books.find((b) => b.is_system);
			if (target) {
				mode = target.is_system ? 'directory' : 'contacts';
				bookId = target.id;
				if (!target.is_system) lastAddressBookId = target.id;
				await loadContacts(true);
			}
		} catch (e) {
			error = errorMessage(e);
		} finally {
			loadingBooks = false;
		}
	}

	function switchMode(next: 'contacts' | 'directory') {
		if (next === mode) return;
		const target =
			next === 'directory'
				? directoryBook
				: (realBooks.find((b) => b.id === lastAddressBookId) ??
					realBooks.find((b) => !b.is_readonly) ??
					realBooks[0]);
		if (!target) return;
		mode = next;
		bookId = target.id;
		if (next === 'contacts') lastAddressBookId = target.id;
		query = '';
		contacts = [];
		selectedId = null;
		cancelForm();
		void loadContacts(true);
	}

	async function loadContacts(reset: boolean) {
		const id = bookId;
		if (!id) return;
		const seq = ++requestSeq;
		const term = query.trim();
		if (reset) {
			loadingContacts = true;
			error = null;
		} else {
			loadingMore = true;
		}
		try {
			const rows = await listContacts(id, {
				limit: PAGE_SIZE,
				offset: reset ? 0 : contacts.length,
				search: term || undefined
			});
			if (seq !== requestSeq) return;
			contacts = reset ? rows : [...contacts, ...rows];
			hasMore = !term && rows.length === PAGE_SIZE;
			if (reset && !rows.some((c) => c.id === selectedId)) {
				selectedId = rows[0]?.id ?? null;
			}
		} catch (e) {
			if (seq !== requestSeq) return;
			error = errorMessage(e);
		} finally {
			if (seq === requestSeq) {
				loadingContacts = false;
				loadingMore = false;
			}
		}
	}

	function onBookChange(event: Event) {
		const value = (event.currentTarget as HTMLSelectElement).value;
		if (value === bookId) return;
		mode = 'contacts';
		bookId = value;
		lastAddressBookId = value;
		query = '';
		contacts = [];
		selectedId = null;
		cancelForm();
		void loadContacts(true);
	}

	function onSearchInput() {
		clearTimeout(searchTimer);
		searchTimer = setTimeout(() => {
			cancelForm();
			void loadContacts(true);
		}, SEARCH_DEBOUNCE_MS);
	}

	function selectContact(id: string) {
		if (editing || creating) cancelForm();
		selectedId = id;
		saveError = null;
	}

	function emptyForm(): ContactForm {
		return {
			full_name: '',
			first_name: '',
			last_name: '',
			nickname: '',
			organization: '',
			title: '',
			notes: '',
			email: [{ email: '', type: 'work' }],
			phone: [{ number: '', type: 'mobile' }],
			address: [{ street: '', city: '', state: '', postal_code: '', country: '', type: 'home' }]
		};
	}

	function toForm(contact: Contact): ContactForm {
		const f = emptyForm();
		f.full_name = contact.full_name ?? '';
		f.first_name = contact.first_name ?? '';
		f.last_name = contact.last_name ?? '';
		f.nickname = contact.nickname ?? '';
		f.organization = contact.organization ?? '';
		f.title = contact.title ?? '';
		f.notes = contact.notes ?? '';
		if (contact.email.length) {
			f.email = contact.email.map((e) => ({ email: e.email, type: e.type }));
		}
		if (contact.phone.length) {
			f.phone = contact.phone.map((p) => ({ number: p.number, type: p.type }));
		}
		if (contact.address.length) {
			f.address = contact.address.map((a) => ({
				street: a.street ?? '',
				city: a.city ?? '',
				state: a.state ?? '',
				postal_code: a.postal_code ?? '',
				country: a.country ?? '',
				type: a.type
			}));
		}
		return f;
	}

	function startCreate() {
		if (!canEdit) return;
		form = emptyForm();
		saveError = null;
		editing = false;
		creating = true;
	}

	function startEdit() {
		if (!selected || !canEdit) return;
		form = toForm(selected);
		saveError = null;
		creating = false;
		editing = true;
	}

	function cancelForm() {
		editing = false;
		creating = false;
		form = null;
		saveError = null;
	}

	function buildUpdate(f: ContactForm): ContactUpdate {
		const emails: EmailDto[] = f.email
			.map((e) => ({ email: e.email.trim(), type: e.type.trim() || 'work' }))
			.filter((e) => e.email.length > 0)
			.map((e, i) => ({ ...e, is_primary: i === 0 }));
		const phones: PhoneDto[] = f.phone
			.map((p) => ({ number: p.number.trim(), type: p.type.trim() || 'mobile' }))
			.filter((p) => p.number.length > 0)
			.map((p, i) => ({ ...p, is_primary: i === 0 }));
		const addresses: AddressDto[] = f.address
			.map((a) => ({
				street: a.street.trim() || null,
				city: a.city.trim() || null,
				state: a.state.trim() || null,
				postal_code: a.postal_code.trim() || null,
				country: a.country.trim() || null,
				type: a.type.trim() || 'home'
			}))
			.filter((a) => a.street || a.city || a.state || a.postal_code || a.country)
			.map((a, i) => ({ ...a, is_primary: i === 0 }));

		return {
			full_name: f.full_name.trim(),
			first_name: f.first_name.trim(),
			last_name: f.last_name.trim(),
			nickname: f.nickname.trim(),
			organization: f.organization.trim(),
			title: f.title.trim(),
			notes: f.notes.trim(),
			email: emails,
			phone: phones,
			address: addresses
		};
	}

	function buildCreate(f: ContactForm): ContactCreate {
		const update = buildUpdate(f);
		return {
			full_name: update.full_name || undefined,
			first_name: update.first_name || undefined,
			last_name: update.last_name || undefined,
			nickname: update.nickname || undefined,
			organization: update.organization || undefined,
			title: update.title || undefined,
			notes: update.notes || undefined,
			email: update.email ?? [],
			phone: update.phone ?? [],
			address: update.address ?? []
		};
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (!form || !bookId) return;
		if (!creating && !selectedId) return;
		saving = true;
		saveError = null;
		try {
			if (creating) {
				const created = await createContact(bookId, buildCreate(form));
				query = '';
				editing = false;
				creating = false;
				form = null;
				await loadContacts(true);
				if (!contacts.some((c) => c.id === created.id)) {
					contacts = [created, ...contacts];
				}
				selectedId = created.id;
			} else if (selectedId) {
				const updated = await updateContact(bookId, selectedId, buildUpdate(form));
				contacts = contacts.map((c) => (c.id === updated.id ? updated : c));
				editing = false;
				form = null;
			}
		} catch (e) {
			saveError = errorMessage(e);
		} finally {
			saving = false;
		}
	}

	onMount(() => {
		void loadBooks();
	});

	onDestroy(() => {
		clearTimeout(searchTimer);
	});
</script>

<svelte:head><title>{t('nav.contacts', 'Contacts')} · OxiCloud</title></svelte:head>

<main class="contacts">
	<header class="contacts__head">
		<div>
			<h1>{t('nav.contacts', 'Contacts')}</h1>
			{#if selectedBook}
				<p class="contacts__subtitle">
					{selectedBook.name}
					{#if selectedBook.is_system}
						<span class="badge">{t('contacts.system_badge', 'System')}</span>
					{:else if selectedBook.is_readonly}
						<span class="badge">{t('contacts.readonly_badge', 'Read only')}</span>
					{/if}
				</p>
			{/if}
		</div>
		<button
			class="btn btn-primary"
			onclick={startCreate}
			disabled={!canEdit}
			data-testid="contacts-new"
		>
			<Icon name="plus" />
			{t('contacts.new', 'New contact')}
		</button>
	</header>

	{#if realBooks.length > 0 && directoryBook}
		<div class="contacts__tabs" role="group" aria-label={t('contacts.sources', 'Contact sources')}>
			<button
				class="contacts__tab"
				class:contacts__tab--active={mode === 'contacts'}
				aria-pressed={mode === 'contacts'}
				onclick={() => switchMode('contacts')}
				data-testid="contacts-tab-address-books"
			>
				{t('contacts.address_books_tab', 'Address books')}
			</button>
			<button
				class="contacts__tab"
				class:contacts__tab--active={mode === 'directory'}
				aria-pressed={mode === 'directory'}
				onclick={() => switchMode('directory')}
				data-testid="contacts-tab-directory"
			>
				{t('contacts.directory_tab', 'Directory')}
			</button>
		</div>
	{/if}

	<div class="contacts__toolbar">
		{#if mode === 'contacts'}
			{#if realBooks.length > 1}
				<label class="contacts__book">
					<span>{t('contacts.address_book', 'Address book')}</span>
					<select value={bookId} onchange={onBookChange} data-testid="contacts-book-select">
						{#each realBooks as book (book.id)}
							<option value={book.id}>
								{book.name}
								{book.is_readonly ? ` · ${t('contacts.shared_book', 'shared')}` : ''}
							</option>
						{/each}
					</select>
				</label>
			{:else if selectedBook}
				<div class="contacts__book">
					<span>{t('contacts.address_book', 'Address book')}</span>
					<span class="contacts__book-name">
						{selectedBook.name}
						{#if selectedBook.is_readonly}
							<span class="badge">{t('contacts.readonly_badge', 'Read only')}</span>
						{/if}
					</span>
				</div>
			{/if}
		{:else}
			<div class="contacts__book">
				<span>{t('contacts.source', 'Source')}</span>
				<span class="contacts__book-name">
					{t('contacts.directory_tab', 'Directory')}
				</span>
			</div>
		{/if}

		<label class="contacts__search">
			<span class="sr-only">{t('common.search', 'Search')}</span>
			<Icon name="search" />
			<input
				type="search"
				bind:value={query}
				oninput={onSearchInput}
				placeholder={t('contacts.search_placeholder', 'Search contacts…')}
				aria-label={t('contacts.search_placeholder', 'Search contacts…')}
				data-testid="contacts-search"
			/>
		</label>
	</div>

	{#if error}
		<EmptyState icon="exclamation-circle" title={error} error />
	{:else if loadingBooks}
		<p class="status">{t('common.loading', 'Loading…')}</p>
	{:else if books.length === 0}
		<EmptyState
			icon="id-card"
			title={t('contacts.empty_state', 'No address books yet')}
			hint={t(
				'contacts.empty_hint',
				'Create an address book in your CardDAV client to get started.'
			)}
		/>
	{:else}
		<div class="contacts__panes">
			<section class="contacts__list-pane" aria-label={t('contacts.list_label', 'Contacts')}>
				{#if loadingContacts}
					<p class="status">{t('common.loading', 'Loading…')}</p>
				{:else if contacts.length === 0}
					<EmptyState
						icon="id-card"
						title={searching
							? t('contacts.no_matches', 'No contacts match your search')
							: mode === 'directory'
								? t('contacts.no_directory_users', 'No other users')
								: t('contacts.no_contacts', 'No contacts yet')}
						hint={searching
							? t('contacts.no_matches_hint', 'Try a different name, e-mail or organisation.')
							: mode === 'directory'
								? t(
										'contacts.no_directory_hint',
										'Internal users of this OxiCloud instance appear here.'
									)
								: t('contacts.no_contacts_hint', 'Contacts synced through CardDAV appear here.')}
					/>
				{:else}
					<ul class="contact-list" data-testid="contacts-list">
						{#each contacts as contact (contact.id)}
							{@const parts = nameParts(contact)}
							<li>
								<button
									class="contact-row"
									class:contact-row--active={contact.id === selectedId}
									onclick={() => selectContact(contact.id)}
									aria-current={contact.id === selectedId ? 'true' : undefined}
									data-testid={`contacts-row-${contact.id}`}
								>
									<span class="avatar" aria-hidden="true">{initials(contact)}</span>
									<span class="contact-row__text">
										<span class="contact-row__name">{displayName(contact)}</span>
										{#if parts && parts !== displayName(contact)}
											<span class="contact-row__meta">{parts}</span>
										{/if}
										{#if primaryEmail(contact)}
											<span class="contact-row__meta">{primaryEmail(contact)}</span>
										{:else if primaryPhone(contact)}
											<span class="contact-row__meta">{primaryPhone(contact)}</span>
										{/if}
									</span>
								</button>
							</li>
						{/each}
					</ul>
					{#if hasMore}
						<button
							class="btn btn-secondary contacts__more"
							onclick={() => void loadContacts(false)}
							disabled={loadingMore}
							data-testid="contacts-load-more"
						>
							{loadingMore ? t('common.loading', 'Loading…') : t('common.load_more', 'Load more')}
						</button>
					{/if}
				{/if}
			</section>

			<section class="contacts__detail-pane" aria-live="polite">
				{#if form && (creating || editing)}
					<form class="contact-form" onsubmit={save} data-testid="contacts-form">
						<div class="contact-form__head">
							<h2>
								{creating
									? t('contacts.new_title', 'New contact')
									: t('contacts.edit_title', 'Edit contact')}
							</h2>
							<div class="contact-form__actions">
								<button
									type="button"
									class="btn btn-secondary"
									onclick={cancelForm}
									disabled={saving}
									data-testid="contacts-cancel"
								>
									<Icon name="times" />
									{t('common.cancel', 'Cancel')}
								</button>
								<button
									type="submit"
									class="btn btn-primary"
									disabled={saving}
									data-testid="contacts-save"
								>
									<Icon name="save" />
									{saving ? t('common.saving', 'Saving…') : t('common.save', 'Save')}
								</button>
							</div>
						</div>

						{#if saveError}
							<p class="status status--error">{saveError}</p>
						{/if}

						<div class="contact-form__grid">
							<label>
								<span>{t('contacts.full_name', 'Display name')}</span>
								<input bind:value={form.full_name} data-testid="contacts-field-full-name" />
							</label>
							<label>
								<span>{t('contacts.first_name', 'First name')}</span>
								<input bind:value={form.first_name} data-testid="contacts-field-first-name" />
							</label>
							<label>
								<span>{t('contacts.last_name', 'Last name')}</span>
								<input bind:value={form.last_name} data-testid="contacts-field-last-name" />
							</label>
							<label>
								<span>{t('contacts.nickname', 'Nickname')}</span>
								<input bind:value={form.nickname} data-testid="contacts-field-nickname" />
							</label>
							<label>
								<span>{t('contacts.organization', 'Organisation')}</span>
								<input bind:value={form.organization} data-testid="contacts-field-organization" />
							</label>
							<label>
								<span>{t('contacts.title', 'Job title')}</span>
								<input bind:value={form.title} data-testid="contacts-field-title" />
							</label>
						</div>

						<fieldset class="contact-form__group">
							<legend>{t('contacts.emails', 'E-mail')}</legend>
							{#each form.email as row, i (i)}
								<div class="contact-form__row">
									<input
										type="email"
										bind:value={row.email}
										placeholder={t('contacts.email_placeholder', 'name@example.com')}
										aria-label={t('contacts.email', 'E-mail')}
										data-testid={`contacts-email-${i}`}
									/>
									<input
										bind:value={row.type}
										placeholder={t('contacts.type', 'Type')}
										aria-label={t('contacts.type', 'Type')}
										data-testid={`contacts-email-type-${i}`}
									/>
									<button
										type="button"
										class="link-btn link-btn--danger"
										onclick={() => form?.email.splice(i, 1)}
										aria-label={t('common.remove', 'Remove')}
									>
										{t('common.remove', 'Remove')}
									</button>
								</div>
							{/each}
							<button
								type="button"
								class="link-btn"
								onclick={() => form?.email.push({ email: '', type: 'work' })}
							>
								+ {t('contacts.add_email', 'Add e-mail')}
							</button>
						</fieldset>

						<fieldset class="contact-form__group">
							<legend>{t('contacts.phones', 'Phone')}</legend>
							{#each form.phone as row, i (i)}
								<div class="contact-form__row">
									<input
										type="tel"
										bind:value={row.number}
										placeholder={t('contacts.phone_placeholder', '+1 555 0100')}
										aria-label={t('contacts.phone', 'Phone')}
										data-testid={`contacts-phone-${i}`}
									/>
									<input
										bind:value={row.type}
										placeholder={t('contacts.type', 'Type')}
										aria-label={t('contacts.type', 'Type')}
										data-testid={`contacts-phone-type-${i}`}
									/>
									<button
										type="button"
										class="link-btn link-btn--danger"
										onclick={() => form?.phone.splice(i, 1)}
										aria-label={t('common.remove', 'Remove')}
									>
										{t('common.remove', 'Remove')}
									</button>
								</div>
							{/each}
							<button
								type="button"
								class="link-btn"
								onclick={() => form?.phone.push({ number: '', type: 'mobile' })}
							>
								+ {t('contacts.add_phone', 'Add phone')}
							</button>
						</fieldset>

						<fieldset class="contact-form__group">
							<legend>{t('contacts.addresses', 'Address')}</legend>
							{#each form.address as row, i (i)}
								<div class="contact-form__address">
									<input
										bind:value={row.street}
										placeholder={t('contacts.street', 'Street')}
										aria-label={t('contacts.street', 'Street')}
										data-testid={`contacts-address-street-${i}`}
									/>
									<input
										bind:value={row.city}
										placeholder={t('contacts.city', 'City')}
										aria-label={t('contacts.city', 'City')}
										data-testid={`contacts-address-city-${i}`}
									/>
									<input
										bind:value={row.state}
										placeholder={t('contacts.state', 'State')}
										aria-label={t('contacts.state', 'State')}
										data-testid={`contacts-address-state-${i}`}
									/>
									<input
										bind:value={row.postal_code}
										placeholder={t('contacts.postal_code', 'Postal code')}
										aria-label={t('contacts.postal_code', 'Postal code')}
										data-testid={`contacts-address-postal-${i}`}
									/>
									<input
										bind:value={row.country}
										placeholder={t('contacts.country', 'Country')}
										aria-label={t('contacts.country', 'Country')}
										data-testid={`contacts-address-country-${i}`}
									/>
									<button
										type="button"
										class="link-btn link-btn--danger"
										onclick={() => form?.address.splice(i, 1)}
										aria-label={t('common.remove', 'Remove')}
									>
										{t('common.remove', 'Remove')}
									</button>
								</div>
							{/each}
							<button
								type="button"
								class="link-btn"
								onclick={() =>
									form?.address.push({
										street: '',
										city: '',
										state: '',
										postal_code: '',
										country: '',
										type: 'home'
									})}
							>
								+ {t('contacts.add_address', 'Add address')}
							</button>
						</fieldset>

						<label class="contact-form__notes">
							<span>{t('contacts.notes', 'Notes')}</span>
							<textarea bind:value={form.notes} rows="4" data-testid="contacts-field-notes"
							></textarea>
						</label>
					</form>
				{:else if !selected}
					<EmptyState
						icon="id-card"
						title={t('contacts.select_contact', 'Select a contact')}
						hint={t(
							'contacts.select_contact_hint',
							'Choose someone from the list to see their details.'
						)}
					/>
				{:else}
					<article class="contact-detail" data-testid="contacts-detail">
						<div class="contact-detail__head">
							<span class="avatar avatar--lg" aria-hidden="true">{initials(selected)}</span>
							<div>
								<h2>{displayName(selected)}</h2>
								{#if selected.organization || selected.title}
									<p class="contact-detail__role">
										{[selected.title, selected.organization].filter(Boolean).join(' · ')}
									</p>
								{/if}
							</div>
							{#if canEdit}
								<button class="btn btn-secondary" onclick={startEdit} data-testid="contacts-edit">
									<Icon name="pencil-alt" />
									{t('contacts.edit', 'Edit')}
								</button>
							{/if}
						</div>

						{#if selected.email.length}
							<section class="contact-detail__section">
								<h3>{t('contacts.emails', 'E-mail')}</h3>
								<ul>
									{#each selected.email as email (email.email + email.type)}
										<li>
											<a href={`mailto:${email.email}`}>{email.email}</a>
											<span class="muted">{email.type}</span>
										</li>
									{/each}
								</ul>
							</section>
						{/if}

						{#if selected.phone.length}
							<section class="contact-detail__section">
								<h3>{t('contacts.phones', 'Phone')}</h3>
								<ul>
									{#each selected.phone as phone (phone.number + phone.type)}
										<li>
											<a href={`tel:${phone.number}`}>{phone.number}</a>
											<span class="muted">{phone.type}</span>
										</li>
									{/each}
								</ul>
							</section>
						{/if}

						{#if selected.address.length}
							<section class="contact-detail__section">
								<h3>{t('contacts.addresses', 'Address')}</h3>
								<ul>
									{#each selected.address as address, i (i)}
										<li>
											{[
												address.street,
												address.city,
												address.state,
												address.postal_code,
												address.country
											]
												.filter(Boolean)
												.join(', ')}
											<span class="muted">{address.type}</span>
										</li>
									{/each}
								</ul>
							</section>
						{/if}

						{#if selected.first_name || selected.last_name || selected.nickname || selected.notes || selected.birthday || selected.anniversary}
							<section class="contact-detail__section">
								<h3>{t('contacts.details', 'Details')}</h3>
								<dl>
									{#if selected.first_name}
										<dt>{t('contacts.first_name', 'First name')}</dt>
										<dd>{selected.first_name}</dd>
									{/if}
									{#if selected.last_name}
										<dt>{t('contacts.last_name', 'Last name')}</dt>
										<dd>{selected.last_name}</dd>
									{/if}
									{#if selected.nickname}
										<dt>{t('contacts.nickname', 'Nickname')}</dt>
										<dd>{selected.nickname}</dd>
									{/if}
									{#if selected.birthday}
										<dt>{t('contacts.birthday', 'Birthday')}</dt>
										<dd>{selected.birthday}</dd>
									{/if}
									{#if selected.anniversary}
										<dt>{t('contacts.anniversary', 'Anniversary')}</dt>
										<dd>{selected.anniversary}</dd>
									{/if}
									{#if selected.notes}
										<dt>{t('contacts.notes', 'Notes')}</dt>
										<dd class="contact-detail__notes">{selected.notes}</dd>
									{/if}
								</dl>
							</section>
						{/if}

						<p class="contact-detail__updated muted">
							{t(
								'contacts.updated_at',
								{ date: formatDate(selected.updated_at) },
								'Updated {{date}}'
							)}
						</p>
					</article>
				{/if}
			</section>
		</div>
	{/if}
</main>

<style>
	.contacts {
		max-width: 72rem;
		margin: 0 auto;
		padding: var(--space-6) var(--space-4);
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
	}

	.contacts__head {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: var(--space-4);
	}

	.contacts__head h1 {
		margin: 0;
		font-size: var(--text-2xl);
	}

	.contacts__subtitle {
		margin: var(--space-1) 0 0;
		display: flex;
		align-items: center;
		gap: var(--space-2);
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}

	.contacts__tabs {
		display: flex;
		align-self: flex-start;
		gap: var(--space-1);
		padding: 3px;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-xl);
		background: var(--color-bg-muted);
	}

	.contacts__tab {
		padding: var(--space-2) var(--space-3);
		border: none;
		border-radius: var(--radius-lg);
		background: transparent;
		color: var(--color-text-muted);
		cursor: pointer;
		font-size: var(--text-sm);
	}

	.contacts__tab:hover {
		color: var(--color-text);
	}

	.contacts__tab--active {
		background: var(--color-bg-surface);
		color: var(--color-text);
		box-shadow: var(--shadow-xs);
	}

	.contacts__toolbar {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}

	.contacts__book,
	.contacts__search {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		font-size: var(--text-sm);
		color: var(--color-text-secondary);
	}

	.contacts__book select,
	.contacts__search input {
		padding: var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		font-size: var(--text-base);
	}

	.contacts__book-name {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) 0;
		color: var(--color-text);
		font-size: var(--text-base);
	}

	.contacts__search {
		position: relative;
	}

	.contacts__search :global(svg) {
		position: absolute;
		bottom: var(--space-2);
		left: var(--space-2);
		color: var(--color-text-faint);
		width: 1rem;
		height: 1rem;
		pointer-events: none;
	}

	.contacts__search input {
		padding-left: var(--space-8);
	}

	.contacts__panes {
		display: grid;
		gap: var(--space-4);
	}

	.contacts__list-pane,
	.contacts__detail-pane {
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		background: var(--color-bg-surface);
		min-height: 24rem;
	}

	.contacts__list-pane {
		padding: var(--space-2);
	}

	.contacts__detail-pane {
		padding: var(--space-4);
	}

	.contact-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.contact-row {
		width: 100%;
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2);
		border: none;
		border-radius: var(--radius-md);
		background: none;
		color: var(--color-text);
		text-align: left;
		cursor: pointer;
	}

	.contact-row:hover {
		background: var(--color-bg-hover);
	}

	.contact-row--active {
		background: var(--color-accent-bg);
	}

	.contact-row__text {
		display: flex;
		flex-direction: column;
		min-width: 0;
	}

	.contact-row__name {
		font-weight: var(--weight-medium);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.contact-row__meta {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.avatar {
		display: grid;
		place-items: center;
		flex: 0 0 auto;
		width: 2rem;
		height: 2rem;
		border-radius: var(--radius-full);
		background: var(--color-accent-bg);
		color: var(--color-accent-text);
		font-weight: var(--weight-semibold);
		font-size: var(--text-sm);
	}

	.avatar--lg {
		width: 3rem;
		height: 3rem;
		font-size: var(--text-lg);
	}

	.contacts__more {
		width: 100%;
		margin-top: var(--space-2);
	}

	.contact-detail {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
	}

	.contact-detail__head {
		display: flex;
		align-items: center;
		gap: var(--space-3);
	}

	.contact-detail__head h2 {
		margin: 0;
		font-size: var(--text-xl);
	}

	.contact-detail__head .btn {
		margin-left: auto;
	}

	.contact-detail__role {
		margin: 0;
		color: var(--color-text-muted);
	}

	.contact-detail__section h3,
	.contact-form__group legend {
		margin: 0 0 var(--space-2);
		font-size: var(--text-sm);
		font-weight: var(--weight-semibold);
		color: var(--color-text-secondary);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}

	.contact-detail__section ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
	}

	.contact-detail__section li {
		display: flex;
		gap: var(--space-2);
		align-items: baseline;
		flex-wrap: wrap;
	}

	.contact-detail__section a {
		color: var(--color-accent-text);
		text-decoration: none;
	}

	.contact-detail__section a:hover {
		text-decoration: underline;
	}

	.contact-detail__section dl {
		margin: 0;
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: var(--space-1) var(--space-3);
	}

	.contact-detail__section dt {
		color: var(--color-text-muted);
	}

	.contact-detail__section dd {
		margin: 0;
	}

	.contact-detail__notes {
		white-space: pre-wrap;
	}

	.contact-detail__updated {
		margin: 0;
		font-size: var(--text-sm);
	}

	.contact-form {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
	}

	.contact-form__head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		flex-wrap: wrap;
	}

	.contact-form__head h2 {
		margin: 0;
		font-size: var(--text-xl);
	}

	.contact-form__actions {
		display: flex;
		gap: var(--space-2);
	}

	.contact-form__grid {
		display: grid;
		gap: var(--space-3);
	}

	.contact-form label,
	.contact-form__notes {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		font-size: var(--text-sm);
		color: var(--color-text-secondary);
	}

	.contact-form input,
	.contact-form textarea {
		padding: var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		font-size: var(--text-base);
	}

	.contact-form__group {
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		padding: var(--space-3);
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	.contact-form__row {
		display: grid;
		grid-template-columns: 1fr minmax(6rem, 9rem) auto;
		gap: var(--space-2);
		align-items: center;
	}

	.contact-form__address {
		display: grid;
		gap: var(--space-2);
	}

	.contact-form__notes textarea {
		resize: vertical;
	}

	.link-btn {
		background: none;
		border: none;
		padding: 0;
		color: var(--color-accent-text);
		cursor: pointer;
		font-size: var(--text-sm);
	}

	.link-btn:hover {
		text-decoration: underline;
	}

	.link-btn--danger {
		color: var(--color-danger-text);
	}

	.badge {
		padding: 0 var(--space-2);
		border-radius: var(--radius-full);
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		font-size: var(--text-xs);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}

	.status {
		color: var(--color-text-muted);
	}

	.status--error {
		color: var(--color-error-text);
	}

	@media (width >= 48rem) {
		.contacts__toolbar {
			flex-direction: row;
			align-items: end;
		}

		.contacts__book {
			min-width: 16rem;
		}

		.contacts__search {
			flex: 1;
		}

		.contacts__panes {
			grid-template-columns: minmax(16rem, 22rem) 1fr;
			align-items: stretch;
		}

		/* Both tiles share one height in every state (empty, results,
		   selected, editing). Without this the grid sized each pane to its
		   own content and the page jumped whenever the right-hand pane
		   switched between the empty state, a contact and the form. The
		   panes scroll internally instead of stretching each other. */
		.contacts__list-pane,
		.contacts__detail-pane {
			height: min(70vh, 46rem);
			overflow-y: auto;
		}

		.contact-form__grid {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
</style>
