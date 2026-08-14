/**
 * OxiCloud — delta-upload worker ("upload only what changed").
 *
 * Runs the whole client side of the delta protocol off the main thread:
 *
 *   read 8 MiB slices ─► FastCDC chunk + BLAKE3 (WASM, same crate and
 *   parameters as the server) ─► negotiate hash batches ─► upload only
 *   the missing chunks (framed, bounded concurrency) ─► commit.
 *
 * The stages OVERLAP: negotiation of batch N and uploads of its missing
 * chunks run while batch N+1 is still being hashed, so wall-clock time
 * approaches max(hash time, upload time) instead of their sum. RAM stays
 * flat: chunk bytes are re-sliced from the File at upload time, never
 * hoarded.
 *
 * Protocol with the spawner:
 *   in  : { file: File, folderId: string, name: string, csrfToken: string }
 *   out : { type: 'progress', hashedBytes, reusedBytes, uploadedBytes, totalBytes }
 *         { type: 'done', status, body }      — conclusive HTTP outcome
 *         { type: 'fallback', reason }        — do a plain byte upload
 */

// Absolute URLs on purpose: vendors/workers are served verbatim in both
// dev and the static build (served verbatim from /static).
const WASM_GLUE_URL = '/vendors/hash-wasm/oxicloud_hash_wasm.js';

/** File read granularity — large enough to amortize Blob→ArrayBuffer. */
const SLICE_BYTES = 8 * 1024 * 1024;
/** Negotiate after this many freshly hashed chunks (~64 MiB of content). */
const NEGOTIATE_BATCH = 256;
/** Default target size of a PUT body (grouping multiple chunk frames).
 *  The orchestrator may override this per-upload via the init message's
 *  `uploadBatchBytes` field, sourced from `window.oxi.UPLOAD_BATCH_BYTES`.
 *  Lowering it (say to 1 MiB) helps clients behind proxies with tight
 *  per-request timeouts (Cloudflare Tunnel: 100 s absolute) at the cost
 *  of more requests per file. */
const UPLOAD_BATCH_BYTES_DEFAULT = 8 * 1024 * 1024;
/** Reclaim consumed queue slots periodically. A head cursor makes dequeue O(1);
 *  compaction bounds the backing array when hashing stays ahead of the network. */
const UPLOAD_QUEUE_COMPACT_AT = 4096;
/** Concurrent chunk-PUT requests. Kept at 1: several folder files upload through
 *  their own workers at once, and the browser only grants ~6 connections per
 *  host. Combined with serialized negotiate (below) each worker holds at most
 *  ~2 connections (one negotiate + one chunk PUT), so a couple of concurrent
 *  large files can't starve the plain uploads of the small ones. */
const UPLOAD_CONCURRENCY = 1;
/** Re-commit attempts when the server answers 409 still_missing. */
const COMMIT_RETRIES = 2;

/**
 * Typed view of the dedicated-worker global scope (jsconfig targets the
 * DOM lib, where `self` is a Window — cast to what this worker uses).
 * @type {{ onmessage: ((event: MessageEvent) => void) | null,
 *          postMessage: (message: unknown) => void }}
 */
const workerScope = /** @type {any} */ (self);

/**
 * One chunk occurrence, in file order.
 * @typedef {{ h: string, s: number, offset: number }} WorkerChunk
 */

/** @returns {Promise<any>} the initialized WASM module */
async function loadWasm() {
    const mod = await import(WASM_GLUE_URL);
    await mod.default();
    return mod;
}

