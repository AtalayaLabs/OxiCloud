import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor, within } from '@testing-library/svelte';

vi.mock('$lib/api/endpoints/contacts', () => ({
	createContact: vi.fn(),
	deleteContact: vi.fn(),
	getContact: vi.fn(),
	listAddressBooks: vi.fn(),
	listContacts: vi.fn(),
	updateContact: vi.fn()
}));

import {
	createContact,
	deleteContact,
	listAddressBooks,
	listContacts,
	updateContact,
	type AddressBook,
	type Contact
} from '$lib/api/endpoints/contacts';
import ContactsPage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

const book: AddressBook = {
	id: 'book-1',
	name: 'Personal',
	owner_id: 'me',
	description: null,
	color: null,
	is_readonly: false,
	is_system: false,
	created_at: '2026-01-01T00:00:00Z',
	updated_at: '2026-01-01T00:00:00Z'
};

const systemBook: AddressBook = {
	...book,
	id: 'system',
	name: 'OxiCloud Users',
	owner_id: 'system',
	is_readonly: true,
	is_system: true
};

function contact(overrides: Partial<Contact> = {}): Contact {
	return {
		id: 'c1',
		address_book_id: book.id,
		uid: 'c1@oxicloud',
		full_name: 'Ada Lovelace',
		first_name: 'Ada',
		last_name: 'Lovelace',
		nickname: null,
		email: [{ email: 'ada@example.com', type: 'work', is_primary: true }],
		phone: [],
		address: [],
		organization: 'Analytical Engines',
		title: 'Mathematician',
		notes: null,
		photo_url: null,
		birthday: null,
		anniversary: null,
		created_at: '2026-01-01T00:00:00Z',
		updated_at: '2026-01-01T00:00:00Z',
		etag: 'e1',
		...overrides
	};
}

beforeEach(() => {
	vi.clearAllMocks();
	m(listAddressBooks).mockResolvedValue([book]);
	m(listContacts).mockResolvedValue([contact()]);
});

it('loads the first address book and opens its first contact for editing', async () => {
	render(ContactsPage);
	await waitFor(() => expect(listContacts).toHaveBeenCalled());
	expect(m(listContacts).mock.calls[0][0]).toBe(book.id);
	await waitFor(() => expect(screen.getByTestId('contacts-form')).toBeTruthy());
	const value = (id: string) => (screen.getByTestId(id) as HTMLInputElement).value;
	expect(value('contacts-field-full-name')).toBe('Ada Lovelace');
	expect(value('contacts-field-first-name')).toBe('Ada');
	expect(value('contacts-field-last-name')).toBe('Lovelace');
	expect(value('contacts-email-0')).toBe('ada@example.com');
});

it('separates the system directory from the real address books', async () => {
	const teamBook: AddressBook = { ...book, id: 'book-2', name: 'Team', is_readonly: true };
	m(listAddressBooks).mockResolvedValue([book, teamBook, systemBook]);
	m(listContacts).mockImplementation((id: string) =>
		Promise.resolve(
			id === 'system'
				? [
						contact({
							id: 'u1',
							address_book_id: 'system',
							full_name: 'Other User',
							first_name: 'Other',
							last_name: 'User',
							email: []
						})
					]
				: [contact()]
		)
	);

	render(ContactsPage);
	await waitFor(() => expect(listContacts).toHaveBeenCalled());

	const picker = screen.getByTestId('contacts-book-select');
	expect(within(picker).queryByText('OxiCloud Users')).toBeNull();
	await fireEvent.click(picker);
	expect(screen.queryByRole('option', { name: /OxiCloud Users/ })).toBeNull();
	expect(screen.getByRole('option', { name: /Team/ })).toBeTruthy();
	expect((screen.getByTestId('contacts-new') as HTMLButtonElement).disabled).toBe(false);

	await fireEvent.click(screen.getByTestId('contacts-tab-directory'));
	await waitFor(() => expect(m(listContacts).mock.calls.at(-1)?.[0]).toBe('system'));
	expect((screen.getByTestId('contacts-new') as HTMLButtonElement).disabled).toBe(true);
	await waitFor(() =>
		expect(within(screen.getByTestId('contacts-detail')).getByText('Other User')).toBeTruthy()
	);
});

