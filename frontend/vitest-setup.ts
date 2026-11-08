import '@testing-library/jest-dom/vitest';
import { afterEach, afterAll } from 'vitest';
import { consoleGate } from '$lib/testing/consoleGate';

// ── Unexpected console output fails the test ────────────────────────────────
//
// Every `console.*` call from a test crosses the worker's RPC as an
// `onUserConsoleLog`. If one is still in flight when the worker starts
// closing, the FILE dies with
//
//   EnvironmentTeardownError: Closing rpc while "onUserConsoleLog" was pending
//
// while every test in it reports green. It is an unhandled rejection at run
// level rather than a test failure, so `retry` cannot help: it re-rolls the
// dice. And the odds scale with how many async logs a file emits, which is
// why it only ever appeared under full-suite load and never when the file ran
// alone — the definition of unpredictable.
//
// Every instance found so far traced to the same defect: an incomplete or
// missing module mock, so app code took a failure path and warned about it
// asynchronously. `AppShell.test.ts` emitted fourteen per run because it
// never mocked `useNotifications`, so each test fetched the bell's first page
// and failed in jsdom. That is a test bug worth seeing, not noise worth
// tolerating.
//
// So: `warn` and `error` are captured rather than forwarded (no forward, no
// RPC, no race) and reported as a failure of the test that produced them.
// `log`, `info` and `debug` are left alone — the bench specs print timings
// through them on purpose.
//
// A suite whose subject logs ON PURPOSE — a circuit breaker tripping, an
// upload falling back — declares it once at the top of the file:
//
//   import { allowConsole } from '$lib/testing/allowConsole';
//   allowConsole('warn', 'need-ticket timeouts and breaker trips');
//
// NOT `vi.spyOn(console, 'warn')`, which looks like it should work and does
// not: the app logs through `loglevel`, which captures `console.warn` by
// reference when a logger is created — during the import of the module under
// test, before any spy exists. The spy is bypassed and this wrapper still
// sees the call. Hence a flag the wrapper reads at log time.
//
// `VITEST_CONSOLE=1 npx vitest …` forwards everything to the terminal and
// disables the gate, for when you need the running commentary while
// debugging.
const GATE_OFF = !!process.env.VITEST_CONSOLE;
const captured: string[] = [];

if (!GATE_OFF) {
	for (const level of ['warn', 'error'] as const) {
		const forward = console[level].bind(console);
		console[level] = (...args: unknown[]) => {
			if (consoleGate.allowed[level]) return;
			captured.push(`console.${level}: ${args.map(describe).join(' ')}`);
			// Deliberately NOT forwarded: forwarding is what queues the RPC
			// this gate exists to prevent. The text travels in the failure
			// message instead.
			void forward;
		};
	}
}

/** Readable one-liner for an argument, including Error and DOM exceptions. */
function describe(value: unknown): string {
	if (value instanceof Error) return `${value.name}: ${value.message}`;
	if (typeof value === 'string') return value;
	try {
		return JSON.stringify(value) ?? String(value);
	} catch {
		return String(value);
	}
}

function reportCaptured(scope: string): void {
	if (captured.length === 0) return;
	const lines = captured.splice(0).join('\n  ');
	throw new Error(
		`Unexpected console output during ${scope}:\n  ${lines}\n\n` +
			'This usually means a module mock is missing an export, so app code ' +
			'took a failure path and logged. Complete the mock, or spy on the ' +
			'console method if the warning is the behaviour under test (see ' +
			'vitest-setup.ts). Left alone it also makes the whole file fail at ' +
			'random with `Closing rpc while "onUserConsoleLog" was pending`.'
	);
}

afterEach(() => reportCaptured('this test'));
// A log that lands after the last test's hooks would otherwise escape the
// gate and still be able to race teardown.
afterAll(() => reportCaptured('this file, after the last test'));

// jsdom lacks ResizeObserver / IntersectionObserver, which several list and
// virtualization components (ResourceList, VirtualList, photos grid) construct
// on mount. Provide inert stubs so component tests can render them.
class StubObserver {
	observe(): void {}
	unobserve(): void {}
	disconnect(): void {}
	takeRecords(): [] {
		return [];
	}
}

const g = globalThis as Record<string, unknown>;
if (!g.ResizeObserver) g.ResizeObserver = StubObserver;
if (!g.IntersectionObserver) g.IntersectionObserver = StubObserver;
if (!g.scrollTo) g.scrollTo = () => {};

// Node 24+ ships a native global `localStorage`/`sessionStorage` (Web Storage
// API) that is unusable without a backing file and shadows jsdom's storage in
// bare-global access — so `localStorage` reads as undefined in some test files
// on newer Node. Install a deterministic in-memory implementation so storage
// behaves identically across Node versions and is fresh for every test file.
class MemoryStorage {
	private store = new Map<string, string>();
	get length(): number {
		return this.store.size;
	}
	clear(): void {
		this.store.clear();
	}
	getItem(key: string): string | null {
		return this.store.has(key) ? (this.store.get(key) as string) : null;
	}
	key(index: number): string | null {
		return [...this.store.keys()][index] ?? null;
	}
	removeItem(key: string): void {
		this.store.delete(key);
	}
	setItem(key: string, value: string): void {
		this.store.set(key, String(value));
	}
}
for (const name of ['localStorage', 'sessionStorage']) {
	try {
		Object.defineProperty(globalThis, name, {
			configurable: true,
			writable: true,
			value: new MemoryStorage() as unknown as Storage
		});
	} catch {
		g[name] = new MemoryStorage() as unknown as Storage;
	}
}

