# Plan: Photos Evolution — Gallery, Places (map) & People (faces)

## Context

The Photos view (`static/js/features/library/photos.js` + `photosLightbox.js`) is a
date-grouped timeline with infinite scroll, multi-select and a lightbox. The backend
already extracts and stores per-photo EXIF — including **GPS latitude/longitude** — in
`storage.file_metadata` (`src/infrastructure/services/exif_service.rs`,
`media_metadata_service.rs`), and serves the timeline via `GET /api/photos`
(`src/interfaces/api/handlers/photos_handler.rs` → `list_media_files` in
`src/infrastructure/repositories/pg/file_blob_read_repository.rs`).

This plan adds, in three phases:

0. **Gallery polish** — performance + modern UX (timeline virtualization already landed).
1. **Places** — a map of geotagged photos. *Most of the data already exists.*
2. **People** — face detection, embedding, identity clustering ("like Apple/Google Photos").

All work follows OxiCloud conventions: hexagonal layering, **AuthZ enforced only in the
application service layer** via `*_with_perms(caller_id)` methods calling
`AuthorizationEngine::require(...)`, **audit logging on every denial**
(`target: "audit"`), feature flags (`OXICLOUD_ENABLE_*`), native `UUID` columns, sqlx
migrations, and a **vanilla-JS / vanilla-CSS** frontend (design tokens from
`static/css/base/variables.css`, JSDoc-typed, BEM).

> ⚠️ New dependencies (JS libraries to vendor, Rust crates, the `pgvector`/`vectorchord`
> Postgres extension, and runtime-downloaded ML models) need explicit sign-off — see
> **§Vendoring & dependencies** and **§Open decisions**. Per repo rules: never hand-edit
> `Cargo.lock` (use `cargo add`), and don't introduce JS frameworks.

---

## Implementation status (updated)

**Phase 0 — Gallery polish: essentially complete** (branch `claude/zealous-faraday-58s1at`).

| Item | Status | Commit / note |
|------|--------|---------------|
| 0.1 Virtualization | ✅ done | `081b2b6` |
| 0.2 width/height on `/api/photos` | ✅ done | `8d09589` — implemented via a flattened `PhotoDto` (`#[serde(flatten)]`) instead of widening `FileDto` + its 6 construction sites; `list_media_files` LEFT JOINs `storage.file_metadata`. `FileItem` gained optional `width`/`height`. |
| 0.3 Justified layout | ✅ done | `75ee9b7` |
| 0.4 Lightbox (zoom/pan, swipe, info panel, favorite fix) | 🟡 mostly | `ca1a8cb`, `e8520e4` — the map pin shows **coordinates as text**; the embedded mini-map / deep-link into Places is deferred until Places exists. |
| 0.5 Shift-select, confirm→Modal, keyboard a11y | 🟡 mostly | `824df4d`, `e8520e4` — optional drag-marquee not done. |
| 0.6 HEIC | ⏸ **deferred, decided 2026-10-07** | Thumbnails wait for a **pure-Rust** HEIF decoder; `libheif-rs` declined (C toolchain + LGPL). Metadata already works. See §0.6. |
| 0.7 Sub-nav tabs | ⬜ pending | deferred to Phase 1 (tabs need the Places/People views). |

**Phase 1 — Places: complete (Approach A).**
- Backend (`f4b431b`): migration `…_places_geo_index.sql`, `FileBlobReadRepository::list_geo_clusters` (plain-SQL grid aggregation, no PostGIS), `PlacesService` (caller_id-scoped), `OXICLOUD_ENABLE_PLACES` (now **default on**, `513b622`), `GET /api/photos/geo`.
- Frontend: vendored MapLibre 5.24.0 + pmtiles 4.4.1 (`bb3d739`); `places.js` (`513b622`) renders the server-aggregated clusters as **HTML thumbnail markers** (no glyphs/sprites, no client-side clustering), refetches on pan/zoom, and drills into the lightbox. Optional Protomaps `.pmtiles` basemap read over HTTP **Range via the existing `ServeDir`** (label-light style, light/dark) with graceful fallback to a themed background; ODbL attribution. "Moments | Places" sub-nav.
- **Deviations from the original plan:** 1.5 serves the basemap as a *static file* (ServeDir Range) instead of the `pmtiles` Rust crate; 1.8 uses MapLibre HTML markers instead of a deck.gl `IconLayer`. Both keep the footprint minimal and need zero new backend code.
- **Pending:** browser smoke-test, and an operator-supplied `static/basemaps/basemap.pmtiles` for the street backdrop (works without it).

