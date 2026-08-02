//! `EncryptedBlobBackend` — v1 blob-format decorator for blob storage.
//!
//! Wraps an inner `BlobStorageBackend` and adds two orthogonal
//! behaviours:
//!
//! 1. **v1 header framing on every new write** — every blob written
//!    by this wrapper starts with a 15-byte header
//!    `OXCPT | <version u16> | <key_fp u64>` so future reads are
//!    self-describing regardless of the entry's current config.
//! 2. **Pair-list encryption** — the wrapper owns an ordered
//!    [`KeyPair`] list. Writes use the LAST pair (the "head"); reads
//!    dispatch on the header's `<key_fp>` field into an O(1)
//!    fp → cipher lookup, so a blob written under any pair still in
//!    the list decrypts without fallback attempts.
//!
//! See `docs/plan/storage-key-rotation.md` for the full design.
//!
//! ## v1 on-disk layout
//!
//! Encrypted-v1 (`head_cipher` is a real AEAD):
//!
//! ```text
//! "OXCPT"         5 bytes  — magic marker
//! <version>       2 bytes  — big-endian u16; v1 = 0x0001
//! <key_fp>        8 bytes  — sha256(key material)[..8], routes reads
//! <nonce>        12 bytes  — random per blob (AES-GCM 96-bit)
//! <ciphertext>    N bytes  — same length as plaintext
//! <auth_tag>     16 bytes  — AEAD authentication tag
//! ```
//!
//! Plaintext-v1 (`head_cipher` is `None`, i.e. entry uses a `none:`
//! head pair or has no encryption declared at all):
//!
//! ```text
//! "OXCPT"         5 bytes  — magic marker
//! <version>       2 bytes  — big-endian u16; v1 = 0x0001
//! <key_fp>        8 bytes  — all zero
//! <payload>       N bytes  — raw plaintext
//! ```
//!
//! ## Legacy fallback on reads
//!
//! Blobs written before this wrapper existed have no OXCPT magic.
//! Reads check the first 5 bytes:
//!
//! * `"OXCPT"` → v1 path (version + key_fp lookup + AEAD or raw).
//! * anything else → **legacy path** — try `head_cipher` (if any)
//!   as an AES-GCM decode over the pre-v1 shape
//!   `[nonce][ciphertext][tag]`; otherwise return raw bytes.
//!
//! Collision probability: 2⁻⁴⁰ per blob for random legacy bytes to
//! start with `"OXCPT"`. If it happens, subsequent version / key_fp
//! checks fail with a hard error (`UnsupportedBlobVersion` or
//! `NoKeyForBlob`) — never silent misread.
//!
//! **IMPORTANT**: BLAKE3 hashing is performed on the *plaintext* by
//! `DedupService` before this layer sees the blob, so content-addressable
//! dedup still works correctly.
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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use aes_gcm::aead::{AeadInPlace, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
use bytes::Bytes;
use std::sync::Arc;
use tokio::fs;

use crate::application::ports::blob_storage_ports::{
    BlobStorageBackend, BlobStream, StorageHealthStatus,
};
use crate::common::config::KeyPair;
use crate::domain::errors::DomainError;

/// v1 magic marker — every v1 blob starts with these 5 ASCII bytes.
/// Chosen for its 2⁻⁴⁰ collision odds against random legacy data
/// and its clean legibility in a `hexdump` (mnemonic:
/// "OXiCloud CiPherText").
const OXCPT_MAGIC: &[u8; 5] = b"OXCPT";

/// v1 header version bytes (big-endian u16 = 0x0001). Future formats
/// bump this in-place — the [`OXCPT_MAGIC`] stays the discriminator
/// against legacy / non-OxiCloud files.
const V1_VERSION_BYTES: [u8; 2] = [0x00, 0x01];

/// On-blob `<key_fp>` field size — 8 bytes = 64-bit truncation of
/// `sha256(key)`. Wide enough that random collisions are 2⁻⁶⁴; the
/// parser also uniqueness-checks pairs on raw key material so
/// duplicates can't sneak in.
const KEY_FP_SIZE: usize = 8;

/// Total v1 header size = magic + version + key_fp.
const HEADER_SIZE: usize = 5 + 2 + KEY_FP_SIZE;

/// Nonce size for AES-256-GCM (96 bits = 12 bytes).
const NONCE_SIZE: usize = 12;

/// AES-256-GCM authentication tag length appended after the ciphertext.
const TAG_SIZE: usize = 16;

/// AEAD overhead per blob (nonce + tag) — 28 bytes, same regardless
/// of header framing.
const AEAD_OVERHEAD: usize = NONCE_SIZE + TAG_SIZE;

/// Per-blob overhead for an encrypted-v1 blob: header + AEAD = 43 bytes.
const ENCRYPTED_V1_OVERHEAD: usize = HEADER_SIZE + AEAD_OVERHEAD;

/// Payloads at or above this size run crypto on the blocking pool; below
/// it the `spawn_blocking` round-trip costs more than the AES work itself.
const CRYPTO_OFFLOAD_THRESHOLD: usize = 64 * 1024;

/// Emission size for decrypted payloads — matches the 64 KiB chunks the
/// unencrypted backends stream, so downstream consumers (HTTP bodies,
/// hashers) see the same backpressure shape either way.
const PLAINTEXT_EMIT_SIZE: usize = 64 * 1024;

/// `BlobStorageBackend` decorator that applies v1 header framing and
/// pair-list-driven encryption. See the module-level docs for the
/// on-disk layout and read-fallback semantics.
pub struct EncryptedBlobBackend {
    inner: Arc<dyn BlobStorageBackend>,
    /// The pair list as declared by the operator. Guaranteed
    /// non-empty by ctor (an empty input auto-synthesises a single
    /// `none:` pair, so the invariant holds). Used by:
    /// * `read_dispatch` legacy-fallback path — iterates in order
    ///   (oldest → newest) to try every real-cipher pair when a
    ///   legacy blob's head-key decrypt fails.
    /// * K3 `storage_rotate` — needs to walk pair indices.
    pairs: Vec<KeyPair>,
    /// `<key_fp>` → per-pair cipher, for O(1) read dispatch on v1
    /// blobs. Excludes any `none:` pair (nothing to build). Cloned
    /// per read via `Arc::clone` — the ~240-byte expanded AES-256
    /// round-key schedule is amortised across every request.
    fp_ciphers: HashMap<[u8; KEY_FP_SIZE], Arc<Aes256Gcm>>,
    /// Cipher used by writes (last pair in the list). `None` when
    /// the head is a `CipherKind::None` pair — in that case writes
    /// emit plaintext-v1 (magic + version + zero fp + raw payload).
    head_cipher: Option<Arc<Aes256Gcm>>,
    /// Head pair's `key_fp` — embedded in every write's v1 header.
    /// `[0u8; 8]` when head is `CipherKind::None`, matching the
    /// plaintext-v1 shape.
    head_key_fp: [u8; KEY_FP_SIZE],
}

impl EncryptedBlobBackend {
    /// Primary constructor. Takes an ordered pair list — the LAST
    /// pair is the write pair (head), every pair is a candidate for
    /// reads via `<key_fp>` dispatch.
    ///
    /// Empty `pairs` is legal and treated as a single implicit
    /// `none:` pair — the wrapper still emits v1 headers on writes
    /// (plaintext-v1 flavor) and still magic-byte-dispatches on
    /// reads (with legacy fallback for header-less blobs). This is
    /// the always-wrap contract used by `entry_backend.rs` under the
    /// K2 "normalize data" rule.
    ///
    /// Panics if any real-cipher pair's key material isn't 32 bytes,
    /// which the parser guarantees — a panic here signals a
    /// programmer bug, not operator error.
    pub fn new(inner: Arc<dyn BlobStorageBackend>, pairs: Vec<KeyPair>) -> Self {
        let pairs = if pairs.is_empty() {
            vec![KeyPair::new_none()]
        } else {
            pairs
        };
        let mut fp_ciphers = HashMap::with_capacity(pairs.len());
        for kp in &pairs {
            if let Some(mat) = kp.key_material.as_ref() {
                let cipher = Aes256Gcm::new_from_slice(mat)
                    .expect("KeyPair invariant: real-cipher pair has 32-byte key");
                fp_ciphers.insert(kp.key_fp(), Arc::new(cipher));
            }
        }
        let head = pairs
            .last()
            .expect("post-normalisation pair list is non-empty");
        let head_key_fp = head.key_fp();
        let head_cipher = fp_ciphers.get(&head_key_fp).cloned();
        Self {
            inner,
            pairs,
            fp_ciphers,
            head_cipher,
            head_key_fp,
        }
    }

    /// Convenience: wrap with a single AES-256-GCM pair. Same effect
    /// as `new(inner, vec![KeyPair::new_aes_gcm(*key)])`. Used by
    /// tests + the pre-multi-entry legacy synthesis fallback in
    /// `di.rs`.
    pub fn new_single_aes(inner: Arc<dyn BlobStorageBackend>, key: &[u8; 32]) -> Self {
        Self::new(inner, vec![KeyPair::new_aes_gcm(*key)])
    }

    /// Generate a random 32-byte key suitable for AES-256.
    pub fn generate_key() -> [u8; 32] {
        use aes_gcm::aead::rand_core::RngCore;
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        key
    }

    /// The format `storage_rotate` should normalise every blob TO —
    /// derived from the wrapper's head pair. When
    /// `head_cipher.is_some()` we're writing encrypted-v1 with the
    /// head pair's `key_fp`; when it's `None` we're writing
    /// plaintext-v1 (all-zero `key_fp`).
    ///
    /// K3 uses this as the "target format" that a per-blob decision
    /// tree compares against `BlobFormat::classify(bytes)` — any
    /// mismatch means the blob needs rewriting.
    pub fn head_format(&self) -> BlobFormat {
        if self.head_cipher.is_some() {
            BlobFormat::EncryptedV1 {
                key_fp: self.head_key_fp,
            }
        } else {
            BlobFormat::PlaintextV1
        }
    }

    /// Fetch, classify, and decrypt a blob in one round-trip. Used by
    /// K3's `storage_rotate` per-blob step: it needs both the
    /// plaintext (to re-encrypt under the head pair) AND the current
    /// on-disk format (to decide whether a rewrite is needed at all).
    ///
    /// The inner backend is read once; the raw bytes are inspected
    /// for their format before being consumed by `read_dispatch`. No
    /// duplicated I/O.
    ///
    /// Returned tuple: `(plaintext, current_format)`. Rotate compares
    /// `current_format` against [`Self::head_format`]; equal → skip,
    /// different → rewrite via the standard write path.
    pub async fn read_and_classify(&self, hash: &str) -> Result<(Bytes, BlobFormat), DomainError> {
        let enc_stream = self.inner.get_blob_stream(hash).await?;
        let raw = collect_stream(enc_stream).await?;
        let format = BlobFormat::classify(&raw);
        let pairs = self.pairs.clone();
        let fp_ciphers = self.fp_ciphers.clone();
        let head_cipher = self.head_cipher.clone();
        let head_key_fp = self.head_key_fp;
        let hash_owned = hash.to_string();
        let len = raw.len();
        let plaintext = offload_crypto(len, move || {
            read_dispatch(
                &pairs,
                &fp_ciphers,
                head_cipher.as_deref(),
                head_key_fp,
                &hash_owned,
                raw,
            )
        })
        .await?;
        Ok((plaintext, format))
    }
}

/// Classification of a raw blob's on-disk format. Exposed for K3's
/// `storage_rotate` decision tree; not used on the hot request path.
///
/// PartialEq is derived so `current == head_format` collapses the
/// plan's six-case decision tree into a single equality check:
///
/// * `Legacy != anything v1`     → always rewrite.
/// * `EncryptedV1{fp_a} != EncryptedV1{fp_b}` when fps differ → rewrite (key rotation).
/// * `PlaintextV1 != EncryptedV1` and vice-versa → rewrite (encrypt / decrypt in place).
/// * Match cases → skip (already normalised).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobFormat {
    /// No `OXCPT` magic. Pre-K2 shape — either raw plaintext or raw
    /// `nonce | ct | tag` AES-GCM output; the wrapper's legacy read
    /// path handles both.
    Legacy,
    /// v1 with a `none:`-style all-zero `key_fp`. Post-header bytes
    /// are raw plaintext.
    PlaintextV1,
    /// v1 with a real cipher pair. `key_fp` identifies which pair
    /// (matches [`KeyPair::key_fp`]).
    EncryptedV1 { key_fp: [u8; KEY_FP_SIZE] },
}

