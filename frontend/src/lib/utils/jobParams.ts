/**
 * Turning a job's declared parameters into one trigger's values.
 *
 * Split out of the dialog component because the rules here are where the
 * subtlety is — "omitted means default" is not the same as "send what the
 * form shows" — and because a pure function is testable without the
 * panel's menu, busy-key and polling machinery.
 *
 * See `docs/plan/admin-job-parameter-dialog.md`.
 */

import type { JobParam, JobParamValues, JobSummary } from '$lib/api/types';

/**
 * One row of the dialog: the declaration, the current value, and whether
 * this value is a *choice*.
 *
 * `touched` is the load-bearing bit. A checkbox sitting at its declared
 * default looks identical whether the operator left it alone or
 * deliberately set it, and the two must serialise differently — see
 * {@link collectValues}.
 */
export interface ParamField {
	param: JobParam;
	/** Booleans hold a real boolean; string/number rows hold the raw input text. */
	value: boolean | string;
	/** Set by a preset, or by the operator editing this row. Sticky. */
	touched: boolean;
}

/**
 * Whether triggering `job` should open the dialog at all.
 *
 * It has to earn the click: a job with no parameters and `mutates: never`
 * has nothing to show and nothing to warn about, and read-only jobs
 * triggering in one click is the point of that flag.
 *
 * Everything else opens — including a `never` job that declares
 * parameters, because otherwise its non-boolean knobs stay unreachable,
 * which is the gap this whole feature exists to close.
 */
export function needsDialog(job: JobSummary): boolean {
	const declared = job.parameters?.length ?? 0;
	return declared > 0 || job.mutates !== 'never';
}

/**
 * Seed one row per declared parameter, with `preset` overlaid.
 *
 * A preset is what a run-menu item means: "Run with repair_divergence"
 * seeds `{ repair_divergence: true }` and leaves everything else at its
 * declaration. Those seeded values arrive `touched`, because the operator
 * chose them by picking that menu item.
 */
export function initialFields(job: JobSummary, preset: JobParamValues = {}): ParamField[] {
	return (job.parameters ?? []).map((param) => {
		const seeded = Object.prototype.hasOwnProperty.call(preset, param.name);
		if (seeded) {
			const v = preset[param.name];
			return {
				param,
				value: param.type === 'boolean' ? Boolean(v) : String(v),
				touched: true
			};
		}
		return {
			param,
			value: param.type === 'boolean' ? Boolean(param.default) : defaultText(param),
			touched: false
		};
	});
}

/** The declared default as input text — empty when there is none. */
function defaultText(param: JobParam): string {
	return param.default === null || param.default === undefined ? '' : String(param.default);
}

/**
 * The values to put on the query string.
 *
 * ⚠ **Only choices, never the whole form.** The engine seeds every run's
 * args from the declarations before overlaying caller values, so omitting
 * a parameter already yields its default. Sending the full rendered set
 * would instead pin *today's* defaults into that run's recorded args, as
 * if the operator had chosen them — so a later change to a default would
 * silently not apply to a replayed run.
 *
 * Hence:
 *
 * - untouched rows are absent, whatever they display;
 * - a row a preset set is present even when it equals its default;
 * - a touched boolean is present even when toggled back to its default,
 *   because the operator asserting "off" is a choice;
 * - a cleared text box is absent rather than `""` — clearing a field
 *   means "don't send it", not "send empty", and `""` would reach a
 *   backend that validates non-empty;
 * - a number that is not a number is dropped rather than sent as `NaN`.
 */
export function collectValues(fields: ParamField[]): JobParamValues {
	const out: JobParamValues = {};
	for (const f of fields) {
		if (!f.touched) continue;
		if (f.param.type === 'boolean') {
			out[f.param.name] = Boolean(f.value);
			continue;
		}
		const text = String(f.value).trim();
		if (text === '') continue;
		if (f.param.type === 'number') {
			const n = Number(text);
			if (!Number.isFinite(n)) continue;
			out[f.param.name] = n;
			continue;
		}
		out[f.param.name] = text;
	}
	return out;
}

/**
 * A stable suffix for the panel's busy key, so two variants of one job
 * cannot lock each other out mid-flight.
 *
 * Sorted, because `{a,b}` and `{b,a}` are the same trigger and must not
 * produce two keys — the panel would then think one is still running.
 */
export function busyKeySuffix(values: JobParamValues): string {
	const parts = Object.keys(values)
		.sort()
		.map((k) => `${k}=${String(values[k])}`);
	return parts.length ? `:${parts.join(',')}` : '';
}
