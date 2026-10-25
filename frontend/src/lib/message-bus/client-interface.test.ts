// Conformance check — the per-tab `MessageBusClient` implementation
// must satisfy the extracted interface at compile time. Guards the
// Gate 1 extraction: a method dropped from either side (interface or
// class) breaks the build here, visibly, instead of surfacing as a
// runtime "undefined is not a function" from a Svelte consumer.
//
// Gate 2 adds a second declaration asserting the SharedWorker bridge
// also conforms; when both are pinned, the interface IS the shared
// language and drift is caught at the file that caused it.
//
// There is no Vitest `it(...)` here on purpose. The entire file is
// type-level. The `describe` wrapper gives `npm run test:unit` a
// visible pass per compile, and the `expect(true).toBe(true)` makes
// Vitest happy about an empty block. The actual pin is the two
// `Assert<Equal<...>>` declarations above — if they fail,
// `svelte-check` + `tsc` emit a type error before this file ever
// runs.

import { describe, it, expect } from 'vitest';

import type { MessageBusClient } from './client-interface';
import { PerTabMessageBusClient } from './client.svelte';
import type { Assert, Equal } from './port-protocol';

// `extends` is one-directional. If the class omits a method, `class
// extends interface` is false. If the interface omits a method the
// class defines, `class extends interface` is still true — the extra
// method is silently fine. `Equal<A, B>` is both directions:
//   true  ⇔ A and B are mutually assignable.
// That's the drift signal we want — adding a method to the class but
// not to the interface should fail here too, because Gate 2's
// SharedWorker bridge won't know about it.

// Shape of the interface restricted to the class's methods + runes.
// `instanceof` picks up only own + prototype keys, which is what the
// per-tab class exposes to the outside world.
type PerTabShape = {
	[K in keyof MessageBusClient]: MessageBusClient[K];
};

// Guard 1: per-tab class implements every method on the interface.
// This is what the `implements MessageBusClient` clause on the class
// already asserts; repeating it as a standalone type-level check
// surfaces the specific binding here when the interface moves faster
// than the class.
type _AssertPerTabCovers = Assert<PerTabMessageBusClient extends MessageBusClient ? true : false>;

// Guard 2: there is no public method on the class the interface
// doesn't name. Keeps the interface the single source of truth: if a
// method lands on the class first, this error marks the omission
// before the Gate 2 bridge is left behind.
type PerTabKeys = Exclude<
	keyof PerTabMessageBusClient,
	// Property keys that are legitimately per-tab-only would list here.
	// Today there are none — the interface names every public method.
	never
>;
type InterfaceKeys = keyof MessageBusClient;
type _AssertNoDrift = Assert<Equal<PerTabKeys, InterfaceKeys>>;

// Satisfy TypeScript's "unused type" lint without emitting runtime
// code. These aliases exist only to force `tsc` to evaluate the two
// asserts above; if either turns `never`, the alias declaration
// fails at build time.
export type __ClientInterfaceConformanceCheck = [PerTabShape, _AssertPerTabCovers, _AssertNoDrift];

describe('MessageBusClient conformance check', () => {
	it('compiles iff the per-tab client matches the interface shape', () => {
		// If this file compiles, the two `Assert` declarations above
		// resolved to `true`. The runtime `expect` just gives Vitest a
		// row to report.
		expect(true).toBe(true);
	});
});