**Phase 2 — People: complete (detector/embedder shipped, opt-in).**
- **Migration** (`…_faces.sql`): `faces` schema with `faces.persons` + `faces.faces`.
  **Deviation from 2.2:** embeddings stored as **`BYTEA`** (512×`f32` little-endian), **no
  `pgvector`** — cosine similarity runs in Rust. This keeps the extension footprint at
  today's `pg_trgm`/`ltree`/`citext` and is fine at personal-library scale; the HNSW/ANN
  path is the documented growth step if it's ever needed.
- **Config:** `OXICLOUD_ENABLE_FACES` (`FeaturesConfig::enable_faces`, **default off** —
  biometric/opt-in). Everything below is inert when off.
- **Domain/ports:** `Face`, `Person`, `BoundingBox`, `DetectedFace` (`domain/entities/face.rs`);
  `FaceAnalyzerPort` (single `analyze(&[u8]) -> Vec<DetectedFace>` + `is_ready()`) and
  `FaceRepository` (`face_ports.rs`). **Deviation from 2.3:** detector+embedder collapsed
  into one `FaceAnalyzerPort` (the analyzer owns detect→align→embed) instead of split
  `FaceDetectorPort`/`FaceEmbedderPort` — simpler seam for a single ONNX session.
- **Repository:** `FacePgRepository` (`infrastructure/repositories/pg/`) — bytea
  encode/decode, person CRUD, `faces_for_*`, `assign_person`, `delete_all_for_user`.
- **Service:** `PeopleService` (`application/services/people_service.rs`) — `recluster()`
  via **union-find connected-components** (cosine ≥ 0.5, `min_faces` 3, immich-style),
  plus list/photos/rename/hide/merge/delete. "List my own people" needs no `authz.require`
  (user-scoped, like `RecentService`/`PlacesService`).
- **Indexing:** `FaceIndexingService` implements `FileLifecycleHook` — background
  detect+embed on image create/copy/update, **dedup by blob hash**. Driven by the
  analyzer port; with the no-op analyzer it does nothing.
- **Analyzer:** two implementations behind `FaceAnalyzerPort`. `NoopFaceAnalyzer`
  (`is_ready()=false`) is the default so the stack compiles/runs **without any ML model**.
  `OnnxFaceAnalyzer` (`12ede47`, behind the **`faces-onnx`** cargo feature) is the real
  SCRFD+ArcFace pipeline; `di::build_face_analyzer` picks it when the feature is compiled in
  and runtime+models are configured, else degrades to the no-op (logged) so startup never
  fails. **Deviation from 2.4:** the error-prone math (SCRFD anchor decode, NMS, the
  closed-form similarity alignment, affine warp, normalization) lives in `face_geometry.rs`,
  compiled in **every** build and covered by 11 unit tests; only the ONNX session calls are
  feature-gated (and untestable here, no models). `ort` uses **load-dynamic** so
  `libonnxruntime` is dlopen'd at runtime and the crate builds without it; loading goes
  through `ort::init_from` (fallible) not ORT's lazy loader, which would `panic` under
  `panic = "abort"`.
- **HTTP:** `people_handler.rs` + routes (gated on `people_service.is_some()`):
  `GET /api/people`, `/api/people/{id}/photos`, `PATCH /api/people/{id}`,
  `POST /api/people/merge`, `/api/people/recluster`, `GET /api/people/data`,
  `GET /api/people/faces/{file_id}`, `POST /api/people/{id}/hide`.
- **Frontend (`6314fa6`):** `people.js` + `people.css` — person grid (circular cover,
  name, count), drill into a person's photos via the existing lightbox, rename via
  `Modal.prompt` + `PATCH`. Wired into the Photos sub-nav as a third **People** tab that a
  capability probe (`GET /api/people`) reveals only when faces are on; otherwise hidden.
  i18n keys in `en.json` (others fall back to English).
