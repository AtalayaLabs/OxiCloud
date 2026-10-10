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
		'@hey-api/sdk',
		{
			name: '@hey-api/client-fetch',
			runtimeConfigPath: './src/lib/api/hey-api'
		}
	]
});
