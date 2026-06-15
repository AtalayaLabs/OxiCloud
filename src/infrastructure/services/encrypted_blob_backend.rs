//! `EncryptedBlobBackend` — AES-256-GCM encryption decorator for blob storage.
//!
//! Transparently encrypts blobs before they reach the inner backend and
//! decrypts them on read.  Each blob gets a random 96-bit nonce which is
//! prepended to the ciphertext.  The GCM authentication tag (16 bytes) is
//! appended by the cipher.
//!
//! **IMPORTANT**: BLAKE3 hashing is performed on the *plaintext* by
//! `DedupService` before this layer sees the blob, so content-addressable
//! dedup still works correctly.
//!
//! Layout on disk/S3: `[12-byte nonce][ciphertext + 16-byte GCM tag]`
//!
//! ## Runtime & memory characteristics
//!
//! GCM is all-or-nothing per blob: a blob can only be decrypted whole, so
//! every read materializes the full plaintext.  This stays bounded because
//! `DedupService` stores all new content as CDC chunks (≤ 1 MiB each) and
//! resolves Range requests to the overlapping chunks *before* calling this
//! backend — an encrypted seek in a large video decrypts a handful of
//! chunks, never the file.  The unbounded case is **legacy whole-file
//! blobs** written before CDC chunking: a range read of one still decrypts
//! the entire blob (re-uploading the file re-stores it chunked).
//!
//! Crypto work for payloads ≥ 64 KiB runs on the blocking pool so AES-GCM
//! never stalls the async runtime, and decryption happens **in place** —
//! the ciphertext buffer is reused for the plaintext instead of allocating
//! a second copy.

use std::path::{Path, PathBuf};
use std::pin::Pin;

use aes_gcm::aead::{Aead, AeadInPlace, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
use bytes::Bytes;
use std::sync::Arc;
use tokio::fs;

use crate::application::ports::blob_storage_ports::{
    BlobStorageBackend, BlobStream, StorageHealthStatus,
};
use crate::domain::errors::DomainError;

/// Nonce size for AES-256-GCM (96 bits = 12 bytes).
const NONCE_SIZE: usize = 12;

/// Payloads at or above this size run crypto on the blocking pool; below
/// it the `spawn_blocking` round-trip costs more than the AES work itself.
const CRYPTO_OFFLOAD_THRESHOLD: usize = 64 * 1024;

/// Emission size for decrypted payloads — matches the 64 KiB chunks the
/// unencrypted backends stream, so downstream consumers (HTTP bodies,
/// hashers) see the same backpressure shape either way.
const PLAINTEXT_EMIT_SIZE: usize = 64 * 1024;

/// `BlobStorageBackend` decorator that encrypts blobs at rest.
pub struct EncryptedBlobBackend {
    inner: Arc<dyn BlobStorageBackend>,
    cipher: Aes256Gcm,
}

impl EncryptedBlobBackend {
    /// Create a new encryption layer wrapping `inner`.
    ///
    /// `key` must be exactly 32 bytes (AES-256).
    pub fn new(inner: Arc<dyn BlobStorageBackend>, key: &[u8; 32]) -> Self {
        let cipher = Aes256Gcm::new_from_slice(key).expect("AES-256 key must be 32 bytes");
        Self { inner, cipher }
    }

    /// Generate a random 32-byte key suitable for AES-256.
    pub fn generate_key() -> [u8; 32] {
        use aes_gcm::aead::rand_core::RngCore;
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        key
    }
}

/// Encrypt `data` into the on-disk layout: `[12-byte nonce][ciphertext + tag]`.
fn encrypt_bytes(cipher: &Aes256Gcm, data: &[u8]) -> Result<Bytes, DomainError> {
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, data)
        .map_err(|e| DomainError::internal_error("Encryption", format!("encrypt failed: {e}")))?;

    let mut encrypted = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    encrypted.extend_from_slice(nonce.as_slice());
    encrypted.extend_from_slice(&ciphertext);
    Ok(Bytes::from(encrypted))
}

