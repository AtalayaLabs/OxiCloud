import type RtSubscribeParams from './RtSubscribeParams';
// AUTO-GENERATED — do not edit by hand.
// Regenerate with `just asyncapi-ts`.
interface RtSubscribeRequestBody {
	id: string | null | number | null | null;
	jsonrpc: '2.0';
	method: 'rt.subscribe';
	params?: RtSubscribeParams;
}
export type { RtSubscribeRequestBody as default };