- **Config (2.4):** `FacesConfig` + `OXICLOUD_FACES_{ORT_DYLIB,DETECTOR_MODEL,
  EMBEDDER_MODEL,DET_SIZE,DET_THRESHOLD,NMS_THRESHOLD,INTRA_THREADS}` (documented in
  `example.env`). To run faces: build `--features faces-onnx`, set `OXICLOUD_ENABLE_FACES=true`,
  and point the three model/runtime paths at an operator-supplied ONNX Runtime +
  SCRFD detector + ArcFace embedder (e.g. InsightFace `buffalo_l`). Nothing is committed.
- **Still open (optional):** per-user opt-in consent gate (2.1), lightbox face-box tagging
  (2.8), and the periodic full re-cluster job (2.6 has on-demand `recluster`; no scheduler
  yet). End-to-end smoke-test needs real models + a browser, which only you can run.

---

## Research summary (the decisions these phases encode)

**Map (no third-party APIs, self-host, extreme perf):**
- **Engine:** MapLibre GL JS v5 (BSD-3, WebGL2, vendorable UMD, no framework).
- **Basemap:** self-hosted **Protomaps `.pmtiles`** (single file) served by Axum via the
  **`pmtiles`** Rust crate over HTTP Range — OxiCloud serves its own basemap. Global
  z0–6 ≈ 60 MB; regional extracts on demand; planet ≈ 120 GB.
- **Clustering:** client-side **Supercluster** (MapLibre `cluster: true`, in a web worker)
  up to ~100k points; beyond that, **plain-SQL grid/geohash aggregation** by zoom+bbox —
  **no PostGIS needed** (only `pg_trgm`/`ltree`/`citext` are enabled today).
- **Gotchas:** self-host glyphs+sprites (not the Protomaps CDN); ODbL attribution
  "Protomaps © OpenStreetMap" is mandatory; dark-mode via `@protomaps/basemaps` flavors.

**Faces (self-host, precision, CPU-first):**
- **Runtime:** **`ort`** (ONNX Runtime). `candle` can't run SCRFD/RetinaFace (missing
  `Resize` op); `tract` is the pure-Rust fallback for a single static binary.
- **Licensing landmine:** no permissive high-accuracy face-recognition checkpoint exists.
  InsightFace `buffalo_l` (IJB-C ~97.3) and EdgeFace weights are **non-commercial**.
- **Recommended (immich/PhotoPrism pattern):** **download** SCRFD + `buffalo_l` weights at
  runtime (not committed); personal self-hosted use is non-commercial-compliant. Offer a
  fully-permissive fallback (RetinaFace-MobileNet0.25 **MIT** + a self-retrained
  EdgeFace/GhostFaceNet embedder, ~94 IJB-C, ~10× smaller).
- **Storage/clustering:** embeddings in Postgres via **pgvector** (HNSW, 512-d), growth
  path to **VectorChord**; **threshold / connected-components incremental clustering**
  (immich-style), not Approximate Rank-Order; ANN + exact re-rank; quality gating
  (det-score ≥0.7, face ≥50–80px, blur); `minFaces ≥3` to promote a cluster to a Person.
- **Privacy:** biometric data (GDPR Art. 9) → **opt-in, OFF by default, per-user
  isolation, cascade-delete**, all local.

---

## Phase 0 — Gallery polish

### Execution order

#### 0.1 Timeline virtualization — ✅ DONE (commit `081b2b6`)
Each date-group is a `<section>` whose grid is materialized only near the viewport.
**Remaining:** browser smoke-test, then it's closed.

#### 0.2 Expose image dimensions on the timeline (enables justified layout, kills CLS)
**`src/application/dtos/file_dto.rs`** — add to `FileDto`:
```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub width: Option<u32>,
#[serde(skip_serializing_if = "Option::is_none")]
pub height: Option<u32>,
```
**`src/infrastructure/repositories/pg/file_blob_read_repository.rs`** — in the
`list_media_files` query, `LEFT JOIN storage.file_metadata fm ON fm.file_id = fi.id` and
select `fm.width, fm.height`; map into the new fields.
**`static/js/core/types.js`** — add `width?`/`height?` to `FileItem` (already on
`FileMetadata`).

