# Retire the pre-computed display fields from the wire

Status: **complete — all six steps plus the `ResourceCategory` follow-up
shipped** (2026-10-24). Written 2026-10-22 from Ed's question: are
`icon_class`, `icon_special_class` and `size_formatted` actually used in the
UI, and can the FE compute them instead?

| Step | What | Status |
|---|---|---|
| 1 | i18n fix — `ResourceList` type-cell through `typeLabel` | ✅ shipped |
| 2 | Drop `icon_special_class` (6 DTOs, zero consumers) | ✅ shipped |
| 3 | FE derives the icon from `mime_type` + extension | ✅ shipped |
| 4 | Drop `icon_class` | ✅ shipped |
| 5 | `PhotoLightbox` → `formatBytes`, drop `size_formatted` | ✅ shipped |
| 6 | Document `category` as a machine key | ✅ shipped |
| — | `is_root` — adjacent removal, not an original step | ✅ shipped |
| 7 | `ResourceCategory` enum, server + wire + SPA | ✅ shipped 2026-10-24 |

Step 5 was taken out of order — it is independent of 3/4 and of the mime work,
so there was no reason to hold it behind the icon chain. Steps 3 and 4 shipped
together, in that order within the commit.

**The `ResourceCategory` enum section below IS now implemented** (2026-10-24)
— the section is kept as the design record, with what actually landed noted
under *As implemented* at its end. Everything above it also describes shipped
state.

One part of it did NOT land, and deliberately: the *Sequencing* note's final
step, dropping `name` from `category_for`. That turned out to be unreachable
rather than merely distant — see *As implemented*.

### What landed beyond the six steps

- `DisplayClass` / `classify_display` are **gone**. The fused classifier
  existed to lower the extension once for three decision trees (ROUND11 §21,
  2.12x); with only `category_for` left there is nothing to fuse. Callers take
  `category_for` directly, and `bench_round11_micro`'s §21 section was removed
  with its subject (section numbers are not renumbered, so archived figures
  keep their labels).
- `search_service`'s private `format_bytes` and
  `display_helpers::format_file_size` are both gone, leaving `formatBytes` as
  the only size formatter in the product.
- `SearchSuggestionItem.icon_class` became `mime_type` — it had no MIME field,
  so suggestions would otherwise have been classifiable by filename alone.
- A real pre-existing bug surfaced: `.log` was in `TEXTY_EXT_RE` (so the SPA
  opens log files in the collab editor) but in **neither** icon table,
  including the Rust one, so `foo.log` drew the generic glyph. Found by the new
  `textyExtensionsWithoutIcon()` drift assertion on its first run, and fixed.
- Three stale doc comments were corrected rather than carried forward: the
  `display_helpers` module header (which stated the now-reversed principle that
  the server pre-computes display fields so the FE need not duplicate them),
  `format_file_size`'s parity claim, and `SearchFileResultDto.category`'s
  lowercase value list.

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

### `size_formatted` — duplicated by construction, and already divergent

One consumer, `PhotoLightbox.svelte:95`. `utils/format.ts::formatBytes()`
already exists, and `size` is already on the wire.

**Correction, found while implementing step 5.** This section originally cited
`display_helpers::format_file_size`'s doc comment — "Matches the JavaScript
`formatFileSize()` output exactly" — as evidence the swap was a no-op. It is
not. That comment refers to the *vanilla* frontend's helper, which went away
with `static/` in `54639d46`; the SvelteKit `formatBytes` is a different
function. Worse, the backend had **two** formatters feeding this one field:

| bytes | `display_helpers::format_file_size` | `search_service::format_bytes` | FE `formatBytes` |
|---|---|---|---|
| 0 | `0 Bytes` | `0 B` | `0 B` |
| 500 | `500 Bytes` | `500 B` | `500 B` |
| 1024 | `1 KB` | `1.0 KB` | `1.0 KB` |
| 3 423 744 | `3.27 MB` | `3.3 MB` | `3.3 MB` |

