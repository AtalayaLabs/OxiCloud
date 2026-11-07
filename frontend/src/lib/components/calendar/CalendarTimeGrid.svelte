<script lang="ts">
	import { t } from '$lib/i18n/index.svelte';
	import {
		dateKey,
		formatDayNumber,
		formatHourLabel,
		formatTime,
		formatWeekdayShort,
		sameDay
	} from '$lib/utils/calendarDate';
	import { readableEventColor } from '$lib/utils/calendarColor';
	import type { EventOccurrence } from '$lib/utils/calendarRecurrence';
	import type { CalendarInfo } from '$lib/api/endpoints/calendars';

	interface Props {
		days: Date[];
		today: Date;
		occByDay: Map<string, EventOccurrence[]>;
		calendarsById: Map<string, CalendarInfo>;
		canCreate: boolean;
		onslot: (day: Date, minutes: number) => void;
		onevent: (occ: EventOccurrence) => void;
		ondayclick: (day: Date) => void;
	}

	let { days, today, occByDay, calendarsById, canCreate, onslot, onevent, ondayclick }: Props =
		$props();

	const HOURS = Array.from({ length: 24 }, (_, hour) => hour);

	interface Layout {
		occ: EventOccurrence;
		top: number;
		height: number;
		left: number;
		width: number;
	}

	interface Placed {
		occ: EventOccurrence;
		column: number;
		endMs: number;
		left: number;
		width: number;
	}

	/** Pack a day's timed occurrences into non-overlapping columns.
	 * A cluster ends at the first event that starts after every open
	 * column has closed, so width is shared only among truly
	 * concurrent blocks. */
	function layoutDay(key: string, dayStartMs: number, dayEndMs: number): Layout[] {
		const items = (occByDay.get(key) ?? []).filter((occ) => !occ.allDay);
		if (items.length === 0) return [];
		const span = dayEndMs - dayStartMs;
		const sorted = [...items].sort((a, b) => {
			const byStart = a.start.getTime() - b.start.getTime();
			return byStart !== 0 ? byStart : a.end.getTime() - b.end.getTime();
		});

		const placed: Placed[] = [];
		let cluster: Placed[] = [];
		let clusterEnd = 0;
		const freeColumns: number[] = [];

		const flush = () => {
			if (cluster.length === 0) return;
			let columns = 0;
			for (const item of cluster) columns = Math.max(columns, item.column + 1);
			const width = 1 / columns;
			for (const item of cluster) {
				item.left = item.column * width;
				item.width = width;
				placed.push(item);
			}
			cluster = [];
			clusterEnd = 0;
			freeColumns.length = 0;
		};

		for (const occ of sorted) {
			const startMs = occ.start.getTime();
			const endMs = occ.end.getTime();
			if (cluster.length > 0 && startMs >= clusterEnd) flush();
			for (let i = 0; i < freeColumns.length; i++) {
				if (freeColumns[i] <= startMs) freeColumns[i] = 0;
			}
			let column = freeColumns.findIndex((end) => end === 0);
			if (column === -1) {
				column = freeColumns.length;
				freeColumns.push(0);
			}
			freeColumns[column] = endMs;
			cluster.push({ occ, column, endMs, left: 0, width: 1 });
			clusterEnd = Math.max(clusterEnd, endMs);
		}
		flush();

		return placed.map((item) => {
			const startMs = Math.max(item.occ.start.getTime(), dayStartMs);
			const endMs = Math.min(item.occ.end.getTime(), dayEndMs);
			return {
				occ: item.occ,
				top: ((startMs - dayStartMs) / span) * 100,
				height: Math.max(((endMs - startMs) / span) * 100, 0.6),
				left: item.left,
				width: item.width
			};
		});
	}

	function allDayItems(key: string): EventOccurrence[] {
		return (occByDay.get(key) ?? []).filter((occ) => occ.allDay);
	}

	function dayBounds(day: Date): { start: number; end: number } {
		const start = new Date(day.getFullYear(), day.getMonth(), day.getDate());
		const end = new Date(day.getFullYear(), day.getMonth(), day.getDate() + 1);
		return { start: start.getTime(), end: end.getTime() };
	}

	function colorOf(occ: EventOccurrence) {
		return readableEventColor(
			calendarsById.get(occ.event.calendar_id)?.color,
			occ.event.calendar_id
		);
	}

	const gridColumns = $derived(`56px repeat(${days.length}, minmax(0, 1fr))`);
</script>

