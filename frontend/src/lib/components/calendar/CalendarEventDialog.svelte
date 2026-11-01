<script lang="ts">
	import Modal from '$lib/components/Modal.svelte';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { errorMessage } from '$lib/utils/errors';
	import { confirmDialog } from '$lib/stores/dialogs.svelte';
	import { DAY_MS } from '$lib/utils/calendarDate';
	import type { CalendarEvent, CalendarInfo, SaveEventBody } from '$lib/api/endpoints/calendars';

	interface Props {
		open: boolean;
		/** `null` = create. */
		event: CalendarEvent | null;
		/** Writable calendars — picks the target on create. */
		calendars: CalendarInfo[];
		/** Target calendar for a new event (ignored when editing). */
		calendarId?: string;
		/** Day clicked / focused — prefill for a new event. */
		defaultDate: Date;
		/** Caller may edit this event (Editor or above). */
		canSave: boolean;
		canDelete: boolean;
		onsave: (body: SaveEventBody) => Promise<void>;
		ondelete: () => Promise<void>;
		onclose: () => void;
	}

	let {
		open = $bindable(false),
		event,
		calendars,
		calendarId = $bindable(''),
		defaultDate,
		canSave,
		canDelete,
		onsave,
		ondelete,
		onclose
	}: Props = $props();

	const REPEAT_VALUES = ['', 'FREQ=DAILY', 'FREQ=WEEKLY', 'FREQ=MONTHLY', 'FREQ=YEARLY'];

	let summary = $state('');
	let description = $state('');
	let location = $state('');
	let allDay = $state(false);
	let startDate = $state('');
	let endDate = $state('');
	let startTime = $state('');
	let endTime = $state('');
	let repeat = $state('');
	let saving = $state(false);
	let deleting = $state(false);
	let error = $state<string | null>(null);

	const calendarName = $derived(
		calendars.find((c) => c.id === calendarId)?.name ?? t('calendar.unknown_calendar', 'Calendar')
	);

	const repeatOptions = $derived.by(() => {
		const options = [
			{ value: '', label: t('calendar.repeat_none', 'Does not repeat') },
			{ value: 'FREQ=DAILY', label: t('calendar.repeat_daily', 'Daily') },
			{ value: 'FREQ=WEEKLY', label: t('calendar.repeat_weekly', 'Weekly') },
			{ value: 'FREQ=MONTHLY', label: t('calendar.repeat_monthly', 'Monthly') },
			{ value: 'FREQ=YEARLY', label: t('calendar.repeat_yearly', 'Yearly') }
		];
		if (repeat && !REPEAT_VALUES.includes(repeat)) {
			options.push({
				value: repeat,
				label: t('calendar.repeat_custom', { rule: repeat }, 'Custom ({{rule}})')
			});
		}
		return options;
	});

	function pad(value: number, width = 2): string {
		return String(value).padStart(width, '0');
	}

	function localTimeInput(value: Date): string {
		return `${pad(value.getFullYear(), 4)}-${pad(value.getMonth() + 1)}-${pad(value.getDate())}T${pad(value.getHours())}:${pad(value.getMinutes())}`;
	}

	function utcDateInput(value: Date): string {
		return `${pad(value.getUTCFullYear(), 4)}-${pad(value.getUTCMonth() + 1)}-${pad(value.getUTCDate())}`;
	}

	/** `YYYY-MM-DD` → UTC midnight (all-day dates are timezone-free). */
	function parseUtcDate(value: string): Date {
		const [y, m, d] = value.split('-').map(Number);
		return new Date(Date.UTC(y, m - 1, d));
	}

	function reset(): void {
		error = null;
		if (event) {
			summary = event.summary;
			description = event.description ?? '';
			location = event.location ?? '';
			allDay = event.all_day;
			repeat = event.rrule ?? '';
			const start = new Date(event.start_time);
			const end = new Date(event.end_time);
			if (!event.all_day) {
				startTime = localTimeInput(start);
				endTime = localTimeInput(end <= start ? new Date(start.getTime() + 3_600_000) : end);
			}
			// Keep both representations populated so toggling All day
			// mid-edit does not blank the date inputs.
			startDate = utcDateInput(start);
			endDate = utcDateInput(new Date(Math.max(end.getTime(), start.getTime() + 1) - 1));
		} else {
			const base = new Date(
				defaultDate.getFullYear(),
				defaultDate.getMonth(),
				defaultDate.getDate(),
				9,
				0
			);
			summary = '';
			description = '';
			location = '';
			allDay = false;
			repeat = '';
			startTime = localTimeInput(base);
			endTime = localTimeInput(new Date(base.getTime() + 3_600_000));
			startDate = utcDateInput(base);
			endDate = utcDateInput(base);
		}
	}

	$effect(() => {
		if (open) reset();
	});

	function buildBody(): SaveEventBody | null {
		let start: Date;
		let end: Date;
		if (allDay) {
			const startUtc = parseUtcDate(startDate);
			const endInclusive = parseUtcDate(endDate);
			start = startUtc;
			end = new Date(Math.max(endInclusive.getTime(), startUtc.getTime()) + DAY_MS);
		} else {
			start = new Date(startTime);
			end = new Date(endTime);
		}
		if (!summary.trim()) {
			error = t('calendar.summary_required', 'Give the event a title');
			return null;
		}
		if (
			Number.isNaN(start.getTime()) ||
			Number.isNaN(end.getTime()) ||
			end.getTime() <= start.getTime()
		) {
			error = t('calendar.invalid_range', 'The end must be after the start');
			return null;
		}
		return {
			summary: summary.trim(),
			description: description.trim() || null,
			location: location.trim() || null,
			start_time: start.toISOString(),
			end_time: end.toISOString(),
			all_day: allDay,
			rrule: repeat || null
		};
	}

	async function submit(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (saving || deleting) return;
		const body = buildBody();
		if (!body) return;
		saving = true;
		error = null;
		try {
			await onsave(body);
			open = false;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			saving = false;
		}
	}

	async function remove(): Promise<void> {
		if (deleting || saving || !event) return;
		const confirmed = await confirmDialog({
			title: t('calendar.delete_event', 'Delete event'),
			message: t('calendar.confirm_delete_event', { name: event.summary }, 'Delete “{{name}}”?'),
			confirmText: t('common.delete', 'Delete'),
			danger: true
		});
		if (!confirmed) return;
		deleting = true;
		error = null;
		try {
			await ondelete();
			open = false;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			deleting = false;
		}
	}