So `/api/files` and `/api/search` disagreed about the same file in the same
field, and the search one silently matched the FE. Step 5 deletes both and the
field, leaving `formatBytes` as the only size formatter in the product.
(`storage_usage_service::format_bytes` survives — it renders quota-exceeded
error messages, not a DTO field.)

One visible consequence, accepted deliberately: the photo lightbox caption
reads "3.3 MB" where it read "3.27 MB". That is the value the grid behind it
always showed — `ResourceList` has always rendered `formatBytes(size)` — so
this closes an inconsistency rather than opening one. Pinned in
`PhotoLightbox.test.ts`, including the `size > 0` guard, which exists for the
`minimalPhotoItem` stub that People/Places open the lightbox with.

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

`mime_type` was already on every relevant DTO (`file`, `folder`, `favorites`,
`recent`, `trash`), and `name` carries the extension — exactly the input the
classifier takes. So no new field was required; the classification moved, it
did not grow. **One exception surfaced:** `SearchSuggestionItem` had no
`mime_type`, so step 4 swapped `icon_class` for it rather than leaving
suggestions classifiable by filename alone.

**The hard constraint was: do not create a third copy of the mapping.** It
lived in `display_helpers.rs` (Rust) and partially in `utils/display.ts` +
`utils/textyExt.ts` (TS). The outcome is two copies, not three: the Rust icon
tables were **deleted**, and `utils/display.ts::iconNameFor` is now the only
icon classifier, with `textyExt.ts` as its sibling — and the two are held in
step by an assertion (`textyExtensionsWithoutIcon`) rather than by a comment,
which is how the `.log` gap was found. The server could not delete its
*category* copy: `category_order_for` depends on `category_for` for the `type`
sort axis, which is server-side by necessity (see
`docs/architecture/resource-listing.md`).

The end state is deliberately asymmetric, and that asymmetry is documented on
`FileDto::category` itself (step 6) so it cannot read as an accident:

- **Server keeps** `category_for` / `category_order_for` — needed for
  `storage.files.category_order`, the indexed `type` sort key.
- **Server no longer emits** the three presentational strings.
- **FE owns** glyph + colour + byte formatting, derived from `mime_type` and
  `name`.
- **`category` stays on the wire** as the machine key, because the sort and the
  bucketing must agree with the server's grouping. Deriving it client-side would
  let the FE's buckets disagree with the server's `ORDER BY type`.

## Adjacent removal: `is_root` (shipped 2026-10-23)

Not one of the six steps, and not a presentational string — recorded here
because it was found by running this plan's audit question ("who actually reads
this?") against the next field along, and it came back with the same answer.

| | |
|---|---|
| FE consumers | **0** — `FolderBreadcrumb`'s `isRootTheLeaf` is a local derived off `visibleCrumbs.length`, unrelated to the field; the other hits were fixtures |
| `tests/` assertions | **0** — no Hurl, no e2e |
| Rust reads | **1** — a unit assert in `folder_handler`'s mount tests |

It was also derivable from a field on the same object: `FolderDto::from`
computed it as `parts.parent_id.is_none()`, and `parent_id` stays on the wire,
so a client that wants it writes `parent_id === null`.

**The reason it had to go rather than stay is that it was unreliable, not just
redundant.** Only `FolderDto::from` derived it. Four production sites hardcoded
`is_root: false` while taking `parent_id` from a nullable column:

| site | `parent_id` | `is_root` |
|---|---|---|
| `folder_dto.rs` | `parts.parent_id` | **derived** ✓ |
| `path_resolver_service.rs` | `fo.parent_id::text` (nullable) | hardcoded `false` |
| `trash_service.rs` | `row.parent_id.map(…)` | hardcoded `false` |
| `recent_handler.rs` | `row.parent_id.map(…)` | hardcoded `false` |
| `favorites_handler.rs` | `row.parent_id.map(…)` | hardcoded `false` |

