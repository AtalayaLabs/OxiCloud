/**
 * Declare that this suite drives code paths which log by design, so the
 * console gate in `vitest-setup.ts` does not fail its tests.
 *
 * Call it once at the top level of a test file, naming WHICH failure paths
 * the suite exercises:
 *
 *   allowConsole('warn', 'need-ticket timeouts and breaker trips');
 *
 * The gate exists because every `console.*` call crosses the worker's RPC,
 * and one still in flight when the worker closes kills the whole file with
 * `Closing rpc while "onUserConsoleLog" was pending` — green tests, failed
 * run, no indication which file did it. Most such logs turn out to be an
 * incomplete module mock, and those should be FIXED rather than allowed; the
 * mock that lacked `apiJson` in `routes/files/page.test.ts` was warning ten
 * times a run and reconnecting a bus that was never going to connect. This
 * helper is for the other kind: a suite whose subject genuinely warns,
 * because the warning is the behaviour under test.
 *
 * Why a shared flag rather than `vi.spyOn(console, 'warn')`: the app logs
 * through `loglevel`, which captures `console.warn` BY REFERENCE when a
 * logger is created. That happens while the module under test is imported —
 * before any spy is installed — so the spy is simply bypassed and the gate
 * still sees the call. The flag is read by the gate at log time, which is
 * the only place that works for both direct `console.*` calls and
 * loglevel-mediated ones.
 *
 * To assert on a message that IS the contract, assert the behaviour it
 * accompanies (a state transition, a posted message) rather than the log
 * line: the text is a human-facing detail, while the transition is the
 * guarantee.
 */
import { consoleGate } from './consoleGate';

export function allowConsole(level: 'warn' | 'error', why: string): void {
	// Set at import time, not in a hook: Vitest isolates the module registry
	// per test file, so this flag is already file-scoped and needs no reset.
	consoleGate.allowed[level] = true;
	consoleGate.reasons.push(`${level}: ${why}`);
}
