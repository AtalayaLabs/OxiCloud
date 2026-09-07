//! Azure Blob Storage Backend — stores blobs in an Azure Storage container.
//!
//! Authenticates via Account Name + Account Key (or SAS token).
//! Blob key scheme mirrors local/S3: `{2-char-prefix}/{hash}.blob`.

use std::path::{Path, PathBuf};
use std::pin::Pin;

use azure_storage::StorageCredentials;
use azure_storage_blobs::prelude::*;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use tokio::fs;

use crate::application::ports::blob_storage_ports::{
    BackendBlobEntry, BackendUnknownEntry, BlobListPage, BlobStorageBackend, BlobStream,
    StorageHealthStatus,
};
use crate::common::config::AzureStorageConfig;
use crate::domain::errors::{DomainError, ErrorKind};

/// Azure Blob Storage backend.
pub struct AzureBlobBackend {
    container_client: ContainerClient,
    container_name: String,
}

impl AzureBlobBackend {
    /// Build a new Azure backend from configuration.
    pub fn new(config: &AzureStorageConfig) -> Self {
        let credentials = if let Some(ref sas) = config.sas_token {
            StorageCredentials::sas_token(sas).expect("Invalid SAS token")
        } else {
            StorageCredentials::access_key(&config.account_name, config.account_key.clone())
        };

        // Custom endpoint (Azurite emulator / private deployment /
        // benches) mirrors S3's `endpoint_url`; default is the public
        // cloud URL derived from the account name.
        let container_client = match &config.endpoint_url {
            Some(uri) => ClientBuilder::with_location(
                azure_storage::CloudLocation::Custom {
                    account: config.account_name.clone(),
                    uri: uri.trim_end_matches('/').to_string(),
                },
                credentials,
            )
            .container_client(&config.container),
            None => ClientBuilder::new(&config.account_name, credentials)
                .container_client(&config.container),
        };

        Self {
            container_client,
            container_name: config.container.clone(),
        }
    }

    /// Inverse of [`Self::blob_name`] — the hash a listing entry names,
    /// or `None` when the entry is not one of ours.
    ///
    /// Mirrors `S3BlobBackend::hash_from_object_key`, including the check
    /// that the shard equals the hash's own first two characters: without
    /// it, `blob_name(hash)` would not reproduce the name we just parsed,
    /// and a mis-sharded object would be reported as a live blob that no
    /// read path can find.
    fn hash_from_blob_name(name: &str) -> Option<String> {
        let (prefix, rest) = name.split_once('/')?;
        if prefix.len() != 2 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let stem = rest.strip_suffix(".blob")?;
        if stem.len() != 64 || !stem.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        if !stem.starts_with(prefix) {
            return None;
        }
        Some(stem.to_string())
    }

    /// Compute the blob name for a given hash.
    fn blob_name(hash: &str) -> String {
        let prefix = &hash[0..2];
        format!("{prefix}/{hash}.blob")
    }

    /// Get a `BlobClient` for a given hash.
    fn blob_client(&self, hash: &str) -> BlobClient {
        self.container_client.blob_client(Self::blob_name(hash))
    }
}

impl BlobStorageBackend for AzureBlobBackend {
    fn initialize(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        Box::pin(async move {
            // Verify container exists by getting its properties
            self.container_client.get_properties().await.map_err(|e| {
                DomainError::internal_error(
                    "Azure",
                    format!("Cannot access container '{}': {}", self.container_name, e),
                )
            })?;

            tracing::info!(
                "Azure blob backend initialized: container={}",
                self.container_name
            );
            Ok(())
        })
    }

