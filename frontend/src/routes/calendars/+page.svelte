<script lang="ts">
	import { onMount } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import CalendarMonthView from '$lib/components/calendar/CalendarMonthView.svelte';
	import CalendarTimeGrid from '$lib/components/calendar/CalendarTimeGrid.svelte';
	import CalendarEventDialog from '$lib/components/calendar/CalendarEventDialog.svelte';
	import {
		createCalendar,
		createEvent,
		deleteCalendar,
		deleteEvent,
		listCalendars,
		listEvents,
		updateCalendar,
		updateEvent,
		type CalendarEvent,
		type CalendarInfo,
		type SaveEventBody
	} from '$lib/api/endpoints/calendars';
	import {
		addDays,
		formatDayTitle,
		formatMonthTitle,
		isSameMonth,
		monthGridDays,
		shiftMonth,
		startOfDay,
		weekDays,
		type CalendarView
	} from '$lib/utils/calendarDate';
	import { readableEventColor } from '$lib/utils/calendarColor';
	import {
		expandEvents,
		groupOccurrencesByDay,
		type EventOccurrence
	} from '$lib/utils/calendarRecurrence';
	import { errorMessage, errorToast } from '$lib/utils/errors';
	import { confirmDialog, promptDialog } from '$lib/stores/dialogs.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { ui } from '$lib/stores/ui.svelte';

	let calendars = $state<CalendarInfo[]>([]);
	let calendarsLoaded = $state(false);
	let calendarsError = $state<string | null>(null);
	const selected = new SvelteSet<string>();
	let selectionInitialized = false;

	let events = $state<CalendarEvent[]>([]);
	let eventsLoading = $state(false);
	let eventsError = $state<string | null>(null);
	let loadTicket = 0;

	let view = $state<CalendarView>('month');
	let anchor = $state(startOfDay(new Date()));

	let dialogOpen = $state(false);
	let editing = $state<CalendarEvent | null>(null);
	let dialogDate = $state(new Date());
	let dialogCalendarId = $state('');

	const viewData = $derived.by(() => {
		if (view === 'month') {
			const days = monthGridDays(anchor);
			return { days, start: days[0], end: addDays(days[41], 1) };
		}
		if (view === 'week') {
			const days = weekDays(anchor);
			return { days, start: days[0], end: addDays(days[6], 1) };
		}
		const day = startOfDay(anchor);
		return { days: [day], start: day, end: addDays(day, 1) };
	});

	const queryStart = $derived(addDays(viewData.start, -1));
	const queryEnd = $derived(addDays(viewData.end, 1));
	const selectedKey = $derived([...selected].sort().join(','));

	const writableCalendars = $derived(
		calendars.filter((cal) => cal.access !== 'read' && !cal.is_readonly)
	);
	const canCreate = $derived(writableCalendars.length > 0);
	const calendarsById = $derived(new Map(calendars.map((cal) => [cal.id, cal])));
	const editingAccess = $derived(
		editing ? calendarsById.get(editing.calendar_id)?.access : undefined
	);
	const dialogCanSave = $derived(
		editing ? editingAccess === 'edit' || editingAccess === 'own' : canCreate
	);
	const dialogCanDelete = $derived(editingAccess === 'edit' || editingAccess === 'own');
	const VIEWS: CalendarView[] = ['month', 'week', 'day'];

	const occurrences = $derived(expandEvents(events, queryStart, queryEnd));
	const occByDay = $derived(groupOccurrencesByDay(occurrences));

	const title = $derived.by(() => {
		if (view === 'month') return formatMonthTitle(anchor);
		if (view === 'day') return formatDayTitle(anchor);
		const first = viewData.days[0];
		const last = viewData.days[6];
		if (isSameMonth(first, last)) {
			return `${formatMonthTitle(first)} · ${first.getDate()}–${last.getDate()}`;
		}
		return `${formatMonthTitle(first)} – ${formatMonthTitle(last)}`;
	});

	let cycleTimer: ReturnType<typeof setInterval> | null = null;

	onMount(() => {
		void loadCalendars();
		// A session outlives midnight. Re-anchor a day view on the new
		// day so "today" is correct without a manual reload.
		cycleTimer = setInterval(() => {
			const now = startOfDay(new Date());
			if (view === 'day' && !sameAnchor(now, anchor)) anchor = now;
		}, 60_000);
		return () => {
			if (cycleTimer) clearInterval(cycleTimer);
		};
	});

	function sameAnchor(a: Date, b: Date): boolean {
		return (
			a.getFullYear() === b.getFullYear() &&
			a.getMonth() === b.getMonth() &&
			a.getDate() === b.getDate()
		);
	}

	$effect(() => {
		const ids = selectedKey ? selectedKey.split(',') : [];
		const start = queryStart;
		const end = queryEnd;
		if (!calendarsLoaded) return;
		void loadEvents(ids, start, end);
	});

	async function loadCalendars(): Promise<void> {
		calendarsError = null;
		try {
			const list = await listCalendars();
			calendars = list;
			const known = new Set(list.map((cal) => cal.id));
			for (const id of [...selected]) {
				if (!known.has(id)) selected.delete(id);
			}
			if (!selectionInitialized) {
				for (const cal of list) selected.add(cal.id);
				selectionInitialized = true;
			}
		} catch (err) {
			calendarsError = errorMessage(err);
		} finally {
			calendarsLoaded = true;
		}
	}

	async function loadEvents(ids: string[], start: Date, end: Date): Promise<void> {
		const ticket = ++loadTicket;
		if (ids.length === 0) {
			events = [];
			eventsError = null;
			eventsLoading = false;
			return;
		}
		eventsLoading = true;
		eventsError = null;
		const results = await Promise.allSettled(ids.map((id) => listEvents(id, start, end)));
		if (ticket !== loadTicket) return;
		const loaded: CalendarEvent[] = [];
		let failed = 0;
		for (const result of results) {
			if (result.status === 'fulfilled') loaded.push(...result.value);
			else failed++;
		}
		events = loaded;
		if (failed > 0) {
			eventsError = t('calendar.partial_load', 'Some calendars could not be loaded');
		}
		eventsLoading = false;
	}

	async function refresh(): Promise<void> {
		const ids = [...selected];
		await loadEvents(ids, queryStart, queryEnd);
	}

	function toggleCalendar(id: string): void {
		if (selected.has(id)) selected.delete(id);
		else selected.add(id);
	}

	function shift(delta: number): void {
		if (view === 'month') anchor = shiftMonth(anchor, delta);
		else if (view === 'week') anchor = addDays(anchor, delta * 7);
		else anchor = addDays(anchor, delta);
	}

	function goToday(): void {
		anchor = startOfDay(new Date());
	}

	function openCreate(day: Date, minutes = 9 * 60): void {
		if (!canCreate) return;
		editing = null;
		dialogDate = new Date(
			day.getFullYear(),
			day.getMonth(),
			day.getDate(),
			Math.floor(minutes / 60),
			minutes % 60
		);
		const preferred = writableCalendars.find((cal) => selected.has(cal.id)) ?? writableCalendars[0];
		dialogCalendarId = preferred.id;
		dialogOpen = true;
	}

	function selectDay(day: Date): void {
		if (view === 'month' && canCreate) {
			anchor = startOfDay(day);
			openCreate(day);
		} else {
			view = 'day';
			anchor = startOfDay(day);
		}
	}

	function openDay(day: Date): void {
		view = 'day';
		anchor = startOfDay(day);
	}

	function openEvent(occ: EventOccurrence): void {
		editing = occ.event;
		dialogCalendarId = occ.event.calendar_id;
		dialogDate = occ.start;
		dialogOpen = true;
	}

	async function saveEvent(body: SaveEventBody): Promise<void> {
		if (editing) {
			await updateEvent(editing.calendar_id, editing.id, body);
			ui.notify(t('calendar.event_updated', 'Event updated'), 'success');
			await refresh();
		} else {
			const target = dialogCalendarId;
			await createEvent(target, body);
			ui.notify(t('calendar.event_created', 'Event created'), 'success');
			// Adding an unselected calendar changes `selectedKey` and
			// reloads via the effect; an already-visible one needs an
			// explicit refresh.
			if (selected.has(target)) await refresh();
			else selected.add(target);
		}
	}

	async function removeEvent(): Promise<void> {
		if (!editing) return;
		await deleteEvent(editing.calendar_id, editing.id);
		ui.notify(t('calendar.event_deleted', 'Event deleted'), 'success');
		await refresh();
	}

	async function onCreateCalendar(): Promise<void> {
		const name = await promptDialog({
			title: t('calendar.new_calendar', 'New calendar'),
			placeholder: t('calendar.calendar_name', 'Calendar name'),
			confirmText: t('common.create', 'Create')
		});
		if (!name?.trim()) return;
		try {
			const created = await createCalendar(name.trim());
			calendars = [...calendars, created];
			selected.add(created.id);
			ui.notify(t('calendar.calendar_created', 'Calendar created'), 'success');
		} catch (err) {
			errorToast(err);
		}
	}

	async function onRenameCalendar(calendar: CalendarInfo): Promise<void> {
		const name = await promptDialog({
			title: t('calendar.rename_calendar', 'Rename calendar'),
			defaultValue: calendar.name,
			confirmText: t('common.save', 'Save')
		});
		if (!name?.trim() || name.trim() === calendar.name) return;
		try {
			const updated = await updateCalendar(calendar.id, { name: name.trim() });
			calendars = calendars.map((cal) => (cal.id === updated.id ? updated : cal));
		} catch (err) {
			errorToast(err);
		}
	}

	async function onDeleteCalendar(calendar: CalendarInfo): Promise<void> {
		const ok = await confirmDialog({
			title: t('calendar.delete_calendar', 'Delete calendar'),
			message: t(
				'calendar.confirm_delete_calendar',
				{ name: calendar.name },
				'Delete “{{name}}” and every event in it?'
			),
			confirmText: t('common.delete', 'Delete'),
			danger: true
		});
		if (!ok) return;
		try {
			await deleteCalendar(calendar.id);
			calendars = calendars.filter((cal) => cal.id !== calendar.id);
			selected.delete(calendar.id);
			ui.notify(t('calendar.calendar_deleted', 'Calendar deleted'), 'success');
		} catch (err) {
			errorToast(err);
		}
	}

	function openNewFromToolbar(): void {
		const day = startOfDay(anchor);
		openCreate(day);
	}
