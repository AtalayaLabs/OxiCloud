<!--
	One dialog for triggering a job with its declared parameters.

	Built entirely from `JobSummary.parameters`, so it carries no per-job
	knowledge: a parameter the backend adds appears here with its own
	description and default, with no frontend release.

	It replaces, rather than supplements, the confirm that a mutating job
	used to show — the dialog IS the confirm, so the operator reads the
	warning and sets the values in one step instead of confirming and
	then being asked.

	Collects values only; `AdminJobsPanel` owns dispatching. The rules for
	WHICH values get sent live in `$lib/utils/jobParams` with their tests.
	See `docs/plan/admin-job-parameter-dialog.md`.
-->
<script lang="ts">
	import Modal from '$lib/components/Modal.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import type { JobSummary } from '$lib/api/generated/openapi/types.gen';
	import type { JobParamValues } from '$lib/api/types';
	import { collectValues, initialFields, type ParamField } from '$lib/utils/jobParams';

	interface Props {
		job: JobSummary;
		/** Values the chosen run-menu item implies, e.g. `{ repair: true }`. */
		preset?: JobParamValues;
		/** Resolves with the values to trigger, or `null` on cancel. */
		onresolve: (values: JobParamValues | null) => void;
	}

	let { job, preset = {}, onresolve }: Props = $props();

	let open = $state(true);
	// Seeded ONCE, on purpose — capturing only the initial `job`/`preset`
	// is the intent, not an oversight. These rows are then edited, so
	// re-deriving them from props would discard the operator's input on
	// any unrelated prop change (the panel polls every 5 s and hands down
	// a fresh `JobSummary` each time). The parent keys this component on
	// the job name, so a genuinely different job remounts it.
	// svelte-ignore state_referenced_locally
	let fields = $state<ParamField[]>(initialFields(job, preset));

	// `repair` is the one parameter whose confirmation copy the BACKEND
	// owns (`repair_description` — what the repair adds, which differs per
	// job: correcting a counter and unlinking files off disk are not the
	// same warning). Surface it live, so ticking the box shows the wording
	// rather than hiding it behind a second dialog.
	const repairArmed = $derived(
		fields.some((f) => f.param.name === 'repair' && f.value === true && f.touched)
	);

	function markTouched(f: ParamField) {
		f.touched = true;
	}

	function cancel() {
		open = false;
		onresolve(null);
	}

	function run() {
		open = false;
		onresolve(collectValues(fields));
	}
</script>

<Modal
	bind:open
	title={t('admin.jobs.params_title', { name: job.name }, 'Run {{name}}')}
	onclose={cancel}
