//! `content_rescan` — re-identify stored content from its magic bytes.
//!
//! `refine_content_type` sniffs content-first on every upload path, which
//! fixes the *inflow*. It does nothing for three populations:
//!
//! * rows written before that landed, holding whatever the client claimed;
//! * rows invalidated by `swap_blob_hash`, which replaces a file's content
//!   on overwrite and leaves `mime_type` describing the PREVIOUS bytes —
//!   an ongoing staleness source, not a legacy one;
//! * content whose format `infer` did not recognise at the time.
//!
//! MIME is not a display hint here, it is a **gate**:
//! `AudioMetadataService::is_audio_file`, `MediaMetadataService::handles`
//! and the thumbnail hooks all test it before doing anything. A file
//! mistyped as `application/octet-stream` is permanently excluded from
//! every derived feature, silently. Re-identification unlocks content; it
//! does not merely relabel it.
//!
//! ### Why this walks CONTENT, not files
//!
//! The two admin endpoints it replaces iterate files, so N copies of one
//! song cost N downloads and N parses. Grouping by `blob_hash` makes the
//! page query's `FROM storage.files` *be* the "referenced by a file"
//! predicate — thumbnails and previews living in `chunk_manifests` are
//! excluded without a subquery — and covers CDC and legacy content in one
//! pass with one cursor.
//!
//! ### Why `infer::get` and not `refine_content_type`
//!
//! Steps 2–4 of that function need a filename and a client claim. A blob
//! has neither: it is bytes addressed by hash, possibly shared by files
//! with different names. Only the magic-byte step is meaningful here, so
//! this calls `infer` directly. Do not "fix" this by reaching for the
//! fuller function — see `decide` for the one place a filename legitimately
//! re-enters.
//!
//! ### Not named `*_consistency`, deliberately
//!
//! `consistency_batch_service` auto-enrols any job whose name ends that way
//! into the weekly batch. This one can cost a full-corpus read on a metered
//! backend, so it is on-demand only.
//!
//! ### Trashed content is skipped
//!
//! The walk excludes `is_trashed`, so content referenced only by trashed
//! files is never re-sniffed and a restored file keeps its stale type until
//! the next run. Accepted: the alternative is paying backend reads for
//! content the user has asked to delete.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::application::dtos::display_helpers::category_order_for;
use crate::domain::errors::DomainError;
use crate::infrastructure::repositories::pg::file_metadata_repository::FileMetadataRepository;
use crate::infrastructure::scheduler::{
    JobParam, JobRegistry, JobRunArgs, JobStore, JobStoreProvider, Mutates, ProgressKind,
    RecoverableJobHandler, RunOutcome, RunStatus, record_or_log,
};
use crate::infrastructure::services::audio_metadata_service::AudioMetadataService;
use crate::infrastructure::services::blob_handler::BlobHandler;
use crate::infrastructure::services::container_probe_service::{self, ContainerProbe};
use crate::infrastructure::services::media_metadata_service::MediaMetadataService;

pub const CONTENT_RESCAN_JOB_NAME: &str = "content_rescan";

/// Distinct contents per page. A "row" here is a network round-trip to the
/// storage backend, not a DB row, so this is two orders of magnitude below
/// a normal batch size.
const BATCH_SIZE: i64 = 100;

/// Bytes read per content to identify it. `infer`'s longest signature is
/// well inside this, and `CDC_MIN_CHUNK` (64 KiB) guarantees a first chunk
/// always covers it.
const HEAD_BYTES: u64 = 8192;

/// Ceiling on content this job will re-derive metadata from.
///
/// Re-deriving EXIF from a 4 GiB video means fetching and decrypting 4 GiB
/// to read a header. A constant rather than a parameter: the value that
/// matters is "not unbounded", and a knob here is one more branch to test
/// for a number nobody tunes. Oversized content is still RETYPED — the
/// sniff only ever reads the head — it is only the re-derive that is
/// skipped, with a finding.
const MAX_DERIVE_BYTES: i64 = 512 * 1024 * 1024;

/// How many files a finding names before it stops listing them.
///
/// Findings are keyed by CONTENT, and dedup means one content can carry
/// hundreds of files — a popular song, a shared PDF. `file_count` always
/// reports the true total; this only bounds the named sample, so a finding
/// stays readable and the row stays a sane size in `job_findings`.
const SAMPLE: usize = 10;

static PARAMETERS: [JobParam; 4] = [
    JobParam::boolean(
        "dry_run",
        false,
        "Report what would change without writing. The backend reads still \
         happen — the cost of the run is identical; only the UPDATEs and \
         re-derives are suppressed.",
    ),
    JobParam::string(
        "rederive",
        "When to re-extract metadata after sniffing: 'changed' (default) \
         only for content whose type actually moved, 'always' for every \
         content visited (the faithful replacement of the old reextract \
         endpoints — and the only mode that repairs a fixed EXTRACTOR, \
         since an extractor fix does not change the sniffed type), or \
         'never' to resniff only.",
    ),
    JobParam::boolean(
        "repair_divergence",
        false,
        "Converge copies of one content whose names imply the same type but \
         whose stored types differ. OFF by default: the only oracle left is \
         the filename, which is the WEAKER one — a specific client claim \
         beats an extension in exactly the ambiguous cases, so converging \
         can replace a right answer with a wrong one. Turn it on when you \
         know the corpus. Groups whose files have no extension are reported \
         either way, there being nothing to converge on.",
    ),
    JobParam::string(
        "mime_prefix",
        "Restrict the walk to content currently stored under this prefix, \
         e.g. 'application/octet-stream'. Narrows in SQL, so out-of-scope \
         content is never read from the backend.",
    ),
];

/// When to re-extract metadata. Parsed up front so a typo fails the run
/// instead of silently selecting the cheap mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rederive {
    Never,
    Changed,
    Always,
}

impl Rederive {
    fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None => Ok(Self::Changed),
            Some("changed") => Ok(Self::Changed),
            Some("always") => Ok(Self::Always),
            Some("never") => Ok(Self::Never),
            Some(other) => Err(format!(
                "unknown rederive mode {other:?} — expected 'changed', 'always' or 'never'"
            )),
        }
    }
}

/// Container formats whose signature identifies the WRAPPER, not the
/// payload. `infer` is right that a `.docx` is a zip; it is also useless,
/// because `mime_type` governs what a browser does on download.
fn generic_envelope(detected: &str) -> bool {
    matches!(
        detected,
        "application/zip"
            | "application/x-tar"
            | "application/gzip"
            | "application/x-ole-storage"
            | "text/xml"
    )
}

/// Is `candidate` a known specialisation of container `generic`?
///
/// Mirrors `common::mime_detect::specialises` exactly — the two layers
/// must agree or an upload and a later rescan will disagree about the same
/// bytes. Kept as a call into that module rather than a copy.
fn specialises(candidate: &str, generic: &str) -> bool {
    crate::common::mime_detect::specialises_for_rescan(candidate, generic)
}

/// What to do about one content's type.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// `infer` said nothing, or agrees with what is stored.
    Leave,
    /// Take this type for every file on the content.
    Retype(String),
    /// A container whose specialisation depends on the FILE name, so the
    /// answer differs per file rather than per content.
    RetypePerFile { fallback: String },
}

/// Decide from the sniffed type alone, per CONTENT.
///
/// `stored` is every distinct `mime_type` the files on this content carry;
/// a mismatch on any of them is a mismatch. Extracted as a free function
/// so the table below is testable without a database or a backend.
pub fn decide(detected: Option<&str>, stored: &[String]) -> Decision {
    // Silence is normal — text, CSV, SVG and every source file have no
    // magic bytes. Reporting it would bury the real findings.
    let Some(d) = detected else {
        return Decision::Leave;
    };

    if generic_envelope(d) {
        // The container is all the bytes can tell us. Whether that is the
        // final answer depends on each file's own name, so defer.
        return Decision::RetypePerFile {
            fallback: d.to_string(),
        };
    }

    if stored.iter().all(|s| s == d) {
        return Decision::Leave;
    }
    Decision::Retype(d.to_string())
}

/// Resolve a container detection for ONE file, using its name.
///
/// This is the single place a filename legitimately re-enters a
/// content-keyed job: a blob shared by `report.docx` and `backup.zip` is
/// genuinely both, and the extension is the only thing that distinguishes
/// them. Keep it narrow — it is why the fan-out degrades to one statement
/// per file for container detections, and only for those.
pub fn resolve_container(name: &str, fallback: &str) -> String {
    let guessed = mime_guess::from_path(name).first_raw();
    match guessed {
        Some(g) if specialises(g, fallback) => g.to_string(),
        _ => fallback.to_string(),
    }
}

#[derive(Debug, sqlx::FromRow)]
struct ContentRow {
    content_hash: String,
    file_ids: Vec<Uuid>,
    file_names: Vec<String>,
    /// Per-file current values, aligned with `file_ids` by the shared
    /// `ORDER BY f.id`. Needed because `category_order` can be stale while
    /// `mime_type` is correct — see `desired_rows`.
    file_mimes: Vec<String>,
    file_categories: Vec<i16>,
    stored_types: Vec<String>,
    size: i64,
}

pub struct ContentRescan {
    pool: Arc<PgPool>,
    dedup: Arc<BlobHandler>,
    audio: Option<Arc<AudioMetadataService>>,
    media: Arc<MediaMetadataService>,
    /// Resolves audio-vs-video for the four container types where magic
    /// bytes cannot. `None` when ffprobe is absent — the container
    /// default then stands and a later run picks it up, the same posture
    /// video thumbnails already take.
    probe: Option<Arc<ContainerProbe>>,
    /// Tier-1 scratch for the probe's tempfile. ffprobe needs a real
    /// path, and an MP4's `moov` atom may be at the END of the file, so
    /// the head this job already read is not enough.
    temp_dir: PathBuf,
}

impl ContentRescan {
    pub fn new(
        pool: Arc<PgPool>,
        dedup: Arc<BlobHandler>,
        audio: Option<Arc<AudioMetadataService>>,
        media: Arc<MediaMetadataService>,
        probe: Option<Arc<ContainerProbe>>,
        temp_dir: PathBuf,
    ) -> Self {
        Self {
            pool,
            dedup,
            audio,
            media,
            probe,
            temp_dir,
        }
    }

