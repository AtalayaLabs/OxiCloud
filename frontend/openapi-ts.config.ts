import { defineConfig } from '@hey-api/openapi-ts';

export default defineConfig({
	input: '../resources/gen/openapi.json',
	output: 'src/lib/api/generated',
	plugins: [
		'@hey-api/typescript',
		'@hey-api/sdk',
		{
			name: '@hey-api/client-fetch',
			runtimeConfigPath: './src/lib/api/hey-api'
		}
	]
});