#### 0.3 Justified rows layout (modern, aspect-preserving)
**`static/js/features/library/photos.js`** — add a `layoutMode: 'square' | 'justified'`
toggle in the toolbar. In justified mode, replace the CSS grid with a row-packing pass
(target row height ~180–220px, distribute by aspect ratio = `width/height`, fallback 1:1
when dimensions are absent). Keep the existing virtualization: row-pack **within each
materialized group**, so it composes with section materialize/dematerialize.
**`static/css/views/photos.css`** — `.photos-grid--justified` (flex rows) variant.

#### 0.4 Lightbox upgrades
**`static/js/features/library/photosLightbox.js`**
- **Zoom/pan** (wheel + pinch) and **mobile swipe** for prev/next.
- **Info panel** (toggle) showing EXIF from `/api/files/{id}/metadata`.
- **Map pin** (resolves the existing `//TODO: add geoloc pointer` at line ~301): when
  `latitude/longitude` present, render a small static MapLibre mini-map / "Show on map"
  link that deep-links into the Places view.
- **Favorite initial state** (bug fix): call `favorites.isFavorite(item.id, 'file')` in
  `_show()` to set the star correctly (today it always starts empty).

#### 0.5 UX & a11y
**`static/js/features/library/photos.js`**
- **Shift-click range select** and optional drag-marquee.
- Replace native `confirm()`/`alert()` with the app modal
  (`static/js/components/modal.js`; add an async `Modal.confirm()` helper).
- Tiles become focusable/role-correct; arrow-key navigation across the grid.

#### 0.6 HEIC support — ⏸ **deferred pending a pure-Rust decoder (decided 2026-10-07)**

Re-audited 2026-10-07 while verifying content-type sniffing. The gap is
narrower than this section originally said: **metadata already works, only
thumbnails are missing.**

What happens to an iPhone photo today:

| | |
|---|---|
| detected type | `image/heif` — from the ISO-BMFF `ftyp` brand. Note **not** `image/heic`: `infer` has no HEIC spelling, and `image/heif` is not a container envelope, so the `.heic` extension does not narrow it. Pinned by `phone_camera_formats_are_detected` in `common/mime_detect.rs` |
| EXIF metadata | ✅ `MediaMetadataService::handles()` gates on the `image/` prefix, and **nom-exif 3.6 parses HEIF** — `captured_at`, GPS and camera make/model come through, so `media_sort_date` is populated and the photo sorts correctly in the timeline |
| width / height | ❌ those come only from the kamadak-exif path, which is JPEG/TIFF-oriented |
| thumbnail | ❌ none, so no tile in the Photos grid |

The thumbnail gate is **honest, not buggy**:

```rust
// ThumbnailService::is_supported_image
matches!(mime_type, "image/jpeg" | "image/jpg" | "image/png" | "image/gif" | "image/webp")
```

That list mirrors `Cargo.toml` exactly —
`image = { features = ["jpeg", "png", "gif", "webp"] }`. The `image` crate
has no HEIF decoder at all, so widening the gate would only move the failure
from "skipped" to "decode error". `image/avif` is absent for the same reason.

**Decision: wait for a pure-Rust HEIF decoder.** The three options and why:

- **`libheif-rs`** — declined. Pulls a C toolchain into every build, and
  libheif is LGPL, which needs care in an MIT project (see AGENTS.md on
  licence compatibility). Too much permanent cost for one input format.
- **Transcode HEIC→JPEG on upload** — declined for now. It mutates the
  user's bytes on ingest, which this project does not do: content is
  stored verbatim and content-addressed. A *derived* JPEG would be the
  shape (`content_derived_blobs`, `kind = 'thumbnail'`), but that still
  needs a decoder to produce it, so it does not avoid the problem.
