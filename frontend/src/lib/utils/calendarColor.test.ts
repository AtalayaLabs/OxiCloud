import { describe, expect, it } from 'vitest';
import { readableEventColor } from './calendarColor';

describe('readableEventColor', () => {
	it('uses dark text on a light calendar colour', () => {
		expect(readableEventColor('#ffff00').foreground).toBe('#111827');
	});

	it('uses light text on a dark calendar colour', () => {
		expect(readableEventColor('#1e293b').foreground).toBe('#ffffff');
	});

	it('falls back to the accent token for a missing colour', () => {
		expect(readableEventColor(null).background).toBe('var(--color-accent)');
	});

	// Colours come from user input. A value that is not a validated
	// hex literal must never reach `style:background-color` verbatim.
	it('rejects a non-hex value instead of injecting it', () => {
		const result = readableEventColor('red; background:url(x)');
		expect(result.background).toBe('var(--color-accent)');
		expect(result.foreground).toBe('var(--color-on-accent)');
	});

	it('derives a stable distinct fallback from the calendar id', () => {
		const a = readableEventColor(null, 'calendar-a');
		const b = readableEventColor(null, 'calendar-a');
		const other = readableEventColor(null, 'calendar-b');
		expect(a).toEqual(b);
		expect(a.background).not.toBe('var(--color-accent)');
		expect(a.background).not.toBe(other.background);
	});
});