workerScope.onmessage = async (event) => {
    const { file, folderId, name, csrfToken, uploadBatchBytes } = /** @type {{ file: File, folderId: string, name: string, csrfToken: string, uploadBatchBytes?: number }} */ (event.data);
    // Per-upload override sourced from `window.oxi.UPLOAD_BATCH_BYTES`
    // in the main thread. Falls back to the module default (8 MiB).
    const uploadBatchBytesEff =
        typeof uploadBatchBytes === 'number' && uploadBatchBytes > 0
            ? uploadBatchBytes
            : UPLOAD_BATCH_BYTES_DEFAULT;

    /**
     * Forward a log line to the main-thread orchestrator, which routes it
     * through the shared `loglevel` logger (namespace `oxi:upload`). Worker
     * can't `import 'loglevel'` — it's served from /static and isn't
     * bundler-resolved — so postMessage is the transport.
     *
     * @param {'debug'|'info'|'warn'|'error'} level
     * @param {string} msg
     * @param {Record<string, unknown>=} extra
     */
    const log = (level, msg, extra) =>
        workerScope.postMessage({ type: 'log', level, msg, extra });

    /**
     * Wrap a long-running fetch so the main-thread stall watchdog stays
     * fresh while the worker is legitimately awaiting the network. The
     * watchdog resets on every worker → main-thread message; heartbeats
     * every 5 s keep it from firing during a slow commit or chunk PUT.
     * Cleanup in `finally` runs regardless of resolve / reject.
     *
     * @template T
     * @param {() => Promise<T>} fn
     * @returns {Promise<T>}
     */
    const withHeartbeat = async (fn) => {
        const hb = setInterval(() => workerScope.postMessage({ type: 'heartbeat' }), 5000);
        try {
            return await fn();
        } finally {
            clearInterval(hb);
        }
    };

    /** @param {string} reason */
    const fallback = (reason) => {
        log('warn', `worker fallback: ${reason}`);
        workerScope.postMessage({ type: 'fallback', reason });
    };

    log('info', `worker start`, { file: name, size: file.size });

    /** @type {Record<string, string>} */
    const mutHeaders = { 'Content-Type': 'application/json' };
    if (csrfToken) mutHeaders['X-CSRF-Token'] = csrfToken;

    let wasm;
    try {
        wasm = await loadWasm();
        log('debug', 'wasm loaded');
    } catch (err) {
        fallback(`wasm unavailable: ${err instanceof Error ? err.message : String(err)}`);
        return;
    }

    // ── Shared pipeline state ─────────────────────────────────────
    /** @type {WorkerChunk[]} */
    const chunks = []; // every occurrence, in file order
    /** @type {Set<string>} */
    const seenForNegotiate = new Set(); // distinct hashes already sent to negotiate
    let reusedBytes = 0;
    let uploadedBytes = 0;
    let hashedBytes = 0;
    let failed = /** @type {string | null} */ (null);

    let lastProgress = 0;
    const progress = (force = false) => {
        const now = Date.now();
        if (!force && now - lastProgress < 150) return;
        lastProgress = now;
        workerScope.postMessage({
            type: 'progress',
            hashedBytes,
            reusedBytes,
            uploadedBytes,
            totalBytes: file.size
        });
    };

    // ── Upload stage: bounded-concurrency drain of uploadByHash ──
    /** @type {(WorkerChunk | undefined)[]} */
    const uploadQueue = [];
    let uploadHead = 0;
    /** @type {Promise<void>[]} */
    const uploadWorkers = [];
    let uploadsClosed = false;
    /** @type {(() => void) | null} */
    let wakeUploader = null;
    const signalUploaders = () => {
        if (wakeUploader) {
            const w = wakeUploader;
            wakeUploader = null;
            w();
        }
    };

    /** Encode a batch of chunks as [u32 BE len][bytes] frames. */
    const encodeFrames = async (/** @type {WorkerChunk[]} */ batch) => {
        const total = batch.reduce((n, c) => n + 4 + c.s, 0);
        const wire = new Uint8Array(total);
        const view = new DataView(wire.buffer);
        let at = 0;
        for (const c of batch) {
            // eslint-disable-next-line no-await-in-loop -- sequential by design: constant RAM
            const bytes = new Uint8Array(await file.slice(c.offset, c.offset + c.s).arrayBuffer());
            view.setUint32(at, c.s, false);
            wire.set(bytes, at + 4);
            at += 4 + c.s;
        }
        return wire;
    };

    const uploadLoop = async () => {
        while (!failed) {
            // Take up to the effective per-PUT byte cap from the queue.
            /** @type {WorkerChunk[]} */
            const batch = [];
            let bytes = 0;
            while (uploadHead < uploadQueue.length && bytes < uploadBatchBytesEff) {
                const c = /** @type {WorkerChunk} */ (uploadQueue[uploadHead]);
                uploadQueue[uploadHead] = undefined;
                uploadHead++;
                batch.push(c);
                bytes += c.s;
            }
            if (uploadHead === uploadQueue.length) {
                uploadQueue.length = 0;
                uploadHead = 0;
            } else if (
                uploadHead >= UPLOAD_QUEUE_COMPACT_AT &&
                uploadHead * 2 >= uploadQueue.length
            ) {
                uploadQueue.copyWithin(0, uploadHead);
                uploadQueue.length -= uploadHead;
                uploadHead = 0;
            }
            if (batch.length === 0) {
                if (uploadsClosed) return;
                // eslint-disable-next-line no-await-in-loop -- queue wait
                await new Promise((resolve) => {
                    wakeUploader = /** @type {() => void} */ (resolve);
                });
                continue;
            }
            try {
                // eslint-disable-next-line no-await-in-loop -- bounded by pool size
                const wire = await encodeFrames(batch);
                log('debug', `chunk PUT: ${batch.length} chunks, ${wire.length} bytes`);
                // eslint-disable-next-line no-await-in-loop -- bounded by pool size
                const response = await withHeartbeat(() =>
                    fetch('/api/files/delta/chunks', {
                        method: 'PUT',
                        headers: {
                            'Content-Type': 'application/octet-stream',
                            ...(csrfToken ? { 'X-CSRF-Token': csrfToken } : {})
                        },
                        body: wire
                    })
                );
                if (!response.ok) {
                    failed = `chunk PUT failed (HTTP ${response.status})`;
                    log('error', failed);
                    return;
                }
                for (const c of batch) uploadedBytes += c.s;
                progress();
            } catch (err) {
                failed = `chunk PUT failed: ${err instanceof Error ? err.message : String(err)}`;
                log('error', failed);
                return;
            }
        }
    };
    for (let i = 0; i < UPLOAD_CONCURRENCY; i++) uploadWorkers.push(uploadLoop());

    // ── Negotiate stage ───────────────────────────────────────────
    // Serialized: each negotiate awaits the previous one, so at most a single
    // negotiate request is ever in flight per worker. Together with the single
    // chunk-PUT lane (UPLOAD_CONCURRENCY = 1) this caps the worker at ~2
    // concurrent connections, leaving room under the browser's ~6-per-host
    // budget for the other folder files uploading in parallel.
    /** @type {Promise<void>[]} */
    const negotiations = [];
    let negotiateTail = Promise.resolve();
    const negotiate = (/** @type {WorkerChunk[]} */ fresh) => {
        if (fresh.length === 0 || failed) return;
        const run = negotiateTail.then(async () => {
            if (failed) return;
            try {
                const response = await withHeartbeat(() =>
                    fetch('/api/files/delta/negotiate', {
                        method: 'POST',
                        headers: mutHeaders,
                        body: JSON.stringify({ chunks: fresh.map(({ h, s }) => ({ h, s })) })
                    })
                );
                if (!response.ok) {
                    failed = failed || `negotiate failed (HTTP ${response.status})`;
                    log('error', `negotiate failed (HTTP ${response.status})`);
                    return;
                }
                const missing = new Set(/** @type {{missing: string[]}} */ (await response.json()).missing);
                for (const c of fresh) {
                    if (missing.has(c.h)) {
                        uploadQueue.push(c);
                    } else {
                        reusedBytes += c.s;
                    }
                }
                log(
                    'info',
                    `negotiate: ${fresh.length} hashes → ${missing.size} missing, ${fresh.length - missing.size} dedup'd`
                );
                signalUploaders();
                progress();
            } catch (err) {
                failed = failed || `negotiate failed: ${err instanceof Error ? err.message : String(err)}`;
                log('error', `negotiate failed: ${err instanceof Error ? err.message : String(err)}`);
            }
        });
        negotiateTail = run.catch(() => {});
        negotiations.push(run);
    };

    // ── Chunking stage (drives the other two) ────────────────────
    try {
        const chunker = new wasm.DeltaChunker();
        /** @type {WorkerChunk[]} */
        let freshBatch = [];
        let offset = 0;

        /** @param {[string, number][]} emitted */
        const onChunks = (emitted) => {
            for (const [h, s] of emitted) {
                /** @type {WorkerChunk} */
                const chunk = { h, s, offset };
                offset += s;
                chunks.push(chunk);
                if (seenForNegotiate.has(h)) {
                    // Repeated content inside the same file: the first
                    // occurrence decides upload vs reuse; later ones are
                    // pure reuse for accounting.
                    reusedBytes += s;
                } else {
                    seenForNegotiate.add(h);
                    freshBatch.push(chunk);
                    if (freshBatch.length >= NEGOTIATE_BATCH) {
                        negotiate(freshBatch);
                        freshBatch = [];
                    }
                }
            }
        };

        for (let read = 0; read < file.size && !failed; read += SLICE_BYTES) {
            const end = Math.min(read + SLICE_BYTES, file.size);
            // eslint-disable-next-line no-await-in-loop -- sequential by design: constant RAM
            const slice = new Uint8Array(await file.slice(read, end).arrayBuffer());
            onChunks(JSON.parse(chunker.update(slice)));
            hashedBytes = end;
            progress();
        }
        const fin = JSON.parse(chunker.finish());
        chunker.free();
        onChunks(fin.chunks);
        negotiate(freshBatch);
        const fileHash = /** @type {string} */ (fin.file_hash);
        hashedBytes = file.size;
        progress(true);
        // Log the whole-file BLAKE3 immediately so it's visible in the
        // trace regardless of whether the commit succeeds. Correlates
        // the client-side view with the server's `file_blobs.hash`.
        log('info', `hashed — blake3=${fileHash} (${chunks.length} chunks)`);

        // ── Drain: negotiations → uploads → commit ───────────────
        await Promise.all(negotiations);
        uploadsClosed = true;
        signalUploaders();
        await Promise.all(uploadWorkers);
        if (failed) {
            fallback(failed);
            return;
        }

        const commitBody = {
            file_hash: fileHash,
            chunks: chunks.map(({ h, s }) => ({ h, s })),
            name,
            folder_id: folderId
        };
        for (let attempt = 0; ; attempt++) {
            // eslint-disable-next-line no-await-in-loop -- retry loop
            const response = await withHeartbeat(() =>
                fetch('/api/files/delta/commit', {
                    method: 'POST',
                    headers: mutHeaders,
                    body: JSON.stringify(commitBody)
                })
            );
            /** @type {any} */
            let body = null;
            try {
                // eslint-disable-next-line no-await-in-loop -- retry loop
                body = await response.json();
            } catch (_) {}

            const stillMissing = response.status === 409 && Array.isArray(body?.still_missing);
            if (stillMissing && attempt < COMMIT_RETRIES) {
                // GC race or a chunk we wrongly assumed claimable: upload
                // exactly what the server names and try again.
                const byHash = new Map(chunks.map((c) => [c.h, c]));
                /** @type {WorkerChunk[]} */
                const retry = [];
                for (const h of body.still_missing) {
                    const c = byHash.get(h);
                    if (!c) {
                        fallback('server requested an unknown chunk');
                        return;
                    }
                    retry.push(c);
                }
                const wire = await encodeFrames(retry);
                // eslint-disable-next-line no-await-in-loop -- retry loop
                const put = await withHeartbeat(() =>
                    fetch('/api/files/delta/chunks', {
                        method: 'PUT',
                        headers: {
                            'Content-Type': 'application/octet-stream',
                            ...(csrfToken ? { 'X-CSRF-Token': csrfToken } : {})
                        },
                        body: wire
                    })
                );
                if (!put.ok) {
                    fallback(`retry chunk PUT failed (HTTP ${put.status})`);
                    return;
                }
                for (const c of retry) uploadedBytes += c.s;
                progress(true);
                continue;
            }

            // Conclusive: 201 created, or a real error (quota, name
            // conflict, validation). The spawner maps it to the uploaders'
            // UploadAnswer contract.
            const ok = response.status >= 200 && response.status < 300;
            if (ok) {
                // Human-friendly outcome line — the raw commit line below
                // still carries the byte counts for anyone who wants them.
                if (uploadedBytes === 0 && reusedBytes > 0) {
                    log('info', `✅ file already on server — 100% dedup, no bytes transferred (${reusedBytes.toLocaleString()} B reused, blake3=${fileHash})`);
                } else if (reusedBytes > 0) {
                    const pct = Math.round((100 * reusedBytes) / file.size);
                    log('info', `✅ committed — uploaded ${uploadedBytes.toLocaleString()} B, reused ${reusedBytes.toLocaleString()} B (${pct}% dedup, blake3=${fileHash})`);
                } else {
                    log('info', `✅ committed — uploaded ${uploadedBytes.toLocaleString()} B (no dedup, blake3=${fileHash})`);
                }
            }
            log(ok ? 'info' : 'warn', `commit HTTP ${response.status}`, {
                blake3: fileHash,
                uploadedBytes,
                reusedBytes,
                totalBytes: file.size,
                attempt,
            });
            // Include the final counters + file hash on the done envelope
            // so the orchestrator's summary is accurate even when the last
            // throttled progress() got skipped (fast dedup-heavy paths
            // complete under 150ms — progress' throttle window — so
            // reusedBytes never surfaced via a progress message).
            workerScope.postMessage({
                type: 'done',
                status: response.status,
                body,
                reusedBytes,
                uploadedBytes,
                fileHash,
            });
            return;
        }
    } catch (err) {
        fallback(err instanceof Error ? err.message : String(err));
    }
};