>
	{#if job.description}
		<p class="job-params__intro">{job.description}</p>
	{/if}

	<div class="job-params__list">
		{#each fields as field (field.param.name)}
			<div class="job-params__row">
				{#if field.param.type === 'boolean'}
					<label class="job-params__check">
						<input
							type="checkbox"
							checked={field.value === true}
							onchange={(e) => {
								field.value = e.currentTarget.checked;
								markTouched(field);
							}}
						/>
						<span class="job-params__name">{field.param.name}</span>
					</label>
				{:else if field.param.allowed?.length}
					<!-- A closed set gets a select, so a typo is not typeable.
					     The server rejects an unlisted value with a 400 either
					     way (`JobParam::enumerated`), but a dropdown means the
					     operator never has to discover that. -->
					<label class="job-params__field">
						<span class="job-params__name">{field.param.name}</span>
						<select
							value={field.value}
							onchange={(e) => {
								field.value = e.currentTarget.value;
								markTouched(field);
							}}
						>
							{#each field.param.allowed as choice (choice)}
								<option value={choice}>{choice}</option>
							{/each}
						</select>
					</label>
				{:else}
					<label class="job-params__field">
						<span class="job-params__name">{field.param.name}</span>
						<input
							type={field.param.type === 'number' ? 'number' : 'text'}
							value={field.value}
							oninput={(e) => {
								field.value = e.currentTarget.value;
								markTouched(field);
							}}
						/>
					</label>
				{/if}

				<span class="job-params__default">
					{#if field.param.type === 'boolean'}
						{t(
							'admin.jobs.params_default',
							{ value: field.param.default ? 'on' : 'off' },
							'default: {{value}}'
						)}
					{:else if field.param.default === null || field.param.default === undefined}
						{t('admin.jobs.params_no_default', 'no default')}
					{:else}
						{t(
							'admin.jobs.params_default',
							{ value: String(field.param.default) },
							'default: {{value}}'
						)}
					{/if}
				</span>

				<!-- Body text, not a tooltip: these are written as one line
				     for exactly this spot, and several are multi-sentence
				     because the decision needs explaining. A hover tooltip
				     is also invisible on touch. -->
				{#if field.param.description}
					<p class="job-params__help">{field.param.description}</p>
				{/if}
			</div>
		{/each}

		{#if fields.length === 0}
			<p class="job-params__help">
				{t('admin.jobs.params_none', 'This job takes no parameters.')}
			</p>
		{/if}
	</div>

	<!-- ⚠ Both blocks are RENDERED whenever the job could ever show them,
	     and the repair one is hidden with `visibility` rather than removed
	     from the flow.

	     Ticking `repair` must not resize the dialog. With `{#if}` the
	     repair wording appeared from nothing — and `repair_description` is
	     several lines, since it is the one warning the backend authors per
	     job — so the modal grew and the footer buttons moved out from
	     under the pointer that had just ticked the box. Reserving the
	     space costs a gap when the box is clear; a control that moves
	     while being used costs a misclick on a destructive run.

	     `visibility: hidden` also takes it out of the accessibility tree,
	     so a screen reader is not read a warning that does not apply. -->
	{#if job.mutates === 'always'}
		<p class="job-params__warning">
			⚠ {t('admin.jobs.run_mutating_confirm_body', 'This job changes stored state when it runs.')}
		</p>
	{/if}
	{#if job.repair_description}
		<p
			class="job-params__warning job-params__warning--danger"
			class:job-params__warning--reserved={!repairArmed}
		>
			⚠ {job.repair_description}
		</p>
	{/if}

	{#snippet footer()}
		<button
			type="button"
			class="btn btn-secondary"
			data-testid="job-params-cancel"
			onclick={cancel}
		>
			{t('common.cancel', 'Cancel')}
		</button>
		<!-- Danger styling follows the same signal the old confirm used:
		     a mutating job, or `repair` armed. Matches DialogHost's
		     `danger ? btn-danger : btn-primary`. -->
		<button
			type="button"
			class="btn {repairArmed || job.mutates === 'always' ? 'btn-danger' : 'btn-primary'}"
			data-testid="job-params-run"
			onclick={run}
		>
			{t('admin.jobs.run', 'Run')}
		</button>
	{/snippet}
</Modal>

<style>
	.job-params__intro {
		margin: 0 0 var(--space-3);
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}

	/* ⚠ The gap BETWEEN parameters must dominate the gap INSIDE one, or a
	   description reads as belonging to whichever control it sits nearest
	   — which with 12px between rows and 4px inside them was ambiguous.
	   24px vs 4px is a 6:1 ratio, so proximity alone answers "whose
	   description is this?" before the left rule below is even noticed. */
	.job-params__list {
		display: flex;
		flex-direction: column;
		gap: var(--space-6);
	}

	.job-params__row {
		display: grid;
		grid-template-columns: 1fr auto;
		gap: var(--space-1) var(--space-2);
		align-items: center;
	}

	.job-params__check {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		cursor: pointer;
	}

	.job-params__field {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.job-params__field input,
	.job-params__field select {
		flex: 1;
		min-width: 0;
		padding: var(--space-1) var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-sm);
		background: var(--color-bg-muted);
		color: var(--color-text);
	}

	/* The head of each couple — mono because it is the literal wire name
	   the operator may also pass on the API, and weighted so the eye
	   lands on it before the description. */
	.job-params__name {
		font-family: var(--font-mono, monospace);
		font-size: var(--text-sm);
		font-weight: 600;
	}

	.job-params__default {
		color: var(--color-text-muted);
		font-size: var(--text-xs);
		white-space: nowrap;
	}

	/* Subordinate to the control above it, two ways at once: indented so it
	   cannot be mistaken for a sibling, and tied by a left rule that runs
	   only beside this description. Belt and braces on purpose — the
	   indent carries at a glance, the rule survives a long wrapped
	   description where the indent alone could look like a new block. */
	.job-params__help {
		grid-column: 1 / -1;
		margin: var(--space-1) 0 0;
		padding-left: var(--space-3);
		border-left: 2px solid var(--color-border-subtle);
		color: var(--color-text-muted);
		font-size: var(--text-xs);
		line-height: 1.45;
		max-width: 52ch;
	}

	.job-params__warning {
		margin: var(--space-3) 0 0;
		color: var(--color-text-muted);
		font-size: var(--text-xs);
	}

	.job-params__warning--danger {
		color: var(--color-warning-text);
		background: var(--color-warning-bg);
		border: 1px solid var(--color-warning-border);
		border-radius: var(--radius-sm);
		padding: var(--space-2);
	}

	/* Holds its box so the footer cannot move under a pointer that just
	   ticked `repair`. NOT `display: none` — that collapses the flow,
	   which is the resize this exists to prevent. */
	.job-params__warning--reserved {
		visibility: hidden;
	}
</style>
