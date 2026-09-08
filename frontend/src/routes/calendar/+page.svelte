<script lang="ts">
	import { onMount } from 'svelte';
	import { Calendar, Willow } from '@svar-ui/svelte-calendar';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { dateTimeFormatFor } from '$lib/utils/display';
	import { errorMessage } from '$lib/utils/errors';
	import {
		fetchCalendarAgenda,
		type CalendarAgendaEvent,
		type CalendarAgendaResult
	} from '$lib/api/endpoints/calendar';
	import { t } from '$lib/i18n/index.svelte';

	type CalendarWidgetEvent = {
		id: string;
		start: Date;
		end: Date;
		allDay?: boolean;
		text: string;
		details?: string;
		color?: string;
	};

	let loading = $state(true);
	let error = $state<string | null>(null);
	let result = $state<CalendarAgendaResult>({ calendars: [], events: [], errors: [] });
	let calendarDate = $state(new Date());

	const timeFmt = dateTimeFormatFor(undefined, { hour: 'numeric', minute: '2-digit' });

	function formatTimeRange(event: CalendarAgendaEvent): string {
		if (event.allDay) return t('calendar.all_day', 'All day');
		return `${timeFmt.format(event.start)} - ${timeFmt.format(event.end)}`;
	}

	function toCalendarEvent(event: CalendarAgendaEvent): CalendarWidgetEvent {
		return {
			id: event.id,
			start: new Date(event.start),
			end: new Date(event.end),
			allDay: event.allDay,
			text: event.summary,
			details: [event.location, event.description].filter(Boolean).join(' • '),
			color: 'var(--color-primary-strong, #6b7cff)'
		};
	}

	async function loadAgenda() {
		loading = true;
		error = null;
		try {
			const nextResult = await fetchCalendarAgenda();
			result = nextResult;
			const nextEvent = [...nextResult.events].sort(
				(a, b) => a.start.getTime() - b.start.getTime()
			)[0];
			calendarDate = nextEvent ? new Date(nextEvent.start) : new Date();
		} catch (err) {
			error = errorMessage(err);
			result = { calendars: [], events: [], errors: [] };
			calendarDate = new Date();
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		void loadAgenda();
	});

	const nextEvent = $derived(result.events[0] ?? null);
	const calendarEvents = $derived(result.events.map(toCalendarEvent));
</script>

<svelte:head>
	<title>{t('nav.calendar', 'Calendar')}</title>
</svelte:head>

