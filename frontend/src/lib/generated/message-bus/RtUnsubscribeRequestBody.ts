import type RtUnsubscribeParams from './RtUnsubscribeParams';
// AUTO-GENERATED — do not edit by hand.
// Regenerate with `just asyncapi-ts`.
interface RtUnsubscribeRequestBody {
	id: string | null | number | null | null;
	jsonrpc: '2.0';
	method: 'rt.unsubscribe';
	params?: RtUnsubscribeParams;
}
export type { RtUnsubscribeRequestBody as default };