/// Decrypt the on-disk layout `[nonce][ciphertext + tag]` **in place**.
///
/// Consumes the encrypted buffer and reuses it for the plaintext, so peak
/// RAM is one buffer — not ciphertext + plaintext side by side (which for
/// legacy whole-file blobs would double a multi-hundred-MB allocation).
fn decrypt_bytes(cipher: &Aes256Gcm, mut encrypted: Vec<u8>) -> Result<Bytes, DomainError> {
    if encrypted.len() < NONCE_SIZE {
        return Err(DomainError::internal_error(
            "Encryption",
            "encrypted blob too short (missing nonce)",
        ));
    }
    let mut ciphertext = encrypted.split_off(NONCE_SIZE); // `encrypted` keeps the nonce
    let nonce = Nonce::from_slice(&encrypted);
    cipher
        .decrypt_in_place(nonce, b"", &mut ciphertext)
        .map_err(|e| DomainError::internal_error("Encryption", format!("decrypt failed: {e}")))?;
    Ok(Bytes::from(ciphertext))
}

/// Run a crypto closure inline for small payloads, on the blocking pool for
/// large ones — AES-GCM over megabytes must not stall async workers.
async fn offload_crypto<T, F>(work_len: usize, job: F) -> Result<T, DomainError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, DomainError> + Send + 'static,
{
    if work_len < CRYPTO_OFFLOAD_THRESHOLD {
        return job();
    }
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|e| DomainError::internal_error("Encryption", format!("crypto task join: {e}")))?
}

/// Turn a decrypted payload into a stream of bounded, zero-copy slices.
fn plaintext_stream(data: Bytes) -> BlobStream {
    let len = data.len();
    let slices: Vec<Result<Bytes, std::io::Error>> = (0..len)
        .step_by(PLAINTEXT_EMIT_SIZE)
        .map(|off| Ok(data.slice(off..len.min(off + PLAINTEXT_EMIT_SIZE))))
        .collect();
    Box::pin(futures::stream::iter(slices))
}

impl BlobStorageBackend for EncryptedBlobBackend {
    fn initialize(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        self.inner.initialize()
    }

    fn put_blob(
        &self,
        hash: &str,
        source_path: &Path,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let source = source_path.to_path_buf();
        let cipher = self.cipher.clone();
        Box::pin(async move {
            // Read plaintext from source
            let plaintext = fs::read(&source).await.map_err(|e| {
                DomainError::internal_error("Encryption", format!("read source: {e}"))
            })?;

            let len = plaintext.len();
            let encrypted = offload_crypto(len, move || encrypt_bytes(&cipher, &plaintext)).await?;

            // Hand the ciphertext straight to the inner backend. The previous
            // implementation spooled it to a `.enc.tmp` file only for the
            // inner backend to read it back — a full extra write + read of
            // every blob that came through this path.
            inner.put_blob_from_bytes(&hash, encrypted).await
        })
    }

    fn put_blob_from_bytes(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let cipher = self.cipher.clone();
        Box::pin(async move {
            let encrypted =
                offload_crypto(data.len(), move || encrypt_bytes(&cipher, data.as_ref())).await?;
            inner.put_blob_from_bytes(&hash, encrypted).await
        })
    }

    fn put_blob_from_bytes_unsynced(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let cipher = self.cipher.clone();
        Box::pin(async move {
            let encrypted =
                offload_crypto(data.len(), move || encrypt_bytes(&cipher, data.as_ref())).await?;
            inner.put_blob_from_bytes_unsynced(&hash, encrypted).await
        })
    }

    fn sync_blobs(
        &self,
        hashes: &[String],
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        // Hashes key the *plaintext* content but address the same inner
        // blobs, so the durability sweep forwards untouched.
        self.inner.sync_blobs(hashes)
    }

    fn get_blob_stream(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let cipher = self.cipher.clone();
        Box::pin(async move {
            // GCM must see the whole message: collect ciphertext, decrypt in
            // place off the runtime, then stream zero-copy plaintext slices.
            let enc_stream = inner.get_blob_stream(&hash).await?;
            let encrypted = collect_stream(enc_stream).await?;
            let len = encrypted.len();
            let plaintext = offload_crypto(len, move || decrypt_bytes(&cipher, encrypted)).await?;
            Ok(plaintext_stream(plaintext))
        })
    }