impl BlobFormat {
    /// Inspect the first `HEADER_SIZE` bytes and classify the blob.
    /// O(1), no allocation. Used by [`EncryptedBlobBackend::read_and_classify`]
    /// but also useful in isolation for offline tools.
    pub fn classify(bytes: &[u8]) -> Self {
        if bytes.len() < 5 || &bytes[..5] != OXCPT_MAGIC {
            return BlobFormat::Legacy;
        }
        // Magic OK. If the rest of the header isn't present the blob
        // is malformed — treat as Legacy so the decision tree marks
        // it for rewrite (and the actual read will surface the error
        // to the finding stream).
        if bytes.len() < HEADER_SIZE {
            return BlobFormat::Legacy;
        }
        // v1 magic + at least a full header. `key_fp` == 0 → plaintext.
        let mut key_fp = [0u8; KEY_FP_SIZE];
        key_fp.copy_from_slice(&bytes[7..HEADER_SIZE]);
        if key_fp == [0u8; KEY_FP_SIZE] {
            BlobFormat::PlaintextV1
        } else {
            BlobFormat::EncryptedV1 { key_fp }
        }
    }
}

impl std::fmt::Display for BlobFormat {
    /// Human-friendly format for audit logs + finding details.
    /// Renders `key_fp` as SSH-style colon-hex (e.g.
    /// `83:96:ff:90:94:d7:ef:de`) instead of the raw byte-array Debug
    /// shape (`[131, 150, 255, ...]`). Same spelling `xxd` produces
    /// when you inspect a blob's on-disk header, so operators can
    /// cross-check without a mental conversion.
    ///
    /// Handlers that render this in tracing macros should use `%`
    /// (Display) — `?` (Debug) still gives the raw byte-array shape
    /// for programmer-consumers who need the exact bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlobFormat::Legacy => write!(f, "legacy"),
            BlobFormat::PlaintextV1 => write!(f, "plaintext-v1"),
            BlobFormat::EncryptedV1 { key_fp } => {
                write!(f, "encrypted-v1 key_fp=")?;
                for (i, byte) in key_fp.iter().enumerate() {
                    if i > 0 {
                        write!(f, ":")?;
                    }
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    }
}

/// Assemble an encrypted-v1 blob:
/// `OXCPT | v1 | key_fp | nonce | ciphertext | tag`.
///
/// Single output buffer, mirroring the read side's in-place detached
/// decrypt: the payload is copied exactly once and encrypted in
/// place with the tag appended. The old shape (pre-K2, no header)
/// let `cipher.encrypt` allocate a full ciphertext `Vec` and then
/// copied it a second time behind the nonce — one extra allocation +
/// a full-size memcpy on every encrypted chunk write
/// (benches/ROUND11.md §15). K2 preserves the single-buffer
/// discipline: we `extend_from_slice` header + nonce + payload, then
/// encrypt in place from `HEADER_SIZE + NONCE_SIZE`.
fn encrypt_v1(
    cipher: &Aes256Gcm,
    head_key_fp: [u8; KEY_FP_SIZE],
    data: &[u8],
) -> Result<Bytes, DomainError> {
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let mut out = Vec::with_capacity(ENCRYPTED_V1_OVERHEAD + data.len());
    out.extend_from_slice(OXCPT_MAGIC);
    out.extend_from_slice(&V1_VERSION_BYTES);
    out.extend_from_slice(&head_key_fp);
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(data);
    let tag = cipher
        .encrypt_in_place_detached(&nonce, b"", &mut out[HEADER_SIZE + NONCE_SIZE..])
        .map_err(|e| DomainError::internal_error("Encryption", format!("encrypt failed: {e}")))?;
    out.extend_from_slice(&tag);
    Ok(Bytes::from(out))
}

/// Assemble a plaintext-v1 blob:
/// `OXCPT | v1 | <8 zero bytes> | payload`.
///
/// No crypto, no allocation beyond the header prefix. Produced when
/// the wrapper's head pair is `CipherKind::None`.
fn write_plaintext_v1(data: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(HEADER_SIZE + data.len());
    out.extend_from_slice(OXCPT_MAGIC);
    out.extend_from_slice(&V1_VERSION_BYTES);
    out.extend_from_slice(&[0u8; KEY_FP_SIZE]);
    out.extend_from_slice(data);
    Bytes::from(out)
}

/// Read dispatch — the K2 core. Given a fetched blob and the
/// wrapper's pair table, returns plaintext.
///
/// * `OXCPT` at position 0 → **v1 path**:
///   * version check (only `0x0001` accepted today);
///   * `key_fp == [0u8; 8]` → plaintext-v1 → return post-header
///     bytes as-is;
///   * else `key_fp` lookup in `fp_ciphers` → AEAD decrypt over
///     the post-header body.
/// * anything else → **legacy path**:
///   * `head_cipher = Some` → AES-GCM decrypt attempts, head-first,
///     then every OTHER real-cipher pair in the wrapper's list.
///     Head-first is the fast path (pre-v1 world used exactly one
///     key which becomes head on upgrade, so it's the correct key
///     for legacy blobs the first time you rotate). The extra
///     fallbacks cover post-rotation scenarios where an operator
///     restores a pre-v1 backup encrypted under a now-non-head
///     pair — that blob was encrypted with K1 but head is now K2;
///     without the fallback the read would fail with tag error
///     even though K1 is still in the pair-list. Each failed tag
///     check is microseconds — bounded by the pair count (1-3 in
///     practice).
///   * `head_cipher = None` → return raw bytes (pre-K2 plaintext
///     deployment).
///
/// Never falls through silently: every failure returns a distinct
/// typed error (`UnsupportedBlobVersion`, `NoKeyForBlob`, AEAD tag
/// failure). Random legacy bytes matching `OXCPT` (2⁻⁴⁰) fail the
/// subsequent version/key_fp check with a hard error, not silent
/// garbage.
fn read_dispatch(
    pairs: &[KeyPair],
    fp_ciphers: &HashMap<[u8; KEY_FP_SIZE], Arc<Aes256Gcm>>,
    head_cipher: Option<&Aes256Gcm>,
    head_key_fp: [u8; KEY_FP_SIZE],
    expected_hash: &str,
    encrypted: Vec<u8>,
) -> Result<Bytes, DomainError> {
    if encrypted.len() >= 5 && &encrypted[..5] == OXCPT_MAGIC {
        return read_v1(fp_ciphers, encrypted);
    }
    match head_cipher {
        Some(head) => {
            // Fast path: try the head pair first. This is the
            // pre-v1-upgrade case (single key = head; correct by
            // construction) and the most common case even
            // post-rotation (head was head just before the operator
            // rotated it to a new pair).
            if let Ok(pt) = decrypt_aead_in_place(head, encrypted.clone()) {
                return Ok(pt);
            }
            // Fallback: try every other real-cipher pair in
            // list order — oldest → newest. Legacy blobs are OLD
            // by definition (pre-K2, no header), so an older pair
            // is more likely to have encrypted them than a newer
            // one. Head is skipped (already tried above). Each
            // failed AEAD tag check is µs; the loop is bounded by
            // the pair count (1-3 in practice).
            //
            // `Vec<KeyPair>` ordering is stable (env-declaration
            // order), unlike `HashMap` iteration which is randomised.
            //
            // Clone per attempt because `decrypt_in_place_detached`
            // leaves the buffer in an undefined state on tag failure
            // — retrying against another key needs a fresh copy.
            for pair in pairs {
                if !pair.cipher.needs_key() {
                    continue; // `none` pair — no cipher to try
                }
                let fp = pair.key_fp();
                if fp == head_key_fp {
                    continue; // already tried above
                }
                if let Some(cipher) = fp_ciphers.get(&fp)
                    && let Ok(pt) = decrypt_aead_in_place(cipher, encrypted.clone())
                {
                    return Ok(pt);
                }
            }
            // ── BLAKE3 rescue (last safety net) ─────────────────
            // If every configured key failed AND the raw bytes
            // BLAKE3 to the expected hash, those bytes ARE the
            // plaintext — the blob was written pre-encryption
            // (pre-K2 legacy plaintext) or via a migration that
            // silently retained plaintext blobs (see the K1.2
            // skip-check bug — historical residue).
            //
            // Zero-false-positive because we're recomputing the
            // content-addressable hash: matching bytes = matching
            // content, period. Cheap (BLAKE3 ~2 GB/s) and only
            // runs on the pathological path where AES already
            // failed. Emits an audit line so operators can spot
            // pre-encryption blobs and decide whether to re-write
            // them under the head via rotate.
            if hex_matches_blake3(expected_hash, &encrypted) {
                tracing::info!(
                    target: "audit",
                    event = "encryption.legacy_plaintext_rescued",
                    hash = %expected_hash,
                    size = encrypted.len(),
                    "🩹 legacy plaintext blob served via BLAKE3 rescue — no configured key \
                     decrypted it, but content hash matched. Run storage_rotate to re-write \
                     under the current head."
                );
                return Ok(Bytes::from(encrypted));
            }
            Err(DomainError::internal_error(
                "Encryption",
                "legacy blob failed to decrypt under any configured key — \
                 wrong key removed from pair-list, or blob is corrupt",
            ))
        }
        None => Ok(Bytes::from(encrypted)),
    }
}

/// Return true iff `expected_hex` is a valid 32-byte BLAKE3 hex
/// digest AND matches `blake3(bytes)`. Case-insensitive on hex.
///
/// Kept out of the hot path — only called from the legacy-fallback
/// last-resort branch when every AES key already failed.
fn hex_matches_blake3(expected_hex: &str, bytes: &[u8]) -> bool {
    if expected_hex.len() != 64 {
        return false;
    }
    let mut expected = [0u8; 32];
    if hex::decode_to_slice(expected_hex, &mut expected).is_err() {
        return false;
    }
    let actual = blake3::hash(bytes);
    actual.as_bytes() == &expected
}

/// The v1 branch of `read_dispatch`, factored out for clarity.
fn read_v1(
    fp_ciphers: &HashMap<[u8; KEY_FP_SIZE], Arc<Aes256Gcm>>,
    encrypted: Vec<u8>,
) -> Result<Bytes, DomainError> {
    if encrypted.len() < HEADER_SIZE {
        return Err(DomainError::internal_error(
            "Encryption",
            format!(
                "v1 blob too short (need at least {HEADER_SIZE} bytes for the header, got {})",
                encrypted.len()
            ),
        ));
    }
    let version = &encrypted[5..7];
    if version != V1_VERSION_BYTES {
        return Err(DomainError::internal_error(
            "Encryption",
            format!(
                "unsupported v1 blob version 0x{:02x}{:02x} — this build only reads 0x0001",
                version[0], version[1]
            ),
        ));
    }
    let mut key_fp = [0u8; KEY_FP_SIZE];
    key_fp.copy_from_slice(&encrypted[7..HEADER_SIZE]);
    let mut body = encrypted;
    body.drain(..HEADER_SIZE);
    if key_fp == [0u8; KEY_FP_SIZE] {
        // Plaintext-v1 — post-header bytes ARE the plaintext.
        return Ok(Bytes::from(body));
    }
    let cipher = fp_ciphers.get(&key_fp).ok_or_else(|| {
        DomainError::internal_error(
            "Encryption",
            format!(
                "v1 blob key_fp {} does not match any configured pair — cannot decrypt",
                hex::encode(key_fp)
            ),
        )
    })?;
    decrypt_aead_in_place(cipher, body)
}

/// Decrypt the AEAD body `[nonce][ciphertext][tag]` **in place**.
///
/// Reuses the encrypted buffer for the plaintext, so peak RAM is one buffer —
/// not ciphertext + plaintext side by side (which for legacy whole-file blobs
/// would double a multi-hundred-MB allocation). The nonce and 16-byte GCM tag
/// are lifted to the stack, the ciphertext body is decrypted in place via the
/// detached API (mirroring the encrypt side's `encrypt_in_place_detached`), and
/// the plaintext is returned as a zero-copy `Bytes::slice` past the nonce.
fn decrypt_aead_in_place(cipher: &Aes256Gcm, mut encrypted: Vec<u8>) -> Result<Bytes, DomainError> {
    let len = encrypted.len();
    if len < AEAD_OVERHEAD {
        return Err(DomainError::internal_error(
            "Encryption",
            "AEAD body too short (missing nonce/tag)",
        ));
    }
    // Nonce (first 12 bytes) and GCM tag (last 16 bytes) copied to the stack so
    // the middle can be borrowed mutably for in-place decryption.
    let mut nonce_buf = [0u8; NONCE_SIZE];
    nonce_buf.copy_from_slice(&encrypted[..NONCE_SIZE]);
    let nonce = Nonce::from_slice(&nonce_buf);
    let tag = aes_gcm::aead::Tag::<Aes256Gcm>::clone_from_slice(&encrypted[len - TAG_SIZE..]);
    cipher
        .decrypt_in_place_detached(nonce, b"", &mut encrypted[NONCE_SIZE..len - TAG_SIZE], &tag)
        .map_err(|e| DomainError::internal_error("Encryption", format!("decrypt failed: {e}")))?;
    // Plaintext now lives at `encrypted[NONCE_SIZE..len - TAG_SIZE]`; drop the
    // tag and hand out a refcounted view past the nonce — no copy, no new alloc.
    encrypted.truncate(len - TAG_SIZE);
    Ok(Bytes::from(encrypted).slice(NONCE_SIZE..))
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
///
/// The emit-slice iterator is handed to `stream::iter` lazily — the closure
/// owns `data` (a refcounted `Bytes`), so each `slice` is produced on demand
/// as the consumer polls, rather than eagerly `collect`ing a `Vec` of
/// ⌈len/64 KiB⌉ slice handles up front (benches/ROUND20.md §I4).
fn plaintext_stream(data: Bytes) -> BlobStream {
    let len = data.len();
    Box::pin(futures::stream::iter(
        (0..len)
            .step_by(PLAINTEXT_EMIT_SIZE)
            .map(move |off| Ok(data.slice(off..len.min(off + PLAINTEXT_EMIT_SIZE)))),
    ))
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
        let head_cipher = self.head_cipher.clone();
        let head_key_fp = self.head_key_fp;
        Box::pin(async move {
            // Read plaintext from source
            let plaintext = fs::read(&source).await.map_err(|e| {
                DomainError::internal_error("Encryption", format!("read source: {e}"))
            })?;
            let out = frame_write(head_cipher, head_key_fp, plaintext).await?;
            inner.put_blob_from_bytes(&hash, out).await
        })
    }

    fn put_blob_from_bytes(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let head_cipher = self.head_cipher.clone();
        let head_key_fp = self.head_key_fp;
        Box::pin(async move {
            let out = frame_write(head_cipher, head_key_fp, data.to_vec()).await?;
            inner.put_blob_from_bytes(&hash, out).await
        })
    }

    fn put_blob_from_bytes_unsynced(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let head_cipher = self.head_cipher.clone();
        let head_key_fp = self.head_key_fp;
        Box::pin(async move {
            let out = frame_write(head_cipher, head_key_fp, data.to_vec()).await?;
            inner.put_blob_from_bytes_unsynced(&hash, out).await
        })
    }

    /// Frame the plaintext with the head pair's format (encrypted-v1
    /// or plaintext-v1), then delegate the atomic replace to the
    /// inner backend. Used by `storage_rotate` to actually change the
    /// on-disk bytes — `put_blob_from_bytes` would silently no-op on
    /// `LocalBlobBackend` when the object key already exists.
    fn put_blob_from_bytes_replace(
        &self,
        hash: &str,
        data: Bytes,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<u64, DomainError>> + Send + '_>> {
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let head_cipher = self.head_cipher.clone();
        let head_key_fp = self.head_key_fp;
        Box::pin(async move {
            let out = frame_write(head_cipher, head_key_fp, data.to_vec()).await?;
            inner.put_blob_from_bytes_replace(&hash, out).await
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
        let pairs = self.pairs.clone();
        let fp_ciphers = self.fp_ciphers.clone();
        let head_cipher = self.head_cipher.clone();
        let head_key_fp = self.head_key_fp;
        Box::pin(async move {
            // Collect the full blob, dispatch on magic bytes off the
            // runtime, then stream zero-copy plaintext slices.
            let enc_stream = inner.get_blob_stream(&hash).await?;
            let encrypted = collect_stream(enc_stream).await?;
            let len = encrypted.len();
            let hash_for_dispatch = hash.clone();
            let plaintext = offload_crypto(len, move || {
                read_dispatch(
                    &pairs,
                    &fp_ciphers,
                    head_cipher.as_deref(),
                    head_key_fp,
                    &hash_for_dispatch,
                    encrypted,
                )
            })
            .await?;
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
        let pairs = self.pairs.clone();
        let fp_ciphers = self.fp_ciphers.clone();
        let head_cipher = self.head_cipher.clone();
        let head_key_fp = self.head_key_fp;
        Box::pin(async move {
            // Decrypt (or unwrap) the full blob, then slice the plaintext
            // range without copying. For CDC chunks (every blob written
            // since chunking landed) this is ≤ 1 MiB; only legacy whole-file
            // blobs pay a full-blob decrypt here — see the module docs.
            let enc_stream = inner.get_blob_stream(&hash).await?;
            let encrypted = collect_stream(enc_stream).await?;
            let len = encrypted.len();
            let hash_for_dispatch = hash.clone();
            let plaintext = offload_crypto(len, move || {
                read_dispatch(
                    &pairs,
                    &fp_ciphers,
                    head_cipher.as_deref(),
                    head_key_fp,
                    &hash_for_dispatch,
                    encrypted,
                )
            })
            .await?;

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
        // Plaintext size = stored size - per-format overhead. The exact
        // overhead depends on which format the blob is in (encrypted-v1
        // = 43, plaintext-v1 = 15, legacy-encrypted = 28, legacy-plain =
        // 0), which we can't know without inspecting bytes. We assume
        // the blob was written under the wrapper's current head — that's
        // true for every new write from K2 onward.
        //
        // For legacy blobs still on disk the estimate is off by
        // ±(HEADER_SIZE) or so. Since `blob_size` is used for capacity
        // metrics and admin dashboards (not byte-exact accounting —
        // content-hash is the source of truth for that), a small drift
        // during the legacy-blob window is acceptable. If a hot path
        // starts depending on byte-exact `blob_size`, revisit.
        let inner = self.inner.clone();
        let hash = hash.to_string();
        let overhead = if self.head_cipher.is_some() {
            ENCRYPTED_V1_OVERHEAD as u64
        } else {
            HEADER_SIZE as u64
        };
        Box::pin(async move {
            let stored = inner.blob_size(&hash).await?;
            Ok(stored.saturating_sub(overhead))
        })
    }

    fn health_check(
        &self,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<StorageHealthStatus, DomainError>> + Send + '_>,
    > {
        let inner = self.inner.clone();
        let (outer_name, cipher_desc) = if self.head_cipher.is_some() {
            ("encrypted", "AES-256-GCM")
        } else {
            ("v1-plaintext", "none")
        };
        Box::pin(async move {
            let mut status = inner.health_check().await?;
            status.backend_type = format!("{outer_name}({})", status.backend_type);
            status.message = format!("{} | Encryption: {cipher_desc}", status.message);
            Ok(status)
        })
    }

    fn backend_type(&self) -> &'static str {
        // Choice 2/B: dynamic — reflects head-pair semantics so admin
        // surfaces show "v1-plaintext(local)" for a `none:`-headed
        // entry instead of misleadingly saying "encrypted(local)".
        if self.head_cipher.is_some() {
            "encrypted"
        } else {
            "v1-plaintext"
        }
    }

    /// Transparent wrapper: the inner backend serves the bytes.
    fn read_prefetch(&self) -> usize {
        self.inner.read_prefetch()
    }

    fn local_blob_path(&self, _hash: &str) -> Option<PathBuf> {
        // Encrypted blobs cannot be served directly from disk
        None
    }

    /// Enumeration = plaintext hashes, same as the inner backend.
    /// Encryption operates on payload bytes, not on the hash key:
    /// blob objects on the inner backend are stored under the
    /// PLAINTEXT hash so dedup works. Delegating list to the inner
    /// backend therefore returns exactly the right identifiers.
    fn list_blob_hashes(
        &self,
        cursor: Option<String>,
        limit: usize,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        crate::application::ports::blob_storage_ports::BlobListPage,
                        DomainError,
                    >,
                > + Send
                + '_,
        >,
    > {
        self.inner.list_blob_hashes(cursor, limit)
    }
}

/// Frame a plaintext payload into a v1 blob per the head-pair
/// configuration. Encrypted case runs the AEAD on the blocking pool
/// for large payloads; plaintext case is a small header-prepend and
/// stays inline (no crypto = no offload).
async fn frame_write(
    head_cipher: Option<Arc<Aes256Gcm>>,
    head_key_fp: [u8; KEY_FP_SIZE],
    plaintext: Vec<u8>,
) -> Result<Bytes, DomainError> {
    match head_cipher {
        Some(cipher) => {
            let len = plaintext.len();
            offload_crypto(len, move || encrypt_v1(&cipher, head_key_fp, &plaintext)).await
        }
        None => Ok(write_plaintext_v1(&plaintext)),
    }
}

/// Collect a byte stream into a single `Vec<u8>`.
///
/// Modern blobs are CDC chunks (≤ `CDC_MAX_CHUNK` + nonce/tag overhead),
/// delivered here as small reader frames — growing from `Vec::new()` paid
/// ~log₂(n) reallocations + a wasted ~0.75×-size memcpy per read. Reserving
/// one chunk's worth up front on the first frame makes the common case a
/// single allocation; legacy whole-file blobs beyond that fall back to
/// normal doubling (benches/ROUND11.md §16: 9 → 1 allocs on a 1 MiB blob).
async fn collect_stream(stream: BlobStream) -> Result<Vec<u8>, DomainError> {
    use futures::StreamExt;
    let mut stream = stream;
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk
            .map_err(|e| DomainError::internal_error("Encryption", format!("stream read: {e}")))?;
        if buf.capacity() == 0 {
            buf.reserve(
                (crate::infrastructure::services::dedup_service::CDC_MAX_CHUNK
                    + NONCE_SIZE
                    + TAG_SIZE)
                    .max(bytes.len()),
            );
        }
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
        let encrypted = EncryptedBlobBackend::new_single_aes(local, &key);

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
        let encrypted = EncryptedBlobBackend::new_single_aes(local, &key);

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
        let encrypted = EncryptedBlobBackend::new_single_aes(local.clone(), &key);

        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        encrypted
            .put_blob_from_bytes(hash, Bytes::from_static(b"sensitive payload"))
            .await
            .unwrap();

        // Corrupt one ciphertext byte on disk. v1 layout: 15-byte
        // header + 12-byte nonce + ciphertext, so the first
        // ciphertext byte is at `HEADER_SIZE + NONCE_SIZE`. Flipping
        // it must fail AEAD tag verification.
        let path = local.local_blob_path(hash).expect("local path");
        let mut raw = std::fs::read(&path).unwrap();
        raw[HEADER_SIZE + NONCE_SIZE] ^= 0xFF;
        std::fs::write(&path, raw).unwrap();

        assert!(encrypted.get_blob_stream(hash).await.is_err());
    }

    /// Decrypting with a different key must fail — post-K2 via the
    /// key_fp lookup (writer's fp isn't in the reader's map).
    #[tokio::test]
    async fn test_wrong_key_fails_decrypt() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let hash = "aaaabbbbccccddddaaaabbbbccccddddaaaabbbbccccddddaaaabbbbccccdddd";
        let writer = EncryptedBlobBackend::new_single_aes(
            local.clone(),
            &EncryptedBlobBackend::generate_key(),
        );
        writer
            .put_blob_from_bytes(hash, Bytes::from_static(b"locked"))
            .await
            .unwrap();

        let reader =
            EncryptedBlobBackend::new_single_aes(local, &EncryptedBlobBackend::generate_key());
        // Result::unwrap_err needs Ok: Debug; BlobStream isn't Debug.
        // Match directly, and extract the message off the DomainError.
        let err = match reader.get_blob_stream(hash).await {
            Ok(_) => panic!("expected a decrypt error, got Ok"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("key_fp") || msg.contains("does not match"),
            "expected NoKeyForBlob-shape error, got: {msg}"
        );
    }

    // ─────────────────────────────────────────────────────────────
    // K2 tests — v1 header format + magic-byte dispatch + legacy
    // fallback + pair-list read routing.
    // ─────────────────────────────────────────────────────────────

    /// Every encrypted-v1 blob starts with the magic + version +
    /// head-pair fingerprint. Pins the on-disk byte layout so a
    /// future refactor can't silently break the format.
    #[tokio::test]
    async fn v1_encrypted_blob_has_expected_header() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let key = [42u8; 32];
        let backend = EncryptedBlobBackend::new_single_aes(local.clone(), &key);

        let hash = "1111111111111111111111111111111111111111111111111111111111111111";
        backend
            .put_blob_from_bytes(hash, Bytes::from_static(b"hello world"))
            .await
            .unwrap();

        let path = local.local_blob_path(hash).expect("local path");
        let raw = std::fs::read(&path).unwrap();
        assert!(
            raw.len() >= ENCRYPTED_V1_OVERHEAD,
            "blob too short: {}",
            raw.len()
        );
        assert_eq!(&raw[..5], OXCPT_MAGIC, "missing OXCPT magic");
        assert_eq!(&raw[5..7], &V1_VERSION_BYTES, "wrong version bytes");
        let expected_fp = KeyPair::new_aes_gcm(key).key_fp();
        assert_eq!(&raw[7..HEADER_SIZE], &expected_fp, "wrong key_fp in header");
    }

    /// `none:`-headed entry emits plaintext-v1 (header + raw
    /// payload, no crypto). Round-trip must yield identity bytes.
    #[tokio::test]
    async fn v1_plaintext_blob_round_trips() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let backend = EncryptedBlobBackend::new(local.clone(), vec![KeyPair::new_none()]);

        let hash = "2222222222222222222222222222222222222222222222222222222222222222";
        let payload = Bytes::from_static(b"cleartext bytes");
        backend
            .put_blob_from_bytes(hash, payload.clone())
            .await
            .unwrap();

        // On-disk shape: magic + version + zero fp + raw payload.
        let path = local.local_blob_path(hash).expect("local path");
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[..5], OXCPT_MAGIC);
        assert_eq!(&raw[5..7], &V1_VERSION_BYTES);
        assert_eq!(&raw[7..HEADER_SIZE], &[0u8; KEY_FP_SIZE]);
        assert_eq!(&raw[HEADER_SIZE..], payload.as_ref());

        // Read must strip the header and return payload identity.
        let stream = backend.get_blob_stream(hash).await.unwrap();
        let round_tripped = collect_stream(stream).await.unwrap();
        assert_eq!(round_tripped, payload.as_ref());
    }

    /// Legacy fallback: a blob written with the pre-K2 AEAD shape
    /// (nonce | ct | tag, no OXCPT header) still decrypts via the
    /// head-pair AES key. This is the guarantee that upgrading to
    /// K2 doesn't break existing encrypted deployments.
    #[tokio::test]
    async fn legacy_encrypted_blob_still_readable() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let key = [0x33u8; 32];
        let backend = EncryptedBlobBackend::new_single_aes(local.clone(), &key);

        // Craft a legacy blob by hand: AES-GCM with random nonce,
        // no OXCPT header. Matches exactly what pre-K2 code wrote.
        let plaintext = b"legacy secret";
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let mut legacy = Vec::new();
        legacy.extend_from_slice(nonce.as_slice());
        legacy.extend_from_slice(plaintext);
        let tag = cipher
            .encrypt_in_place_detached(&nonce, b"", &mut legacy[NONCE_SIZE..])
            .unwrap();
        legacy.extend_from_slice(&tag);

        // Write the raw bytes directly onto the local backend, bypassing
        // the wrapper (else it'd add a v1 header).
        local
            .put_blob_from_bytes(
                "3333333333333333333333333333333333333333333333333333333333333333",
                Bytes::from(legacy),
            )
            .await
            .unwrap();

        // The wrapper's read path must dispatch on absent magic →
        // legacy branch → head-pair AES decode.
        let stream = backend
            .get_blob_stream("3333333333333333333333333333333333333333333333333333333333333333")
            .await
            .unwrap();
        let got = collect_stream(stream).await.unwrap();
        assert_eq!(got, plaintext);
    }

    /// Legacy fallback for pure plaintext: entry has no encryption
    /// (empty pair list → `none:` synthesised), reads of a raw-byte
    /// blob written before the wrapper existed still return raw.
    #[tokio::test]
    async fn legacy_plaintext_blob_still_readable() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let backend = EncryptedBlobBackend::new(local.clone(), vec![]);

        let hash = "4444444444444444444444444444444444444444444444444444444444444444";
        let raw = b"just some bytes, no header";
        // Bypass the wrapper — write raw plaintext directly.
        local
            .put_blob_from_bytes(hash, Bytes::from_static(raw))
            .await
            .unwrap();

        // No magic → legacy path → head is None → return bytes as-is.
        let stream = backend.get_blob_stream(hash).await.unwrap();
        let got = collect_stream(stream).await.unwrap();
        assert_eq!(got, raw);
    }

    /// Pair-list key rotation: write under pair[0]'s key, add
    /// pair[1] as head, read must still succeed via pair[0]'s
    /// key_fp entry in the lookup table.
    #[tokio::test]
    async fn read_dispatches_by_key_fp_in_pair_list() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let k_old = [0x11u8; 32];
        let k_new = [0x22u8; 32];

        // Write with a single-pair wrapper using k_old.
        let writer = EncryptedBlobBackend::new_single_aes(local.clone(), &k_old);
        let hash = "5555555555555555555555555555555555555555555555555555555555555555";
        writer
            .put_blob_from_bytes(hash, Bytes::from_static(b"payload"))
            .await
            .unwrap();

        // Reader has BOTH keys: k_old at position 0, k_new at head.
        // The blob's key_fp field points at k_old → lookup succeeds
        // even though writes now go under k_new.
        let reader = EncryptedBlobBackend::new(
            local,
            vec![KeyPair::new_aes_gcm(k_old), KeyPair::new_aes_gcm(k_new)],
        );
        let stream = reader.get_blob_stream(hash).await.unwrap();
        let got = collect_stream(stream).await.unwrap();
        assert_eq!(got, b"payload");
    }

    /// Malformed v1 blob (correct magic, unknown version bytes) →
    /// hard error, never silent misread. Guards against the
    /// theoretical 2⁻⁴⁰ magic-collision case on random legacy data.
    #[tokio::test]
    async fn unknown_v1_version_returns_hard_error() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let backend = EncryptedBlobBackend::new(local.clone(), vec![]);

        let hash = "6666666666666666666666666666666666666666666666666666666666666666";
        // Magic OK, version = 0xFFFF (future format we don't know).
        let mut bogus = Vec::from(*OXCPT_MAGIC);
        bogus.extend_from_slice(&[0xFF, 0xFF]);
        bogus.extend_from_slice(&[0u8; KEY_FP_SIZE]);
        bogus.extend_from_slice(b"payload");
        local
            .put_blob_from_bytes(hash, Bytes::from(bogus))
            .await
            .unwrap();

        let err = match backend.get_blob_stream(hash).await {
            Ok(_) => panic!("expected an UnsupportedBlobVersion error, got Ok"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("unsupported v1 blob version"),
            "expected UnsupportedBlobVersion error, got: {msg}"
        );
    }

    // ─────────────────────────────────────────────────────────────
    // K3 tests — BlobFormat classifier + head_format + read_and_classify.
    //
    // These pin the format-inspection contract that `storage_rotate`
    // depends on. The rotate job's per-blob decision tree collapses
    // to `current != head_format ? rewrite : skip`, so any drift in
    // either helper would silently change rotation semantics.
    // ─────────────────────────────────────────────────────────────

    #[test]
    fn classify_recognises_encrypted_v1() {
        let mut blob = Vec::from(*OXCPT_MAGIC);
        blob.extend_from_slice(&V1_VERSION_BYTES);
        let key_fp = [0x11u8; KEY_FP_SIZE];
        blob.extend_from_slice(&key_fp);
        blob.extend_from_slice(b"nonce_ct_tag_bytes_would_go_here");
        assert_eq!(
            BlobFormat::classify(&blob),
            BlobFormat::EncryptedV1 { key_fp }
        );
    }

    #[test]
    fn classify_recognises_plaintext_v1() {
        let mut blob = Vec::from(*OXCPT_MAGIC);
        blob.extend_from_slice(&V1_VERSION_BYTES);
        blob.extend_from_slice(&[0u8; KEY_FP_SIZE]);
        blob.extend_from_slice(b"raw payload after header");
        assert_eq!(BlobFormat::classify(&blob), BlobFormat::PlaintextV1);
    }

    #[test]
    fn classify_recognises_legacy_no_magic() {
        // Random bytes with no OXCPT prefix.
        let raw = b"some legacy bytes not starting with the magic";
        assert_eq!(BlobFormat::classify(raw), BlobFormat::Legacy);
    }

    #[test]
    fn classify_treats_short_magic_only_blob_as_legacy() {
        // 5 bytes = magic only, no room for version+key_fp. Malformed
        // v1; treated as Legacy so the decision tree flags it for
        // rewrite instead of pretending it's a real v1 blob.
        let raw = Vec::from(*OXCPT_MAGIC);
        assert_eq!(BlobFormat::classify(&raw), BlobFormat::Legacy);
    }

    #[test]
    fn classify_empty_is_legacy() {
        assert_eq!(BlobFormat::classify(&[]), BlobFormat::Legacy);
    }

    #[tokio::test]
    async fn head_format_matches_encrypted_head_pair_fp() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let key = [0x77u8; 32];
        let backend = EncryptedBlobBackend::new_single_aes(local, &key);
        match backend.head_format() {
            BlobFormat::EncryptedV1 { key_fp } => {
                assert_eq!(key_fp, KeyPair::new_aes_gcm(key).key_fp());
            }
            other => panic!("expected EncryptedV1, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn head_format_is_plaintext_v1_for_none_head() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        // Empty pair list → wrapper synthesises a single `none:` pair.
        let backend = EncryptedBlobBackend::new(local, vec![]);
        assert_eq!(backend.head_format(), BlobFormat::PlaintextV1);
    }

    #[tokio::test]
    async fn read_and_classify_returns_plaintext_and_current_format() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let k_old = [0xAAu8; 32];
        let k_new = [0xBBu8; 32];
        let hash = "7777777777777777777777777777777777777777777777777777777777777777";

        // Write under the OLD key.
        let writer = EncryptedBlobBackend::new_single_aes(local.clone(), &k_old);
        writer
            .put_blob_from_bytes(hash, Bytes::from_static(b"secret payload"))
            .await
            .unwrap();

        // Reader has BOTH keys, k_new at head. Rotate scenario:
        // classifier should report EncryptedV1{k_old_fp}, decrypt
        // succeeds via key_fp lookup, plaintext round-trips.
        let reader = EncryptedBlobBackend::new(
            local,
            vec![KeyPair::new_aes_gcm(k_old), KeyPair::new_aes_gcm(k_new)],
        );
        let (plaintext, current) = reader.read_and_classify(hash).await.unwrap();
        assert_eq!(plaintext, b"secret payload".as_slice());
        match current {
            BlobFormat::EncryptedV1 { key_fp } => {
                assert_eq!(key_fp, KeyPair::new_aes_gcm(k_old).key_fp());
            }
            other => panic!("expected EncryptedV1 with old fp, got {other:?}"),
        }
        // Confirms the rotate decision: current != head_format →
        // rewrite (key rotation case).
        assert_ne!(current, reader.head_format());
    }

    /// Legacy blob (no OXCPT header) encrypted under a NON-head pair
    /// still decrypts via the fallback loop. This is the
    /// "operator restored a pre-K2 backup post-rotation" case:
    /// the blob was originally encrypted with K1, then K2 was added
    /// and rotated to head. Reading the restored bytes under a
    /// `[K1, K2]` pair-list where K2 is head should succeed by
    /// falling through to K1.
    #[tokio::test]
    async fn legacy_blob_under_non_head_key_still_readable() {
        let tmp = TempDir::new().unwrap();
        let local = Arc::new(LocalBlobBackend::new(&tmp.path().join("blobs")));
        local.initialize().await.unwrap();

        let k_old = [0x11u8; 32]; // will be non-head after rotation
        let k_new = [0x22u8; 32]; // will be head after rotation

        // Craft a legacy blob by hand: AES-GCM with K_OLD, no OXCPT
        // header. Matches exactly what pre-K2 code wrote.
        let plaintext = b"pre-K2 secret restored post-rotation";
        let cipher_old = Aes256Gcm::new_from_slice(&k_old).unwrap();
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let mut legacy = Vec::new();
        legacy.extend_from_slice(nonce.as_slice());
        legacy.extend_from_slice(plaintext);
        let tag = cipher_old
            .encrypt_in_place_detached(&nonce, b"", &mut legacy[NONCE_SIZE..])
            .unwrap();
        legacy.extend_from_slice(&tag);
        let hash = "9999999999999999999999999999999999999999999999999999999999999999";
        local
            .put_blob_from_bytes(hash, Bytes::from(legacy))
            .await
            .unwrap();

        // Reader has BOTH keys, with K_NEW as head. The legacy blob's
        // head decrypt attempt (with K_NEW) will fail on the tag —
        // the fallback loop then tries K_OLD (the only other pair)
        // and succeeds.
        let reader = EncryptedBlobBackend::new(
            local,
            vec![KeyPair::new_aes_gcm(k_old), KeyPair::new_aes_gcm(k_new)],
        );
        let stream = reader.get_blob_stream(hash).await.unwrap();
        let got = collect_stream(stream).await.unwrap();
        assert_eq!(got, plaintext);
    }
}
