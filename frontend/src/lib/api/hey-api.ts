/** Runtime configuration shared by generated SDK operations and endpoint adapters. */
import type { CreateClientConfig } from './generated/client.gen';
import { apiTransport } from './transport';

export const generatedApiFetch = (
	input: RequestInfo | URL,
	init?: RequestInit,
	transport: typeof fetch = apiTransport
): Promise<Response> => transport(input, init);

export const createClientConfig: CreateClientConfig = (config) => ({
	...config,
	baseUrl: globalThis.location?.origin ?? 'http://localhost',
	credentials: 'same-origin',
	fetch: generatedApiFetch
});
