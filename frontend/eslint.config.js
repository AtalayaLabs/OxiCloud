import js from '@eslint/js';
import svelte from 'eslint-plugin-svelte';
import prettier from 'eslint-config-prettier';
import globals from 'globals';
import ts from 'typescript-eslint';

export default ts.config(
	js.configs.recommended,
	...ts.configs.recommended,
	...svelte.configs['flat/recommended'],
	prettier,
	...svelte.configs['flat/prettier'],
	{
		languageOptions: {
			globals: {
				...globals.browser,
				...globals.node
			}
		},
		rules: {
			// `_`-prefixed args are the codebase's "intentionally unused"
			// convention — mostly Svelte snippet positional params that
			// have to be declared but aren't read (e.g. `dateCell(_item,
			// ctx)`). Match the widely-used JS/TS ecosystem pattern so
			// the intent is respected without per-line disable comments.
			'@typescript-eslint/no-unused-vars': [
				'error',
				{ argsIgnorePattern: '^_', varsIgnorePattern: '^_' }
			]
		}
	},
	{
		// `.svelte` components and `.svelte.ts`/`.svelte.js` rune modules are all
		// parsed by svelte-eslint-parser under eslint-plugin-svelte v3; it needs the
		// TS parser for the embedded/whole-file TypeScript or it chokes on type syntax.
		files: ['**/*.svelte', '**/*.svelte.ts', '**/*.svelte.js'],
		languageOptions: {
			parserOptions: {
				parser: ts.parser
			}
		},
		// TypeScript + svelte-check already resolve identifiers (including `<script
		// generics>` type params, which core `no-undef` can't see). Defer to them.
		rules: {
			'no-undef': 'off'
		}
	},
	{
		// Auto-generated AsyncAPI DTOs (Modelina output). Empty
		// interfaces are legitimate for wire messages whose `data`
		// field is intentionally a no-fields object (pure-poke events
		// like `notification_received`). See
		// `docs/plan/templated-messages.md § Bus event is a pure poke`.
		files: ['src/lib/generated/message-bus/**/*.ts'],
		rules: {
			'@typescript-eslint/no-empty-object-type': 'off'
		}
	},
	{
		// `static/` holds vendored, verbatim assets (the delta-upload worker and
		// the wasm-bindgen hash glue) — lint them as the upstream ships them.
		ignores: ['build/', '.svelte-kit/', 'package/', 'static/', 'bench/']
	}
);