</script>

<Modal
	bind:open
	title={event ? t('calendar.edit_event', 'Edit event') : t('calendar.new_event', 'New event')}
	{onclose}
	size="lg"
>
	<form class="event-form" onsubmit={submit}>
		{#if event}
			<p class="event-form__calendar">
				<Icon name="calendar" />
				<span>{calendarName}</span>
			</p>
			{#if event.recurrence_id}
				<p class="event-form__hint">
					{t(
						'calendar.override_hint',
						'This occurrence was changed separately from the rest of the series.'
					)}
				</p>
			{:else if event.rrule}
				<p class="event-form__hint">
					{t('calendar.series_hint', 'This event repeats. Changes apply to the whole series.')}
				</p>
			{/if}
		{:else}
			<div class="form-group">
				<label for="calendar-event-calendar">{t('calendar.calendar', 'Calendar')}</label>
				<select id="calendar-event-calendar" bind:value={calendarId} required>
					{#each calendars as calendar (calendar.id)}
						<option value={calendar.id}>{calendar.name}</option>
					{/each}
				</select>
			</div>
		{/if}

		{#if error}
			<p class="event-form__error" role="alert">{error}</p>
		{/if}

		<div class="form-group">
			<label for="calendar-event-summary">{t('calendar.event_title', 'Title')}</label>
			<input
				id="calendar-event-summary"
				bind:value={summary}
				required
				maxlength="255"
				placeholder={t('calendar.event_title_placeholder', 'Add a title')}
				disabled={!canSave}
			/>
		</div>

		<label class="event-form__check">
			<input type="checkbox" bind:checked={allDay} disabled={!canSave} />
			<span>{t('calendar.all_day', 'All day')}</span>
		</label>

		{#if allDay}
			<div class="event-form__row">
				<div class="form-group">
					<label for="calendar-event-start">{t('calendar.starts', 'Starts')}</label>
					<input
						id="calendar-event-start"
						type="date"
						bind:value={startDate}
						required
						disabled={!canSave}
					/>
				</div>
				<div class="form-group">
					<label for="calendar-event-end">{t('calendar.ends', 'Ends')}</label>
					<input
						id="calendar-event-end"
						type="date"
						bind:value={endDate}
						required
						min={startDate}
						disabled={!canSave}
					/>
				</div>
			</div>
		{:else}
			<div class="event-form__row">
				<div class="form-group">
					<label for="calendar-event-start">{t('calendar.starts', 'Starts')}</label>
					<input
						id="calendar-event-start"
						type="datetime-local"
						bind:value={startTime}
						required
						disabled={!canSave}
					/>
				</div>
				<div class="form-group">
					<label for="calendar-event-end">{t('calendar.ends', 'Ends')}</label>
					<input
						id="calendar-event-end"
						type="datetime-local"
						bind:value={endTime}
						required
						min={startTime}
						disabled={!canSave}
					/>
				</div>
			</div>
		{/if}

		<div class="event-form__row">
			<div class="form-group">
				<label for="calendar-event-repeat">{t('calendar.repeat', 'Repeats')}</label>
				<select id="calendar-event-repeat" bind:value={repeat} disabled={!canSave}>
					{#each repeatOptions as option (option.value)}
						<option value={option.value}>{option.label}</option>
					{/each}
				</select>
			</div>
			<div class="form-group">
				<label for="calendar-event-location">{t('calendar.location', 'Location')}</label>
				<input
					id="calendar-event-location"
					bind:value={location}
					maxlength="255"
					disabled={!canSave}
				/>
			</div>
		</div>

		<div class="form-group">
			<label for="calendar-event-description">{t('calendar.description', 'Description')}</label>
			<textarea
				id="calendar-event-description"
				bind:value={description}
				rows="3"
				disabled={!canSave}></textarea>
		</div>

		<div class="event-form__actions">
			{#if event && canDelete}
				<Button variant="danger" icon="trash" onclick={() => void remove()} disabled={deleting}>
					{t('common.delete', 'Delete')}
				</Button>
			{/if}
			<div class="event-form__spacer"></div>
			<Button onclick={onclose}>{t('common.cancel', 'Cancel')}</Button>
			{#if canSave}
				<Button variant="primary" type="submit" disabled={saving || deleting}>
					{event ? t('common.save', 'Save') : t('common.create', 'Create')}
				</Button>
			{/if}
		</div>
	</form>
</Modal>

<style>
	.event-form__calendar {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		margin: 0 0 var(--space-4);
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}

	.event-form__hint {
		margin: calc(var(--space-2) * -1) 0 var(--space-4);
		font-size: var(--text-xs);
		color: var(--color-text-muted);
	}

	.event-form__error {
		margin: 0 0 var(--space-4);
		padding: var(--space-2) var(--space-3);
		border-radius: var(--radius-md);
		background: var(--color-error-bg);
		color: var(--color-error-text);
		font-size: var(--text-sm);
	}

	.event-form__check {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		margin: 0 0 var(--space-4);
		color: var(--color-text-secondary);
	}

	.event-form__check input {
		width: auto;
	}

	.event-form__row {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: var(--space-3);
	}

	.event-form__row :global(.form-group) {
		margin-bottom: var(--space-3);
	}

	.event-form__actions {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		margin-top: var(--space-2);
	}

	.event-form__spacer {
		flex: 1;
	}

	select {
		width: 100%;
		padding: var(--space-2-5);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
		font-size: var(--text-base);
	}

	@media (width <= 640px) {
		.event-form__row {
			grid-template-columns: 1fr;
		}
	}
</style>
