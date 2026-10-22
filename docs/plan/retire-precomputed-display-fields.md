# Retire the pre-computed display fields from the wire

Status: **steps 1–2 shipped**, 3–6 open. Written 2026-10-22 from Ed's
question: are `icon_class`, `icon_special_class` and `size_formatted` actually
used in the UI, and can the FE compute them instead?

| Step | What | Status |
|---|---|---|
| 1 | i18n fix — `ResourceList` type-cell through `typeLabel` | ✅ shipped |
| 2 | Drop `icon_special_class` (6 DTOs, zero consumers) | ✅ shipped |
| 3 | FE derives the icon from `mime_type` + extension | open |
| 4 | Drop `icon_class` — **blocked on 3** | open |
| 5 | `PhotoLightbox` → `formatBytes`, drop `size_formatted` | open |
| 6 | Document `category` as a machine key | open |

The `ResourceCategory` enum section below is unstarted, and deliberately so —
see **Sequencing**.

Answer: yes to the second. Audited every consumer in `frontend/src`
(excluding `/generated/` and test fixtures). The fields are not merely
replaceable — two of the four are already round-trips the FE undoes, one is
dead, and the audit surfaced a live i18n bug.

## Audit

| Field | Real consumers | Verdict |
|---|---|---|
| `icon_special_class` | **0** | Dead on the wire |
| `icon_class` | 2 | Round-trip — both discard the value |
| `size_formatted` | 1 | FE already has `formatBytes()` |
| `category` | 6 | **Keep the data**, but see the i18n bug |

### `icon_special_class` — dead

Its only non-test reference is `lib/utils/media.ts:68`, which sets it to `''`
when synthesising a `FileItem` from a photo DTO. That is a *producer* filling a
required field, not a consumer. Nothing reads it.

**And it was dead at the root, not just unread.** The strings it emitted
("pdf-icon", "code-icon js-icon") named CSS rules in `static/css/style.css`,
and that file was deleted wholesale with the legacy vanilla frontend in
`54639d46` (2026-06-21). The classes are defined nowhere in the repo — not in
`frontend/src`, not in any remaining stylesheet. So for four months the server
ran a ~120-line decision tree per row and serialised a class name for a
stylesheet that no longer existed. Nothing could have flagged it: the producer
compiled, the field was required, and the only evidence was a stylesheet's
absence.

### `icon_class` — a round-trip the FE immediately undoes

Both consumers (`ResourceList.svelte:526` via `iconClassOf`,
`AppShell.svelte:881`) pass the value straight into
`utils/display.ts::iconNameFromClass`, which **parses the Font Awesome token
back out**: the server sends `"fas fa-file-pdf"`, the FE extracts `"file-pdf"`
and looks that up in its icon registry. So the API serialises a CSS class string
that is never used as a CSS class.

The FE already owns the next hop — `fileIconKind(iconName)` maps names to
`--file-kind-*` colour buckets — so half the table is client-side already.

### `size_formatted` — duplicated by construction

One consumer, `PhotoLightbox.svelte:95`. `utils/format.ts::formatBytes()`
already exists, and `display_helpers::format_file_size`'s own doc says it
matches the JavaScript output *exactly*. Two implementations kept in step by
hand, while `size` is already on the wire. The parity comment is the admission.

### `category` — keep the data, and fix the bug it exposed

Genuinely consumed, six sites: the `type-cell` in `ResourceList.svelte:1247`,
`bucketOf` on four routes, and `typeLabel(item.category)` in `/files`.

**The bug.** `utils/resourceBuckets.ts::typeLabel` maps the server's English
category to a *localised* label (`Image` → `t('category.images')`, …), so the FE
already owns a complete category→label table with i18n keys. But
`ResourceList.svelte:1247` renders `{item.category ?? ''}` **raw**. Only
`/files` routes through `typeLabel`. So the "Type" column shows untranslated
English — "Image", "Spreadsheet" — on `/trash`, `/favorites`, `/recent` and
`/shared-with-me` for every non-English locale, while `/files` shows it
translated.

That is a user-visible defect independent of this plan and worth fixing first,
in one line, with its own regression pin.

It also settles what `category` is *for*: a stable machine key (bucket id +
`typeLabel` input), never display text. The fix is to route the type-cell
through `typeLabel` too, which makes the English value purely internal.

## What the FE needs in exchange

`mime_type` is already on every relevant DTO (`file`, `folder`, `favorites`,
`recent`, `trash`, `search`), and `name` carries the extension — which is
exactly the input `display_helpers::classify_display(name, mime)` takes. So no
new field is required; the classification moves, it does not grow.

**The hard constraint: do not create a third copy of the mapping.** It exists
today in `display_helpers.rs` (Rust) and partially in `utils/display.ts` +
`utils/textyExt.ts` (TS — whose comment says it mirrors the Rust classifier's
code/script/prose extension families; step 2 repointed that comment from
`icon_special_class_with_ext` to `icon_class_with_ext`, which carries the same
families on coarser glyphs). The server cannot simply delete its copy:
`category_order_for` depends on `category_for` for the `type` sort axis, which
is server-side by necessity (see `docs/architecture/resource-listing.md`).

