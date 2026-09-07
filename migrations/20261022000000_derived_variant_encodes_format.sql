-- Put the output format inside `variant`, where the plan says new axes go.
--
-- `content_derived_blobs.variant` held the size alone (`icon` | `preview` |
-- `large`), so a size could hold exactly ONE stored artifact regardless of
-- codec. That surfaced when the read order flipped (step 10c): a JPEG request
-- matched the WebP row and would have been served the wrong codec, which the
-- old ordering hid because the `.jpg` sidecar won first. The flip had to be
-- gated to WebP, which in turn means JPEG clients can never leave the sidecar
-- — so the sidecar can never be deleted.
--
-- It blocks transcodes harder still: those are multi-format by nature, so
-- without a format term two output codecs of one source collide on the
-- primary key.
--
-- Per the column's own comment — "new axes go inside this string, never into
-- new columns" — the axis goes in the string rather than into a fourth PK
-- column. The PK stays `(source_hash, kind, variant)`.
--
-- Shape: `{size}.{ext}` — `preview.webp`, `icon.jpg`, and later `720p.webp`
-- for transcodes.
--
-- The backfill is deterministic rather than a guess: `store_derived_blob` has
-- only ever been called with `"image/webp"` for thumbnails, so every existing
-- thumbnail row is WebP. `content_type` is checked anyway rather than assumed
-- — if that assumption is ever wrong, the row is left alone for a human to
-- look at instead of being silently mislabelled.

UPDATE storage.content_derived_blobs
   SET variant = variant || '.webp'
 WHERE kind = 'thumbnail'
   AND content_type = 'image/webp'
   -- Idempotent: skip anything already carrying a format suffix, so a
   -- re-applied migration cannot produce `preview.webp.webp`.
   AND variant NOT LIKE '%.%';

-- Anything left without a format suffix did not match the WebP assumption.
-- Surfaced as a warning rather than coerced: the read path will simply miss
-- those rows and fall back to the sidecar, which is safe, whereas guessing a
-- codec would serve the wrong bytes.
DO $$
DECLARE
    v_unsuffixed INT;
BEGIN
    SELECT COUNT(*) INTO v_unsuffixed
      FROM storage.content_derived_blobs
     WHERE kind = 'thumbnail' AND variant NOT LIKE '%.%';

    IF v_unsuffixed > 0 THEN
        RAISE WARNING
            'derived_variant_encodes_format: % thumbnail row(s) have no format suffix (content_type was not image/webp). They will be ignored by the read path and re-derived on demand; inspect before deleting the sidecars.',
            v_unsuffixed;
    END IF;
END $$;

COMMENT ON COLUMN storage.content_derived_blobs.variant IS
    'Opaque discriminator carrying every axis but the source and the kind: size AND output format, as {size}.{ext} (preview.webp | icon.jpg | 720p.webp). New axes go inside this string, never into new columns. A format term is required — without one, two codecs of the same source collide on the primary key, and the read path cannot tell which codec a row holds.';