    fn get_blob_range_stream(
        &self,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<BlobStream, DomainError>> + Send + '_>>
    {
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let cipher = self.cipher.clone();
        Box::pin(async move {
            // Decrypt the full blob, then slice the plaintext range without
            // copying. For CDC chunks (every blob written since chunking
            // landed) this is ≤ 1 MiB; only legacy whole-file blobs pay a
            // full-blob decrypt here — see the module docs.
            let enc_stream = inner.get_blob_stream(&hash).await?;
            let encrypted = collect_stream(enc_stream).await?;
            let len = encrypted.len();
            let plaintext = offload_crypto(len, move || decrypt_bytes(&cipher, encrypted)).await?;

            // `end` is exclusive — same contract as `LocalBlobBackend`, whose
            // implementation reads `end - start` bytes. The previous version
            // here treated it as inclusive and returned one extra byte on
            // every bounded range, corrupting 206 responses when encryption
            // was enabled.
            let total = plaintext.len();
            let end_excl = end.map(|e| e as usize).unwrap_or(total).min(total);
            let start = (start as usize).min(end_excl);

            Ok(plaintext_stream(plaintext.slice(start..end_excl)))
        })
    }

    fn delete_blob(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + '_>> {
        self.inner.delete_blob(hash)
    }

    fn blob_exists(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<bool, DomainError>> + Send + '_>> {
        self.inner.blob_exists(hash)
    }

    fn blob_size(
        &self,
        hash: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        // The stored size includes nonce + GCM tag overhead.
        // Return the *plaintext* size by subtracting overhead.
        let inner = self.inner.clone();
        let hash = hash.to_string();
        Box::pin(async move {
            let encrypted_size = inner.blob_size(&hash).await?;
            // overhead = 12 (nonce) + 16 (GCM tag) = 28 bytes
            Ok(encrypted_size.saturating_sub(28))
        })
    }

    fn health_check(
        &self,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<StorageHealthStatus, DomainError>> + Send + '_>,
    > {
        let inner = self.inner.clone();
        Box::pin(async move {
            let mut status = inner.health_check().await?;
            status.backend_type = format!("encrypted({})", status.backend_type);
            status.message = format!("{} | Encryption: AES-256-GCM", status.message);
            Ok(status)
        })
    }

    fn backend_type(&self) -> &'static str {
        "encrypted"
    }

    /// Transparent wrapper: the inner backend serves the bytes.
    fn read_prefetch(&self) -> usize {
        self.inner.read_prefetch()
    }

    fn local_blob_path(&self, _hash: &str) -> Option<PathBuf> {
        // Encrypted blobs cannot be served directly from disk
        None
    }
}

/// Collect a byte stream into a single `Vec<u8>`.
async fn collect_stream(stream: BlobStream) -> Result<Vec<u8>, DomainError> {
    use futures::StreamExt;
    let mut stream = stream;
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk
            .map_err(|e| DomainError::internal_error("Encryption", format!("stream read: {e}")))?;
        buf.extend_from_slice(&bytes);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::services::local_blob_backend::LocalBlobBackend;
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn test_encrypt_decrypt_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let blob_dir = tmp.path().join("blobs");
        let local = Arc::new(LocalBlobBackend::new(&blob_dir));
        local.initialize().await.unwrap();

        let key = EncryptedBlobBackend::generate_key();
        let encrypted = EncryptedBlobBackend::new(local, &key);

        // Write a test blob
        let data = b"Hello, encrypted world!";
        let source = tmp.path().join("test.tmp");
        let mut f = fs::File::create(&source).await.unwrap();
        f.write_all(data).await.unwrap();
        f.flush().await.unwrap();
        drop(f);

        let hash = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        encrypted.put_blob(hash, &source).await.unwrap();

        // Read back via stream
        let stream = encrypted.get_blob_stream(hash).await.unwrap();
        let decrypted = collect_stream(stream).await.unwrap();
        assert_eq!(decrypted, data);

        // Read range — `end` is exclusive, matching LocalBlobBackend
        let range_stream = encrypted
            .get_blob_range_stream(hash, 7, Some(16))
            .await
            .unwrap();
        let range_data = collect_stream(range_stream).await.unwrap();
        assert_eq!(range_data, b"encrypted");

        // Size should reflect plaintext
        let size = encrypted.blob_size(hash).await.unwrap();
        assert_eq!(size, data.len() as u64);

        // Exists
        assert!(encrypted.blob_exists(hash).await.unwrap());

        // Delete
        encrypted.delete_blob(hash).await.unwrap();
        assert!(!encrypted.blob_exists(hash).await.unwrap());
    }

