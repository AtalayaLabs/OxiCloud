import '@testing-library/jest-dom/vitest';

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