<div class="calendar-page">
	<section class="calendar-page__hero">
		<div class="calendar-page__headline">
			<p class="calendar-page__eyebrow">{t('nav.calendar', 'Calendar')}</p>
			<h1>{t('calendar.agenda', 'Agenda')}</h1>
			<p class="calendar-page__lede">
				{t('calendar.lede', 'A rolling agenda across your CalDAV calendars.')}
			</p>
		</div>
		<button
			class="calendar-page__refresh button"
			type="button"
			onclick={loadAgenda}
			disabled={loading}
		>
			<Icon name="rotate" />
			{t('common.refresh', 'Refresh')}
		</button>
	</section>

	<section class="calendar-page__stats" aria-label={t('calendar.summary', 'Agenda summary')}>
		<article class="calendar-stat">
			<p class="calendar-stat__label">{t('calendar.calendars', 'Calendars')}</p>
			<p class="calendar-stat__value">{result.calendars.length}</p>
		</article>
		<article class="calendar-stat">
			<p class="calendar-stat__label">{t('calendar.events', 'Upcoming events')}</p>
			<p class="calendar-stat__value">{result.events.length}</p>
		</article>
		<article class="calendar-stat">
			<p class="calendar-stat__label">{t('calendar.next_event', 'Next event')}</p>
			<p class="calendar-stat__value">
				{nextEvent ? formatTimeRange(nextEvent) : t('calendar.none', 'None')}
			</p>
		</article>
	</section>

	{#if error}
		<EmptyState
			icon="calendar-xmark"
			title={t('calendar.load_failed', 'Calendar unavailable')}
			hint={error}
		>
			<button
				class="calendar-page__retry button button--secondary"
				type="button"
				onclick={loadAgenda}
			>
				{t('common.retry', 'Retry')}
			</button>
		</EmptyState>
	{:else if loading}
		<EmptyState
			icon="calendar"
			title={t('calendar.loading', 'Loading agenda')}
			hint={t('calendar.loading_hint', 'Fetching calendars and upcoming events.')}
		/>
	{:else}
		{#if result.errors.length > 0}
			<div class="calendar-page__warning" role="status">
				<Icon name="triangle-exclamation" />
				<p>
					{t(
						'calendar.partial_load',
						{ n: result.errors.length },
						'Loaded with {{n}} calendar error(s).'
					)}
				</p>
			</div>
		{/if}

		{#if result.calendars.length === 0}
			<EmptyState
				icon="calendar"
				title={t('calendar.empty', 'No upcoming events')}
				hint={t('calendar.no_calendars', 'No calendars were returned by CalDAV.')}
			/>
		{:else}
			<div class="calendar-page__calendar" aria-label={t('nav.calendar', 'Calendar')}>
				<Willow>
					<Calendar events={calendarEvents} date={calendarDate} view="month" readonly />
				</Willow>
			</div>
		{/if}
	{/if}
</div>

<style>
	.calendar-page {
		padding: clamp(1.25rem, 2vw, 2rem);
		background:
			radial-gradient(circle at top left, var(--color-bg-muted) 0%, transparent 42%),
			linear-gradient(180deg, var(--color-bg-page), var(--color-bg-base, var(--color-bg-page)));
	}

	.calendar-page__hero {
		display: flex;
		gap: var(--space-4);
		justify-content: space-between;
		align-items: flex-start;
		margin-bottom: var(--space-5);
	}

	.calendar-page__headline {
		max-width: 40rem;
	}

	.calendar-page__eyebrow {
		margin: 0 0 var(--space-1);
		text-transform: uppercase;
		letter-spacing: 0.14em;
		font-size: var(--text-xs);
		font-weight: var(--weight-semibold);
		color: var(--color-text-muted);
	}

	.calendar-page h1 {
		margin: 0;
		font-size: clamp(2rem, 4vw, 3.25rem);
		line-height: 1;
		letter-spacing: -0.04em;
	}

	.calendar-page__lede {
		margin: var(--space-3) 0 0;
		max-width: 36rem;
		color: var(--color-text-muted);
	}

	.calendar-page__refresh,
	.calendar-page__retry {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
	}

	.calendar-page__stats {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: var(--space-3);
		margin-bottom: var(--space-5);
	}

	.calendar-stat,
	.calendar-page__warning {
		border: 1px solid var(--color-border);
		background: var(--color-bg-surface);
		border-radius: var(--radius-lg);
		box-shadow: var(--shadow-sm);
	}

	.calendar-stat {
		padding: var(--space-4);
	}

	.calendar-stat__label {
		margin: 0 0 var(--space-1);
		font-size: var(--text-sm);
		color: var(--color-text-muted);
	}

	.calendar-stat__value {
		margin: 0;
		font-size: var(--text-2xl);
		font-weight: var(--weight-semibold);
	}

	.calendar-page__warning {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		padding: var(--space-3) var(--space-4);
		margin-bottom: var(--space-4);
		background: var(--color-warning-bg, var(--color-bg-muted));
		color: var(--color-warning-text, var(--color-text-muted));
	}

	.calendar-page__warning :global(svg) {
		flex: none;
	}

	.calendar-page__warning p {
		margin: 0;
	}

	.calendar-page__calendar {
		padding: var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		background: var(--color-bg-surface);
		box-shadow: var(--shadow-sm);
		overflow: hidden;
	}

	.calendar-page__calendar :global(.wx-calendar) {
		display: block;
		height: clamp(34rem, 72vh, 52rem);
	}

	.calendar-page__calendar :global(.wx-calendar-main),
	.calendar-page__calendar :global(.wx-sections) {
		height: 100%;
	}

	/* Fallback icons when the remote wx-icons font is blocked/unavailable. */
	.calendar-page__calendar :global(.wxi-angle-left::before) {
		content: "\2039";
		font-family: var(--wx-font-family, sans-serif);
	}

	.calendar-page__calendar :global(.wxi-angle-right::before) {
		content: "\203A";
		font-family: var(--wx-font-family, sans-serif);
	}

	.calendar-page__calendar :global(.wxi-angle-down::before) {
		content: "\25BE";
		font-family: var(--wx-font-family, sans-serif);
	}

	.calendar-page__calendar :global(.wxi-menu::before) {
		content: "\2630";
		font-family: var(--wx-font-family, sans-serif);
	}

	@media (width <= 900px) {
		.calendar-page__hero,
		.calendar-page__stats {
			grid-template-columns: 1fr;
			display: grid;
		}
	}

	@media (width <= 640px) {
		.calendar-page {
			padding: var(--space-3);
		}

		.calendar-page__calendar :global(.wx-calendar) {
			height: 70vh;
		}

		.calendar-page__hero {
			gap: var(--space-3);
		}

		.calendar-page__refresh {
			width: 100%;
			justify-content: center;
		}
	}
</style>