it('shows the first and last name in the list when they differ from the display name', async () => {
	m(listContacts).mockResolvedValue([
		contact({ full_name: 'Ada', first_name: 'Ada', last_name: 'Lovelace' })
	]);
	render(ContactsPage);
	await waitFor(() => expect(screen.getByTestId('contacts-list')).toBeTruthy());
	expect(within(screen.getByTestId('contacts-list')).getByText('Ada Lovelace')).toBeTruthy();
});

it('searches server-side after the debounce', async () => {
	render(ContactsPage);
	await waitFor(() => expect(listContacts).toHaveBeenCalled());

	await fireEvent.input(screen.getByTestId('contacts-search'), {
		target: { value: 'ada' }
	});

	await waitFor(
		() =>
			expect(m(listContacts).mock.calls.at(-1)?.[1]).toEqual(
				expect.objectContaining({ search: 'ada' })
			),
		{ timeout: 2000 }
	);
});

it('creates a new contact and selects it', async () => {
	const created = contact({
		id: 'c2',
		uid: 'c2@oxicloud',
		full_name: 'Grace Hopper',
		first_name: 'Grace',
		last_name: 'Hopper',
		email: [{ email: 'grace@example.com', type: 'work', is_primary: true }]
	});
	m(createContact).mockResolvedValue(created);
	render(ContactsPage);
	await waitFor(() => expect(screen.getByTestId('contacts-form')).toBeTruthy());

	await fireEvent.click(screen.getByTestId('contacts-new'));
	await fireEvent.input(screen.getByTestId('contacts-field-full-name'), {
		target: { value: 'Grace Hopper' }
	});
	await fireEvent.input(screen.getByTestId('contacts-email-0'), {
		target: { value: 'grace@example.com' }
	});
	await fireEvent.submit(screen.getByTestId('contacts-form'));

	await waitFor(() => expect(createContact).toHaveBeenCalled());
	expect(m(createContact).mock.calls[0][0]).toBe(book.id);
	expect(m(createContact).mock.calls[0][1]).toEqual(
		expect.objectContaining({
			full_name: 'Grace Hopper',
			email: [expect.objectContaining({ email: 'grace@example.com' })]
		})
	);
	await waitFor(() =>
		expect((screen.getByTestId('contacts-field-full-name') as HTMLInputElement).value).toBe(
			'Grace Hopper'
		)
	);
});

it('edits a contact and swaps the updated row into the list', async () => {
	m(updateContact).mockResolvedValue(contact({ full_name: 'Ada King' }));
	render(ContactsPage);
	await waitFor(() => expect(screen.getByTestId('contacts-form')).toBeTruthy());

	const name = screen.getByTestId('contacts-field-full-name') as HTMLInputElement;
	expect(name.value).toBe('Ada Lovelace');
	await fireEvent.input(name, { target: { value: 'Ada King' } });
	await fireEvent.submit(screen.getByTestId('contacts-form'));

	await waitFor(() => expect(updateContact).toHaveBeenCalled());
	expect(m(updateContact).mock.calls[0][0]).toBe(book.id);
	expect(m(updateContact).mock.calls[0][1]).toBe('c1');
	expect(m(updateContact).mock.calls[0][2]).toEqual(
		expect.objectContaining({ full_name: 'Ada King' })
	);
	await waitFor(() =>
		expect(within(screen.getByTestId('contacts-detail')).getByText('Ada King')).toBeTruthy()
	);
});

