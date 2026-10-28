import { describe, expect, it } from 'vitest';
import { busyKeySuffix, collectValues, initialFields, needsDialog } from './jobParams';
import type { JobParam, JobSummary } from '$lib/api/generated/openapi/types.gen';

/** `content_rescan`'s real declarations — the job the dialog exists for. */
const RESCAN_PARAMS: JobParam[] = [
	{ name: 'dry_run', type: 'boolean', default: false, description: 'Report without writing.' },
	{
		name: 'rederive',
		type: 'string',
		// `JobParam::enumerated` — a real default AND a closed set. It
		// declared `default: null` until then, which made the dialog
		// render "no default" directly above a description naming one.
		default: 'changed',
		allowed: ['changed', 'always', 'never'],
		description: "'changed' (the default), 'always' or 'never'"
	},
	{ name: 'repair_divergence', type: 'boolean', default: false, description: 'Converge copies.' },
	{ name: 'mime_prefix', type: 'string', default: null, description: 'Narrow the walk.' }
];

function job(over: Partial<JobSummary> = {}): JobSummary {
	return {
		name: 'content_rescan',
		mutates: 'always',
		parameters: RESCAN_PARAMS,
		running: false,
		...over
	} as JobSummary;
}

describe('needsDialog', () => {
	it('skips only when there is nothing to show AND nothing to warn about', () => {
		// The one case that stays a single click: read-only, no knobs.
		expect(needsDialog(job({ mutates: 'never', parameters: undefined }))).toBe(false);

		// ⚠ A read-only job WITH parameters still opens — otherwise its
		// non-boolean knobs stay unreachable, which is the gap this
		// feature exists to close.
		expect(needsDialog(job({ mutates: 'never' }))).toBe(true);

		// Mutating jobs always open: the warning has to be shown.
		expect(needsDialog(job({ mutates: 'always', parameters: undefined }))).toBe(true);
		expect(needsDialog(job({ mutates: 'on_repair_only', parameters: undefined }))).toBe(true);
	});

	it('treats an empty array like no parameters', () => {
		expect(needsDialog(job({ mutates: 'never', parameters: [] }))).toBe(false);
	});
});

describe('initialFields', () => {
	it('seeds every row from its declaration, untouched', () => {
		const fields = initialFields(job());
		expect(fields.map((f) => f.param.name)).toEqual([
			'dry_run',
			'rederive',
			'repair_divergence',
			'mime_prefix'
		]);
		expect(fields.every((f) => !f.touched)).toBe(true);
		// Booleans hold booleans.
		expect(fields[0].value).toBe(false);
		// A string WITH a default shows it (`rederive` → 'changed'); one
		// with none shows empty rather than the literal "null".
		expect(fields[1].value).toBe('changed');
		expect(fields[3].value).toBe('');
	});

	it('marks a preset value as touched, and only that one', () => {
		const fields = initialFields(job(), { repair_divergence: true });
		const byName = Object.fromEntries(fields.map((f) => [f.param.name, f]));
		expect(byName.repair_divergence.value).toBe(true);
		expect(byName.repair_divergence.touched).toBe(true);
		// Everything else is still a default, not a choice.
		expect(byName.dry_run.touched).toBe(false);
		expect(byName.rederive.touched).toBe(false);
	});

	it('seeds an enum row from its declared default, not from empty', () => {
		// The bug: with `default: null` this row showed blank and the
		// dialog captioned it "no default", while the description said
		// 'changed' is the default. The declaration now carries it.
		const rederive = initialFields(job()).find((f) => f.param.name === 'rederive')!;
		expect(rederive.value).toBe('changed');
		// Still untouched — a displayed default is not a choice, so a
		// dialog opened and confirmed unchanged sends nothing.
		expect(rederive.touched).toBe(false);
		expect(collectValues([rederive])).toEqual({});
	});

	it('renders a string preset as text so the input can show it', () => {
		const fields = initialFields(job(), { rederive: 'always' });
		expect(fields[1].value).toBe('always');
		expect(fields[1].touched).toBe(true);
	});
});

describe('collectValues', () => {
	it('sends nothing when the operator only looked', () => {
		// The whole form is rendered, every row has a visible value, and
		// the correct query string is still empty — the engine applies
		// the declared defaults itself.
		expect(collectValues(initialFields(job()))).toEqual({});
	});

	it('⚠ sends a preset value even when it equals its declared default', () => {
		// A box the preset ticked is a CHOICE. Dropping it because it
		// matches the default would make "Run with dry_run" silently
		// identical to "Run".
		const fields = initialFields(job(), { dry_run: false });
		expect(collectValues(fields)).toEqual({ dry_run: false });
	});

	it('⚠ sends a boolean toggled back to its default', () => {
		// Same rule from the other direction: the operator ticked and
		// unticked, which asserts "off". Only `touched` can tell this
		// apart from never having been looked at.
		const fields = initialFields(job(), { repair_divergence: true });
		const row = fields.find((f) => f.param.name === 'repair_divergence')!;
		row.value = false;
		expect(collectValues(fields)).toEqual({ repair_divergence: false });
	});

	it('⚠ drops a cleared text box rather than sending an empty string', () => {
		// Clearing a field means "do not send it". `""` would reach a
		// backend that validates non-empty — `mime_prefix=''` is not the
		// same request as omitting it.
		const fields = initialFields(job(), { mime_prefix: 'image/' });
		const row = fields.find((f) => f.param.name === 'mime_prefix')!;
		row.value = '   ';
		expect(collectValues(fields)).toEqual({});
	});

	it('trims text and drops a non-numeric number', () => {
		const fields = initialFields(job(), { rederive: '  always  ' });
		expect(collectValues(fields)).toEqual({ rederive: 'always' });

		const numeric = initialFields(
			job({ parameters: [{ name: 'batch', type: 'number', default: 500 }] }),
			{ batch: 'not-a-number' }
		);
		// NaN on the wire is worse than the default; drop it.
		expect(collectValues(numeric)).toEqual({});

		const good = initialFields(
			job({ parameters: [{ name: 'batch', type: 'number', default: 500 }] }),
			{ batch: '250' }
		);
		expect(collectValues(good)).toEqual({ batch: 250 });
	});

	it('carries a full combination, which no single menu item could express', () => {
		// The reason the dialog exists: `dry_run` AND `rederive` together
		// is the obvious way to preview an extractor backfill, and the
		// one-boolean-per-menu-item shape could not say it.
		const fields = initialFields(job(), { dry_run: true });
		fields.find((f) => f.param.name === 'rederive')!.value = 'always';
		fields.find((f) => f.param.name === 'rederive')!.touched = true;
		expect(collectValues(fields)).toEqual({ dry_run: true, rederive: 'always' });
	});
});

describe('busyKeySuffix', () => {
	it('is stable regardless of key order', () => {
		// `{a,b}` and `{b,a}` are the same trigger. Two keys for one
		// dispatch would leave the panel thinking one is still in flight.
		expect(busyKeySuffix({ dry_run: true, rederive: 'always' })).toBe(
			busyKeySuffix({ rederive: 'always', dry_run: true })
		);
	});

	it('distinguishes variants, and is empty for a bare run', () => {
		expect(busyKeySuffix({})).toBe('');
		expect(busyKeySuffix({ dry_run: true })).not.toBe(busyKeySuffix({ dry_run: false }));
	});
});
