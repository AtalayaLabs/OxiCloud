import type RtCollabFlushParams from './RtCollabFlushParams';
// AUTO-GENERATED — do not edit by hand.
// Regenerate with `just asyncapi-ts`.
interface RtCollabFlushRequestBody {
	id: string | null | number | null | null;
	jsonrpc: '2.0';
	method: 'rt.collab_flush';
	params?: RtCollabFlushParams;
}
export type { RtCollabFlushRequestBody as default };
