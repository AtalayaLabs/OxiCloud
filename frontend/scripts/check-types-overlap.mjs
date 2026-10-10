#!/usr/bin/env node
/**
 * Lint-level guard for hand-written DTO drift against the generated
 * OpenAPI TS SDK. §Phase 1 of the generated-types normalisation.
 *
 * `src/lib/api/types.ts` holds hand-written DTO shapes that predate the
 * `@hey-api/openapi-ts` pipeline iltumio introduced. The SDK now
 * publishes authoritative types under `src/lib/api/generated/openapi/
 * types.gen.ts`, so any hand-written type that shares a name with a
 * generated one is drift waiting to happen: the server-side DTO can
 * change, the generator picks it up, but the hand-written sibling in
 * `types.ts` stays stale and consumers importing from
 * `$lib/api/types` keep seeing the old shape.
 *
 * This script fails with a non-zero exit the moment such a collision
 * exists. It is exposed as `npm run check:types-overlap` today —
 * opt-in diagnostic only. §Phase 2 of the normalisation plan will
 * clear the 9 current collisions, then this script gets promoted
 * into the default `npm run check` so new drift fails CI on sight.
 *
 * The check does NOT compare shapes — collision by name alone is
 * enough signal that one of two things must happen:
 *
 *   1. The hand-written type is a stale duplicate — delete it and
 *      re-export (or let consumers import) the generated one.
 *   2. The hand-written type is an intentional FE-shaped projection
 *      — rename it (e.g. `FileItem` → `FileView`) so the collision
 *      is explicit and future authors don't accidentally reach for
 *      the hand-written version when they wanted the SDK one.
 *
 * Rename-pattern drift (`FileItem` ↔ `FileDto`) is NOT caught here
 * — that's §Phase 3 of the normalisation plan. Phase 1 covers the
 * easy half: exact-name overlap.
 *
 * The regex grammar below is intentionally narrow (`^export (type|
 * interface) <Name>`). It would miss re-exports (`export { X }`) and
 * namespace exports — none of which appear in either file today —
 * so adding a full TypeScript AST pass would be noise. If a future
 * refactor introduces either, swap the regex for a `ts.createSource-
 * File` walk; the fail-shape stays identical.
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

const __dirname = dirname(fileURLToPath(import.meta.url));
const frontendRoot = resolve(__dirname, '..');

const HANDWRITTEN = resolve(frontendRoot, 'src/lib/api/types.ts');
const GENERATED = resolve(frontendRoot, 'src/lib/api/generated/openapi/types.gen.ts');

/** Extract names from `^export (type|interface) <Name>` declarations. */
function exportedTypeNames(path) {
	const src = readFileSync(path, 'utf8');
	const names = new Set();
	const re = /^export (?:type|interface) ([A-Za-z_$][\w$]*)/gm;
	let m;
	while ((m = re.exec(src)) !== null) names.add(m[1]);
	return names;
}

const handwritten = exportedTypeNames(HANDWRITTEN);
const generated = exportedTypeNames(GENERATED);

const colliding = [...handwritten].filter((n) => generated.has(n)).sort();

if (colliding.length === 0) {
	// Quiet success — `npm run check` already prints a lot; one more
	// green line per subcheck adds noise more than signal.
	process.exit(0);
}

console.error('');
console.error(
	`✗ ${colliding.length} hand-written type(s) in src/lib/api/types.ts collide ` +
		`with names already exported by the generated SDK at ` +
		`src/lib/api/generated/openapi/types.gen.ts:`
);
console.error('');
for (const name of colliding) console.error(`    - ${name}`);
console.error('');
console.error('Each collision is drift waiting to happen: a server-side DTO change updates');
console.error('the generated version while the hand-written sibling stays stale, and');
console.error('TypeScript resolves `$lib/api/types` imports to the stale one. Fix each one:');
console.error('');
console.error('  - If the hand-written type is a stale duplicate — delete it and let');
console.error('    consumers import from `$lib/api/generated/openapi/types.gen` (or add a');
console.error('    re-export in types.ts as a one-release transition shim).');
console.error('  - If the hand-written type is an intentional FE-shaped projection —');
console.error('    rename it so the collision is explicit (e.g. `FileItem` → `FileView`).');
console.error('');
console.error('See the §Phase 2 normalisation plan or the top-of-file comment on this script.');
process.exit(1);
