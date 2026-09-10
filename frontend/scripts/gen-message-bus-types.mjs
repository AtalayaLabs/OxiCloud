#!/usr/bin/env node
// Message bus — TypeScript DTOs generated from `resources/gen/asyncapi.json`.
//
// Sits on the same axis as `resources/gen/openapi.json`: the wire spec
// (authored by `cargo run --features dev_tools --bin generate-asyncapi`)
// is the source of truth, and this script projects it into typed FE
// interfaces so `lib/composables/useTopic.ts` and every folder-view
// switch statement is compile-time exhaustive over the `rt.event` variants.
//
// Regenerate: `just asyncapi-ts` (or `npm run gen:message-bus`).
// CI is expected to run the same command and fail if the working tree is
// dirty afterwards — same discipline `just openapi` follows.
//
// Design notes:
//   * `modelType: 'interface'` — plain records, not classes-with-getters.
//     Matches the FE codebase style (see `lib/api/types.ts`).
//   * Output goes to `src/lib/generated/message-bus/` — a directory reserved
//     for auto-generated files. Never hand-edit anything inside.
//   * Every file gets a `AUTO-GENERATED` banner via a preset so a stray
//     edit is obvious at review time.
//   * Modelina auto-detects AsyncAPI 3.0 from the top-level `asyncapi`
//     field. No explicit input-type flag needed.