So the end state is deliberately asymmetric, and that asymmetry must be
documented or it will read as an accident:

- **Server keeps** `category_for` / `category_order_for` — needed for
  `storage.files.category_order`, the indexed `type` sort key.
- **Server stops emitting** the three presentational strings.
- **FE owns** glyph + colour + byte formatting, derived from `mime_type` and
  `name`.
- **`category` stays on the wire** as the machine key, because the sort and the
  bucketing must agree with the server's grouping. Deriving it client-side would
  let the FE's buckets disagree with the server's `ORDER BY type`.

## `ResourceCategory` — type the machine key (added 2026-10-22)

Decided with Ed while starting this plan: **two enums, the search filter being
the aggregated view of the other.** Not one — collapsing them makes something
worse in both directions.

| | values | drives | typed today |
|---|---|---|---|
| **fine** `ResourceCategory` | Image, Video, Audio, Pdf, Document, Spreadsheet, Presentation, Archive, Code, Markdown, Text, Installer, Folder, **Unknown** | the "Type" column, group-by buckets, the `category` wire field, `category_order_for` | ❌ `&'static str` → `Arc<str>` → TS `string` |
| **coarse** `SearchTypeFilter` | all, folder, image, video, audio, document, archive, code, unknown | the search filter picker | ✅ already an enum |

One enum cannot serve both: the coarse `document` deliberately spans Pdf +
Document + Spreadsheet + Presentation + Markdown + Text, so merging would either
coarsen the Type column (users lose "Spreadsheet" vs "PDF") or inflate the
picker to fourteen options.

What IS worth unifying is the *relationship*. Today the coarse→fine mapping is
expressed in **integers** — `category_orders_for_filter` returns
`[400,500,600,700,1000,1100]` — a second encoding of a mapping that already
exists in `category_order_for`. With a typed fine category the filter maps
coarse → fine **categories**, and `category_order_for` becomes the only place
those integers appear.

### The defect this exposes — unknown files are labelled "Document"

Found while sizing the work, and the reason the fine set needs an explicit
`Unknown`. Both catch-alls in `category_with_ext` return `"Document"`: the
unknown-extension arm and the no-extension arm. Measured:

```
blob.xyzzy  application/octet-stream  ->  Document / 500
noext                                 ->  Document / 500
a.qqq                                 ->  Document / 500
```

Three things are wrong at once:

1. The Type column calls an unidentifiable blob a **"Document"**.
2. `category_order_for`'s `_ => 9999` arm, commented *"Other / unknown"*, is
   **unreachable** — it keys off `category_for`, which never returns anything
   outside the twelve named values.
3. `9999` therefore only ever arrives from the column DEFAULT, which is why the
   live instance showed zero rows at 9999 — and why `type=unknown` in search
   finds no unknown files, only rows nobody wrote a category for.

Typing the enum forces this into the open: a `ResourceCategory::Unknown` variant
makes the fallback name itself instead of borrowing `Document`.

**On the name.** `Unknown` over `Other`, Ed's call: it is less friendly as a
column label but it is the truth, where "Document" is a confident lie. The
coarse filter's shipped `other` value is renamed to `unknown` in the same pass
so one bucket has one name — safe because that value exists only on an
unmerged branch.

### Target: mime is the source of truth, and the extension fallback dies

Ed's direction, and it reshapes the end state: **once mime is normalised, the
category derives from mime alone.** `category_for(name, mime)` consults both
today only because a stored mime could be generic — `application/octet-stream`
with the real type recoverable only from the filename. Content sniffing on the
upload path (`refine_content_type`) already removed that for new rows, and
`content_rescan` removes it for old ones.

So the signature target is `category_for(mime)`, with the extension arms of
`category_with_ext` deleted rather than kept forever. Two things unlock when
that lands, and both are things I recorded as permanent constraints elsewhere:

1. **The `content_rescan` fan-out can stay per-hash.** `blob-resniff.md` §B says
   `category_order` must be computed **per row** because the classifier takes
   `(name, mime)` and so is not computable from the content hash alone. That is
   TRANSITIONAL, not permanent — a mime-only classifier is one value per
   content, so the fan-out reverts to the simpler "bind the hash, not
   `file_ids`" shape that section prefers everywhere else.
2. **`category_order` could move to a content-keyed table.** §A.1's re-keying
   excluded it for the same name-dependence reason. Mime-only removes the
   objection, though whether it is worth moving is a separate question — it is
   cheap to read where it is.