<div class="timed">
	<div class="timed__head" style:grid-template-columns={gridColumns}>
		<div class="timed__gutter"></div>
		{#each days as day (dateKey(day))}
			<button
				type="button"
				class="timed__day"
				class:timed__day--today={sameDay(day, today)}
				onclick={() => ondayclick(day)}
			>
				<span class="timed__day-name">{formatWeekdayShort(day)}</span>
				<span class="timed__day-num">{formatDayNumber(day)}</span>
			</button>
		{/each}
	</div>

	<div class="timed__allday" style:grid-template-columns={gridColumns}>
		<div class="timed__gutter timed__gutter--allday">
			{t('calendar.all_day_short', 'All day')}
		</div>
		{#each days as day (dateKey(day))}
			{@const key = dateKey(day)}
			<div class="timed__allday-cell">
				{#each allDayItems(key) as occ (occ.key)}
					{@const colors = colorOf(occ)}
					<button
						type="button"
						class="timed__allday-chip"
						style:background-color={colors.background}
						style:color={colors.foreground}
						title={occ.event.summary}
						onclick={() => onevent(occ)}
					>
						{occ.event.summary}
					</button>
				{/each}
			</div>
		{/each}
	</div>

	<div class="timed__scroll">
		<div class="timed__body" style:grid-template-columns={gridColumns}>
			<div class="timed__hours">
				{#each HOURS as hour (hour)}
					<div class="timed__hour-label">{formatHourLabel(hour)}</div>
				{/each}
			</div>

			{#each days as day (dateKey(day))}
				{@const key = dateKey(day)}
				{@const bounds = dayBounds(day)}
				{@const layouts = layoutDay(key, bounds.start, bounds.end)}
				<div class="timed__col">
					{#each HOURS as hour (hour)}
						<button
							type="button"
							class="timed__slot"
							disabled={!canCreate}
							aria-label={`${formatWeekdayShort(day)} ${formatDayNumber(day)} ${formatHourLabel(hour)}`}
							onclick={() => onslot(day, hour * 60)}
						></button>
					{/each}

					<div class="timed__events">
						{#each layouts as layout (layout.occ.key)}
							{@const colors = colorOf(layout.occ)}
							<button
								type="button"
								class="timed__event"
								style:top="{layout.top}%"
								style:height="{layout.height}%"
								style:left="{layout.left * 100}%"
								style:width="calc({layout.width * 100}% - 2px)"
								style:background-color={colors.background}
								style:color={colors.foreground}
								title={`${formatTime(layout.occ.start)} ${layout.occ.event.summary}`}
								onclick={() => onevent(layout.occ)}
							>
								<span class="timed__event-time">{formatTime(layout.occ.start)}</span>
								<span class="timed__event-title">{layout.occ.event.summary}</span>
								{#if layout.occ.event.location}
									<span class="timed__event-location">{layout.occ.event.location}</span>
								{/if}
							</button>
						{/each}
					</div>
				</div>
			{/each}
		</div>
	</div>
</div>

<style>
	.timed {
		display: flex;
		flex-direction: column;
		min-height: 0;
		flex: 1;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		background: var(--color-bg-surface);
		overflow: hidden;
	}

	.timed__head,
	.timed__allday {
		display: grid;
		border-bottom: 1px solid var(--color-border);
	}

	.timed__head {
		background: var(--color-bg-muted);
	}

	.timed__gutter {
		border-right: 1px solid var(--color-border-light);
	}

	.timed__gutter--allday {
		display: flex;
		align-items: center;
		justify-content: flex-end;
		padding: var(--space-1) var(--space-2);
		font-size: var(--text-2xs);
		color: var(--color-text-faint);
	}

	.timed__day {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: var(--space-1-5);
		padding: var(--space-2) var(--space-1);
		border: none;
		background: transparent;
		color: var(--color-text-secondary);
		font: inherit;
		cursor: pointer;
	}

	.timed__day:hover {
		background: var(--color-bg-hover);
	}

	.timed__day-name {
		font-size: var(--text-xs);
		text-transform: uppercase;
		letter-spacing: var(--tracking-wide);
		color: var(--color-text-muted);
	}

	.timed__day-num {
		font-size: var(--text-md);
		font-weight: var(--weight-semibold);
	}

	.timed__day--today .timed__day-num {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 28px;
		height: 28px;
		border-radius: var(--radius-full);
		background: var(--color-accent);
		color: var(--color-on-accent);
	}

	.timed__allday-cell {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-height: 28px;
		padding: 2px;
		border-right: 1px solid var(--color-border-light);
	}

	.timed__allday-chip {
		width: 100%;
		padding: 2px var(--space-1-5);
		border: none;
		border-radius: var(--radius-sm);
		font: inherit;
		font-size: var(--text-2xs);
		font-weight: var(--weight-semibold);
		text-align: left;
		cursor: pointer;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.timed__scroll {
		flex: 1;
		overflow-y: auto;
		min-height: 0;
	}

	.timed__body {
		display: grid;
	}

	.timed__hours {
		border-right: 1px solid var(--color-border-light);
	}

	.timed__hour-label {
		height: var(--calendar-hour-height, 48px);
		padding: 0 var(--space-2);
		text-align: right;
		font-size: var(--text-2xs);
		color: var(--color-text-faint);
		transform: translateY(-0.5em);
	}

	.timed__col {
		position: relative;
		border-right: 1px solid var(--color-border-light);
	}

	.timed__slot {
		display: block;
		width: 100%;
		height: var(--calendar-hour-height, 48px);
		border: none;
		border-bottom: 1px solid var(--color-border-light);
		background: transparent;
		cursor: pointer;
	}

	.timed__slot:hover:not(:disabled) {
		background: var(--color-accent-bg);
	}

	.timed__slot:disabled {
		cursor: default;
	}

	.timed__events {
		position: absolute;
		inset: 0;
		pointer-events: none;
	}

	.timed__event {
		position: absolute;
		display: flex;
		flex-direction: column;
		gap: 1px;
		padding: 2px var(--space-1-5);
		border: none;
		border-radius: var(--radius-sm);
		font: inherit;
		font-size: var(--text-2xs);
		line-height: var(--leading-tight);
		text-align: left;
		overflow: hidden;
		pointer-events: auto;
		cursor: pointer;
		opacity: 0.94;
	}

	.timed__event:hover {
		opacity: 1;
		filter: brightness(1.05);
	}

	.timed__event-time {
		opacity: 0.85;
	}

	.timed__event-title {
		font-weight: var(--weight-semibold);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.timed__event-location {
		opacity: 0.8;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
</style>