import { execFile as execFileCb } from 'node:child_process';
import { readFile, readdir, rm, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { promisify } from 'node:util';

import { TypeScriptFileGenerator } from '@asyncapi/modelina';

const execFile = promisify(execFileCb);

// Anchor everything on this script's location so `just asyncapi-ts` from
// the repo root and `npm run gen:message-bus` from the frontend both work.
const __dirname = dirname(fileURLToPath(import.meta.url));
const frontendRoot = resolve(__dirname, '..');
const repoRoot = resolve(frontendRoot, '..');

const specPath = resolve(repoRoot, 'resources/gen/asyncapi.json');
const outputDir = resolve(frontendRoot, 'src/lib/generated/message-bus');

// Load the spec. Failing here means the wire spec hasn't been generated
// yet — hint the operator at the right command.
let spec;
try {
	spec = JSON.parse(await readFile(specPath, 'utf8'));
} catch (err) {
	console.error(
		`gen-message-bus-types: cannot read ${specPath}: ${err.message}\n` +
			`\nDid you run \`just asyncapi\` first? The Rust generator writes\n` +
			`resources/gen/asyncapi.json; this script consumes it.`
	);
	process.exit(1);
}

// Fresh output directory every run — no stale files from a schema that
// was removed since last run. CI dirty-tree check catches drift both
// ways (missing new + leftover old).
await rm(outputDir, { recursive: true, force: true });
await mkdir(outputDir, { recursive: true });

const generator = new TypeScriptFileGenerator({
	// Plain interfaces, no class scaffolding. FE consumers use structural
	// types via `useTopic<...>` and plain object literals.
	modelType: 'interface',
	// Use inline types where possible (nested objects) rather than
	// generating a separate model for every anonymous subschema — keeps
	// the file count tractable.
	rawPropertyNames: true,
	presets: [
		{
			// File-level banner. `class` preset covers both class and
			// interface output in Modelina's TS generator.
			class: {
				self({ content }) {
					const banner =
						'// AUTO-GENERATED — do not edit by hand.\n' +
						'// Regenerate with `just asyncapi-ts` (which runs\n' +
						'// `node frontend/scripts/gen-message-bus-types.mjs`).\n' +
						'// Source of truth: resources/gen/asyncapi.json,\n' +
						'// authored by the Rust `generate-asyncapi` binary.\n';
					return `${banner}${content}`;
				}
			},
			interface: {
				self({ content }) {
					const banner =
						'// AUTO-GENERATED — do not edit by hand.\n' +
						'// Regenerate with `just asyncapi-ts`.\n';
					return `${banner}${content}`;
				}
			}
		}
	]
});

// Modelina auto-detects AsyncAPI 3.0 from the `asyncapi` root field.
// `generateToFiles` writes one file per top-level model and returns the
// list of models. Any generation error propagates up as a rejection.
const models = await generator.generateToFiles(spec, outputDir, {
	moduleSystem: 'ESM'
});

// Post-process for `verbatimModuleSyntax: true` — Modelina 5.x emits
// pre-verbatim shapes (`import X from`, `export default X`) that
// modern strict TS rejects. Two mechanical rewrites make the output
// pass `svelte-check` under the frontend's tsconfig:
//
//   1. `import X from './X';`        → `import type X from './X';`
//   2. `export default X;`           → `export type { X as default };`
//
// Both rewrites are safe because we run Modelina in `modelType:
// 'interface'` mode — every top-level export is a type, and every
// cross-file default import is a type import. If we ever add
// value-emitting output (enums, const objects), tighten this.
const files = await readdir(outputDir);
let rewritten = 0;
for (const f of files) {
	if (!f.endsWith('.ts')) continue;
	const path = resolve(outputDir, f);
	let content = await readFile(path, 'utf8');
	const before = content;
	// Match `import <Ident> from '<relative-path>';` anywhere in the
	// file. Modelina puts these at the top; `^...$` with the `m` flag
	// scopes to whole lines.
	content = content.replace(/^import (\w+) from '(\.\/[\w_]+)';$/gm, "import type $1 from '$2';");
	// Match the trailing `export default <Ident>;`. Turn it into the
	// type-only default-export form the TS spec accepts.
	content = content.replace(/^export default (\w+);$/gm, 'export type { $1 as default };');
	// Modelina-limitation escape hatch: bare `any` → `unknown`.
	//
	// JSON Schema has no way to express "any JSON value" in a way
	// Modelina projects into TypeScript cleanly — a schema of
	// `{"type": ["object", "array", "string", "number", "boolean",
	// "null"]}` (every JSON type) or an untyped `{}` still comes out
	// as `any` in Modelina's default output. The two sites this
	// affects are:
	//
	//   * `RtErrorObject.data` — JSON-RPC 2.0 spec: "A Primitive or
	//     Structured value that contains additional information."
	//   * `RtSuccessResponseBody.result` — the generic base; each
	//     specific method has its own typed result schema.
	//
	// Both are honestly open on the wire; the client checks a
	// discriminator (`code` / `method`) before narrowing.
	//
	// `unknown` is the correct TS type here — strict supertype of
	// `any`, forces the consumer to narrow. Every OTHER wart (`Map`,
	// `additionalProperties`, `AnonymousSchema_N`) MUST be fixed at
	// the AsyncAPI schema level per project convention; this rewrite
	// is the sole exception, gated to a Modelina defect.
	content = content.replace(/\bany\b/g, 'unknown');
	if (content !== before) {
		await writeFile(path, content);
		rewritten++;
	}
}

// Guard against reintroducing anonymous schemas. Modelina falls back
// to `AnonymousSchema_N` for every inline / nested schema in the
// AsyncAPI spec that doesn't have an explicit component name — the
// resulting TS files are unreadable in code review, opaque in imports,
// and don't refactor safely. Every real schema should be hoisted to
// `#/components/schemas/<Name>` in `src/bin/generate-asyncapi.rs` and
// referenced via `$ref` instead of embedded inline.
//
// If this guard trips, look at which inline schema in the AsyncAPI
// spec triggered it — usually a nested `params`, `result`, `error`,
// or an inline `enum` array — and hoist it to a named schema.
const anonymous = files.filter((f) => f.endsWith('.ts') && /^AnonymousSchema_/i.test(f));
if (anonymous.length > 0) {
	console.error(
		`gen-message-bus-types: FAIL — Modelina produced ${anonymous.length} ` +
			`AnonymousSchema_N file(s):`
	);
	for (const f of anonymous) console.error(`  - ${f}`);
	console.error(
		`\nHoist the corresponding inline schema in\n` +
			`  src/bin/generate-asyncapi.rs\n` +
			`to a named entry under \`components.schemas\` and\n` +
			`reference it via \`ref_schema("<Name>")\` instead of\n` +
			`embedding the object inline. Regenerate with\n` +
			`  just asyncapi-ts\n` +
			`and the file count for this run should show 0 AnonymousSchema.\n`
	);
	process.exit(1);
}

// Run the repo's Prettier over the generated output so the committed
// files match the same style as hand-written code — otherwise
// `npm run check`'s `prettier --check` step fails. Uses the local
// binary so config (.prettierrc, plugins) applies. Run via npx to
// stay agnostic of monorepo hoisting.
try {
	await execFile('npx', ['--no-install', 'prettier', '--write', outputDir, '--log-level', 'warn'], {
		cwd: frontendRoot
	});
} catch (err) {
	console.error(
		`gen-message-bus-types: prettier --write failed: ${err.message}\n` +
			`The generated files may still be usable but will fail\n` +
			`\`npm run check\` on the prettier step. Fix prettier setup\n` +
			`(is @prettier installed in frontend/node_modules?) then\n` +
			`re-run \`just asyncapi-ts\`.`
	);
	process.exit(1);
}

console.log(
	`gen-message-bus-types: wrote ${models.length} model(s) to ${outputDir}` +
		` (rewrote ${rewritten} for verbatimModuleSyntax, 0 AnonymousSchema,` +
		` prettier-formatted)`
);