    /// Resolve the audio-vs-video ambiguity for one content, returning
    /// the corrected type when the probe disagrees with `mime`.
    ///
    /// Unlike every other correction in this job, this one is NOT driven
    /// by a disagreement between detected and stored: an audio-only MP4
    /// is stored as `video/mp4` and `infer` says `video/mp4` too, so the
    /// two agree and are both wrong. The probe therefore has to run on
    /// types that look settled — which is why it is keyed on the
    /// EFFECTIVE type rather than on a mismatch.
    ///
    /// Cost is honest: a full blob fetch per ambiguous content per run,
    /// because `moov` can be at the end of the file. Bounded by the four
    /// types in the table and by `MAX_DERIVE_BYTES`.
    async fn resolve_container_ambiguity(
        &self,
        hash: &str,
        mime: &str,
        size: i64,
    ) -> Result<Option<&'static str>, DomainError> {
        let Some(probe) = &self.probe else {
            return Ok(None);
        };
        if !container_probe_service::is_ambiguous_container(mime) || size > MAX_DERIVE_BYTES {
            return Ok(None);
        }

        let named = self
            .dedup
            .stream_blob_to_tempfile(hash, &self.temp_dir, ".media")
            .await?;
        let has_video = probe.has_video_stream(named.path()).await?;
        drop(named);

