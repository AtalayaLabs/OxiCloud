<script lang="ts">
	import { onMount } from 'svelte';
	import { SvelteDate, SvelteMap } from 'svelte/reactivity';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { dateTimeFormatFor, formatDate } from '$lib/utils/display';
	import { errorMessage } from '$lib/utils/errors';
	import {
		fetchCalendarAgenda,
		type CalendarAgendaEvent,
		type CalendarAgendaResult
	} from '$lib/api/endpoints/calendar';
	import { t } from '$lib/i18n/index.svelte';

	let loading = $state(true);
	let error = $state<string | null>(null);
	let result = $state<CalendarAgendaResult>({ calendars: [], events: [], errors: [] });

	const timeFmt = dateTimeFormatFor(undefined, { hour: 'numeric', minute: '2-digit' });

	function dayKey(date: Date): string {
		const year = date.getFullYear();
		const month = `${date.getMonth() + 1}`.padStart(2, '0');
		const day = `${date.getDate()}`.padStart(2, '0');
		return `${year}-${month}-${day}`;
	}

	function dayLabel(date: Date): string {
		const today = new SvelteDate();
		today.setHours(0, 0, 0, 0);
		const target = new SvelteDate(date);
		target.setHours(0, 0, 0, 0);
		const deltaDays = Math.round((target.getTime() - today.getTime()) / 86_400_000);
		if (deltaDays === 0) return t('calendar.today', 'Today');
		if (deltaDays === 1) return t('calendar.tomorrow', 'Tomorrow');
		return formatDate(date.getTime());
	}

	function formatTimeRange(event: CalendarAgendaEvent): string {
		if (event.allDay) return t('calendar.all_day', 'All day');
		return `${timeFmt.format(event.start)} - ${timeFmt.format(event.end)}`;
	}

	function formatAgendaSections(
		events: CalendarAgendaEvent[]
	): { key: string; date: Date; events: CalendarAgendaEvent[] }[] {
		const groups = new SvelteMap<string, { date: SvelteDate; events: CalendarAgendaEvent[] }>();
		for (const event of events) {
			const key = dayKey(event.start);
			const bucket = groups.get(key);
			if (bucket) bucket.events.push(event);
			else groups.set(key, { date: new SvelteDate(event.start), events: [event] });
		}
		return Array.from(groups.entries()).map(([key, value]) => ({ key, ...value }));
	}

	async function loadAgenda() {
		loading = true;
		error = null;
		try {
			result = await fetchCalendarAgenda();
		} catch (err) {
			error = errorMessage(err);
			result = { calendars: [], events: [], errors: [] };
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		void loadAgenda();
	});

	const sections = $derived(formatAgendaSections(result.events));
	const nextEvent = $derived(result.events[0] ?? null);
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
	{:else if result.events.length === 0}
		<EmptyState
			icon="calendar"
			title={t('calendar.empty', 'No upcoming events')}
			hint={result.calendars.length > 0
				? t(
						'calendar.empty_hint',
						'Your calendars are connected, but nothing is scheduled in the next month.'
					)
				: t('calendar.no_calendars', 'No calendars were returned by CalDAV.')}
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

		<div class="calendar-page__agenda">
			{#each sections as section (section.key)}
				<section class="calendar-day">
					<header class="calendar-day__header">
						<div>
							<p class="calendar-day__label">{dayLabel(section.date)}</p>
							<p class="calendar-day__meta">{formatDate(section.date.getTime())}</p>
						</div>
						<span class="calendar-day__count">{section.events.length}</span>
					</header>
					<div class="calendar-day__events">
						{#each section.events as event (event.id)}
							<article class="calendar-event">
								<div class="calendar-event__time">{formatTimeRange(event)}</div>
								<div class="calendar-event__body">
									<div class="calendar-event__title-row">
										<h2 class="calendar-event__title">{event.summary}</h2>
										<span class="calendar-event__calendar">{event.calendarName}</span>
									</div>
									{#if event.location}
										<p class="calendar-event__detail">{event.location}</p>
									{/if}
									{#if event.description}
										<p class="calendar-event__detail calendar-event__detail--muted">
											{event.description}
										</p>
									{/if}
								</div>
							</article>
						{/each}
					</div>
				</section>
			{/each}
		</div>
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
	.calendar-day,
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

	.calendar-page__agenda {
		display: grid;
		gap: var(--space-4);
	}

	.calendar-day {
		padding: var(--space-4);
	}

	.calendar-day__header {
		display: flex;
		justify-content: space-between;
		gap: var(--space-4);
		align-items: flex-start;
		margin-bottom: var(--space-3);
		padding-bottom: var(--space-3);
		border-bottom: 1px solid var(--color-border);
	}

	.calendar-day__label {
		margin: 0;
		font-size: var(--text-lg);
		font-weight: var(--weight-semibold);
	}

	.calendar-day__meta {
		margin: var(--space-1) 0 0;
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}

	.calendar-day__count {
		padding: var(--space-1) var(--space-2);
		border-radius: var(--radius-full);
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		font-size: var(--text-sm);
		font-weight: var(--weight-semibold);
	}

	.calendar-day__events {
		display: grid;
		gap: var(--space-3);
	}

	.calendar-event {
		display: grid;
		grid-template-columns: minmax(8rem, 12rem) minmax(0, 1fr);
		gap: var(--space-4);
		padding: var(--space-3);
		border-radius: var(--radius-md);
		background: var(--color-bg-muted);
	}

	.calendar-event__time {
		font-size: var(--text-sm);
		font-weight: var(--weight-semibold);
		color: var(--color-text-muted);
	}

	.calendar-event__title-row {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: var(--space-2);
	}

	.calendar-event__title {
		margin: 0;
		font-size: var(--text-base);
		font-weight: var(--weight-semibold);
	}

	.calendar-event__calendar {
		padding: 0.1rem 0.5rem;
		border-radius: var(--radius-full);
		background: var(--color-bg-surface);
		color: var(--color-text-muted);
		font-size: var(--text-xs);
	}

	.calendar-event__detail {
		margin: var(--space-2) 0 0;
		color: var(--color-text);
	}

	.calendar-event__detail--muted {
		color: var(--color-text-muted);
	}

	@media (width <= 900px) {
		.calendar-page__hero,
		.calendar-page__stats {
			grid-template-columns: 1fr;
			display: grid;
		}

		.calendar-event {
			grid-template-columns: 1fr;
		}
	}

	@media (width <= 640px) {
		.calendar-page {
			padding: var(--space-3);
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
