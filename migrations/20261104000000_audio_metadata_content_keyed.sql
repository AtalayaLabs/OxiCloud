-- Re-key audio metadata from the FILE to the CONTENT.
--
-- `audio.file_metadata` was keyed `file_id UUID PRIMARY KEY REFERENCES
-- storage.files(id) ON DELETE CASCADE`. But ID3 / Vorbis tags are a
-- deterministic pure function of the source bytes, which by the keying rule
-- in docs/plan/derived-blobs.md makes them content-addressable — the same
-- class as `storage.blob_extracted_text` and `faces.faces`, both of which
-- chose content-keying for exactly this reason.
--
-- The giveaway that the key was wrong is a function that existed only to
-- compensate for it: `AudioMetadataService::clone_from_source_background`
-- copied a metadata row to every new file that shared a blob. Content-keyed,
-- the row is already there and that whole path deletes.
--
-- What file-keying cost:
--   * N identical rows for N copies of one song, re-inserted on every copy.
--   * Re-extraction after delete + re-upload: ON DELETE CASCADE dropped the
--     row, so identical bytes were streamed to a tempfile and re-parsed even
--     when another file still held the blob.
--
-- ── Lifecycle, and why there is no FK ────────────────────────────────────
--
-- `source_hash` is a content address, not a row reference. A file's hash may
-- resolve to either `storage.chunk_manifests.file_hash` (CDC) or
-- `storage.blobs.hash` (legacy pre-CDC whole-file), so there is no single
-- table to point a foreign key at — the same reason
-- `storage.blob_extracted_text` has none.
--
-- Consequently this row is a DEPENDENT reference: it holds no `ref_count`
-- and keeps nothing alive. It must never be registered as a
-- `BlobReferenceSource` — if it were, deleting every file on a blob would
-- leave the blob pinned alive by its own metadata, forever. Cleanup is by
-- the reap-time purge in `BlobHandler::purge_derived_blobs`, with
-- `satellites_consistency` as the backstop for reap paths that miss.
-- The invariant to hold: metadata lifetime == blob lifetime.
--
-- Deliberately NOT recreated: idx_audio_metadata_{artist,album,genre,year}.
-- They served only `PlaylistPgRepository::list_by_{artist,album,genre}`,
-- which have no callers — there is no browse-by-artist feature — so they
-- were four indexes maintained on every write for reads nobody performs.
-- `year` never had a query at all, even in the dead code.

CREATE TABLE IF NOT EXISTS audio.content_metadata (
    source_hash   VARCHAR(64) PRIMARY KEY,
    title         TEXT,
    artist        TEXT,
    album         TEXT,
    album_artist  TEXT,
    genre         TEXT,
    track_number  INTEGER,
    disc_number   INTEGER,
    year          INTEGER,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    bitrate       INTEGER,
    sample_rate   INTEGER,
    channels      SMALLINT,
    format        TEXT,
    -- ⚠ `extracted_at`, NOT `created_at`, and the name is load-bearing.
    --
    -- `docs/plan/normalize-file-dates.md` reserves `created_at` on a
    -- content-keyed metadata table for a DIFFERENT meaning: the content's
    -- own claimed creation date (EXIF DateTimeOriginal, a Word or PDF
    -- creation date), which `nc:creation_time` will read once the
    -- extractor widens. In a table keyed by content, "created" most
    -- naturally reads as "when the content was created" — so that is the
    -- meaning it is reserved for, and row bookkeeping needs a name of its
    -- own rather than quietly claiming the same word one schema away.
    extracted_at  TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

COMMENT ON TABLE audio.content_metadata IS
    'Audio tags (ID3, Vorbis) keyed by the BLAKE3 of the source content. Content-keyed on purpose: identical bytes share one extraction, so N copies of a song cost one parse and a copy costs nothing. Dependent reference — holds no ref_count and keeps no blob alive; reaped with its source.';

COMMENT ON COLUMN audio.content_metadata.source_hash IS
    'The Blob these tags were parsed from. No FK: a file hash may resolve to chunk_manifests.file_hash (CDC) or blobs.hash (legacy), so there is no single referent. Deleted when the source Blob is reaped.';

COMMENT ON COLUMN audio.content_metadata.extracted_at IS
    'When WE first parsed these tags — row bookkeeping, not a property of the content. Deliberately not named created_at: docs/plan/normalize-file-dates.md reserves that name on content-keyed metadata for the content''s own claimed creation date.';

-- ── Backfill ────────────────────────────────────────────────────────────
--
-- N file-keyed rows collapse to one per distinct content. `DISTINCT ON`
-- needs a deterministic tie-break or the surviving row would be arbitrary
-- and the migration non-reproducible: newest `updated_at` wins, then
-- `file_id` as a total order so even equal timestamps resolve the same way
-- on every run.
--
-- Rows whose file has since vanished cannot be mapped to content at all
-- (the join drops them); they were already unreachable through the readers,
-- which all resolve file → hash.
--
-- ⚠ The timeout lift is not optional. Migrations run on the PRIMARY pool,
-- which sets `statement_timeout = 30s` by default
-- (`OXICLOUD_DB_STATEMENT_TIMEOUT_SECS`, `infrastructure/db.rs`), and this
-- INSERT sorts the whole metadata table — unbounded in corpus size. Exceeding
-- it rolls the migration back and the server never finishes booting, on every
-- restart. That is not hypothetical: `20261017000002_repair_existing_refcount_drift`
-- carries the same lift for the same reason, after it happened to an operator.
-- `SET LOCAL` is transaction-scoped and resets at COMMIT, so later migrations
-- and the running app keep the safety net.
SET LOCAL statement_timeout = 0;

INSERT INTO audio.content_metadata
    (source_hash, title, artist, album, album_artist, genre, track_number,
     disc_number, year, duration_secs, bitrate, sample_rate, channels,
     format, extracted_at, updated_at)
SELECT DISTINCT ON (f.blob_hash)
       f.blob_hash, m.title, m.artist, m.album, m.album_artist, m.genre,
       m.track_number, m.disc_number, m.year, m.duration_secs, m.bitrate,
       m.sample_rate, m.channels, m.format, m.created_at, m.updated_at
  FROM audio.file_metadata m
  JOIN storage.files f ON f.id = m.file_id
 ORDER BY f.blob_hash, m.updated_at DESC, m.file_id
ON CONFLICT (source_hash) DO NOTHING;

-- Report what collapsed, so an operator can see the dedup factor rather
-- than inferring it from a row-count difference after the fact.
--
-- If that number looks wrong, this is RECOVERABLE and the DROP below is
-- not the cliff it appears to be. These tags are a deterministic pure
-- function of the bytes — the table is a cache, not a record — so the
-- whole thing rebuilds from the blobs with:
--
--     POST /api/admin/audio/metadata/reextract
--
-- which walks DISTINCT blob_hash and costs one parse per distinct song.
-- (That endpoint is itself slated for replacement by
-- `content_rescan --rederive=always`; see docs/plan/blob-resniff.md §B.7.)
-- The cost of getting this backfill wrong is therefore an I/O pass, not
-- lost data — which is precisely the property that made content-keying
-- the right call.
DO $$
DECLARE
    old_rows BIGINT;
    new_rows BIGINT;
BEGIN
    SELECT count(*) INTO old_rows FROM audio.file_metadata;
    SELECT count(*) INTO new_rows FROM audio.content_metadata;

    IF old_rows > 0 THEN
        RAISE NOTICE
            'audio metadata re-keyed to content: % file-keyed row(s) collapsed to % content row(s).',
            old_rows, new_rows;
    END IF;
END
$$;

DROP TABLE audio.file_metadata;