`path_resolver_service` is the clearest case: its query is `WHERE fo.path = $1`
over `storage.folders` with no parent filter, so resolving a drive root's path
returned `parent_id: None` next to `is_root: false` — the field contradicting
the field it summarises. Deleting it removes that class of bug outright;
keeping it would have meant fixing the derivation at four callsites to maintain
a field with no readers.

No test pins this, for the same reason step 2 had none: a field with zero
readers has no behaviour to regress. The oasdiff spec diff is the guard, with
13 signals enumerated per-signal so any OTHER property vanishing from those
endpoints still fails the gate.

**Follow-up worth doing, not done here:** the same audit question has not been
asked of `tree_modified_at`, `etag`, `path` or `drive_id` on the same two DTOs.
`is_root` turned up because someone wondered aloud about it, which is not a
reliable discovery mechanism — one sweep would produce a single list to decide
from instead of finding these one at a time.

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
- The 6 DTO `category` fields (`file`, `folder`, `search`, `trash`, `recent`,
  `favorites`) and every producer. Recounted after steps 2–5, since
  `DisplayClass` no longer exists — this list was written when it did:
  - **13 `category_for` callsites**: `file_dto`, `recent_dto`,
    `favorites_dto`, `mount_dto` (×2), `trash_service` (×2, one via
    `trash_service_test`), `path_resolver_service`, `recent_handler`,
    `favorites_handler`, `folder_handler` (×2).
  - **13 `"Folder"` constants**: `folder_dto` (×2), `search_dto`, `mount_dto`
    (×2), `trash_service`, `path_resolver_service`, `report_handler`,
    `nextcloud/webdav_handler`, `api/webdav_handler`, `favorites_handler`,
    `recent_handler`, `folder_handler` (×2). These become
    `ResourceCategory::Folder` and stop being stringly-typed — which is half
    the value of the change, since a typo in any one of them is currently
    silent.
  - `category_order_for` already matches on `category_for`'s output
    (`display_helpers.rs:288`), so it is the natural place for the exhaustive
    match.
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

### As implemented (2026-10-24)

Nineteen categories, not fourteen. The design listed Image, Video, Audio,
Pdf, Document, Spreadsheet, Presentation, Archive, Code, Markdown, Text,
Installer, Folder, Unknown. What shipped renames two and adds five, each
for a reason found while building:

| change | why |
|---|---|
| `Document` → `WordProcessor` | `SearchTypeFilter::Document` is the COARSE bucket spanning Pdf + this + Spreadsheet + Presentation + Markdown + Text. One word for two sets is how a reader concludes the filter returns only `.docx`. `TextDocument` was rejected: `Text` already exists and most desktops call a `.txt` a "text document" |
| `Installer` → `Package` | once `.exe` moved to `Executable`, what remained really is the package formats (`.deb`, `.rpm`, `.dmg`, `.msi`, `.pkg`), all of which ARE separable by content |
| + `Executable` | `.exe` was filed as an installer, and ELF / Mach-O / wasm / Java class had no category at all. NOT named `Application`: `application/` is the MIME top-level type for most formats in that file, so the name would mean two things at once |
| + `Font` | `font/*` was in no arm, so every font read as Unknown |
| + `Ebook` | `application/epub+zip` matched neither the zip arm (which is exact-match) nor any prefix, so EPUBs read as Unknown |
| + `Config` | JSON/YAML/TOML/INI split out of `Code` — a deployment's worth of `.yaml` is not what a user means by code, and it is what an operator searches for |
| + `DiskImage` | ISO / qcow2 / VMDK / VDI, previously Unknown |