    /// Payloads above `CRYPTO_OFFLOAD_THRESHOLD` take the spawn_blocking
    /// path and are emitted as multiple bounded slices — the roundtrip and
    /// range semantics must be identical to the inline path.
    #[tokio::test]
    async fn test_large_blob_offloaded_roundtrip_and_ranges() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let key = EncryptedBlobBackend::generate_key();
        let encrypted = EncryptedBlobBackend::new(local, &key);

        // 300 KiB of a repeating pattern — crosses the offload threshold and
        // spans several PLAINTEXT_EMIT_SIZE slices.
        let data: Vec<u8> = (0..300 * 1024).map(|i| (i % 251) as u8).collect();
        let hash = "feedbeef1234567890feedbeef1234567890feedbeef1234567890feedbeef12";
        encrypted
            .put_blob_from_bytes(hash, Bytes::from(data.clone()))
            .await
            .unwrap();

        // Full roundtrip
        let stream = encrypted.get_blob_stream(hash).await.unwrap();
        let decrypted = collect_stream(stream).await.unwrap();
        assert_eq!(decrypted, data);

        // Mid-file range crossing an emission boundary (`end` exclusive)
        let (start, end) = (60_000u64, 200_000u64);
        let stream = encrypted
            .get_blob_range_stream(hash, start, Some(end))
            .await
            .unwrap();
        let ranged = collect_stream(stream).await.unwrap();
        assert_eq!(ranged, &data[start as usize..end as usize]);

        // Open-ended suffix range
        let stream = encrypted
            .get_blob_range_stream(hash, 299 * 1024, None)
            .await
            .unwrap();
        let suffix = collect_stream(stream).await.unwrap();
        assert_eq!(suffix, &data[299 * 1024..]);

        // Range entirely past EOF yields empty content
        let stream = encrypted
            .get_blob_range_stream(hash, data.len() as u64 + 10, None)
            .await
            .unwrap();
        assert!(collect_stream(stream).await.unwrap().is_empty());

        // Plaintext size reported
        assert_eq!(encrypted.blob_size(hash).await.unwrap(), data.len() as u64);
    }

    /// A flipped ciphertext byte must fail GCM authentication, never return
    /// corrupted plaintext.
    #[tokio::test]
    async fn test_tampered_ciphertext_fails_decrypt() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let key = EncryptedBlobBackend::generate_key();
        let encrypted = EncryptedBlobBackend::new(local.clone(), &key);

        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        encrypted
            .put_blob_from_bytes(hash, Bytes::from_static(b"sensitive payload"))
            .await
            .unwrap();

        // Corrupt one ciphertext byte on disk (past the 12-byte nonce).
        let path = local.local_blob_path(hash).expect("local path");
        let mut raw = std::fs::read(&path).unwrap();
        raw[NONCE_SIZE] ^= 0xFF;
        std::fs::write(&path, raw).unwrap();

        assert!(encrypted.get_blob_stream(hash).await.is_err());
    }

    /// Decrypting with a different key must fail authentication.
    #[tokio::test]
    async fn test_wrong_key_fails_decrypt() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let hash = "aaaabbbbccccddddaaaabbbbccccddddaaaabbbbccccddddaaaabbbbccccdddd";
        let writer =
            EncryptedBlobBackend::new(local.clone(), &EncryptedBlobBackend::generate_key());
        writer
            .put_blob_from_bytes(hash, Bytes::from_static(b"locked"))
            .await
            .unwrap();

        let reader = EncryptedBlobBackend::new(local, &EncryptedBlobBackend::generate_key());
        assert!(reader.get_blob_stream(hash).await.is_err());
    }
}
