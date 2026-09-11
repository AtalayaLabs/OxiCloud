// Admin-job-dashboard sugar around `useTopic`.
//
// Subscribes to `job:{name}` and dispatches the three `rt.event`
// variants — `job_run_started`, `job_run_progress`, `job_run_ended`
// — to per-verb handlers. Admin-only server-side (Class 3, see
// `application/ports/message_bus_ports.rs::required_perm`); a
// non-admin caller sees `topic_forbidden` on subscribe and the
// subscription is dropped.
//
// This composable mirrors `useFolderTopic` but is deliberately
// separate: the two share nothing beyond `useTopic`, and merging
// them would smear two AuthZ classes (ResourceRead vs RoleAdmin)
// into one call surface.

import { useTopic } from './useTopic.svelte';
import type RtEventParams from '$lib/generated/message-bus/RtEventParams';
import type RtRevokedParams from '$lib/generated/message-bus/RtRevokedParams';
import type JobRunStartedData from '$lib/generated/message-bus/JobRunStartedData';
import type JobRunProgressData from '$lib/generated/message-bus/JobRunProgressData';
import type JobRunEndedData from '$lib/generated/message-bus/JobRunEndedData';

/**
 * Optional per-verb handlers for a single job's run stream. Any
 * subset is accepted; unhandled verbs fall through silently.
 *
 * `onEnded` is the canonical "the server is done publishing on this
 * topic for now" signal — the admin dashboard uses it to switch a
 * row back to "idle" and stop expecting progress updates. The
 * subscription itself stays open (jobs can run again), so callers
 * that want a one-shot pattern should track that in their own state.
 */
export interface JobTopicHandlers {
	onStarted?: (data: JobRunStartedData) => void;
	onProgress?: (data: JobRunProgressData) => void;
	onEnded?: (data: JobRunEndedData) => void;
	/** Server evicted the subscription — admin role revoked, or
	 *  the message bus itself was disabled mid-session. */
	onRevoked?: (params: RtRevokedParams) => void;
}

/**
 * Subscribe to `job:{name}` and dispatch each `rt.event`
 * notification to the matching per-verb handler.
 *
 * `name` accepts the same shapes as `useTopic`'s `topic` — a plain
 * string, a nullable string (null = don't subscribe yet), or a
 * getter that reads from reactive state so the subscription follows
 * the currently-selected job.
 */
export function useJobTopic(
	name: string | null | (() => string | null),
	handlers: JobTopicHandlers
): void {
	const topic = () => {
		const n = typeof name === 'function' ? name() : name;
		return n ? `job:${n}` : null;
	};
	useTopic(topic, (params) => dispatch(params, handlers), handlers.onRevoked);
}

function dispatch(params: RtEventParams, handlers: JobTopicHandlers): void {
	// The generated `RtEventKind` string-enum values match the Rust
	// `#[serde(rename_all = "snake_case")]` variants exactly — see
	// `application/ports/message_bus_ports.rs::MessageBusEvent`.
	switch (params.event) {
		case 'job_run_started':
			handlers.onStarted?.(params.data as JobRunStartedData);
			return;
		case 'job_run_progress':
			handlers.onProgress?.(params.data as JobRunProgressData);
			return;
		case 'job_run_ended':
			handlers.onEnded?.(params.data as JobRunEndedData);
			return;
	}
}