**`name` cannot leave `category_for`, and the plan's sequencing for it was
wrong.** It said: normalise mime with `content_rescan`, then drop the
extension arms once a count of generic-mime rows reads zero. That count can
never reach zero. Source code has NO magic bytes — `infer` returns `None`
for every language and always will — and `mime_guess` has no entry for
`.go` at all, so such a file is stored `application/octet-stream`
permanently and the extension is the only signal it will ever have.
Separately, `mime_guess` maps `.c`, `.cpp` and `.py` to `text/plain`, which
is true and useless: only the name separates a C file from a shopping list.
So the extension arms are permanent, narrowed to the cases MIME provably
cannot answer, and documented as such at the callsite.

**The consequence for `blob-resniff.md` §B is the opposite of what both
plans said.** Both recorded the per-row `category_order` fan-out as
TRANSITIONAL, reverting to a per-hash UPDATE once the classifier went
mime-only. It does not: the classifier stays name-dependent, so the fan-out
stays per row, permanently.

#### Bugs this exposed, all pre-existing

Typing the set was supposed to be a tidy-up. It found six real defects:

1. **`text/plain` source files were categorised Text, not Code.** The
   `text/` prefix arm returned before the extension was consulted, so every
   `.c`, `.cpp` and `.py` in the corpus was Text. Invisible, because Text is
   a plausible answer for a text file.
2. **`Installer` was in no `SearchTypeFilter` bucket**, so `.msi` / `.dmg`
   could not be found by any type filter. The hand-written integer list
   `[400,500,600,700,1000,1100]` omitted 1200 the same way. Caught by
   `every_category_is_reachable_from_some_filter` on its first run.
3. **The `Installer` MIME arm was dead for `.exe`**: it named
   `application/x-msdownload`, but `infer` emits
   `application/vnd.microsoft.portable-executable`. Verified against real
   magic bytes rather than assumed.
4. **`minimalPhotoItem` set `category: 'image'`** — lowercase, so it never
   matched `typeLabel`'s map and the Type cell rendered the raw key. A test
   pinned the wrong value.
5. **Four group-by definitions used `item.category ?? 'other'`** — `'other'`
   was never a category, so `typeLabel` passed it through and the group
   header read a literal lowercase "other".
6. **`.txt` was a word-processor document** when its mime was unrecognised
   but Text when it was `text/plain` — the same file categorised two ways
   depending on the client's Content-Type.

#### Drift guards

Five tests, because the mapping now lives in three places (variant list,
`order_of`, `categories_for_filter`) and the wire key in two (`as_str` and
serde):

* `the_variant_list_is_complete` — `order_of` is an exhaustive match so a new
  variant breaks the build there, but it would NOT break the test-local `ALL`
  list that every other guard iterates. The count pins it.
* `as_str_matches_the_serialised_key` and `the_wire_key_round_trips` — two
  hand-written copies of the same wire key, with `Pdf`/`"PDF"` the likeliest
  casualty since it is the one variant whose key is not its Rust name.
* `every_category_has_its_own_order` — a shared integer would silently merge
  two categories in the Type sort AND make the filter return the other one's
  rows.
* `every_category_is_reachable_from_some_filter` — a category in no bucket is
  unfindable by type. This is the one that found defect 2.

On the SPA side `typeLabel` is `Record<ResourceCategory, string>`, so a new
server category fails the type-check rather than rendering its raw English
key. Group-by keys arrive as plain `string`, so `typeLabelOf` narrows at
runtime and falls back to the localised Unknown label — a cast there would
have re-opened the raw-key hole the union just closed.

#### Not done

* `category_order` could now move to a content-keyed table (the design noted
  this unlocks with mime-only). It does NOT unlock, per the name-dependence
  above.
* A dedicated `SearchTypeFilter` option for packages / executables /
  executables-vs-archives. They share the `Archive` bucket, which is a
  compromise: a `.deb` is a package and a `.exe` is not an archive. The
  alternative was leaving them unfindable.
* `Font` and `Unknown` exist as filter values but are not offered in the SPA
  picker — the pattern the design already set for `Unknown`.
