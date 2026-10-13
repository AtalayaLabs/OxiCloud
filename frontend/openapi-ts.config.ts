import { defineConfig } from '@hey-api/openapi-ts';

export default defineConfig({
	input: '../resources/gen/openapi.json',
	// Each generated-protocol tree lives in its own leaf under
	// `src/lib/api/generated/*/`. openapi-ts defaults to
	// `clean: true` on its output directory, so the AsyncAPI tree
	// (`src/lib/api/generated/asyncapi/`) must stay in a sibling
	// subdir — nesting both at the same level stops the openapi-ts
	// rmSync from wiping the message-bus output.
	output: 'src/lib/api/generated/openapi',
	plugins: [
		'@hey-api/typescript',
		// SDK operations default to:
		//   - throwOnError: true    → non-2xx responses throw an `ApiError`-
		//     ish shape instead of returning `{ error }`, matching the
		//     hand-written `apiJson` ergonomic exactly.
		//   - responseStyle: 'data' → the Promise resolves to the response
		//     body directly, not `{ data, request, response }`. Call sites
		//     write `const cfg = await getConfig()` and get a `ServerConfigDto`.
		// Set globally so every one of the 224 operations inherits the shape;
		// Phase 4 consumers never have to repeat the two keys per call.
		{ name: '@hey-api/sdk', throwOnError: true, responseStyle: 'data' },
		{
			name: '@hey-api/client-fetch',
			runtimeConfigPath: './src/lib/api/hey-api'
		}
	]
});