it('opens the form for a contact as soon as its row is clicked', async () => {
	m(listContacts).mockResolvedValue([
		contact(),
		contact({ id: 'c2', uid: 'c2@oxicloud', full_name: 'Grace Hopper', first_name: 'Grace' })
	]);
	render(ContactsPage);
	await waitFor(() => expect(screen.getByTestId('contacts-form')).toBeTruthy());

	await fireEvent.click(screen.getByTestId('contacts-row-c2'));
	await waitFor(() =>
		expect((screen.getByTestId('contacts-field-full-name') as HTMLInputElement).value).toBe(
			'Grace Hopper'
		)
	);

	await fireEvent.click(screen.getByTestId('contacts-cancel'));
	await waitFor(() => expect(screen.queryByTestId('contacts-form')).toBeNull());
	expect(within(screen.getByTestId('contacts-detail')).getByText('Grace Hopper')).toBeTruthy();

	await fireEvent.click(screen.getByTestId('contacts-row-c2'));
	await waitFor(() => expect(screen.getByTestId('contacts-form')).toBeTruthy());
});

it('removes an e-mail row on the trash click without asking', async () => {
	render(ContactsPage);
	await waitFor(() => expect(screen.getByTestId('contacts-form')).toBeTruthy());

	await fireEvent.click(screen.getAllByRole('button', { name: 'Remove' })[0]);

	await waitFor(() => expect(screen.queryByTestId('contacts-email-0')).toBeNull());
});

it('suggests countries as you type and still accepts free text', async () => {
	render(ContactsPage);
	await waitFor(() => expect(screen.getByTestId('contacts-form')).toBeTruthy());

	const country = screen.getByTestId('contacts-address-country-0') as HTMLInputElement;
	await fireEvent.input(country, { target: { value: 'germ' } });

	await waitFor(() => expect(screen.getByRole('option', { name: 'Germany' })).toBeTruthy());
	await fireEvent.click(screen.getByRole('option', { name: 'Germany' }));
	expect(country.value).toBe('Germany');

	await fireEvent.input(country, { target: { value: 'Atlantis' } });
	expect(country.value).toBe('Atlantis');
	expect(screen.queryByRole('option')).toBeNull();
});

it('opens the detail screen on a row click and returns with Back', async () => {
	render(ContactsPage);
	await waitFor(() => expect(screen.getByTestId('contacts-list')).toBeTruthy());
	const main = document.querySelector('.contacts');
	expect(main?.classList.contains('contacts--detail')).toBe(false);

	await fireEvent.click(screen.getByTestId('contacts-row-c1'));
	expect(main?.classList.contains('contacts--detail')).toBe(true);

	await fireEvent.click(screen.getByTestId('contacts-back'));
	expect(main?.classList.contains('contacts--detail')).toBe(false);
});

it('returns to the list after saving on mobile', async () => {
	m(updateContact).mockResolvedValue(contact({ full_name: 'Ada King' }));
	render(ContactsPage);
	await waitFor(() => expect(screen.getByTestId('contacts-list')).toBeTruthy());

	await fireEvent.click(screen.getByTestId('contacts-row-c1'));
	const main = document.querySelector('.contacts');
	expect(main?.classList.contains('contacts--detail')).toBe(true);

	await fireEvent.input(screen.getByTestId('contacts-field-full-name'), {
		target: { value: 'Ada King' }
	});
	await fireEvent.submit(screen.getByTestId('contacts-form'));

	await waitFor(() => expect(updateContact).toHaveBeenCalled());
	await waitFor(() => expect(main?.classList.contains('contacts--detail')).toBe(false));
});

it('deletes the selected contact only after the confirm click', async () => {
	m(deleteContact).mockResolvedValue(undefined);
	render(ContactsPage);
	await waitFor(() => expect(screen.getByTestId('contacts-form')).toBeTruthy());

	const del = screen.getByTestId('contacts-delete');
	await fireEvent.click(del);
	expect(deleteContact).not.toHaveBeenCalled();

	await fireEvent.click(del);
	await waitFor(() => expect(deleteContact).toHaveBeenCalledWith(book.id, 'c1'));
	await waitFor(() => expect(screen.queryByTestId('contacts-form')).toBeNull());
	expect(screen.queryByTestId('contacts-row-c1')).toBeNull();
});