    fn put_blob(
        &self,
        hash: &str,
        source_path: &Path,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        let source_path = source_path.to_owned();
        Box::pin(async move {
            let client = self.blob_client(&hash);

            // Check if blob already exists (idempotent)
            if client.get_properties().await.is_ok() {
                let file_size = fs::metadata(&source_path)
                    .await
                    .map_err(|e| {
                        DomainError::internal_error(
                            "Azure",
                            format!("Failed to stat source file: {e}"),
                        )
                    })?
                    .len();
                let _ = fs::remove_file(&source_path).await;
                return Ok(file_size);
            }

            // Read file and upload as block blob
            let data = fs::read(&source_path).await.map_err(|e| {
                DomainError::internal_error("Azure", format!("Failed to read source: {e}"))
            })?;
            let file_size = data.len() as u64;

            client.put_block_blob(data).await.map_err(|e| {
                DomainError::internal_error("Azure", format!("Failed to upload blob {hash}: {e}"))
            })?;

            let _ = fs::remove_file(&source_path).await;
            Ok(file_size)
        })
    }

    fn put_blob_from_bytes(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let client = self.blob_client(&hash);
            let size = data.len() as u64;

            // Idempotent: skip if exists
            if client.get_properties().await.is_ok() {
                return Ok(size);
            }

            // `Bytes` converts into `azure_core::Body` by reference count —
            // the old `data.to_vec()` copied every chunk once more.
            client.put_block_blob(data).await.map_err(|e| {
                DomainError::internal_error("Azure", format!("Failed to upload blob {hash}: {e}"))
            })?;

            Ok(size)
        })
    }

    /// Dedup settle path: PUT unconditionally. Content-addressed keys make
    /// re-PUTs idempotent, so the `get_properties` probe
    /// `put_blob_from_bytes` pays is a pure extra round-trip on every NEW
    /// chunk (2 RTTs -> 1, benches/S3-PUT.md — same shape as S3).
    fn put_blob_from_bytes_unsynced(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let client = self.blob_client(&hash);
            let size = data.len() as u64;
            client.put_block_blob(data).await.map_err(|e| {
                DomainError::internal_error("Azure", format!("Failed to upload blob {hash}: {e}"))
            })?;
            Ok(size)
        })
    }

    /// Atomic overwrite path used by `backend_rotate` and
    /// `backend_migration` when re-writing an already-present blob
    /// under a new head key/format. Trait default delegates to
    /// `put_blob_from_bytes` which `get_properties`-probes and
    /// silently skips — exactly wrong for the rotate/migrate use
    /// case (the whole point is to replace the existing bytes).
    /// Override delegates to the same unconditional PUT as
    /// `put_blob_from_bytes_unsynced` — Azure's PUT is durable on
    /// return, no separate sync barrier needed.
    fn put_blob_from_bytes_replace(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        self.put_blob_from_bytes_unsynced(hash, data)
    }

    fn get_blob_stream(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let hash = hash.to_owned();
        Box::pin(async move {
            let client = self.blob_client(&hash);

            // The old implementation drained the ENTIRE blob into one
            // `Vec<u8>` before yielding a single mega-chunk — whole-blob
            // RAM residency per reader, and with `read_prefetch() = 8`
            // up to 8 entire chunk-blobs resident at once during CDC
            // reassembly. Now the SDK's page/body streams forward
            // directly. The FIRST page is still awaited eagerly so a
            // missing blob surfaces as the same up-front NotFound the
            // old code produced; later pages/chunks map to io::Error
            // items like every other backend's stream.
            let mut pages = client.get().into_stream();
            let first = match pages.next().await {
                Some(Ok(response)) => response,
                Some(Err(e)) => {
                    return Err(DomainError::new(
                        ErrorKind::NotFound,
                        "Azure",
                        format!("Failed to get blob {hash}: {e}"),
                    ));
                }
                None => {
                    let empty: BlobStream =
                        Box::pin(futures::stream::once(async move { Ok(Bytes::new()) }));
                    return Ok(empty);
                }
            };

            let first_body = first.data.map(|chunk| {
                chunk.map_err(|e| std::io::Error::other(format!("Stream read error: {e}")))
            });
            let tail = pages
                .map(|page| match page {
                    Ok(response) => Ok(response.data.map(|chunk| {
                        chunk.map_err(|e| std::io::Error::other(format!("Stream read error: {e}")))
                    })),
                    Err(e) => Err(std::io::Error::other(format!(
                        "Failed to get blob page: {e}"
                    ))),
                })
                .try_flatten();
            let stream: BlobStream = Box::pin(first_body.chain(tail));
            Ok(stream)
        })
    }

    /// # Known incompatibility: sub-4 MiB ranges break on Azurite
    ///
    /// `azure_core` 0.21's `Range::as_headers`
    /// (`src/request_options/range.rs`) attaches
    /// `x-ms-range-get-content-crc64: true` to **any range shorter than
    /// 4 MiB**, unconditionally and with no opt-out. Real Azure honours
    /// it; Azurite answers 500. `azure_core` then classifies 500 as
    /// retryable and loops on a deterministic error, forever.
    ///
    /// The reachable path is `backend_migration` →
    /// `EncryptedBlobBackend::head_check` →
    /// `get_blob_range_stream(hash, 0, HEADER_SIZE)`. `HEADER_SIZE` is a
    /// few dozen bytes, and it runs against the TARGET before each write,
    /// so a local→Azurite migration hangs on its first blob while holding
    /// `migration_readonly` — writes refused application-wide.
    ///
    /// **Deliberately not worked around here.** The available workaround
    /// is to issue an unranged `get()` for small requests (its 16 MiB
    /// `initial_range` clears the threshold, so the header is never sent)
    /// and truncate client-side. That is correct against real Azure but
    /// pays for an emulator with production cost: a ~40-byte format probe
    /// becomes a whole-blob transfer, and it puts new offset arithmetic
    /// on the read path, where a mistake serves wrong bytes silently
    /// rather than failing.
    ///
    /// The real fix is the official `azure_storage_blob` 1.x, where
    /// `range_get_content_crc64` is an explicit field on
    /// `BlobClientDownloadOptions` — leave it unset and the request is
    /// never made. Until then, the Azurite suite exercises enumeration
    /// and round-trips but not migration; see
    /// `tests/api/backend_consistency_azure.hurl`.
    fn get_blob_range_stream(
        &self,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let hash = hash.to_owned();
        Box::pin(async move {
            let client = self.blob_client(&hash);

            let range = match end {
                Some(e) => azure_core::request_options::Range::new(start, e),
                None => azure_core::request_options::Range::new(start, u64::MAX),
            };

            // Same forwarding shape as `get_blob_stream` — a ranged read
            // doubly so: the caller explicitly asked NOT to pay for the
            // whole blob, yet the old code buffered the full range.
            let mut pages = client.get().range(range).into_stream();
            let first = match pages.next().await {
                Some(Ok(response)) => response,
                Some(Err(e)) => {
                    return Err(DomainError::new(
                        ErrorKind::NotFound,
                        "Azure",
                        format!("Failed to get blob range {hash}: {e}"),
                    ));
                }
                None => {
                    let empty: BlobStream =
                        Box::pin(futures::stream::once(async move { Ok(Bytes::new()) }));
                    return Ok(empty);
                }
            };

            let first_body = first.data.map(|chunk| {
                chunk.map_err(|e| std::io::Error::other(format!("Stream range read error: {e}")))
            });
            let tail = pages
                .map(|page| match page {
                    Ok(response) => Ok(response.data.map(|chunk| {
                        chunk.map_err(|e| {
                            std::io::Error::other(format!("Stream range read error: {e}"))
                        })
                    })),
                    Err(e) => Err(std::io::Error::other(format!(
                        "Failed to get blob range page: {e}"
                    ))),
                })
                .try_flatten();
            let stream: BlobStream = Box::pin(first_body.chain(tail));
            Ok(stream)
        })
    }

    fn delete_blob(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let client = self.blob_client(&hash);

            // Azure delete is not fully idempotent — 404 is expected for missing blobs
            match client.delete().await {
                Ok(_) => Ok(()),
                Err(e) => {
                    // If 404, treat as success (idempotent)
                    let status = e.as_http_error().map(|h| h.status());
                    if status == Some(azure_core::StatusCode::NotFound) {
                        Ok(())
                    } else {
                        Err(DomainError::internal_error(
                            "Azure",
                            format!("Failed to delete blob {hash}: {e}"),
                        ))
                    }
                }
            }
        })
    }

    fn blob_exists(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let client = self.blob_client(&hash);
            match client.get_properties().await {
                Ok(_) => Ok(true),
                Err(e) => {
                    let status = e.as_http_error().map(|h| h.status());
                    if status == Some(azure_core::StatusCode::NotFound) {
                        Ok(false)
                    } else {
                        Err(DomainError::internal_error(
                            "Azure",
                            format!("Failed to check blob {hash}: {e}"),
                        ))
                    }
                }
            }
        })
    }

    fn blob_size(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let hash = hash.to_owned();
        Box::pin(async move {
            let client = self.blob_client(&hash);
            let props = client.get_properties().await.map_err(|e| {
                DomainError::new(
                    ErrorKind::NotFound,
                    "Azure",
                    format!("Failed to stat blob {hash}: {e}"),
                )
            })?;
            Ok(props.blob.properties.content_length)
        })
    }

    fn health_check(
        &self,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<StorageHealthStatus, DomainError>> + Send + '_>,
    > {
        Box::pin(async move {
            match self.container_client.get_properties().await {
                Ok(_) => Ok(StorageHealthStatus {
                    connected: true,
                    backend_type: "azure".to_string(),
                    message: format!("Azure container '{}' is accessible", self.container_name),
                    available_bytes: None,
                }),
                Err(e) => Ok(StorageHealthStatus {
                    connected: false,
                    backend_type: "azure".to_string(),
                    message: format!(
                        "Azure container '{}' is not accessible: {}",
                        self.container_name, e
                    ),
                    available_bytes: None,
                }),
            }
        })
    }

    /// Enumerate blob hashes in lexicographic order, so
    /// `backend_consistency` can merge-join against `storage.blobs`
    /// instead of degrading to a per-row probe that structurally cannot
    /// see orphans.
    ///
    /// ## Why this is a shard walk and not one flat listing
    ///
    /// **The cursor IS a blob hash**, not a provider token. The caller
    /// forces that: it advances ONE cursor across both sides of the join,
    /// feeding the same value here and to `WHERE hash > $1` in SQL. S3
    /// satisfies it with `start_after(object_key(cursor))`.
    ///
    /// Azure has no `StartAfter`. REST API 2023-05-03 added `startFrom`,
    /// which would be the direct equivalent — but this SDK
    /// (`azure_storage_blobs` 0.21, archived) never sends it: `ListBlobs`
    /// exposes only `prefix`, `delimiter`, `max_results` and `marker`,
    /// and `marker` is an opaque continuation token that cannot be
    /// derived from a hash.
    ///
    /// So resume rides on `prefix` instead. Names are
    /// `{hash[0..2]}/{hash}.blob`, which partitions the container into
    /// 256 shards that are themselves in hash order. Walking
    /// `00/` … `ff/` therefore yields exactly the global hash order, and
    /// a cursor names the shard to restart in. Re-listing on resume is
    /// bounded by shard width — 1/256th of the container — rather than
    /// by the whole container, which is what a client-side skip over a
    /// flat listing would cost on every single page.
    ///
    /// `marker` is used only INSIDE one call, to page within a shard, and
    /// never escapes as the cursor — the same treatment the S3 impl gives
    /// its continuation token.
    ///
    /// ## What this does NOT see, unlike S3
    ///
    /// S3 lists the bucket with no prefix, so any foreign object lands in
    /// `unknowns`. Constraining to `{2-hex}/` means foreign names outside
    /// that shape are invisible here.
    ///
    /// That asymmetry is deliberate and safe in the direction that
    /// matters: an orphan is a blob **we** wrote and later stopped
    /// referencing, so it always has the canonical name and is always
    /// enumerated. Only genuinely foreign files — another workload
    /// sharing the container — can be missed, and they are informational
    /// notices, never findings. Trading them for O(N) enumeration instead
    /// of O(N²/limit) is worth it.
    fn list_blob_hashes(
        &self,
        cursor: Option<String>,
        limit: usize,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobListPage, DomainError>> + Send + '_>>
    {
        Box::pin(async move {
            // A run of foreign entries can't produce a resume cursor, and
            // buffering the container to find one blob is worse than
            // failing. Mirrors the S3 impl's bound, and like it is on
            // entries accumulated rather than requests made: request
            // count scales with the caller's `limit`, so a request cap
            // would fire on a healthy container merely because the caller
            // paged finely.
            const MAX_UNKNOWNS: usize = 10_000;

            // A shard is `{2-hex}/`, so the space is 0x00..=0xff.
            const LAST_SHARD: u16 = 0xff;

            let mut blobs: Vec<BackendBlobEntry> = Vec::new();
            let mut unknowns: Vec<BackendUnknownEntry> = Vec::new();

            // Resume in the cursor's own shard — its remaining entries
            // still sort after it, and the client-side skip below drops
            // the ones that don't. A malformed cursor is a bug in the
            // caller's checkpoint, and silently restarting from `00`
            // would re-report every blob as new, so refuse it.
            let mut shard: u16 = match cursor.as_deref() {
                Some(c) => u16::from(u8::from_str_radix(c.get(0..2).unwrap_or(""), 16).map_err(
                    |_| {
                        DomainError::internal_error(
                            "Blob",
                            format!(
                                "Azure enumeration cursor '{c}' is not a blob hash — it must \
                                 start with the two hex characters naming its shard"
                            ),
                        )
                    },
                )?),
                None => 0,
            };

            // Azure caps a page at 5000; asking for the caller's `limit`
            // keeps a small page cheap. `MaxResults` rejects zero, and a
            // caller asking for nothing still needs a well-formed
            // request — and, more importantly, must not be answered with
            // an empty page and a `None` cursor, which would read as
            // "container fully enumerated, nothing here".
            let want = limit.max(1);
            let page_size = want.min(5000) as u32;

            'shards: while shard <= LAST_SHARD {
                let prefix = format!("{shard:02x}/");
                // `Pageable` follows `next_marker` itself, so one stream
                // covers the whole shard however many round-trips it takes.
                let mut pages = self
                    .container_client
                    .list_blobs()
                    .prefix(prefix)
                    .max_results(std::num::NonZeroU32::new(page_size).expect("clamped above 0"))
                    .into_stream();

                while let Some(page) = pages.next().await {
                    let page = page.map_err(|e| {
                        DomainError::internal_error(
                            "Blob",
                            format!(
                                "Azure ListBlobs failed on shard {shard:02x} of container '{}': {e}",
                                self.container_name
                            ),
                        )
                    })?;

                    for blob in page.blobs.blobs() {
                        let name = blob.name.clone();
                        // `OffsetDateTime` → chrono, for the caller's
                        // grace window. A value outside chrono's range
                        // degrades to `None`, which the port documents as
                        // "treat as old enough" — the conservative side,
                        // since it only ever suppresses a finding on a
                        // freshly-written blob.
                        let mtime = chrono::DateTime::<chrono::Utc>::from_timestamp(
                            blob.properties.last_modified.unix_timestamp(),
                            blob.properties.last_modified.nanosecond(),
                        );

                        match Self::hash_from_blob_name(&name) {
                            Some(hash) => {
                                // `prefix` is inclusive of the cursor's own
                                // entry and of everything before it in the
                                // shard. Without this skip the caller sees
                                // a hash it already consumed and the
                                // merge-join never advances past it.
                                if cursor.as_deref().is_some_and(|c| hash.as_str() <= c) {
                                    continue;
                                }
                                blobs.push(BackendBlobEntry { hash, mtime });
                            }
                            // Not ours — a foreign workload sharing the
                            // container. Surfaced rather than dropped so
                            // an operator can see it.
                            None => unknowns.push(BackendUnknownEntry { path: name, mtime }),
                        }
                    }

                    if blobs.len() >= want {
                        break 'shards;
                    }

                    if unknowns.len() >= MAX_UNKNOWNS {
                        return Err(DomainError::internal_error(
                            "Blob",
                            format!(
                                "Azure enumeration accumulated {} non-blob entrie(s) without \
                                 filling a page, so no resume cursor can be produced. Container \
                                 '{}' likely holds a large foreign namespace — give OxiCloud a \
                                 dedicated container.",
                                unknowns.len(),
                                self.container_name,
                            ),
                        ));
                    }
                }

                shard += 1;
            }

            // Exhausting every shard is the ONLY end of enumeration.
            // Stopping early because one shard was empty would truncate
            // the sweep and report the rest of the container as absent,
            // so `shard > LAST_SHARD` — not "this page was empty" — is
            // what produces `None`.
            let next_cursor = if shard > LAST_SHARD {
                None
            } else {
                blobs.last().map(|entry| entry.hash.clone())
            };

            Ok(BlobListPage {
                blobs,
                unknowns,
                next_cursor,
            })
        })
    }

    fn backend_type(&self) -> &'static str {
        "azure"
    }

    /// Remote object store: overlap chunk GETs to hide per-request latency.
    fn read_prefetch(&self) -> usize {
        8
    }

    fn local_blob_path(&self, _hash: &str) -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";

    /// The enumeration cursor is fed straight back in as a shard prefix,
    /// so a name that does not round-trip would resume in the wrong shard
    /// and silently skip everything between.
    #[test]
    fn blob_name_round_trips_through_hash_from_blob_name() {
        let name = AzureBlobBackend::blob_name(H);
        assert_eq!(name, format!("0a/{H}.blob"));
        assert_eq!(
            AzureBlobBackend::hash_from_blob_name(&name).as_deref(),
            Some(H)
        );
    }

    /// Each of these would otherwise be treated as a hash — and the
    /// resume path slices `[0..2]` off it to pick the next shard.
    #[test]
    fn non_canonical_names_are_rejected() {
        let cases = [
            "0a/junk.tmp".to_string(),        // spool file
            "junk.tmp".to_string(),           // no shard
            "0a/junk".to_string(),            // no suffix
            "thumbnails/abc.jpg".to_string(), // foreign namespace
            format!("0a/{H}.blob.corrupt"),   // sidecar
            format!("0a/{H}"),                // suffix missing
            format!("zz/{H}.blob"),           // non-hex shard
            format!("ff/{H}.blob"),           // shard != hash prefix
            format!("0a/{}.blob", &H[..63]),  // wrong length
        ];
        for name in &cases {
            assert_eq!(
                AzureBlobBackend::hash_from_blob_name(name),
                None,
                "must not be read as a blob: {name}"
            );
        }
    }

    /// The shard walk relies on `{hash[0..2]}/…` ordering lexicographic
    /// names into exactly the order `ORDER BY hash` produces. If the
    /// shard were not the hash's own prefix the two sequences would
    /// interleave differently and the merge-join would emit phantom
    /// findings in BOTH directions.
    #[test]
    fn shard_order_matches_hash_order() {
        let hashes = ["00aa", "0a1b", "0aff", "b0cd", "ffff"]
            .map(|p| format!("{p}{}", "0".repeat(60)))
            .to_vec();

        let mut names: Vec<String> = hashes
            .iter()
            .map(|h| AzureBlobBackend::blob_name(h))
            .collect();
        names.sort();

        let recovered: Vec<String> = names
            .iter()
            .filter_map(|n| AzureBlobBackend::hash_from_blob_name(n))
            .collect();

        let mut sorted_hashes = hashes.clone();
        sorted_hashes.sort();
        assert_eq!(recovered, sorted_hashes);
    }
}
