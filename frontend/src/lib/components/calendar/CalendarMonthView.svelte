<script lang="ts">
	import { t } from '$lib/i18n/index.svelte';
	import {
		dateKey,
		formatDayNumber,
		formatDayTitle,
		formatTime,
		formatWeekdayShort,
		isSameMonth,
		sameDay
	} from '$lib/utils/calendarDate';
	import { readableEventColor } from '$lib/utils/calendarColor';
	import type { EventOccurrence } from '$lib/utils/calendarRecurrence';
	import type { CalendarInfo } from '$lib/api/endpoints/calendars';

	interface Props {
		days: Date[];
		anchor: Date;
		today: Date;
		occByDay: Map<string, EventOccurrence[]>;
		calendarsById: Map<string, CalendarInfo>;
		onselectday: (day: Date) => void;
		onevent: (occ: EventOccurrence) => void;
		onmore: (day: Date) => void;
	}

	let { days, anchor, today, occByDay, calendarsById, onselectday, onevent, onmore }: Props =
		$props();

	const VISIBLE = 3;

	function dayEvents(key: string): EventOccurrence[] {
		const items = occByDay.get(key);
		if (!items) return [];
		// All-day first, then chronological. Stable enough for a day cell.
		return [...items].sort((a, b) => {
			if (a.allDay !== b.allDay) return a.allDay ? -1 : 1;
			return a.start.getTime() - b.start.getTime();
		});
	}

	function colorOf(occ: EventOccurrence) {
		return readableEventColor(
			calendarsById.get(occ.event.calendar_id)?.color,
			occ.event.calendar_id
		);
	}
</script>

<div class="month" aria-label={t('calendar.month_grid', 'Month')}>
	<div class="month__weekdays">
		{#each days.slice(0, 7) as day (dateKey(day))}
			<div class="month__weekday">{formatWeekdayShort(day)}</div>
		{/each}
	</div>

	<div class="month__grid">
		{#each days as day (dateKey(day))}
			{@const key = dateKey(day)}
			{@const items = dayEvents(key)}
			{@const outside = !isSameMonth(day, anchor)}
			<div
				class="month__cell"
				class:month__cell--outside={outside}
				class:month__cell--today={sameDay(day, today)}
			>
				<button
					type="button"
					class="month__daynum"
					aria-label={formatDayTitle(day)}
					onclick={() => onselectday(day)}
				>
					{formatDayNumber(day)}
				</button>

				<ul class="month__events">
					{#each items.slice(0, VISIBLE) as occ (occ.key)}
						{@const colors = colorOf(occ)}
						<li>
							<button
								type="button"
								class="month__chip"
								class:month__chip--allday={occ.allDay}
								style:background-color={colors.background}
								style:color={colors.foreground}
								title={occ.event.summary}
								onclick={() => onevent(occ)}
							>
								{#if !occ.allDay}
									<span class="month__chip-time">{formatTime(occ.start)}</span>
								{/if}
								<span class="month__chip-title">{occ.event.summary}</span>
							</button>
						</li>
					{/each}
					{#if items.length > VISIBLE}
						<li>
							<button type="button" class="month__more" onclick={() => onmore(day)}>
								{t('calendar.more', { count: items.length - VISIBLE }, '+{{count}} more')}
							</button>
						</li>
					{/if}
				</ul>
			</div>
		{/each}
	</div>
</div>

<style>
	.month {
		display: flex;
		flex-direction: column;
		min-height: 0;
		flex: 1;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		background: var(--color-bg-surface);
		overflow: hidden;
	}

	.month__weekdays,
	.month__grid {
		display: grid;
		grid-template-columns: repeat(7, minmax(0, 1fr));
	}

	.month__weekdays {
		border-bottom: 1px solid var(--color-border);
		background: var(--color-bg-muted);
	}

	.month__weekday {
		padding: var(--space-2) var(--space-2);
		font-size: var(--text-xs);
		font-weight: var(--weight-semibold);
		color: var(--color-text-muted);
		text-align: center;
		text-transform: uppercase;
		letter-spacing: var(--tracking-wide);
	}

	.month__grid {
		flex: 1;
		grid-auto-rows: minmax(92px, 1fr);
	}

	.month__cell {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		min-width: 0;
		padding: var(--space-1-5);
		border-right: 1px solid var(--color-border-light);
		border-bottom: 1px solid var(--color-border-light);
		overflow: hidden;
	}

	.month__cell:nth-child(7n) {
		border-right: none;
	}

	.month__cell--outside {
		background: var(--color-bg-subtle);
	}

	.month__cell--outside .month__daynum {
		color: var(--color-text-faint);
	}

	.month__cell--today {
		background: var(--color-accent-bg);
	}

	.month__daynum {
		align-self: flex-start;
		min-width: 24px;
		padding: 0 var(--space-1);
		border: none;
		border-radius: var(--radius-full);
		background: transparent;
		color: var(--color-text-secondary);
		font: inherit;
		font-size: var(--text-xs);
		font-weight: var(--weight-semibold);
		line-height: 24px;
		cursor: pointer;
	}

	.month__cell--today .month__daynum {
		background: var(--color-accent);
		color: var(--color-on-accent);
	}

	.month__daynum:hover:not(:disabled) {
		background: var(--color-bg-hover);
	}

	.month__daynum:disabled {
		cursor: default;
	}

	.month__events {
		display: flex;
		flex-direction: column;
		gap: 2px;
		margin: 0;
		padding: 0;
		list-style: none;
		min-width: 0;
	}

	.month__chip {
		display: flex;
		align-items: baseline;
		gap: var(--space-1);
		width: 100%;
		padding: 2px var(--space-1-5);
		border: none;
		border-radius: var(--radius-sm);
		font: inherit;
		font-size: var(--text-2xs);
		line-height: var(--leading-tight);
		text-align: left;
		cursor: pointer;
		overflow: hidden;
	}

	.month__chip:hover {
		filter: brightness(1.06);
	}

	.month__chip-time {
		flex: none;
		opacity: 0.85;
	}

	.month__chip-title {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.month__chip--allday .month__chip-title {
		font-weight: var(--weight-semibold);
	}

	.month__more {
		padding: 0 var(--space-1);
		border: none;
		background: transparent;
		color: var(--color-text-muted);
		font: inherit;
		font-size: var(--text-2xs);
		text-align: left;
		cursor: pointer;
	}

	.month__more:hover {
		color: var(--color-accent-text);
	}

	@media (width <= 640px) {
		.month__grid {
			grid-auto-rows: minmax(64px, 1fr);
		}

		.month__cell {
			padding: 2px;
		}

		.month__chip {
			padding: 1px var(--space-1);
		}
	}
</style>
