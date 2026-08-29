-- Negative rows in `storage.content_derived_blobs`.
--
-- Some derivations can only be known to be useless by doing the whole
-- expensive job. `ImageTranscodeService` learns that WebP is not smaller
-- than the original by decoding and re-encoding the whole image; a
-- thumbnail renderer learns a source is undecodable, or over the
-- 50-megapixel ceiling, only by attempting it. Recomputing that verdict
-- on every request is the same cost as computing it the first time.
--
-- Today those verdicts live in RAM (moka's zero-weight empty-Bytes
-- convention) and, for transcodes, as zero-byte `.skip` files on local
-- disk. Both vanish: moka evicts, and the local disk is exactly what
-- this plan is deleting. So the verdict is stored here, next to the
-- positive derivations, as a row whose derived Blob is NULL.
--
-- ## Why NULL rather than a sentinel hash
--
-- A reserved hash was considered and rejected. It would stop
-- `blob_hash` naming a real Blob, and every consumer — the refcount
-- recompute in `manifests_consistency`, `dedup_gc`'s reap predicate,
-- `satellites_consistency`'s dangling check — would need to learn the
-- exception or silently mis-handle it. NULL is already the SQL way to
-- say "no Blob", and those consumers all join on `blob_hash`, so a NULL
-- drops out of the join instead of matching something fictional.
--
-- ## The CHECK matters
--
-- A row with a `blob_hash` but no `content_type` is unserveable; a row
-- with a `content_type` but no `blob_hash` claims a type for bytes that
-- do not exist. Both are bugs that would surface far from their cause,
-- so the pair moves together or not at all.
--
-- ## What must NOT become a negative row
--
-- Only failures that are DETERMINISTIC IN THE CONTENT. A transcode that
-- was not smaller, or a source that cannot be decoded, will fail the
-- same way forever — those are worth remembering. A generation timeout,
-- a closed semaphore, an I/O error reading the source Blob are
-- properties of the moment, not the content; persisting one marks a
-- perfectly good image as underivable permanently, and nothing ever
-- retries it. The asymmetry sets the default: a wrongly-cached
-- transient is silent and forever, a missing negative merely costs
-- repeated work. When in doubt, do not write the row.

ALTER TABLE storage.content_derived_blobs
    ALTER COLUMN blob_hash    DROP NOT NULL,
    ALTER COLUMN content_type DROP NOT NULL;

ALTER TABLE storage.content_derived_blobs
    DROP CONSTRAINT IF EXISTS content_derived_blobs_positive_or_negative;

ALTER TABLE storage.content_derived_blobs
    ADD CONSTRAINT content_derived_blobs_positive_or_negative
    CHECK (
        (blob_hash IS NOT NULL AND content_type IS NOT NULL)
     OR (blob_hash IS     NULL AND content_type IS     NULL)
    );

COMMENT ON COLUMN storage.content_derived_blobs.blob_hash IS
    'The derived Blob, or NULL for a NEGATIVE row: the derivation was attempted and is known not to be worth storing (transcode came out larger, source undecodable, source over the decode ceiling). Reference HOLDER when present — bumps chunk_manifests.ref_count via DedupService::add_reference. Only content-deterministic failures may be recorded as negatives; transient ones (timeout, semaphore, I/O) must not, or a momentary failure becomes permanent.';

COMMENT ON COLUMN storage.content_derived_blobs.content_type IS
    'MIME type of the derived Blob. NULL exactly when blob_hash is NULL — the CHECK keeps the pair together, since a type without bytes describes nothing and bytes without a type cannot be served.';