- **Pure-Rust decoder** — the one that costs nothing permanent. Revisit
  when a maintained crate can decode HEIF/HEIC stills (the AVIF side is
  closer than HEIC's HEVC-in-HEIF).

##### Candidates evaluated

**`heic` (imazen/heic) — rejected 2026-10-07 on licence.**
`AGPL-3.0-only OR LicenseRef-Imazen-Commercial`. AGPL is disqualifying
per AGENTS.md, and worse than GPL for this project specifically: §13
triggers on *network interaction*, which is what a self-hosted server
serving thumbnails over HTTP does. The commercial option (they advertise
$1 startup licences) does not rescue it — OxiCloud is self-hosted, so
**every operator** would need their own licence, which is exactly the
"no feature gated behind a licence key" prohibition, pushed onto users.

Worth knowing it is otherwise the strongest candidate seen, so nobody
re-evaluates it hoping the terms changed without checking: pure Rust,
`#![forbid(unsafe_code)]`, FFI isolated in optional subcrates, and it
decodes **HEVC-in-HEIF natively** — the hard part that other pure-Rust
efforts skip in favour of AVIF only. 49/49 conformance vectors.

Even setting licence aside it would be early for us: 0.1.6, first
released 2026-03-29, 0.2.0 yanked, 12 GitHub stars and a single vendor.
118/162 HEIF corpus files decode (~27% fail), HEVC **I-frames only**,
and no PQ/HLG EOTF — "callers handle tone-mapping", which modern iPhones
need since they shoot HDR HEIC.

Re-check criteria for any future candidate, in order: (1) licence
MIT/Apache/BSD-compatible — not AGPL, not dual-commercial; (2) no C
toolchain in the default build; (3) decodes HEVC-in-HEIF stills, not
just AVIF; (4) handles HDR transfer functions or we accept writing
tone-mapping.

Until then: no code change, and `is_supported_image` carries a pointer
here so the next reader finds the decision rather than re-deriving it.

#### 0.7 Sub-navigation inside Photos
**`static/index.html`** + **`static/js/app/navigation.js`** (`switchToPhotosSection`,
line ~434) + i18n: add a tab strip **Moments · Places · People** within the Photos view.
Places/People tabs are hidden unless their feature flags are on. This is the mount point
for Phases 1 & 2.

---

## Phase 1 — Places (map)

Data already exists (`storage.file_metadata.latitude/longitude`, `DOUBLE PRECISION`).
No PostGIS.

### Execution order

#### 1.1 Migration — index (+ optional geohash)
**New file:** `migrations/<ts>_places_geo_index.sql`
```sql
-- Fast bbox scans over geotagged photos
CREATE INDEX IF NOT EXISTS idx_file_metadata_geo
    ON storage.file_metadata (latitude, longitude)
    WHERE latitude IS NOT NULL AND longitude IS NOT NULL;
-- Optional (scale): a geohash/quadkey integer + btree for prefix grouping by zoom.
-- ALTER TABLE storage.file_metadata ADD COLUMN geohash BIGINT;
```

#### 1.2 Application port + PG repository (grid aggregation)
**`src/application/ports/`** — new `GeoPhotoReadPort` (or extend an existing media port):
```rust
pub struct GeoCluster { pub lng: f64, pub lat: f64, pub count: i64, pub sample_file_id: Uuid }
pub struct GeoBounds { pub w: f64, pub s: f64, pub e: f64, pub n: f64 }

#[async_trait]
pub trait GeoPhotoReadPort: Send + Sync {
    async fn clusters_in_bounds(&self, user_id: Uuid, b: GeoBounds, cell: f64)
        -> Result<Vec<GeoCluster>, DomainError>;
    async fn photos_in_bounds(&self, user_id: Uuid, b: GeoBounds, limit: i64)
        -> Result<Vec<FileDto>, DomainError>;
}
```
**`src/infrastructure/repositories/pg/`** — PG impl. Grid aggregation (no PostGIS):
```sql
SELECT round(fm.longitude / $6) * $6 AS gx,
       round(fm.latitude  / $6) * $6 AS gy,
       count(*)          AS n,
       avg(fm.longitude) AS clng,
       avg(fm.latitude)  AS clat,
       min(fm.file_id)   AS sample_id
FROM storage.file_metadata fm
JOIN storage.files fi ON fi.id = fm.file_id
WHERE fi.user_id = $1::uuid AND NOT fi.is_trashed
  AND fm.longitude BETWEEN $2 AND $3    -- west .. east
  AND fm.latitude  BETWEEN $4 AND $5    -- south .. north
  AND fm.latitude IS NOT NULL
GROUP BY gx, gy;
```
`$6` (`cell`) shrinks with zoom. Single indexed scan + hash aggregate; the browser only
receives `{count, center, sample_file_id}` per cell.

#### 1.3 Application service (AuthZ + audit)
**`src/application/services/places_service.rs`** (new):
```rust
pub async fn list_clusters_with_perms(
    &self, caller_id: Uuid, bounds: GeoBounds, zoom: u8,
) -> Result<Vec<GeoCluster>, AppError> {
    // Scoped to the caller's own library; no cross-user data.
    self.authz.require(caller_id, /* own photos */).await?; // audit on deny inside require()
    let cell = cell_for_zoom(zoom);
    self.geo.clusters_in_bounds(caller_id, bounds, cell).await
}
```
Wire it in **`src/common/di.rs`** (`AppServiceFactory` → `AppState`), `Option<Arc<…>>`
gated on the feature flag.

#### 1.4 Config flag
**`src/common/config.rs`** — `FeaturesConfig::enable_places` from `OXICLOUD_ENABLE_PLACES`.

#### 1.5 Basemap serving (PMTiles via Axum)
- Add the **`pmtiles`** crate (`cargo add pmtiles`).
- Ship a `.pmtiles` basemap (config: path; default global z0–6 ≈ 60 MB) + self-hosted
  **glyphs** and **sprites** under `static/` (from `basemaps-assets`).
- **`src/interfaces/api/handlers/basemap_handler.rs`** (new): open the reader once
  (`AsyncPmTilesReader::new_with_path`, `Arc` into `AppState`), serve
  `GET /api/basemap/{z}/{x}/{y}.mvt` (`reader.get_tile(...)`). *Alt:* serve the raw
  `.pmtiles` over Range and let `pmtiles.js` do directory math (no tile handler).

#### 1.6 HTTP endpoints + routes
**`src/interfaces/api/handlers/places_handler.rs`** (new):
- `GET /api/photos/geo?bbox=w,s,e,n&zoom=Z` → `Vec<GeoCluster>` (auth middleware injects
  `caller_id`; handler does **no** AuthZ — service does).
- `GET /api/photos/geo/cell?bbox=…` → photos in a cell (opens lightbox).
Register in **`src/interfaces/api/routes.rs`** (protected routes) + the basemap route
(public/cached).

#### 1.7 Frontend — vendored map + Places module
- **Vendor** (needs sign-off): `maplibre-gl` (UMD + CSS), `pmtiles.js`,
  `@protomaps/basemaps` style JSON → `static/js/vendors/` + `static/css/`.
- **`static/js/features/library/places.js`** (+ `static/css/views/places.css`): init
  MapLibre with the self-hosted style (light/dark flavor by theme), register the
  `pmtiles://` protocol, add a clustered GeoJSON source fed from `/api/photos/geo`
  (`cluster: true`) — or, above ~100k, the server-aggregated endpoint. Click cluster →
  zoom; click point → open lightbox filtered to that cell. Mandatory ODbL attribution
  control.
- **`static/js/core/types.js`** — `GeoCluster` typedef.
- Mount under the **Places** sub-nav tab (§0.7).

#### 1.8 (Optional) thumbnail markers
deck.gl `IconLayer` (MIT, no React) atlas for **visible cluster representatives only** —
never atlas all points. Start without it (count bubbles), add later.

---

## Phase 2 — People (faces)

Feature-flagged, opt-in, OFF by default. Biometric data → privacy-first.

### Execution order

#### 2.1 Config flag + privacy switch
**`src/common/config.rs`** — `OXICLOUD_ENABLE_FACES`. Plus a **per-user opt-in** setting
(stored in `auth.users` or a user-settings table) — clustering only runs for users who
opted in.

#### 2.2 Migration — pgvector + schema
**New file:** `migrations/<ts>_faces.sql`
```sql
CREATE EXTENSION IF NOT EXISTS vector;       -- pgvector (or vectorchord)
CREATE SCHEMA IF NOT EXISTS faces;

CREATE TABLE faces.persons (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id       UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    display_name  TEXT,                       -- null = unnamed
    cover_face_id UUID,
    is_hidden     BOOLEAN NOT NULL DEFAULT false,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE faces.faces (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    file_id    UUID NOT NULL REFERENCES storage.files(id) ON DELETE CASCADE,
    user_id    UUID NOT NULL REFERENCES auth.users(id)   ON DELETE CASCADE,
    bbox       REAL[4] NOT NULL,              -- x,y,w,h (normalized)
    det_score  REAL NOT NULL,
    quality    REAL,                          -- blur/size gate result
    embedding  vector(512) NOT NULL,
    person_id  UUID REFERENCES faces.persons(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_faces_embedding ON faces.faces
    USING hnsw (embedding vector_cosine_ops);
CREATE INDEX idx_faces_person ON faces.faces (person_id);
CREATE INDEX idx_faces_user   ON faces.faces (user_id);
```
Cascade-delete guarantees the **right to erasure**: deleting a file/user removes its
faces; deleting a Person unlinks its faces.

#### 2.3 Domain + ports
**`src/domain/entities/`** — `Face`, `Person`.
**`src/application/ports/face_ports.rs`** (new):
```rust
pub struct DetectedFace { pub bbox: [f32;4], pub landmarks: [[f32;2];5], pub score: f32 }

#[async_trait] pub trait FaceDetectorPort: Send + Sync {
    async fn detect(&self, image: &DynamicImage) -> Result<Vec<DetectedFace>, DomainError>;
}
#[async_trait] pub trait FaceEmbedderPort: Send + Sync {
    async fn embed(&self, aligned_112: &DynamicImage) -> Result<[f32;512], DomainError>;
}
```
**`src/application/ports/`** — `FaceRepository` (CRUD + ANN search via pgvector `<=>`).

#### 2.4 Infrastructure — ONNX runtime adapter
- `cargo add ort ndarray`.
- **`src/infrastructure/services/onnx_face_service.rs`** (new): loads detector + embedder
  ONNX models, runs on a **dedicated thread pool** (mirror `thumbnail_service.rs` /
  `image_transcode_service.rs` to avoid starving Tokio). Pipeline: detect → 5-point
  similarity align to 112×112 → embed → L2-normalize. Implements `FaceDetectorPort` +
  `FaceEmbedderPort`.
- **Models** are **downloaded at runtime** to a models dir (NOT committed). Default:
  SCRFD-2.5G + `buffalo_l/w600k_r50` (immich pattern). Config switch to the
  permissive fallback (RetinaFace-MobileNet0.25 MIT + bundled-by-you embedder).
- GPU optional via `ort` execution providers (`ORT_DYLIB_PATH` / EP Cargo features); same
  code path falls back to CPU.

#### 2.5 Indexing pipeline (lifecycle hook + backfill)
- **`src/infrastructure/services/face_indexing_service.rs`** (new) implements
  `FileLifecycleHook` (same pattern as `media_metadata_service.rs`): on image create →
  decode (reuse decode path) → detect → **quality-gate** (score ≥0.7, face ≥50–80px,
  Laplacian blur) → embed → store. **Dedup by `blob_hash`**: identical photos reuse faces.
- **Backfill**: a throttled background job over the existing library on the **maintenance
  pool**.

#### 2.6 Clustering (incremental + periodic) — application service
**`src/application/services/people_service.rs`** (new). All methods
`*_with_perms(caller_id)` → `authz.require(...)` → audit on deny.
- **Online (per import):** ANN candidate via pgvector `<=>` + **exact cosine re-rank**;
  assign to existing Person if within the *match* threshold (tighter), else leave
  unassigned. Thresholds: form ≈ cosine-sim 0.75–0.80 (Euclid ≈ 0.5); match tighter (≈0.4
  Euclid) for precision.
- **Periodic full re-cluster:** threshold connected-components over the user's faces;
  `minFaces ≥3` to promote a cluster to a Person; singletons → "Unknown".

#### 2.7 HTTP endpoints + routes (AuthZ in service)
**`src/interfaces/api/handlers/people_handler.rs`** (new):
- `GET /api/people` — persons (cover + count).
- `GET /api/people/{id}/photos`.
- `PATCH /api/people/{id}` — rename.
- `POST /api/people/merge` · `/split` · `POST /api/people/{id}/hide`.
- `GET /api/files/{id}/faces` — face boxes for lightbox tagging.
- Settings: enable/disable, re-index, **delete all my face data**.
Register in `routes.rs`. Wire service in `di.rs` (`Option<Arc<PeopleService>>`).

#### 2.8 Frontend — People module
- **`static/js/features/library/people.js`** (+ `people.css`): grid of person tiles
  (circular cover face + name), click → that person's photos; rename/merge/hide UI;
  lightbox face boxes + "tag person".
- **`static/js/core/types.js`** — `Person`, `Face` typedefs.
- Mount under the **People** sub-nav tab (§0.7); show an explicit **opt-in consent** gate
  before first indexing.

#### 2.9 (Optional, later) Semantic search
CLIP/SigLIP via the same `ort` stack → natural-language photo search ("beach", "cake").
Reuses the embedding-in-Postgres + ANN infrastructure.

---

## Vendoring & dependencies (need sign-off)

| Kind | Item | License | Notes |
|------|------|---------|-------|
| JS (vendor) | `maplibre-gl` (UMD+CSS) | BSD-3 | Map engine; no framework |
| JS (vendor) | `pmtiles.js` | BSD-3 | Range-reads `.pmtiles` in browser |
| JS (vendor) | `@protomaps/basemaps` style + assets | code BSD-3 / design CC0 | self-host glyphs+sprites |
| JS (vendor, opt) | `deck.gl` core+layers | MIT | thumbnail `IconLayer` only |
| Rust crate | `pmtiles` | MIT/Apache-2.0 | serve basemap from Axum (`cargo add`) |
| Rust crate | `ort` (+`ndarray`) | MIT/Apache-2.0 | ONNX runtime; ships `libonnxruntime.so` |
| Rust crate (opt) | `libheif-rs` | LGPL | HEIC decode (native dep) |
| PG extension | `pgvector` (→ `vectorchord`) | PostgreSQL / Apache-2.0 | 512-d embeddings + HNSW |
| Asset (basemap) | Protomaps `.pmtiles` | data ODbL | self-hosted; attribution required |
| ML models (runtime DL, NOT committed) | SCRFD + `buffalo_l` | **non-commercial** | personal self-host OK; commercial = license InsightFace |

Repo rules respected: no hand-editing `Cargo.lock`; no JS framework; design tokens only
for CSS; `target:"audit"` denial logs; AuthZ exclusively in services.

---

## Open decisions (need your call)

1. **Faces embedder strategy:** (a) **immich pattern** — runtime-download `buffalo_l`
   (~97 IJB-C, non-commercial, recommended default) · (b) fully-permissive bundle —
   retrain EdgeFace/GhostFaceNet (~94, real ML project) · (c) defer People.
2. **Basemap extent / hosting:** global z0–6 (~60 MB, simplest) vs regional extract vs
   full planet (~120 GB) — and store path / how shipped.
3. **Vector store start:** `pgvector` now (simplest) vs `VectorChord` from day one
   (immich's scaled choice).
4. **Map thumbnails:** start with count bubbles (MapLibre only) vs deck.gl `IconLayer`
   from the start.
5. **HEIC:** in scope for Phase 0, or deferred (native `libheif` dep)?
6. **MapLibre vendoring approval** (new JS library — per repo rules, needs explicit OK).

---

## Suggested sequencing

| Phase | Risk | Notes |
|-------|------|-------|
| 0.1 virtualization | done | smoke-test pending |
| 0.2–0.5 polish | low | self-contained, verifiable |
| 1 Places | low–med | data ready; new vendored map + basemap serving |
| 2 People | high | new ML stack, pgvector, privacy, licensing decision |
| 0.6 HEIC / 2.9 search | opt | independent, schedule freely |

Recommended order: finish **Phase 0**, ship **Places**, then tackle **People** once the
embedder-licensing decision (#1) is made.
