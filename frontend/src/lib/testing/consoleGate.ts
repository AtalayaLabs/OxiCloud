/**
 * Shared state between the console gate in `vitest-setup.ts` and the
 * `allowConsole()` helper a test file calls.
 *
 * It is a module rather than a global because Vitest gives each test file its
 * own module registry: the setup file and the test file resolve this to the
 * same instance within a file, and to a fresh one for the next file. So an
 * allowance cannot leak across files, and nothing has to reset it.
 */
export const consoleGate = {
	/** Levels this file is permitted to log at. */
	allowed: { warn: false, error: false } as Record<'warn' | 'error', boolean>,
	/** Why, for the record — surfaced if the gate ever needs to explain itself. */
	reasons: [] as string[]
};