        Ok(container_probe_service::corrected_mime(mime, has_video))
    }

    pub async fn register_recoverable_job(
        self: Arc<Self>,
        registry: &JobRegistry,
        provider: &Arc<dyn JobStoreProvider>,
    ) -> Arc<Self> {
        registry
            .register_recoverable_job(self.clone(), provider.clone(), None)
            .await;
        self
    }

    /// One page of distinct content, with the fan-out set attached.
    ///
    /// `array_agg` hands over the file ids and names the per-file
    /// container resolution and the `category_order` recompute both need,
    /// so neither costs a second query. Ordering inside the aggregates is
    /// pinned so `file_ids[i]` and `file_names[i]` describe the same row.
    const PAGE_SQL: &'static str = "
        SELECT f.blob_hash                        AS content_hash,
               array_agg(f.id   ORDER BY f.id)             AS file_ids,
               array_agg(f.name ORDER BY f.id)             AS file_names,
               array_agg(f.mime_type ORDER BY f.id)        AS file_mimes,
               array_agg(f.category_order ORDER BY f.id)   AS file_categories,
               array_agg(DISTINCT f.mime_type)             AS stored_types,
               max(f.size)                        AS size
          FROM storage.files f
         WHERE ($1::text IS NULL OR f.blob_hash > $1)
           AND ($3::text IS NULL OR f.mime_type LIKE $3 || '%')
           AND NOT f.is_trashed
         GROUP BY f.blob_hash
         ORDER BY f.blob_hash
         LIMIT $2";

    /// Apply one content's new type to every file on it.
    ///
    /// Binds the HASH, not the file ids: re-reading under the UPDATE's own
    /// snapshot picks up files inserted since the page was fetched, and
    /// `IS DISTINCT FROM` makes it idempotent.
    ///
    /// `category_order` moves in the SAME statement, and that is not
    /// tidiness. Since the search type filter landed, that column is what
    /// the filter READS — so a row retyped without it becomes invisible
    /// under its own category (a `.bin` resniffed to `audio/mpeg` keeps
    /// 9999 "Other" and never appears under Audio). Two statements would
    /// leave a window, and a cancel between them would make it permanent.
    ///
    /// It has to be a per-row VALUES join because `category_order_for`
    /// takes `(name, mime)` — the classifier falls back to the extension
    /// when the mime is generic, so the value is not computable per
    /// content. That is transitional: once mime is the source of truth the
    /// classifier loses its `name` arm and this reverts to the plain
    /// per-hash shape.
    ///
    /// `updated_at` is deliberately NOT touched — the content did not
    /// change, only our description of it, and bumping it would make every
    /// WebDAV and sync client re-fetch the corpus.
    async fn apply_types(&self, rows: &[(Uuid, String, i16)]) -> Result<u64, DomainError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let ids: Vec<Uuid> = rows.iter().map(|(id, _, _)| *id).collect();
        let mimes: Vec<String> = rows.iter().map(|(_, m, _)| m.clone()).collect();
        let orders: Vec<i16> = rows.iter().map(|(_, _, c)| *c).collect();

        let res = sqlx::query(
            "UPDATE storage.files f
                SET mime_type      = v.mime,
                    category_order = v.cat
               FROM (SELECT * FROM unnest($1::uuid[], $2::text[], $3::smallint[])
                       AS t(id, mime, cat)) v
              WHERE f.id = v.id
                AND (f.mime_type IS DISTINCT FROM v.mime
                     OR f.category_order IS DISTINCT FROM v.cat)",
        )
        .bind(&ids)
        .bind(&mimes)
        .bind(&orders)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("ContentRescan", format!("apply types: {e}")))?;

        Ok(res.rows_affected())
    }

    /// Re-extract metadata for one content, dispatching on its CURRENT
    /// type. Carries no extraction logic of its own — both arms are a
    /// single call into a service that is already content-keyed.
    async fn rederive_one(&self, hash: &str, mime: &str) -> Result<bool, DomainError> {
        if AudioMetadataService::is_audio_file(mime) {
            let Some(audio) = &self.audio else {
                return Ok(false);
            };
            audio.extract_and_save(hash).await?;
            return Ok(true);
        }
        if MediaMetadataService::handles(mime) {
            self.media.extract_and_save(hash, mime).await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Drop metadata that the new type says cannot exist.
    ///
    /// The case this exists for: content stored as `audio/mpeg` that
    /// resniffs to something that is not audio at all. Re-deriving would
    /// correctly extract nothing — and therefore leave the OLD row in
    /// place, because `extract_and_save` skips the upsert when the parse
    /// yields nothing. So a file that stopped being a song would keep its
    /// title and artist forever, and the playlist UI would go on showing
    /// them.
    ///
    /// Deleting per table rather than "delete everything then re-extract"
    /// keeps the common case (audio stays audio) free of a pointless
    /// delete, and means a transient failure cannot leave content with no
    /// metadata at all.
    ///
    /// ⚠ Each predicate asks **"can this table legitimately hold a row
    /// for this type?"**, NOT "does today's extractor handle it". The
    /// distinction is load-bearing and will bite: document creation dates
    /// are coming, and a `.docx` date belongs in `storage.content_metadata`
    /// exactly as an EXIF date does. Asking the extractor would answer
    /// "no" for documents and delete them corpus-wide on the next sweep.
    /// So the question belongs to the table's owner —
    /// `FileMetadataRepository::applies_to` — and extending the extractor
    /// without extending that predicate is the bug to watch for.
    async fn drop_stale_metadata(&self, hash: &str, new_mime: &str) -> Result<(), DomainError> {
        if !AudioMetadataService::is_audio_file(new_mime)
            && let Some(audio) = &self.audio
        {
            audio.delete_metadata_for_content(hash).await?;
        }
        if !FileMetadataRepository::applies_to(new_mime) {
            self.media.delete_metadata_for_content(hash).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl RecoverableJobHandler for ContentRescan {
    fn name(&self) -> &str {
        CONTENT_RESCAN_JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Re-identifies stored content from its magic bytes, corrects the \
         stored MIME type (and the category it sorts and filters under), \
         and re-extracts metadata for what changed. Walks each distinct \
         content once rather than once per file."
    }

    /// Unlike its siblings, this job writes by DEFAULT rather than behind
    /// `?repair=true` — it is routine maintenance, like `uploads_cleanup`,
    /// and a resniff that reported without correcting would just be a
    /// slower way to produce a list nobody can act on in bulk. `dry_run`
    /// is the escape hatch, and it is the inverse polarity of the rest of
    /// the family: here the SAFE mode is the one you have to ask for.
    fn mutates(&self) -> Mutates {
        Mutates::Always
    }

    fn parameters(&self) -> &'static [JobParam] {
        &PARAMETERS
    }

    /// `true` **despite writing nothing to the backend**, and that is
    /// deliberate — do not "correct" it.
    ///
    /// The flag's literal wording is "issues PUT/DELETE"; its actual job
    /// is to gate the dispatch prologue on the backend write lock, i.e.
    /// "must not run while the backend is under exclusive maintenance".
    /// This job qualifies on that meaning for two independent reasons:
    ///
    /// * it READS the head of every distinct content in the corpus, and
    ///   under `rederive` streams whole blobs — competing for the exact
    ///   bandwidth a migration or rotation is saturating, and risking
    ///   reads against a backend mid-cutover;
    /// * it rewrites `mime_type` and `category_order` corpus-wide, which
    ///   is the last thing wanted while the storage layer underneath is
    ///   itself being rewritten.
    ///
    /// The gate covers admin-triggered runs as well as scheduled ticks
    /// (`registry::trigger` passes it to `dispatch`), which matters here
    /// because this job has no interval and ONLY ever runs on demand —
    /// returning `false` would have left it completely ungated in
    /// practice.
    fn is_backend_writer(&self) -> bool {
        true
    }

    /// **Never resume unattended.** The default keys on `repair`/`deep`;
    /// this job declares neither, so with `Mutates::Always` it would
    /// inherit `true` and a paused full-corpus sweep could be restarted by
    /// a scheduled tick with nobody watching. A run that reads every blob
    /// on a metered backend is one a human should choose to continue.
    fn unattended_resume_allowed(&self, _args: &JobRunArgs) -> bool {
        false
    }

    /// Proxy, not a definitive count: `count_total` takes no args, so it
    /// cannot see `mime_prefix`, and a scoped run would otherwise show a
    /// bar stuck at 3 %.
    fn progress_kind(&self) -> ProgressKind {
        ProgressKind::Approximate
    }

    async fn count_total(&self) -> Option<u64> {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(DISTINCT blob_hash) FROM storage.files WHERE NOT is_trashed",
        )
        .fetch_one(self.pool.as_ref())
        .await
        .ok()
        .map(|n| n as u64)
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        let dry_run = args.get_bool("dry_run");
        let repair_divergence = args.get_bool("repair_divergence");
        let mime_prefix = args.get_str("mime_prefix").map(|s| s.to_string());

        // Parsed BEFORE any work: silently defaulting a typo'd `alwyas` to
        // `changed` would make a requested full backfill quietly do
        // nothing and report success.
        let rederive = match Rederive::parse(args.get_str("rederive")) {
            Ok(r) => r,
            Err(message) => return RunOutcome::Failed { message },
        };

        let mut cursor: Option<String> = match resume_cursor {
            Some(bytes) if !bytes.is_empty() => match String::from_utf8(bytes) {
                Ok(s) => Some(s),
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("cursor is not valid UTF-8: {e}"),
                    };
                }
            },
            _ => None,
        };

        let mut head_reads = 0u64;
        let mut mime_changed = 0u64;
        let mut files_retyped = 0u64;
        let mut rederived = 0u64;
        let mut failures = 0u64;
        let mut divergences = 0u64;

        loop {
            match store.status().await {
                Ok(RunStatus::CancelRequested) => {
                    return RunOutcome::Paused {
                        cursor: cursor.map(|c| c.into_bytes()).unwrap_or_default(),
                    };
                }
                Ok(_) => {}
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("status: {e}"),
                    };
                }
            }

            let rows: Vec<ContentRow> = match sqlx::query_as(Self::PAGE_SQL)
                .bind(cursor.as_deref())
                .bind(BATCH_SIZE)
                .bind(mime_prefix.as_deref())
                .fetch_all(self.pool.as_ref())
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return RunOutcome::from_domain_error(
                        cursor.as_ref().map(|c| c.as_bytes()),
                        "content page",
                        &DomainError::internal_error("ContentRescan", e.to_string()),
                    );
                }
            };
            if rows.is_empty() {
                break;
            }

            // One manifest query for the whole page: CDC content reads its
            // head from the first CHUNK (bounded decrypt), legacy content
            // from the Blob hash itself.
            let hashes: Vec<String> = rows.iter().map(|r| r.content_hash.clone()).collect();
            let manifests: HashMap<String, (String, i64)> =
                match self.dedup.first_chunk_hashes(&hashes).await {
                    Ok(m) => m,
                    Err(e) => {
                        return RunOutcome::from_domain_error(
                            cursor.as_ref().map(|c| c.as_bytes()),
                            "resolve manifests",
                            &e,
                        );
                    }
                };

            // The last content this page reached a conclusion about.
            // Everything a pause reports must be at or before it, so a
            // resumed run redoes the unconcluded one rather than skipping
            // it — a skipped content would be silently never re-sniffed.
            let mut settled: Option<String> = cursor.clone();

            for row in &rows {
                let object_hash = manifests
                    .get(&row.content_hash)
                    .map(|(first, _)| first.as_str())
                    .unwrap_or(row.content_hash.as_str());

                // Zero-byte content: nothing to read, and nothing to sniff.
                // `size` is `max(f.size)` over the group, and every file on
                // one hash has the same content, so `== 0` is exact rather
                // than a heuristic.
                //
                // ⚠ This has to come BEFORE the read, not as recovery after
                // a failed one. A chunkless manifest has no first chunk, so
                // the `unwrap_or` above falls back to the LEGACY whole-file
                // path and asks the backend for an object named after the
                // empty BLAKE3 hash (`af1349b9…`) — which was never stored,
                // because storing zero bytes stores nothing. The backend
                // answers `no such key`: a PERMANENT error, so the sweep
                // recorded `blob_head_unreadable` against a file that is
                // perfectly intact. That is the one finding an operator must
                // never be trained to ignore, and the branch `continue`s, so
                // the row silently lost its category backfill as well.
                //
                // Falling through with an empty head rather than skipping the
                // row is deliberate: `infer` yields nothing, `decide` leaves
                // the type alone, and the category backfill below still runs
                // — which is the only work an empty file actually needs.
                let head = if row.size == 0 {
                    bytes::Bytes::new()
                } else {
                    match self
                        .dedup
                        .read_object_head_uncached(object_hash, HEAD_BYTES)
                        .await
                    {
                        Ok(h) => {
                            head_reads += 1;
                            h
                        }
                        // ⚠ A TRANSIENT failure stops the run; it must never
                        // become a finding. If the backend is unreachable,
                        // every content in the corpus fails identically — so
                        // treating it per-row would emit thousands of
                        // findings and then report Completed, i.e. a run that
                        // re-sniffed nothing claiming success. Pause at the
                        // last settled content instead, and let a human
                        // decide when the backend is back.
                        Err(e) if e.is_transient() => {
                            return RunOutcome::from_domain_error(
                                settled.as_ref().map(|c| c.as_bytes()),
                                "content head read",
                                &e,
                            );
                        }
                        // A PERMANENT error is about this one object — the
                        // bytes are gone or corrupt — so it is a finding and
                        // the sweep carries on.
                        Err(e) => {
                            failures += 1;
                            record_or_log(
                                store,
                                CONTENT_RESCAN_JOB_NAME,
                                "blob_head_unreadable",
                                // `anomaly`, not `data_loss`: files_consistency
                                // owns `missing_blob` and is the authority for
                                // that claim. Two jobs disagreeing about the
                                // same condition is worse than one
                                // under-claiming.
                                "anomaly",
                                None,
                                serde_json::json!({
                                    "content_hash": row.content_hash,
                                    "object_hash":  object_hash,
                                    "file_count":   row.file_ids.len(),
                                    // ⚠ Named, not just counted. Two hashes
                                    // and a count cannot be acted on, and
                                    // this is the finding that says bytes may
                                    // be MISSING — so the only question the
                                    // operator has is WHICH files. Capped the
                                    // same way as the sample below.
                                    "files": row
                                        .file_ids
                                        .iter()
                                        .zip(row.file_names.iter())
                                        .take(SAMPLE)
                                        .map(|(id, name)| {
                                            serde_json::json!({
                                                "file_id": id.to_string(),
                                                "name":    name,
                                            })
                                        })
                                        .collect::<Vec<_>>(),
                                    "error":        e.to_string(),
                                }),
                            )
                            .await;
                            // Concluded, unhappily: this content was examined
                            // and found unreadable. A resumed run should not
                            // retry it forever.
                            settled = Some(row.content_hash.clone());
                            continue;
                        }
                    }
                };

                let detected = infer::get(&head).map(|k| k.mime_type());
                let decision = decide(detected, &row.stored_types);

                // ⚠ Divergence the NAMES do not explain.
                //
                // Copies of one content are allowed to disagree. A zip
                // shared by `report.docx` and `backup.zip` genuinely is
                // both — that is what `RetypePerFile` exists for — and
                // `data.json` beside `data.txt` is two right answers from
                // one byte string. So `stored_types.len() > 1` is NOT the
                // predicate; it would fire on the carve-out the job was
                // built around.
                //
                // What should not happen is two files whose NAMES imply the
                // same type holding different stored types. Gated on
                // `Leave` because the other decisions resolve it: a real
                // detection converges every copy (and reports that as
                // `mime_type_mismatch`), while a generic envelope is
                // SUPPOSED to diverge per name. Silence is the case where
                // nothing will ever reconcile them.
                //
                // Repair is OPT-IN (`repair_divergence`), because picking a
                // winner can make things worse. The only oracle left is the
                // name, and the name is the WEAKER one: step 2 of
                // `refine_content_type` prefers a specific client claim
                // precisely because the extension is wrong in the ambiguous
                // cases. Two files named `notes.ts`, one stored
                // `text/typescript` (a correct claim) and one `video/mp2t`
                // (what `mime_guess` says), converge to `video/mp2t` — both
                // wrong where one was right. So the default reports, and
                // repair is something an operator asks for knowing the
                // corpus. Discovery-only unless repair is requested is the
                // house rule.
                let mut divergence_repairs: Vec<(Uuid, String)> = Vec::new();
                if matches!(decision, Decision::Leave) && row.stored_types.len() > 1 {
                    let mut by_name: HashMap<Option<String>, Vec<usize>> = HashMap::new();
                    for (i, name) in row.file_names.iter().enumerate() {
                        by_name
                            .entry(crate::common::mime_detect::type_from_name(name))
                            .or_default()
                            .push(i);
                    }

                    let unexplained = by_name.iter().filter(|(_, at)| {
                        at.iter()
                            .filter_map(|i| row.file_mimes.get(*i))
                            .collect::<std::collections::BTreeSet<_>>()
                            .len()
                            > 1
                    });

                    let mut groups: Vec<serde_json::Value> = Vec::new();
                    for (implies, at) in unexplained {
                        // Only a name that says something can settle the
                        // group. With no extension there is nothing to
                        // converge ON, so those stay reported-only even
                        // under `repair_divergence`.
                        if let Some(target) = implies.as_deref()
                            && repair_divergence
                        {
                            for i in at {
                                if row.file_mimes.get(*i).map(String::as_str) != Some(target)
                                    && let Some(id) = row.file_ids.get(*i)
                                {
                                    divergence_repairs.push((*id, target.to_string()));
                                }
                            }
                        }
                        groups.push(serde_json::json!({
                            "name_implies": implies,
                            "files": at.iter().take(SAMPLE).map(|i| serde_json::json!({
                                "file_id": row.file_ids.get(*i).map(|u| u.to_string()),
                                "name":    row.file_names.get(*i),
                                "stored":  row.file_mimes.get(*i),
                            })).collect::<Vec<_>>(),
                        }));
                    }

                    if !groups.is_empty() {
                        divergences += 1;
                        record_or_log(
                            store,
                            CONTENT_RESCAN_JOB_NAME,
                            "stored_type_divergence",
                            // `anomaly`: surprising, no known impact, and
                            // nothing is unreachable. Not `inconsistent`,
                            // which alerts through `FindingNotifier` —
                            // claim-vs-extension ordering differs across
                            // upload paths, so first-run volume on a real
                            // corpus is unknown and a kind that pages on
                            // sight would train operators to mute the
                            // channel. Promote it once the volume is known
                            // to be small.
                            "anomaly",
                            None,
                            serde_json::json!({
                                "content_hash": row.content_hash,
                                "file_count":   row.file_ids.len(),
                                "groups":       groups,
                                // False both when repair was not asked for
                                // and when it was but no group had a name
                                // to converge on.
                                "corrected": !divergence_repairs.is_empty() && !dry_run,
                            }),
                        )
                        .await;
                    }
                }

                // Resolve to a concrete (file, mime) list. Non-container
                // detections give every file the same answer; container
                // detections are the one case where the answer is per-file,
                // because the extension is the only signal separating
                // `report.docx` from `backup.zip` on identical bytes.
                let mut targets: Vec<(Uuid, String, i16)> = Vec::new();
                let mut container_disambiguated = false;
                // True when the only thing being corrected is the
                // denormalised category, with every mime already right.
                let mut category_only = false;
                match &decision {
                    Decision::Leave => {}
                    Decision::Retype(mime) => {
                        for (id, name) in row.file_ids.iter().zip(row.file_names.iter()) {
                            targets.push((*id, mime.clone(), category_order_for(name, mime)));
                        }
                    }
                    Decision::RetypePerFile { fallback } => {
                        container_disambiguated = true;
                        for (id, name) in row.file_ids.iter().zip(row.file_names.iter()) {
                            let mime = resolve_container(name, fallback);
                            let cat = category_order_for(name, &mime);
                            targets.push((*id, mime, cat));
                        }
                    }
                }

                // Divergence repair rides the same statement as everything
                // else. It has to be merged BEFORE the category-only
                // backfill below, which rebuilds targets from
                // `row.file_mimes` — the pre-repair values — and would
                // otherwise put the diverged type straight back.
                let divergence_repaired = !divergence_repairs.is_empty();
                for (id, mime) in divergence_repairs {
                    let name = row
                        .file_ids
                        .iter()
                        .position(|f| *f == id)
                        .and_then(|i| row.file_names.get(i))
                        .map(String::as_str)
                        .unwrap_or_default();
                    let cat = category_order_for(name, &mime);
                    targets.push((id, mime, cat));
                }

                // A container detection that resolves to exactly what is
                // already stored everywhere is not news.
                if container_disambiguated
                    && targets
                        .iter()
                        .all(|(_, m, _)| row.stored_types.iter().all(|s| s == m))
                {
                    targets.clear();
                }

                // ── Stale `category_order` with a CORRECT mime ───────
                //
                // `category_order = f(name, mime)`, so it goes stale when
                // either input moves — and also when the CLASSIFIER moves,
                // which is the case nothing else can repair. Example: the
                // 2026-10-24 fix that made `text/plain` source files Code
                // instead of Text changed the answer for every `.c`, `.cpp`
                // and `.py` row in the corpus without changing one mime.
                //
                // So the retype set cannot be keyed on "did the mime
                // change". Rebuild it from what actually DIFFERS per row,
                // leaving the mime alone where it is already right. Without
                // this the job silently refuses to backfill its own
                // classifier, and the search type filter — which reads this
                // column, not the mime — keeps hiding those files under the
                // wrong category.
                if targets.is_empty() {
                    for (i, id) in row.file_ids.iter().enumerate() {
                        let Some(name) = row.file_names.get(i) else {
                            continue;
                        };
                        let Some(mime) = row.file_mimes.get(i) else {
                            continue;
                        };
                        let want = category_order_for(name, mime);
                        if row.file_categories.get(i) != Some(&want) {
                            targets.push((*id, mime.clone(), want));
                        }
                    }
                    // Not a mime finding — the type was right all along.
                    category_only = !targets.is_empty();
                }

                // ── Container ambiguity ──────────────────────────────
                // Runs on the EFFECTIVE type, not on a mismatch: an
                // audio-only MP4 is stored `video/mp4` and detected
                // `video/mp4`, so everything above agrees and both are
                // wrong. Only a stream probe can tell, so this is the
                // one correction that fires on content the rest of the
                // job considers settled.
                let effective = targets
                    .first()
                    .map(|(_, m, _)| m.clone())
                    .or_else(|| row.stored_types.first().cloned())
                    .unwrap_or_default();

                let mut container_corrected = false;
                match self
                    .resolve_container_ambiguity(&row.content_hash, &effective, row.size)
                    .await
                {
                    Ok(Some(fixed)) => {
                        container_corrected = true;
                        // Per CONTENT, not per file: whether a stream
                        // table holds video is a property of the bytes,
                        // so unlike the envelope case the filename has
                        // no say.
                        targets = row
                            .file_ids
                            .iter()
                            .zip(row.file_names.iter())
                            .map(|(id, name)| {
                                (*id, fixed.to_string(), category_order_for(name, fixed))
                            })
                            .collect();
                    }
                    Ok(None) => {}
                    Err(e) if e.is_transient() => {
                        return RunOutcome::from_domain_error(
                            settled.as_ref().map(|c| c.as_bytes()),
                            "container probe",
                            &e,
                        );
                    }
                    Err(e) => {
                        // A probe that cannot read this one file is not
                        // a reason to abandon the sweep, and not a
                        // reason to guess: the container default stands.
                        failures += 1;
                        record_or_log(
                            store,
                            CONTENT_RESCAN_JOB_NAME,
                            "container_probe_failed",
                            "anomaly",
                            None,
                            serde_json::json!({
                                "content_hash": row.content_hash,
                                "mime_type":    effective,
                                "error":        e.to_string(),
                                "note": "audio-vs-video left at the container default",
                            }),
                        )
                        .await;
                    }
                }

                if targets.is_empty() {
                    if rederive == Rederive::Always {
                        let mime = row.stored_types.first().cloned().unwrap_or_default();
                        if row.size <= MAX_DERIVE_BYTES && !dry_run {
                            match self.rederive_one(&row.content_hash, &mime).await {
                                Ok(true) => rederived += 1,
                                Ok(false) => {}
                                // Same rule as the head read: the
                                // extractor streams the blob, so a
                                // backend outage here would otherwise
                                // fail every content in turn.
                                Err(e) if e.is_transient() => {
                                    return RunOutcome::from_domain_error(
                                        settled.as_ref().map(|c| c.as_bytes()),
                                        "re-derive",
                                        &e,
                                    );
                                }
                                Err(e) => {
                                    failures += 1;
                                    record_or_log(
                                        store,
                                        CONTENT_RESCAN_JOB_NAME,
                                        "rederive_failed",
                                        "anomaly",
                                        None,
                                        serde_json::json!({
                                            "content_hash": row.content_hash,
                                            "mime_type":    mime,
                                            "error":        e.to_string(),
                                        }),
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                    settled = Some(row.content_hash.clone());
                    continue;
                }

                // `mime_changed` counts CONTENT re-identified, so a
                // category-only backfill must not inflate it — otherwise a
                // classifier change reads as a corpus-wide mis-detection.
                if !category_only {
                    mime_changed += 1;
                }
                let new_mime = targets[0].1.clone();

                let affected = if dry_run {
                    0
                } else {
                    match self.apply_types(&targets).await {
                        Ok(n) => {
                            files_retyped += n;
                            n
                        }
                        // The write is the job's whole point, so a failure
                        // here is never a finding to walk past — if the
                        // database is refusing writes, continuing would
                        // read the entire corpus and change nothing while
                        // reporting success.
                        Err(e) => {
                            return RunOutcome::from_domain_error(
                                settled.as_ref().map(|c| c.as_bytes()),
                                "apply types",
                                &e,
                            );
                        }
                    }
                };

                // `anomaly`, not `inconsistent`: a resniffed type is NEW
                // INFORMATION about the bytes, not a contradiction between
                // two stores. The old value was the client's best guess and
                // was never promised to be right. Severity gating would
                // otherwise page an operator for every correctly-working
                // sweep.
                record_or_log(
                    store,
                    CONTENT_RESCAN_JOB_NAME,
                    // Two different events, so two different kinds. A
                    // category backfill is NOT a mime mismatch: the stored
                    // type was right, and filing it as one produced findings
                    // where `previous_types` and `applied` were identical —
                    // "something mismatched" with no visible difference. An
                    // operator filtering for real mis-detections got a
                    // corpus-wide classifier change mixed in.
                    if category_only {
                        "category_order_refreshed"
                    } else {
                        "mime_type_mismatch"
                    },
                    "anomaly",
                    None,
                    serde_json::json!({
                        "content_hash": row.content_hash,
                        "detected":     detected,
                        // Was `stored`, which did not read as "the value
                        // before this run".
                        "previous_types": row.stored_types,
                        "applied":      new_mime,
                        "file_count":   row.file_ids.len(),
                        // The per-file before→after. `detected` and
                        // `previous_types` describe the CONTENT; this names
                        // which file moved and from what — including the
                        // CATEGORY, which is the only thing that moves on a
                        // backfill and was previously absent from the payload
                        // entirely.
                        //
                        // Capped: a song with 400 copies must not produce a
                        // 400-entry finding, but naming a few makes it
                        // actionable. Only files that actually changed appear.
                        "changes": targets
                            .iter()
                            .take(SAMPLE)
                            .map(|(id, mime, cat)| {
                                let at = row.file_ids.iter().position(|f| f == id);
                                serde_json::json!({
                                    "file_id":       id.to_string(),
                                    "name":          at.and_then(|i| row.file_names.get(i)),
                                    "from_type":     at.and_then(|i| row.file_mimes.get(i)),
                                    "to_type":       mime,
                                    "from_category": at.and_then(|i| row.file_categories.get(i)),
                                    "to_category":   cat,
                                })
                            })
                            .collect::<Vec<_>>(),
                        "files_updated": affected,
                        "corrected":     !dry_run,
                        "reason": if divergence_repaired {
                            // Not `magic_bytes`: no signature was involved.
                            // The name broke a tie the bytes could not.
                            "divergence_repair"
                        } else if category_only {
                            // The mime was already right; only the
                            // denormalised category had drifted, which
                            // happens when the classifier changes.
                            "category_backfill"
                        } else if container_corrected {
                            // Resolved by reading the stream table, not
                            // the signature. Both show up as a changed
                            // container type and have different causes,
                            // so an operator reading findings needs them
                            // distinguished.
                            "stream_probe"
                        } else if container_disambiguated {
                            "container_extension"
                        } else {
                            "magic_bytes"
                        },
                    }),
                )
                .await;

                let wants_rederive =
                    matches!(rederive, Rederive::Always | Rederive::Changed) && !dry_run;

                // Runs regardless of `rederive`, including under `never`:
                // this is not deriving anything, it is removing a claim
                // the new type contradicts. Leaving a stale artist on
                // content that is no longer audio is a correctness bug,
                // not a missed optimisation.
                if !dry_run
                    && let Err(e) = self.drop_stale_metadata(&row.content_hash, &new_mime).await
                {
                    if e.is_transient() {
                        return RunOutcome::from_domain_error(
                            settled.as_ref().map(|c| c.as_bytes()),
                            "drop stale metadata",
                            &e,
                        );
                    }
                    failures += 1;
                    record_or_log(
                        store,
                        CONTENT_RESCAN_JOB_NAME,
                        "rederive_failed",
                        "anomaly",
                        None,
                        serde_json::json!({
                            "content_hash": row.content_hash,
                            "mime_type":    new_mime,
                            "stage":        "drop_stale_metadata",
                            "error":        e.to_string(),
                        }),
                    )
                    .await;
                }

                if wants_rederive {
                    if row.size > MAX_DERIVE_BYTES {
                        record_or_log(
                            store,
                            CONTENT_RESCAN_JOB_NAME,
                            "rederive_failed",
                            "anomaly",
                            None,
                            serde_json::json!({
                                "content_hash": row.content_hash,
                                "size":         row.size,
                                "limit":        MAX_DERIVE_BYTES,
                                "reason": "content exceeds MAX_DERIVE_BYTES; retyped but not \
                                           re-derived",
                            }),
                        )
                        .await;
                    } else {
                        match self.rederive_one(&row.content_hash, &new_mime).await {
                            Ok(true) => rederived += 1,
                            Ok(false) => {}
                            Err(e) if e.is_transient() => {
                                return RunOutcome::from_domain_error(
                                    settled.as_ref().map(|c| c.as_bytes()),
                                    "re-derive",
                                    &e,
                                );
                            }
                            Err(e) => {
                                failures += 1;
                                record_or_log(
                                    store,
                                    CONTENT_RESCAN_JOB_NAME,
                                    "rederive_failed",
                                    "anomaly",
                                    None,
                                    serde_json::json!({
                                        "content_hash": row.content_hash,
                                        "mime_type":    new_mime,
                                        "error":        e.to_string(),
                                    }),
                                )
                                .await;
                            }
                        }
                    }
                }

                // Concluded: the retype is committed and any re-derive
                // has been attempted. A pause from here on may report
                // this hash.
                settled = Some(row.content_hash.clone());
            }

            let scanned = rows.len() as u64;
            let last = rows.last().unwrap().content_hash.clone();
            cursor = Some(last.clone());

            // Counters every batch, absolute not delta: `extra_stats` on
            // Completed only covers the segment since the last resume, so a
            // run that paused once would otherwise under-report.
            let _ = store
                .checkpoint_counters(&serde_json::Map::from_iter([
                    ("head_reads".to_string(), head_reads.into()),
                    ("mime_changed".to_string(), mime_changed.into()),
                    ("files_retyped".to_string(), files_retyped.into()),
                    ("rederived".to_string(), rederived.into()),
                    ("failures".to_string(), failures.into()),
                    ("divergences".to_string(), divergences.into()),
                ]))
                .await;

            if let Err(e) = store.checkpoint(last.into_bytes(), scanned).await {
                return RunOutcome::Failed {
                    message: format!("checkpoint: {e}"),
                };
            }
            if scanned < BATCH_SIZE as u64 {
                break;
            }
        }

        tracing::info!(
            target: "oxicloud::jobs",
            event = "content_rescan.completed",
            run_id = %store.run_id(),
            dry_run = dry_run,
            head_reads = head_reads,
            mime_changed = mime_changed,
            files_retyped = files_retyped,
            rederived = rederived,
            failures = failures,
            divergences = divergences,
            "content_rescan completed: {} content re-identified, {} file(s) retyped",
            mime_changed,
            files_retyped,
        );

        RunOutcome::Completed {
            extra_stats: serde_json::Map::from_iter([
                ("head_reads".to_string(), head_reads.into()),
                ("mime_changed".to_string(), mime_changed.into()),
                ("files_retyped".to_string(), files_retyped.into()),
                ("rederived".to_string(), rederived.into()),
                ("failures".to_string(), failures.into()),
                ("divergences".to_string(), divergences.into()),
            ]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// Silence is the common case, not a finding: text, CSV, SVG and every
    /// source file have no magic bytes. A job that reported each one would
    /// bury its real output under the majority of the corpus.
    #[test]
    fn silence_leaves_the_stored_type_alone() {
        assert_eq!(decide(None, &s(&["text/plain"])), Decision::Leave);
        assert_eq!(
            decide(None, &s(&["application/octet-stream"])),
            Decision::Leave
        );
    }

    #[test]
    fn agreement_is_not_a_change() {
        assert_eq!(
            decide(Some("image/png"), &s(&["image/png"])),
            Decision::Leave
        );
    }

    /// Specific → specific is a retype, and this is the case the write
    /// policy exists to settle: the stored value was a client's claim, the
    /// detected one is what the bytes say.
    #[test]
    fn a_specific_disagreement_is_a_retype() {
        assert_eq!(
            decide(Some("image/png"), &s(&["image/jpeg"])),
            Decision::Retype("image/png".into()),
        );
        assert_eq!(
            decide(Some("audio/mpeg"), &s(&["application/octet-stream"])),
            Decision::Retype("audio/mpeg".into()),
        );
    }

    /// One content, several files, disagreeing types — any mismatch is a
    /// mismatch, or the odd row out never gets fixed.
    #[test]
    fn partial_agreement_across_copies_still_retypes() {
        assert_eq!(
            decide(Some("image/png"), &s(&["image/png", "image/jpeg"])),
            Decision::Retype("image/png".into()),
        );
    }

    /// A container defers, because the bytes identify the WRAPPER and the
    /// final answer depends on each file's own name.
    #[test]
    fn containers_defer_to_the_per_file_arm() {
        assert_eq!(
            decide(Some("application/zip"), &s(&["application/zip"])),
            Decision::RetypePerFile {
                fallback: "application/zip".into()
            },
        );
    }

    /// The container table, mirroring `mime_detect`'s so the two layers
    /// cannot drift: an upload and a later rescan must agree about the
    /// same bytes.
    #[test]
    fn container_resolution_uses_the_extension_only_to_specialise() {
        assert!(
            resolve_container("report.docx", "application/zip")
                .ends_with("wordprocessingml.document")
        );
        assert!(
            resolve_container("sheet.xlsx", "application/zip").ends_with("spreadsheetml.sheet")
        );
        assert_eq!(
            resolve_container("book.epub", "application/zip"),
            "application/epub+zip"
        );

        // ⚠ The inverse, and the point of the group: the rule must not
        // stop an archive being an archive, and must not hand the
        // filename authority it was never given.
        assert_eq!(
            resolve_container("backup.zip", "application/zip"),
            "application/zip"
        );
        assert_eq!(
            resolve_container("archive", "application/zip"),
            "application/zip"
        );
        // An unrelated extension must NOT win — otherwise a renamed file
        // could claim any type it liked, which is the exact authority the
        // content-first sniffing commit took away from the client.
        assert_eq!(
            resolve_container("evil.png", "application/zip"),
            "application/zip"
        );
    }

    /// A typo must fail the run, not select the cheap mode. Asking for
    /// `always` and silently getting `changed` would make a requested full
    /// backfill do nothing and report success.
    #[test]
    fn rederive_rejects_a_typo_by_name() {
        assert_eq!(Rederive::parse(None).unwrap(), Rederive::Changed);
        assert_eq!(Rederive::parse(Some("always")).unwrap(), Rederive::Always);
        assert_eq!(Rederive::parse(Some("never")).unwrap(), Rederive::Never);
        let err = Rederive::parse(Some("alwyas")).unwrap_err();
        assert!(
            err.contains("alwyas"),
            "the error must name the typo: {err}"
        );
    }

    /// Documents MAY hold content facts — the tripwire, flipped.
    ///
    /// `drop_stale_metadata` DELETES a content's `storage.content_metadata`
    /// row when `applies_to` says that type cannot have one. This test used
    /// to assert documents *could not*, as a reminder for whoever landed a
    /// document extractor: get it wrong and the job silently deletes every
    /// document's metadata, corpus-wide, on every sweep, with nothing
    /// logged as an error.
    ///
    /// It fired on exactly the change it was written for (2026-10, document
    /// creation dates) and is now inverted to pin the other direction. The
    /// stronger guard lives beside the predicate itself —
    /// `file_metadata_repository::every_type_the_extractor_handles_may_legitimately_keep_its_row`
    /// asserts `handles` implies `applies_to` for every type, so it cannot
    /// go stale the way a hardcoded list does. This one stays because it
    /// names the consequence from the deleting side.
    #[test]
    fn documents_may_hold_content_facts() {
        use crate::infrastructure::repositories::pg::file_metadata_repository::FileMetadataRepository as R;

        assert!(R::applies_to("image/jpeg"));
        assert!(R::applies_to("video/mp4"));

        for mime in [
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/vnd.oasis.opendocument.text",
            "application/pdf",
        ] {
            assert!(
                R::applies_to(mime),
                "⚠ {mime} is extracted but `applies_to` rejects it, so \
                 content_rescan will DELETE every row of that type on its \
                 next sweep — silently, because nothing errors and the rows \
                 simply stop existing",
            );
        }

        // And content with no document or media shape still cannot, so the
        // predicate did not simply become `true`.
        assert!(!R::applies_to("application/zip"));
        assert!(!R::applies_to("application/octet-stream"));
    }

    /// The job must defer while the backend write lock is held, even
    /// though it writes nothing to the backend.
    ///
    /// Pinned because the flag reads like a lie: the trait calls it
    /// "issues PUT/DELETE", this job issues neither, and the obvious
    /// tidy-up is to set it `false`. Doing so would leave the job
    /// completely ungated — it has no interval, so the admin trigger is
    /// the only path, and that path consults the same gate — free to
    /// stream the entire corpus while a migration is moving it.
    #[tokio::test]
    async fn it_defers_to_the_backend_write_lock() {
        let pool = Arc::new(
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
                .expect("lazy pool never connects"),
        );
        let job = ContentRescan::new(
            pool.clone(),
            Arc::new(BlobHandler::new_for_test(pool.clone())),
            None,
            Arc::new(MediaMetadataService::new(
                pool.clone(),
                Arc::new(BlobHandler::new_for_test(pool.clone())),
                std::env::temp_dir(),
            )),
            None,
            std::env::temp_dir(),
        );
        assert!(
            job.is_backend_writer(),
            "content_rescan must be gated by the backend write lock",
        );
        // And it must never be resumed by a scheduled tick: a paused
        // full-corpus sweep restarting unattended is a surprise bill.
        assert!(!job.unattended_resume_allowed(&JobRunArgs::default()));
    }

    /// The name must never end in `_consistency`:
    /// `consistency_batch_service` auto-enrols any such job into the weekly
    /// batch, and this one can cost a full-corpus backend read.
    #[test]
    fn the_name_stays_out_of_the_weekly_batch() {
        assert!(!CONTENT_RESCAN_JOB_NAME.ends_with("_consistency"));
    }
}

// Requires a test PG on 5433 with `oxicloud_test`, schema applied via
// `tests/common/init-test-schema.sh`:
//   just test-integration content_rescan
//
// Everything that matters here needs a database AND real bytes on a
// backend: the `unnest` fan-out, the alignment between `file_ids` and
// `file_names` inside `array_agg`, the keyset cursor, and the dedup
// guarantee that N files on one content cost ONE head read. None of it
// is reachable from a unit test, and the job rewrites `mime_type`
// corpus-wide by default — so "it compiles" is a long way from "it is
// safe to trigger".
#[cfg(integration_tests)]
#[allow(dead_code, unused_imports)]
mod integration_tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    async fn test_pool() -> Arc<sqlx::PgPool> {
        let url = crate::integration_test_support::test_db_url();
        Arc::new(
            PgPoolOptions::new()
                .max_connections(4)
                .connect(&url)
                .await
                .expect("connect to test DB — run tests/common/spawn-db.sh first"),
        )
    }

    /// A real PNG on the backend, plus THREE files on that one content —
    /// all claiming `image/jpeg`, all with the Other category (9999).
    ///
    /// Three rather than one is the point: it is what makes
    /// `head_reads == 1` meaningful, and it is the exact waste the two
    /// file-iterating endpoints this job replaces could not avoid.
    async fn seed(
        pool: &sqlx::PgPool,
        dedup: &BlobHandler,
        tag: &str,
    ) -> (String, Vec<Uuid>, Uuid, Uuid) {
        // ⚠ Clear this tag's rows FIRST. `cleanup` runs at the end of a
        // test, so a failed assertion leaks its fixtures, and
        // `init-test-schema.sh` only applies migrations — it never clears
        // data. A leaked row then points at a backend object held in a
        // previous process's temp dir, so the next sweep gets `no such key`
        // and files `blob_head_unreadable` against it: one failure used to
        // make every later run fail for an unrelated reason.
        for tagged in [
            format!("c7{tag}{}", "0".repeat(64 - 2 - tag.len())),
            format!("e0{tag}{}", "0".repeat(64 - 2 - tag.len())),
            format!("d1{tag}{}", "0".repeat(64 - 2 - tag.len())),
        ] {
            let _ = sqlx::query("DELETE FROM storage.files WHERE blob_hash = $1")
                .bind(&tagged)
                .execute(pool)
                .await;
        }

        // Smallest valid PNG: signature + IHDR. `infer` keys off the
        // 8-byte signature, so this is genuinely detected rather than
        // pattern-matched by length.
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89,
        ];
        let hash = format!("c7{tag}{}", "0".repeat(64 - 2 - tag.len()));

        // The local backend shards by hash prefix and does not create
        // parents on write — `initialize()` is what lays out the 256
        // shard directories, and the boot flow calls it. A test that
        // skips it fails with a bare ENOENT from deep inside the write.
        dedup
            .backend()
            .initialize()
            .await
            .expect("initialize the test backend");

        dedup
            .backend()
            .put_blob_from_bytes(&hash, bytes::Bytes::from(png.to_vec()))
            .await
            .expect("seed blob bytes");

        // Legacy whole-file shape (no manifest), so the job reads the
        // head from the content hash directly — the `first_chunk_hashes`
        // miss path.
        sqlx::query(
            "INSERT INTO storage.blobs (hash, size, ref_count)
             VALUES ($1, $2, 3) ON CONFLICT (hash) DO NOTHING",
        )
        .bind(&hash)
        .bind(png.len() as i64)
        .execute(pool)
        .await
        .expect("seed blobs row");

        // Upsert, not insert. `cleanup` removes files, manifests and blobs
        // — never the user or the drive — so a plain INSERT made this test
        // pass exactly ONCE per database. It only looked reliable because
        // `spawn-db.sh` does `down -v` before `up -d`, handing every
        // `just test-integration` a fresh volume; run it twice against one
        // database and the second fails on `users_username_key`, which is
        // how a real diagnosis gets mistaken for leftover junk.
        let user: Uuid = sqlx::query_scalar(
            "INSERT INTO auth.users (id, username, email, password_hash, role,
                                     storage_quota_bytes, storage_used_bytes)
             VALUES ($1, $2, $3, 'x', 'user', 0, 0)
             ON CONFLICT (username) DO UPDATE SET email = EXCLUDED.email
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(format!("cr-{tag}"))
        .bind(format!("cr-{tag}@test.local"))
        .fetch_one(pool)
        .await
        .expect("seed user");

        // Lookup-then-insert rather than `ON CONFLICT`: the drive is found
        // by `default_for_user` (never by name — see the house rule), and
        // `DO NOTHING` would leave `drive` holding a UUID that was never
        // written, failing the files insert on its FK instead.
        let drive: Uuid = match sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM storage.drives WHERE default_for_user = $1",
        )
        .bind(user)
        .fetch_optional(pool)
        .await
        .expect("look up the drive")
        {
            Some(existing) => existing,
            None => {
                let fresh = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO storage.drives (id, kind, default_for_user)
                     VALUES ($1, 'personal', $2)",
                )
                .bind(fresh)
                .bind(user)
                .execute(pool)
                .await
                .expect("seed drive");
                fresh
            }
        };

        let mut ids = Vec::new();
        for i in 0..3 {
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO storage.files
                     (id, name, drive_id, blob_hash, mime_type, size,
                      category_order, created_by, updated_by)
                 VALUES ($1, $2, $3, $4, 'image/jpeg', $5, 9999, $6, $6)",
            )
            .bind(id)
            .bind(format!("shot-{tag}-{i}.bin"))
            .bind(drive)
            .bind(&hash)
            .bind(png.len() as i64)
            .bind(user)
            .execute(pool)
            .await
            .expect("seed file");
            ids.push(id);
        }
        (hash, ids, user, drive)
    }

    /// Copies of one content that disagree about their type, in both the
    /// legitimate and the illegitimate shape.
    ///
    /// The bytes are plain text, so `infer` is silent and `decide` returns
    /// `Leave` — nothing will ever reconcile the copies. Three files:
    ///
    /// * `div-{tag}-a.ini` stored `text/plain`
    /// * `div-{tag}-b.ini` stored `application/x-wine-extension-ini`
    /// * `div-{tag}-c.json` stored `application/json`
    ///
    /// `a` and `b` are the defect: identical names-implied type, two stored
    /// types, and no byte-level signal to settle it. `c` must NOT be
    /// reported — a different extension on identical bytes is two correct
    /// answers, the same reason `report.docx` may sit beside `backup.zip`.
    ///
    /// Returns the content hash.
    async fn seed_divergent(
        pool: &sqlx::PgPool,
        dedup: &BlobHandler,
        tag: &str,
        user: Uuid,
        drive: Uuid,
    ) -> String {
        let text: &[u8] = b"[section]\nkey = value\n";
        let hash = format!("d1{tag}{}", "0".repeat(64 - 2 - tag.len()));

        dedup
            .backend()
            .initialize()
            .await
            .expect("initialize the test backend");
        dedup
            .backend()
            .put_blob_from_bytes(&hash, bytes::Bytes::from(text.to_vec()))
            .await
            .expect("seed blob bytes");

        sqlx::query(
            "INSERT INTO storage.blobs (hash, size, ref_count)
             VALUES ($1, $2, 3) ON CONFLICT (hash) DO NOTHING",
        )
        .bind(&hash)
        .bind(text.len() as i64)
        .execute(pool)
        .await
        .expect("seed blobs row");

        for (name, mime) in [
            (format!("div-{tag}-a.ini"), "text/plain"),
            (
                format!("div-{tag}-b.ini"),
                "application/x-wine-extension-ini",
            ),
            (format!("div-{tag}-c.json"), "application/json"),
        ] {
            sqlx::query(
                "INSERT INTO storage.files
                     (id, name, drive_id, blob_hash, mime_type, size,
                      category_order, created_by, updated_by)
                 VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, 950, $6, $6)",
            )
            .bind(name)
            .bind(drive)
            .bind(&hash)
            .bind(mime)
            .bind(text.len() as i64)
            .bind(user)
            .execute(pool)
            .await
            .expect("seed divergent file");
        }

        hash
    }

    /// A manifest with NO chunks, which is what a zero-byte file has.
    ///
    /// `chunk_hashes[1]` on an empty array is NULL, and decoding that as a
    /// `String` made `first_chunk_hashes` fail — taking the whole sweep
    /// with it, because the manifest probe runs once per PAGE. So a single
    /// empty file stopped the job for the entire corpus. Found on a real
    /// instance, not here: the seeded fixtures all had content.
    ///
    /// The SECOND defect on the same fixture: once the chunkless manifest
    /// was correctly dropped from the probe's map, the per-row `unwrap_or`
    /// fell back to the legacy whole-file path and asked the backend for an
    /// object named after the empty hash. Nothing was ever stored there —
    /// storing zero bytes stores nothing — so the backend said `no such
    /// key`, a PERMANENT error, and the sweep filed `blob_head_unreadable`
    /// against an intact file. Also found on a real instance.
    ///
    /// The file is seeded `.ini` with a stale `category_order` of 9999 so
    /// the backfill is observable: an empty `.ini` is `text/plain`/Config,
    /// and Config is 950. A `.bin` would have classified as Unknown, which
    /// IS 9999, and the assertion would have passed without the fix.
    ///
    /// Returns a hash referenced by one file, so the walk actually visits
    /// it.
    async fn seed_chunkless(pool: &sqlx::PgPool, tag: &str, user: Uuid, drive: Uuid) -> String {
        let hash = format!("e0{tag}{}", "0".repeat(64 - 2 - tag.len()));
        sqlx::query(
            "INSERT INTO storage.chunk_manifests
                 (file_hash, chunk_hashes, chunk_sizes, total_size, chunk_count, ref_count)
             VALUES ($1, '{}', '{}', 0, 0, 1)
             ON CONFLICT (file_hash) DO NOTHING",
        )
        .bind(&hash)
        .execute(pool)
        .await
        .expect("seed chunkless manifest");

        sqlx::query(
            "INSERT INTO storage.files
                 (id, name, drive_id, blob_hash, mime_type, size,
                  category_order, created_by, updated_by)
             VALUES (gen_random_uuid(), $1, $2, $3, 'text/plain', 0,
                     9999, $4, $4)",
        )
        .bind(format!("empty-{tag}.ini"))
        .bind(drive)
        .bind(&hash)
        .bind(user)
        .execute(pool)
        .await
        .expect("seed chunkless file");

        hash
    }

    /// The database's clock, for scoping finding queries to this test run.
    ///
    /// The DB clock, not the app's: the two are known to disagree on this
    /// deployment (`bug_job_run_timestamps_two_clocks`), and
    /// `run_findings.created_at` defaults to `NOW()`, so a cutoff taken from
    /// the app could sit after rows the run is about to write.
    async fn db_now(pool: &sqlx::PgPool) -> chrono::DateTime<chrono::Utc> {
        sqlx::query_scalar("SELECT now()")
            .fetch_one(pool)
            .await
            .expect("read the database clock")
    }

    /// How many findings of one kind this test run has filed against one
    /// content.
    ///
    /// **Scoping to the run is the only honest way to assert "this run
    /// reported X".** The job sweeps the WHOLE corpus, so its run counters
    /// — `failures`, `mime_changed`, `head_reads`, `files_retyped`,
    /// `divergences` — describe the entire test database, not this test's
    /// fixtures. They inflate whenever another test leaves a row in
    /// `storage.files`, and `tests/common/init-test-schema.sh` only applies
    /// migrations: it never clears data, so a fixture leaked by an earlier
    /// FAILED run (cleanup runs last, after the assertions) persists and
    /// poisons every later run. Worse, such a row points at a backend object
    /// from a different process's temp dir, so the sweep gets `no such key`
    /// and files `blob_head_unreadable` — a corpus-wide `failures == 0` then
    /// fails forever, for a reason unrelated to whatever broke first.
    ///
    /// Unscoped counts are no good either: `cleanup` removes files, not
    /// findings, and `jobs.run_findings` rows outlive it via their run — so
    /// a test run against a database that has seen an earlier one counts
    /// that one's findings too. Hence `since`.
    async fn finding_count(
        pool: &sqlx::PgPool,
        hash: &str,
        kind: &str,
        since: chrono::DateTime<chrono::Utc>,
    ) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM jobs.run_findings
              WHERE kind = $1 AND detail->>'content_hash' = $2
                AND created_at >= $3",
        )
        .bind(kind)
        .bind(hash)
        .bind(since)
        .fetch_one(pool)
        .await
        .expect("count findings")
    }

    /// The distinct finding KINDS filed against one content, each with its
    /// most recent detail.
    ///
    /// Distinct-on rather than every row, for two reasons: the dry-run phase
    /// files its own findings (with `corrected: false`), and `jobs.run_findings`
    /// outlives `cleanup`, which only removes the seeded files. Asserting on
    /// a row count would pass once and then drift.
    ///
    /// `since` is load-bearing for the same reason: without it an earlier
    /// run's phase-4 repair finding shows up among phase 2's kinds, so the
    /// test passes exactly once per database.
    async fn findings_for(
        pool: &sqlx::PgPool,
        hash: &str,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Vec<(String, serde_json::Value)> {
        sqlx::query_as(
            "SELECT DISTINCT ON (kind) kind, detail FROM jobs.run_findings
              WHERE detail->>'content_hash' = $1 AND created_at >= $2
              ORDER BY kind, created_at DESC",
        )
        .bind(hash)
        .bind(since)
        .fetch_all(pool)
        .await
        .expect("read findings")
    }

    async fn category_of(pool: &sqlx::PgPool, hash: &str) -> i16 {
        sqlx::query_scalar("SELECT category_order FROM storage.files WHERE blob_hash = $1")
            .bind(hash)
            .fetch_one(pool)
            .await
            .expect("read back category")
    }

    async fn types_and_categories(pool: &sqlx::PgPool, ids: &[Uuid]) -> Vec<(String, i16)> {
        sqlx::query_as("SELECT mime_type, category_order FROM storage.files WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(pool)
            .await
            .expect("read back types")
    }

    async fn run_job(pool: &Arc<sqlx::PgPool>, args: &JobRunArgs) -> serde_json::Value {
        let provider: Arc<dyn JobStoreProvider> = Arc::new(
            crate::infrastructure::scheduler::PgJobStoreProvider::new(pool.clone()),
        );
        let dedup = Arc::new(BlobHandler::new_for_test(pool.clone()));
        let media = Arc::new(MediaMetadataService::new(
            pool.clone(),
            dedup.clone(),
            std::env::temp_dir(),
        ));
        let handler: Arc<dyn RecoverableJobHandler> = Arc::new(ContentRescan::new(
            pool.clone(),
            dedup,
            None,
            media,
            None,
            std::env::temp_dir(),
        ));
        let outcome =
            crate::infrastructure::scheduler::run_or_resume(handler, provider, args).await;
        assert!(outcome.is_ok(), "run must complete: {outcome:?}");

        let crate::infrastructure::scheduler::JobOutcome::Ok { extra, .. } = outcome else {
            panic!("expected Ok");
        };
        // `is_ok()` alone is not proof: `open_or_start` permits one
        // non-terminal run per job, so a caller arriving while another
        // run is open gets a successful "already running" envelope with
        // no dispatch at all — indistinguishable from a clean run that
        // found nothing.
        assert_eq!(
            extra.get("completed").and_then(|v| v.as_bool()),
            Some(true),
            "the run did not dispatch — another run was probably still open: {extra}",
        );
        extra
    }

    /// Counters live at `extra.extra_stats.<key>`, not at `extra.<key>`:
    /// the recoverable engine nests a handler's `extra_stats` one level
    /// down inside the outcome envelope. Reading the wrong level yields
    /// `None`, which an `assert_eq!(…, Some(0))` happily accepts — so
    /// this helper exists to keep every assertion honest about where the
    /// number comes from.
    fn stat(extra: &serde_json::Value, key: &str) -> Option<u64> {
        extra.get("extra_stats")?.get(key)?.as_u64()
    }

    async fn cleanup(pool: &sqlx::PgPool, hash: &str) {
        let _ = sqlx::query("DELETE FROM storage.files WHERE blob_hash = $1")
            .bind(hash)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM storage.chunk_manifests WHERE file_hash = $1")
            .bind(hash)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM storage.blobs WHERE hash = $1")
            .bind(hash)
            .execute(pool)
            .await;
    }

    /// All phases in ONE test, for the reason `satellites_consistency`
    /// documents: cargo runs tests in parallel, `open_or_start` permits
    /// one non-terminal run per job name, and the loser attaches to the
    /// winner's run rather than dispatching — so a second `#[tokio::test]`
    /// would pass without ever executing.
    #[tokio::test]
    async fn it_retypes_every_copy_from_one_read_and_moves_the_category() {
        let pool = test_pool().await;
        let dedup = BlobHandler::new_for_test(pool.clone());
        let (hash, ids, user, drive) = seed(&pool, &dedup, "a1").await;
        // One zero-byte file in the same corpus. Its manifest has no
        // chunks, which used to abort the entire sweep.
        let empty_hash = seed_chunkless(&pool, "a1", user, drive).await;
        // And one content whose copies disagree about their type — in both
        // the legitimate and the illegitimate shape. Folded into THIS test
        // rather than its own: the engine permits one non-terminal run per
        // job, so a second test calling `run_job` concurrently gets an
        // `already_running` envelope with no dispatch, and whichever run
        // does dispatch sweeps the other's fixtures and miscounts.
        let div_hash = seed_divergent(&pool, &dedup, "a1", user, drive).await;

        // Every assertion below counts findings filed SINCE here, because
        // the run counters are corpus-wide and the findings table outlives
        // `cleanup` — see `finding_count`.
        let t0 = db_now(&pool).await;

        // ── Phase 1: dry_run reports and writes nothing ──────────────
        let args = JobRunArgs::new(
            [(
                "dry_run".to_string(),
                crate::infrastructure::scheduler::JobParamValue::Boolean(true),
            )]
            .into_iter()
            .collect(),
        );
        let extra = run_job(&pool, &args).await;
        assert_eq!(
            stat(&extra, "files_retyped"),
            Some(0),
            "a dry run wrote something",
        );
        for (mime, cat) in types_and_categories(&pool, &ids).await {
            assert_eq!(mime, "image/jpeg", "dry run changed a stored type");
            assert_eq!(cat, 9999, "dry run changed a category");
        }

        // ── Phase 2: the real run ────────────────────────────────────
        // No binding: phase 2 asserts entirely through findings scoped to
        // its own content hashes, never the corpus-wide run counters.
        run_job(&pool, &JobRunArgs::default()).await;

        for (mime, cat) in types_and_categories(&pool, &ids).await {
            assert_eq!(mime, "image/png", "the stored type was not corrected");
            // 100 = Image. The whole reason this moves in the same
            // statement: the search type filter reads this column, so a
            // retyped row that kept 9999 would be invisible under its
            // own category.
            assert_eq!(cat, 100, "category_order did not follow the mime");
        }
        // THE dedup guarantee, and the reason this job walks content rather
        // than files: ONE conclusion covering three files.
        //
        // Asserted through the finding rather than `head_reads`, which is
        // corpus-wide — see `finding_count`. `file_count: 3` with
        // `files_updated: 3` on a single finding says the same thing and
        // says it about OUR content: the job reached one verdict and fanned
        // it out, instead of reading the bytes once per file.
        // TWO, not one: the dry run in phase 1 reported it as well. That is
        // the contract — `dry_run` suppresses the UPDATEs, not the findings,
        // which is the whole reason a preview is useful.
        assert_eq!(
            finding_count(&pool, &hash, "mime_type_mismatch", t0).await,
            2,
            "the content was not re-identified once per run (dry + real)",
        );

        // ⚠ The zero-byte file is INTACT, so it must produce no
        // `blob_head_unreadable` at all — and its stale category must still
        // be refreshed. Before the fix the backend was asked for an object
        // that was never stored, which filed that finding against a healthy
        // file and `continue`d past the backfill, so the row reappeared on
        // every subsequent run.
        //
        // Scoped to the empty content's own hash. The run's `failures`
        // counter is a catch-all over blob_head_unreadable /
        // container_probe_failed / rederive_failed for the ENTIRE corpus, so
        // asserting it is 0 made this test fail whenever any other row in
        // the database was unreadable — which a leaked fixture guarantees.
        assert_eq!(
            finding_count(&pool, &empty_hash, "blob_head_unreadable", t0).await,
            0,
            "the zero-byte content was reported as unreadable",
        );
        assert_eq!(
            category_of(&pool, &empty_hash).await,
            950,
            "the empty file's stale category was never refreshed (Config = 950)",
        );

        // The two events must not share a kind. A category backfill was
        // filed as `mime_type_mismatch`, which produced findings whose
        // before and after were IDENTICAL — "something mismatched" with no
        // visible difference — and mixed a corpus-wide classifier change
        // into the kind an operator filters for real mis-detections.
        let empty = findings_for(&pool, &empty_hash, t0).await;
        assert_eq!(
            empty.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["category_order_refreshed"],
            "a category-only backfill was filed under the wrong kind",
        );
        let change = &empty[0].1["changes"][0];
        assert_eq!(change["from_category"], 9999);
        assert_eq!(change["to_category"], 950);
        assert_eq!(
            change["from_type"], change["to_type"],
            "a category backfill must not claim the type moved",
        );

        // And a real mis-detection keeps the original kind, naming the file
        // and BOTH types. `file_count` alone cannot be acted on.
        let retyped = findings_for(&pool, &hash, t0).await;
        assert_eq!(
            retyped.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["mime_type_mismatch"],
        );
        // One finding, three files, three rows written — the fan-out.
        assert_eq!(retyped[0].1["file_count"], 3);
        assert_eq!(retyped[0].1["files_updated"], 3);
        let change = &retyped[0].1["changes"][0];
        assert_eq!(change["from_type"], "image/jpeg");
        assert_eq!(change["to_type"], "image/png");
        assert!(
            change["name"].is_string(),
            "the finding named no file: {change}",
        );

        // ── Divergence the names cannot explain ──────────────────────
        assert_eq!(
            finding_count(&pool, &div_hash, "stored_type_divergence", t0).await,
            2,
            "the unexplained divergence was not reported once per run (dry + real)",
        );
        let found = findings_for(&pool, &div_hash, t0).await;
        assert_eq!(
            found.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["stored_type_divergence"],
        );

        // ⚠ ONE group. The `.json` copy also disagrees with the other two,
        // and reporting it would be the false positive the predicate exists
        // to avoid: a different extension on identical bytes is two right
        // answers, the same reason `report.docx` may sit beside
        // `backup.zip`.
        let groups = found[0].1["groups"].as_array().expect("groups array");
        assert_eq!(
            groups.len(),
            1,
            "a legitimately-divergent copy was reported: {groups:?}",
        );
        let stored: Vec<&str> = groups[0]["files"]
            .as_array()
            .expect("files array")
            .iter()
            .map(|f| f["stored"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(stored.len(), 2, "the reported group was not the .ini pair");
        assert!(
            stored.contains(&"text/plain") && stored.contains(&"application/x-wine-extension-ini"),
            "the group named the wrong files: {stored:?}",
        );

        // Report only BY DEFAULT: converging means picking a winner, and
        // the name is the weaker oracle — a specific claim beats an
        // extension in exactly the ambiguous cases, so a default repair
        // could replace a right answer with a wrong one.
        let divergent_types = async |pool: &Arc<sqlx::PgPool>| -> Vec<String> {
            sqlx::query_scalar(
                "SELECT mime_type FROM storage.files
                  WHERE blob_hash = $1 ORDER BY name",
            )
            .bind(&div_hash)
            .fetch_all(pool.as_ref())
            .await
            .expect("read back the divergent copies")
        };
        assert_eq!(
            divergent_types(&pool).await,
            vec![
                "text/plain",
                "application/x-wine-extension-ini",
                "application/json",
            ],
            "the job picked a winner without being asked to",
        );

        // ── Phase 3: idempotent ──────────────────────────────────────
        //
        // Scoped to our own contents, not the run counters: a second run
        // must report nothing NEW about them, and must not rewrite rows that
        // already agree — `IS DISTINCT FROM` not holding would churn the
        // tree ETag on every sweep.
        let before = types_and_categories(&pool, &ids).await;
        run_job(&pool, &JobRunArgs::default()).await;
        assert_eq!(
            finding_count(&pool, &hash, "mime_type_mismatch", t0).await,
            2,
            "a second run reported the content as mismatched again",
        );
        assert_eq!(
            finding_count(&pool, &empty_hash, "category_order_refreshed", t0).await,
            2,
            "a second run backfilled the category again",
        );
        assert_eq!(
            types_and_categories(&pool, &ids).await,
            before,
            "a second run rewrote rows that already agreed",
        );

        // ── Phase 4: repair_divergence converges the group ───────────
        //
        // Only the two `.ini` copies move, and they move to what their
        // NAME implies — not to whichever stored value happened to look
        // more specific. The `.json` copy is untouched: its divergence was
        // explained by its own name, so it was never in a reported group
        // and must not be dragged along by a repair of its neighbours.
        let args = JobRunArgs::new(
            [(
                "repair_divergence".to_string(),
                crate::infrastructure::scheduler::JobParamValue::Boolean(true),
            )]
            .into_iter()
            .collect(),
        );
        run_job(&pool, &args).await;
        assert_eq!(
            finding_count(&pool, &div_hash, "stored_type_divergence", t0).await,
            4,
            "the repairing run did not report the divergence it acted on",
        );
        assert_eq!(
            finding_count(&pool, &div_hash, "mime_type_mismatch", t0).await,
            1,
            "the repair itself was not reported",
        );
        assert_eq!(
            divergent_types(&pool).await,
            vec![
                // `div-a.ini`, `div-b.ini` — both now the name's answer,
                // which for `.ini` is `text/plain`. Note the winner is NOT
                // the more specific-looking stored value: the group
                // converges on what the NAME says, so
                // `application/x-wine-extension-ini` loses to it.
                "text/plain",
                "text/plain",
                // `div-c.json`, untouched — its divergence was explained by
                // its own name, so it was never in a reported group.
                "application/json",
            ],
            "repair_divergence did not converge the unexplained group",
        );

        // The finding says so, and names the reason the types moved
        // without any signature being involved.
        let repaired = findings_for(&pool, &div_hash, t0).await;
        let by_kind = |k: &str| {
            repaired
                .iter()
                .find(|(kind, _)| kind == k)
                .map(|(_, d)| d.clone())
                .unwrap_or_default()
        };
        assert_eq!(by_kind("stored_type_divergence")["corrected"], true);
        assert_eq!(by_kind("mime_type_mismatch")["reason"], "divergence_repair");

        // And it is now settled: a further run reports nothing new about
        // this content, which is the point of repairing rather than
        // reporting forever.
        run_job(&pool, &JobRunArgs::default()).await;
        assert_eq!(
            finding_count(&pool, &div_hash, "stored_type_divergence", t0).await,
            4,
            "the divergence was reported again after being repaired",
        );

        cleanup(&pool, &hash).await;
        cleanup(&pool, &empty_hash).await;
        cleanup(&pool, &div_hash).await;
    }

    /// A typo must fail the run before any work, not silently select the
    /// cheap mode — otherwise a requested full backfill does nothing and
    /// reports success.
    #[tokio::test]
    async fn an_unknown_rederive_mode_fails_the_run() {
        let pool = test_pool().await;
        let provider: Arc<dyn JobStoreProvider> = Arc::new(
            crate::infrastructure::scheduler::PgJobStoreProvider::new(pool.clone()),
        );
        let dedup = Arc::new(BlobHandler::new_for_test(pool.clone()));
        let media = Arc::new(MediaMetadataService::new(
            pool.clone(),
            dedup.clone(),
            std::env::temp_dir(),
        ));
        let handler: Arc<dyn RecoverableJobHandler> = Arc::new(ContentRescan::new(
            pool.clone(),
            dedup,
            None,
            media,
            None,
            std::env::temp_dir(),
        ));
        let args = JobRunArgs::new(
            [(
                "rederive".to_string(),
                crate::infrastructure::scheduler::JobParamValue::String(Some("alwyas".to_string())),
            )]
            .into_iter()
            .collect(),
        );
        let outcome =
            crate::infrastructure::scheduler::run_or_resume(handler, provider, &args).await;
        assert!(
            !outcome.is_ok(),
            "a typo'd rederive mode ran anyway: {outcome:?}",
        );
    }
}