</script>

<div class="calendar-page">
	<aside class="calendar-side" aria-label={t('calendar.calendars', 'Calendars')}>
		<div class="calendar-side__head">
			<h1 class="calendar-side__title">{t('nav.calendars', 'Calendars')}</h1>
			<button
				type="button"
				class="icon-btn"
				title={t('calendar.new_calendar', 'New calendar')}
				aria-label={t('calendar.new_calendar', 'New calendar')}
				onclick={() => void onCreateCalendar()}
			>
				<Icon name="plus" />
			</button>
		</div>

		{#if calendarsError}
			<p class="calendar-side__error" role="alert">{calendarsError}</p>
		{:else if !calendarsLoaded}
			<p class="calendar-side__muted">{t('common.loading', 'Loading…')}</p>
		{:else if calendars.length === 0}
			<p class="calendar-side__muted">
				{t('calendar.no_calendars', 'No calendars yet. Create one to get started.')}
			</p>
		{:else}
			<ul class="calendar-list">
				{#each calendars as calendar (calendar.id)}
					<li class="calendar-list__item">
						<label class="calendar-list__label">
							<input
								type="checkbox"
								checked={selected.has(calendar.id)}
								onchange={() => toggleCalendar(calendar.id)}
							/>
							<span
								class="calendar-list__dot"
								style:background-color={readableEventColor(calendar.color, calendar.id).background}
							></span>
							<span class="calendar-list__name">{calendar.name}</span>
						</label>
						{#if calendar.access === 'own'}
							<div class="calendar-list__actions">
								<button
									type="button"
									class="icon-btn icon-btn--xs"
									title={t('calendar.rename_calendar', 'Rename calendar')}
									aria-label={t('calendar.rename_calendar', 'Rename calendar')}
									onclick={() => void onRenameCalendar(calendar)}
								>
									<Icon name="pen" />
								</button>
								<button
									type="button"
									class="icon-btn icon-btn--xs icon-btn--danger"
									title={t('calendar.delete_calendar', 'Delete calendar')}
									aria-label={t('calendar.delete_calendar', 'Delete calendar')}
									onclick={() => void onDeleteCalendar(calendar)}
								>
									<Icon name="trash" />
								</button>
							</div>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}
	</aside>

	<section class="calendar-main">
		<header class="calendar-toolbar">
			<div class="calendar-toolbar__nav">
				<button
					type="button"
					class="icon-btn"
					aria-label={t('calendar.previous', 'Previous')}
					title={t('calendar.previous', 'Previous')}
					onclick={() => shift(-1)}
				>
					<Icon name="chevron-left" />
				</button>
				<button type="button" class="icon-btn icon-btn--text" onclick={goToday}>
					{t('calendar.today', 'Today')}
				</button>
				<button
					type="button"
					class="icon-btn"
					aria-label={t('calendar.next', 'Next')}
					title={t('calendar.next', 'Next')}
					onclick={() => shift(1)}
				>
					<Icon name="chevron-right" />
				</button>
			</div>

			<h2 class="calendar-toolbar__title">{title}</h2>

			<div class="calendar-toolbar__end">
				{#if eventsLoading}
					<span class="calendar-toolbar__busy" aria-hidden="true"></span>
				{/if}
				<div class="seg" role="group" aria-label={t('calendar.view', 'Calendar view')}>
					{#each VIEWS as option (option)}
						<button
							type="button"
							class="seg__btn"
							class:seg__btn--active={view === option}
							aria-pressed={view === option}
							onclick={() => (view = option)}
						>
							{t(
								`calendar.view_${option}`,
								option === 'month' ? 'Month' : option === 'week' ? 'Week' : 'Day'
							)}
						</button>
					{/each}
				</div>
				<Button
					variant="primary"
					icon="plus"
					small
					disabled={!canCreate}
					onclick={openNewFromToolbar}
				>
					{t('calendar.new_event', 'New event')}
				</Button>
			</div>
		</header>

		{#if eventsError}
			<p class="calendar-banner" role="alert">{eventsError}</p>
		{/if}

		<div class="calendar-body" aria-busy={eventsLoading}>
			{#if !calendarsLoaded && !calendarsError}
				<EmptyState icon="calendar" title={t('common.loading', 'Loading…')} />
			{:else if selectedKey === ''}
				<EmptyState
					icon="calendar"
					title={t('calendar.none_selected', 'No calendars selected')}
					hint={t('calendar.none_selected_hint', 'Pick a calendar on the left to see its events.')}
				/>
			{:else if view === 'month'}
				<CalendarMonthView
					days={viewData.days}
					{anchor}
					today={new Date()}
					{occByDay}
					{calendarsById}
					onselectday={selectDay}
					onevent={openEvent}
					onmore={openDay}
				/>
			{:else}
				<CalendarTimeGrid
					days={viewData.days}
					today={new Date()}
					{occByDay}
					{calendarsById}
					{canCreate}
					onslot={openCreate}
					onevent={openEvent}
					ondayclick={openDay}
				/>
			{/if}
		</div>
	</section>
</div>

<CalendarEventDialog
	bind:open={dialogOpen}
	event={editing}
	calendars={editing ? calendars : writableCalendars}
	bind:calendarId={dialogCalendarId}
	defaultDate={dialogDate}
	canSave={dialogCanSave}
	canDelete={dialogCanDelete}
	onsave={saveEvent}
	ondelete={removeEvent}
	onclose={() => (dialogOpen = false)}
/>

<style>
	.calendar-page {
		display: grid;
		grid-template-columns: 210px minmax(0, 1fr);
		gap: var(--space-4);
		height: calc(100dvh - 70px - var(--space-5) * 2);
		min-height: 480px;
	}

	.calendar-side {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
		min-height: 0;
		padding: var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		background: var(--color-bg-surface);
		overflow-y: auto;
	}

	.calendar-side__head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-2);
	}

	.calendar-side__title {
		margin: 0;
		font-size: var(--text-lg);
		font-weight: var(--weight-semibold);
		color: var(--color-text-heading);
	}

	.calendar-side__error {
		margin: 0;
		font-size: var(--text-sm);
		color: var(--color-error-text);
	}

	.calendar-side__muted {
		margin: 0;
		font-size: var(--text-sm);
		color: var(--color-text-muted);
	}

	.calendar-list {
		display: flex;
		flex-direction: column;
		gap: 2px;
		margin: 0;
		padding: 0;
		list-style: none;
	}

	.calendar-list__item {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		padding: 2px 0;
		border-radius: var(--radius-sm);
	}

	.calendar-list__item:hover {
		background: var(--color-bg-hover);
	}

	.calendar-list__label {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		flex: 1;
		min-width: 0;
		padding: var(--space-1) var(--space-1-5);
		cursor: pointer;
		font-size: var(--text-sm);
		color: var(--color-text-secondary);
	}

	.calendar-list__label input {
		width: auto;
		margin: 0;
	}

	.calendar-list__dot {
		flex: none;
		width: 10px;
		height: 10px;
		border-radius: var(--radius-full);
	}

	.calendar-list__name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.calendar-list__actions {
		display: none;
		align-items: center;
		gap: 2px;
	}

	.calendar-list__item:hover .calendar-list__actions,
	.calendar-list__item:focus-within .calendar-list__actions {
		display: flex;
	}

	.calendar-main {
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
		min-width: 0;
		min-height: 0;
	}

	.calendar-toolbar {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		flex-wrap: wrap;
	}

	.calendar-toolbar__nav {
		display: flex;
		align-items: center;
		gap: var(--space-1);
	}

	.calendar-toolbar__title {
		flex: 1;
		min-width: 0;
		margin: 0;
		font-size: var(--text-xl);
		font-weight: var(--weight-semibold);
		color: var(--color-text-heading);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.calendar-toolbar__end {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.calendar-toolbar__busy {
		width: 12px;
		height: 12px;
		border: 2px solid var(--color-border-medium);
		border-top-color: var(--color-accent);
		border-radius: var(--radius-full);
		animation: calendar-spin var(--motion-spinner) linear infinite;
	}

	@keyframes calendar-spin {
		to {
			transform: rotate(360deg);
		}
	}

	.calendar-banner {
		margin: 0;
		padding: var(--space-2) var(--space-3);
		border-radius: var(--radius-md);
		background: var(--color-warning-bg);
		color: var(--color-warning-text);
		font-size: var(--text-sm);
	}

	.calendar-body {
		display: flex;
		flex-direction: column;
		flex: 1;
		min-height: 0;
	}

	.icon-btn {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 32px;
		height: 32px;
		padding: 0;
		border: 1px solid transparent;
		border-radius: var(--radius-md);
		background: transparent;
		color: var(--color-text-secondary);
		cursor: pointer;
	}

	.icon-btn:hover {
		background: var(--color-bg-hover);
	}

	.icon-btn--text {
		width: auto;
		padding: 0 var(--space-2);
		border-color: var(--color-border);
		font-size: var(--text-sm);
	}

	.icon-btn--xs {
		width: 24px;
		height: 24px;
		font-size: var(--text-xs);
	}

	.icon-btn--danger:hover {
		color: var(--color-error-text);
		background: var(--color-error-bg);
	}

	.seg {
		display: inline-flex;
		padding: 2px;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-muted);
	}

	.seg__btn {
		padding: var(--space-1) var(--space-2-5);
		border: none;
		border-radius: var(--radius-sm);
		background: transparent;
		color: var(--color-text-muted);
		font: inherit;
		font-size: var(--text-sm);
		cursor: pointer;
	}

	.seg__btn:hover {
		color: var(--color-text);
	}

	.seg__btn--active {
		background: var(--color-bg-surface);
		color: var(--color-text-heading);
		box-shadow: var(--shadow-xs);
	}

	@media (hover: none), (width <= 900px) {
		.calendar-list__actions {
			display: flex;
		}
	}

	@media (width <= 900px) {
		.calendar-page {
			grid-template-columns: minmax(0, 1fr);
			grid-template-rows: auto minmax(0, 1fr);
			height: auto;
		}

		.calendar-side {
			max-height: 220px;
		}

		.calendar-body {
			min-height: 60vh;
		}
	}

	@media (width <= 640px) {
		.calendar-page {
			height: calc(100dvh - 70px - var(--space-4) * 2);
		}

		.calendar-toolbar__title {
			order: 3;
			flex-basis: 100%;
		}
	}
</style>
