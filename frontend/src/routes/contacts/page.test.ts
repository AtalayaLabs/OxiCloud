import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor, within } from '@testing-library/svelte';

vi.mock('$lib/api/endpoints/contacts', () => ({
	getContact: vi.fn(),
	listAddressBooks: vi.fn(),
	listContacts: vi.fn(),
	updateContact: vi.fn()
}));

import {
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
