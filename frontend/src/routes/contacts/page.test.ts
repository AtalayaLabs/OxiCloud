import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor, within } from '@testing-library/svelte';

vi.mock('$lib/api/endpoints/contacts', () => ({
	createContact: vi.fn(),
	getContact: vi.fn(),
	listAddressBooks: vi.fn(),
	listContacts: vi.fn(),
	updateContact: vi.fn()
}));

import {
	createContact,
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

it('loads the first address book and selects its first contact', async () => {
	render(ContactsPage);
	await waitFor(() => expect(listContacts).toHaveBeenCalled());
	expect(m(listContacts).mock.calls[0][0]).toBe(book.id);
	await waitFor(() => expect(screen.getByTestId('contacts-detail')).toBeTruthy());
	const detail = within(screen.getByTestId('contacts-detail'));
	expect(detail.getByText('Ada Lovelace')).toBeTruthy();
	expect(detail.getByText('ada@example.com')).toBeTruthy();
	expect(detail.getByText('First name')).toBeTruthy();
	expect(detail.getByText('Ada')).toBeTruthy();
	expect(detail.getByText('Last name')).toBeTruthy();
	expect(detail.getByText('Lovelace')).toBeTruthy();
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
	await waitFor(() => expect(screen.getByTestId('contacts-detail')).toBeTruthy());

	await fireEvent.click(screen.getByTestId('contacts-new'));
	expect(screen.queryByTestId('contacts-detail')).toBeNull();
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
		expect(within(screen.getByTestId('contacts-detail')).getByText('Grace Hopper')).toBeTruthy()
	);
});

it('edits a contact and swaps the updated row into the list', async () => {
	m(updateContact).mockResolvedValue(contact({ full_name: 'Ada King' }));
	render(ContactsPage);
	await waitFor(() => expect(screen.getByTestId('contacts-detail')).toBeTruthy());

	await fireEvent.click(screen.getByTestId('contacts-edit'));
	const name = screen.getByTestId('contacts-field-full-name') as HTMLInputElement;
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
