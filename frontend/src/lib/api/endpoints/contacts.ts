/** Contacts (CardDAV address books) REST browse endpoints. */
import { apiJson } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type {
	AddressBookResponse,
	ContactDto,
	UpdateContactRequest
} from '$lib/api/generated/openapi/types.gen';

export type AddressBook = AddressBookResponse;
export type Contact = ContactDto;
export type ContactUpdate = UpdateContactRequest;

const JSON_HEADERS = { 'Content-Type': 'application/json' };
const enc = encodeURIComponent;

export interface ListContactsOptions {
	/** Max rows. Omit to let the backend decide (full book / 100 for the system directory). */
	limit?: number;
	/** Pagination offset. Ignored by system-directory search. */
	offset?: number;
	/** Case-insensitive substring search over name, nickname, org, e-mail and phone. */
	search?: string;
	signal?: AbortSignal;
}

function contactsUrl(bookId: string): string {
	return `/api/address-books/${enc(bookId)}/contacts`;
}

/** Address books the caller can read, plus the virtual read-only system book. */
export function listAddressBooks(signal?: AbortSignal): Promise<AddressBook[]> {
	return apiJson<AddressBook[]>('/api/address-books', { credentials: 'same-origin', signal });
}

/**
 * List or search contacts inside one address book.
 *
 * Search is server-side: the backend runs a trigram-indexed `ILIKE` and bounds
 * the result, so the browser never downloads a whole address book to filter it.
 */
export function listContacts(
	bookId: string,
	{ limit, offset, search, signal }: ListContactsOptions = {}
): Promise<Contact[]> {
	const params = new URLSearchParams();
	if (limit != null) params.set('limit', String(limit));
	if (offset != null) params.set('offset', String(offset));
	const term = search?.trim();
	if (term) params.set('search', term);
	const qs = params.toString();
	return apiJson<Contact[]>(`${contactsUrl(bookId)}${qs ? `?${qs}` : ''}`, {
		credentials: 'same-origin',
		signal
	});
}

/** Fetch a single contact by id (includes the ETag header, consumed server-side). */
export function getContact(
	bookId: string,
	contactId: string,
	signal?: AbortSignal
): Promise<Contact> {
	return apiJson<Contact>(`${contactsUrl(bookId)}/${enc(contactId)}`, {
		credentials: 'same-origin',
		signal
	});
}

/**
 * Update a contact. Omitted fields keep their current value; the backend
 * regenerates the vCard and ETag. The system book is read-only and rejects
 * this server-side.
 */
export function updateContact(
	bookId: string,
	contactId: string,
	update: ContactUpdate
): Promise<Contact> {
	return apiJson<Contact>(`${contactsUrl(bookId)}/${enc(contactId)}`, {
		method: 'PUT',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify(update)
	});
}
