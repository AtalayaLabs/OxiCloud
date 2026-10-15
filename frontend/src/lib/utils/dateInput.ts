/**
 * Generic date-input helpers for `<input type="date">` plumbing.
 *
 * `expiryToIso` converts the picker's YYYY-MM-DD value into the
 * ISO-8601 UTC midnight the server expects. `todayIso` returns
 * today in YYYY-MM-DD form for the `min` attribute so the picker
 * won't let the user select a past date.
 *
 * Lives in `utils/` — these are picker-roundtrip helpers, not
 * display formatters (`display.ts::formatDate` handles the "render a
 * date to the user" direction) and not grant-specific (any form
 * with a `<input type="date">` can use them).
 */

/** Convert a YYYY-MM-DD date (or null) to an ISO-8601 datetime at midnight UTC. */
export function expiryToIso(date: string | null | undefined): string | null {
	return date ? new Date(`${date}T00:00:00Z`).toISOString() : null;
}

/**
 * Today's date in YYYY-MM-DD form (local time zone). Used as the `min`
 * attribute on grant / share expiry date inputs so the native picker
 * refuses to select a past date. Callers should also validate the
 * changed value in their `onchange` handler as a belt-and-braces guard
 * (some browsers still let scripted / paste input bypass `min`).
 */
export function todayIso(): string {
	const now = new Date();
	const y = now.getFullYear();
	const m = String(now.getMonth() + 1).padStart(2, '0');
	const d = String(now.getDate()).padStart(2, '0');
	return `${y}-${m}-${d}`;
}