// Deterministic default locale for Intl.RelativeTimeFormat. Node's Intl
// reads the macOS system locale via Core Foundation regardless of LANG /
// LC_ALL env vars, so a developer on a French-locale machine sees
// "il y a 2 ans" where `relativeTimeAgo` tests assert /year/. Force
// 'en-US' when no locale is explicitly specified; tests that pass a
// specific locale as the first argument are unaffected.
//
// Scoped to the test harness only — app code still uses the OS default
// in production.
const OriginalRTF = Intl.RelativeTimeFormat;
class PatchedRTF extends OriginalRTF {
	constructor(locale?: Intl.LocalesArgument, options?: Intl.RelativeTimeFormatOptions) {
		super(locale ?? 'en-US', options);
	}
}
Intl.RelativeTimeFormat = PatchedRTF as unknown as typeof Intl.RelativeTimeFormat;

// jsdom has no IndexedDB. `$lib/auth/dpop` uses it as a single-entry
// key/value store for the browser DPoP keypair; without a working
// backing store every test that touches login / fetch prints a
// fail-open `console.debug` on stdout. Rather than pull in
// `fake-indexeddb` for one call-site, provide a minimal in-memory
// shim that covers exactly the API surface `dpop.ts` uses:
//
//   indexedDB.open(name)                        → IDBOpenDBRequest
//     .onupgradeneeded / .onsuccess / .onerror  → callback slots
//     .result → { objectStoreNames.contains,
//                 createObjectStore, transaction, close }
//   store.get(key)  / .put(value, key)  / .delete(key)
//   tx.oncomplete / .onerror
//
// Tests that WANT to exercise DPoP semantics still mock the module
// (see `src/lib/auth/dpop-proof.test.ts`). This shim is for the
// login-path traversals that were noisy without it.
if (!g.indexedDB) {
	type Store = Map<string, unknown>;
	type Db = {
		stores: Map<string, Store>;
		objectStoreNames: { contains: (n: string) => boolean };
		createObjectStore: (n: string) => void;
		transaction: (n: string, mode: 'readonly' | 'readwrite') => FakeTx;
		close: () => void;
	};
	type FakeReq<T> = {
		result: T | undefined;
		error: unknown;
		onsuccess: ((this: unknown, ev: Event) => void) | null;
		onerror: ((this: unknown, ev: Event) => void) | null;
	};
	type FakeTx = {
		objectStore: (n: string) => FakeStore;
		oncomplete: ((this: unknown, ev: Event) => void) | null;
		onerror: ((this: unknown, ev: Event) => void) | null;
		_done: () => void;
	};
	type FakeStore = {
		get: (key: string) => FakeReq<unknown>;
		put: (value: unknown, key: string) => FakeReq<void>;
		delete: (key: string) => FakeReq<void>;
	};

	// Per-database persistence: opening the same name again gives you
	// back your previously-created stores + entries, so the module's
	// "read a value someone else wrote" flow works across
	// open→close→open cycles within one test.
	const databases = new Map<string, Map<string, Store>>();

	function makeStore(map: Store, tx: FakeTx): FakeStore {
		const microDone = () => queueMicrotask(() => tx._done());
		return {
			get(key: string): FakeReq<unknown> {
				const req: FakeReq<unknown> = {
					result: map.get(key),
					error: undefined,
					onsuccess: null,
					onerror: null
				};
				queueMicrotask(() => req.onsuccess?.call(req, new Event('success')));
				microDone();
				return req;
			},
			put(value: unknown, key: string): FakeReq<void> {
				map.set(key, value);
				const req: FakeReq<void> = {
					result: undefined,
					error: undefined,
					onsuccess: null,
					onerror: null
				};
				queueMicrotask(() => req.onsuccess?.call(req, new Event('success')));
				microDone();
				return req;
			},
			delete(key: string): FakeReq<void> {
				map.delete(key);
				const req: FakeReq<void> = {
					result: undefined,
					error: undefined,
					onsuccess: null,
					onerror: null
				};
				queueMicrotask(() => req.onsuccess?.call(req, new Event('success')));
				microDone();
				return req;
			}
		};
	}

	function makeDb(name: string): Db {
		let stores = databases.get(name);
		if (!stores) {
			stores = new Map();
			databases.set(name, stores);
		}
		return {
			stores,
			objectStoreNames: { contains: (n: string) => stores!.has(n) },
			createObjectStore(n: string): void {
				if (!stores!.has(n)) stores!.set(n, new Map());
			},
			transaction(n: string, _mode: 'readonly' | 'readwrite'): FakeTx {
				const store = stores!.get(n);
				if (!store) throw new Error(`fake-idb: store '${n}' not found`);
				const tx: FakeTx = {
					objectStore: () => makeStore(store, tx),
					oncomplete: null,
					onerror: null,
					_done: () => tx.oncomplete?.call(tx, new Event('complete'))
				};
				return tx;
			},
			close(): void {
				/* no-op — databases map keeps state across close */
			}
		};
	}

	g.indexedDB = {
		open(name: string) {
			const req: FakeReq<Db> & {
				onupgradeneeded: ((this: unknown, ev: Event) => void) | null;
			} = {
				result: undefined,
				error: undefined,
				onsuccess: null,
				onerror: null,
				onupgradeneeded: null
			};
			queueMicrotask(() => {
				const db = makeDb(name);
				req.result = db;
				// Fire upgradeneeded on FIRST open per database, so the
				// module can call createObjectStore('keypair') exactly
				// like the real API expects.
				const stores = databases.get(name)!;
				if (stores.size === 0) req.onupgradeneeded?.call(req, new Event('upgradeneeded'));
				req.onsuccess?.call(req, new Event('success'));
			});
			return req;
		}
	};
}
