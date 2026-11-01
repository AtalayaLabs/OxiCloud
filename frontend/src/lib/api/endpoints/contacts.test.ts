import { beforeEach, expect, it, vi } from 'vitest';

vi.mock('$lib/api/client', () => ({ apiJson: vi.fn() }));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({ 'X-CSRF-Token': 'csrf' }) }));

import { apiJson } from '$lib/api/client';
import { getContact, listAddressBooks, listContacts, updateContact } from './contacts';

const json = vi.mocked(apiJson);

beforeEach(() => {
	json.mockReset();
	json.mockResolvedValue([]);
});

it('lists address books', async () => {
	await listAddressBooks();
	expect(json).toHaveBeenCalledWith('/api/address-books', {
		credentials: 'same-origin',
		signal: undefined
	});
});

it('lists contacts without query params by default', async () => {
	await listContacts('book id');
	expect(json).toHaveBeenCalledWith('/api/address-books/book%20id/contacts', {
		credentials: 'same-origin',
		signal: undefined
	});
});

it('moves search and pagination to the server', async () => {
	await listContacts('book', { limit: 200, offset: 400, search: '  john  ' });
	expect(json).toHaveBeenCalledWith(
		'/api/address-books/book/contacts?limit=200&offset=400&search=john',
		expect.objectContaining({ credentials: 'same-origin' })
	);
});

it('omits a whitespace-only search term', async () => {
	await listContacts('book', { search: '   ' });
	expect(json).toHaveBeenCalledWith(
		'/api/address-books/book/contacts',
		expect.objectContaining({ credentials: 'same-origin' })
	);
});

it('fetches one contact by id', async () => {
	await getContact('book', 'c 1');
	expect(json).toHaveBeenCalledWith('/api/address-books/book/contacts/c%201', {
		credentials: 'same-origin',
		signal: undefined
	});
});

it('serializes updates to PUT with CSRF headers', async () => {
	await updateContact('book', 'c1', { first_name: 'Ada', email: [] });
	expect(json).toHaveBeenCalledWith('/api/address-books/book/contacts/c1', {
		method: 'PUT',
		credentials: 'same-origin',
		headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': 'csrf' },
		body: '{"first_name":"Ada","email":[]}'
	});
});