**Sequencing.** Do not delete the extension arms in this plan. They are the only
thing classifying legacy rows whose mime is still generic, and that population
is non-zero until `content_rescan` has converged — the same convergence
condition the legacy-blob work waits on. The order is: type the enum here (with
`(name, mime)` intact), let `content_rescan` normalise mime and backfill, then
drop the name parameter and the extension arms as a follow-up once a count of
generic-mime rows reads zero.

### Migration consequence — the backfill is not SQL-able

Rows already stored carry `category_order = 500` for unknown files, and the
stored integer cannot distinguish "genuinely a Document" from "unknown that fell
back to Document". Recovering the truth means recomputing from `(name, mime)`,
and that mapping lives in Rust — so no migration can do it.

**`content_rescan` (`docs/plan/blob-resniff.md` §B) owns this backfill**, for the
same reason it owns `category_order` freshness: it already recomputes
`category_order_for(name, mime)` per row. Add it to that job's scope rather than
inventing a second mechanism. Until it runs, unknown files keep reading
"Document" — a display wart, not data loss.

### Why this belongs on top of `fix/search-type`

`SearchTypeFilter` and `category_orders_for_filter` live there and both get
refactored. This branch was rebased onto it for that reason.

### Scope

- `ResourceCategory` in `display_helpers.rs`, `ToSchema` + registered in
  `interfaces/api/mod.rs` (`ToSchema` alone emits a dangling `$ref` — see
  `project_openapi_enum_gotchas`).
- `category_for` / `category_with_ext` return it; `category_order_for` matches on
  it **exhaustively**, no `_` arm.
- `category_orders_for_filter` maps `SearchTypeFilter` → `&[ResourceCategory]`,
  so the integers appear once.
- `DisplayClass.category`, the 6 DTO `category` fields, `mount_dto` (×2),
  `folder_handler` (×1) and the `Folder` constant on `FolderDto`.
- FE: `typeLabel` becomes exhaustive over the generated union, removing its
  `?? category` fallback — the silent English-passthrough that step 1 is about.
- Wire change: `category` goes `string` → enum, so it needs its own
  `breaking-allow.conf` entry with a delete-on-merge note.

## Steps

1. **Fix the i18n bug first, standalone.** Route `ResourceList`'s type-cell
   through `typeLabel`. Vitest pin that a non-English locale renders the
   translated label. Independent of everything below.
2. **Drop `icon_special_class`** from all six DTOs. Zero consumers, so this is
   pure deletion — no FE work. Do it first of the three; it proves the
   DTO-surface mechanics with no behavioural risk.
3. **FE: derive the icon from `mime_type` + extension.** Extend
   `utils/display.ts` to produce the registry name directly, folding in
   `textyExt.ts`'s existing extension logic. Then `iconNameFromClass` and its
   callers go away rather than being re-pointed.
4. **Drop `icon_class`.** Only after step 3 lands, or the glyphs vanish.
5. **FE: `PhotoLightbox` uses `formatBytes(p.size)`.** Then drop
   `size_formatted`.
6. **Keep `category`.** Document on the DTO that it is a machine key, not
   display text, and that `typeLabel` is the only legitimate consumer path.

Breaking, no deprecation window, on the same grounds as §6/§7 of
`normalize-file-dates.md`: the API is recent and the SPA is its only consumer.
Unlike those, a stale client here degrades rather than errors — a missing
`icon_class` yields the `'file'` fallback in `iconNameFromClass`, so an old
build shows generic glyphs instead of failing.

## Measured: this is not a bandwidth win (settled)

These fields are `Arc<str>`-interned precisely *because* they repeat per row
(`intern_display`), which looks like evidence of payload cost. Step 2 measured
it instead of assuming. Two synthetic 50-row `/api/files` pages built to the
real DTO shape, with and without `icon_special_class`, gzip -9:

| Page | raw | gzip -9 | per row (gzip) |
|---|---|---|---|
| uniform (50 PDFs, one repeated value) | −1600 B (−5.6%) | **−11 B (−1.2%)** | 0.22 B |
| mixed (20 distinct values) | −1900 B (−6.7%) | **−117 B (−10.7%)** | 2.34 B |

So the real-world saving is **0.2–2.3 bytes per row after compression**. The raw
figure (~32–38 B/row) is the one that looks impressive and the one that never
reaches the wire — the repetition that justified interning is exactly what
DEFLATE removes. Note the uniform page compresses the field almost entirely
away while the mixed page keeps ~10%: the saving scales with value diversity,
which is the opposite of the intuition that repeated fields cost the most.

**Do not sell any of these three removals as a bandwidth win.** The real wins
are: one fewer classification table to keep in sync across two languages, three
fewer fields on six DTOs, and the `Arc<str>` interning machinery in
`display_helpers` becoming dead weight that goes with them. Step 2 alone
deleted ~120 lines of decision tree and 28 intern-table entries.

(Figures are from constructed payloads, not a live instance — measuring against
the running sandbox is off limits. The DTO field set and value distribution are
taken from the real serialiser, so the per-row deltas are exact; only the
row mix is synthetic.)
