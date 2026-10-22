//! Bounded image decoding for untrusted uploads.
//!
//! Every decode of user-supplied bytes goes through here so the memory bound
//! is stated once instead of per call site.
//!
//! ## Why the crate default is not enough
//!
//! `image::load_from_memory` builds an `ImageReader` with `Limits::default()`,
//! which sets `max_alloc: Some(512 MiB)` — so JPEG, PNG and GIF are already
//! bounded, because those codecs call `Limits::reserve` as they allocate.
//!
//! **WebP does not.** `max_alloc` is documented as "non-strict by default and
//! some decoders may ignore it", and the webp codec contains no reference to
//! limits at all. It also does not implement `set_limits`, so it inherits the
//! `ImageDecoder` trait default — and that default checks only
//! `check_dimensions`, which consults `max_image_width` / `max_image_height`.
//! Both are `None` by default. The net effect: a malformed or deliberately
//! crafted WebP has **no allocation ceiling**, and an OOM kills the process —
//! `catch_unwind` cannot catch an allocation failure.
//!
//! ## What this module does instead
//!
//! Bound the *area* before decoding, which works for every codec because it
//! never relies on the codec honouring anything:
//!
//! 1. Read the header only (`into_dimensions`) — cheap, allocates no pixels.
//! 2. Reject if `width * height` exceeds [`MAX_DECODE_PIXELS`].
//! 3. Decode with explicit [`image::Limits`], making the allocation ceiling
//!    visible and tunable rather than an inherited default, and setting the
//!    per-dimension caps that the trait default *does* enforce — so even a
//!    codec that ignores `max_alloc` is still bounded.
//!
//! A rejected image is not a failed request: the caller treats it as "no
//! thumbnail / no transcode for this file", which is the correct degradation
//! for a file we can store and serve but not preview.

use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use std::io::Cursor;

/// Largest image we will decode, in pixels. 50 megapixels → ~200 MB RGBA.
///
/// **Moved here from `thumbnail_service`, value unchanged.** That module had
/// already set this policy and enforced it in two places; the number is kept
/// exactly so this consolidation changes no behaviour on the paths that were
/// already guarded, and extends the same rule to the three that were not.
///
/// Expressed as an area rather than per-side limits on purpose: a 20000×5000
/// panorama is 100 MP and would be rejected, but any square-ish per-side cap
/// tight enough to bound memory rejects far more. If the cap ever needs
/// raising for large-sensor or stitched images, this is the one line.
pub const MAX_DECODE_PIXELS: u64 = 50_000_000;

/// Hard per-side ceiling, as defence for codecs that ignore `max_alloc`.
///
/// Only reached by images that slip past the area check, which cannot happen
/// via [`decode_bounded`] — this exists because the trait-default
/// `set_limits` enforces *these* and nothing else, so they are the only knob
/// guaranteed to bind on every codec.
const MAX_SIDE: u32 = 65_535;

/// Allocation ceiling handed to the decoder.
///
/// Same value as `Limits::default()`, set explicitly so it is visible here
/// rather than inherited, and so changing it is a one-line edit in one place.
const MAX_ALLOC_BYTES: u64 = 512 * 1024 * 1024;

/// Built from `default()` and mutated — `image::Limits` is
/// `#[non_exhaustive]`, so it cannot be constructed field-by-field. That is
/// the better default anyway: a future `image` release adding another limit
/// gets its upstream value rather than being silently left unset by us.
fn limits() -> Limits {
    let mut l = Limits::default();
    l.max_image_width = Some(MAX_SIDE);
    l.max_image_height = Some(MAX_SIDE);
    l.max_alloc = Some(MAX_ALLOC_BYTES);
    l
}

/// Why a decode was refused before it began.
#[derive(Debug)]
pub enum DecodeRejection {
    /// Header parsed, but the image is larger than [`MAX_DECODE_PIXELS`].
    TooManyPixels { width: u32, height: u32 },
    /// The bytes are not a readable image at all.
    Unreadable(String),
}

impl std::fmt::Display for DecodeRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyPixels { width, height } => write!(
                f,
                "image too large to decode: {width}x{height} exceeds {MAX_DECODE_PIXELS} pixels"
            ),
            Self::Unreadable(e) => write!(f, "unreadable image: {e}"),
        }
    }
}

/// Check the header's declared dimensions against the pixel budget.
///
/// Split out so a caller that already holds a reader can reuse the rule.
fn check_area(width: u32, height: u32) -> Result<(), DecodeRejection> {
    if u64::from(width) * u64::from(height) > MAX_DECODE_PIXELS {
        return Err(DecodeRejection::TooManyPixels { width, height });
    }
    Ok(())
}

/// Decode with the format guessed from the bytes.
pub fn decode_bounded(bytes: &[u8]) -> Result<DynamicImage, DecodeRejection> {
    decode_inner(bytes, None)
}

/// Decode with the format already known (the transcode path knows it from the
/// stored mime, so re-guessing would be wasted work).
pub fn decode_bounded_with_format(
    bytes: &[u8],
    format: ImageFormat,
) -> Result<DynamicImage, DecodeRejection> {
    decode_inner(bytes, Some(format))
}

/// Build a reader, honouring a known format or sniffing one.
fn reader_for(
    bytes: &[u8],
    format: Option<ImageFormat>,
) -> Result<ImageReader<Cursor<&[u8]>>, DecodeRejection> {
    match format {
        Some(f) => {
            let mut r = ImageReader::new(Cursor::new(bytes));
            r.set_format(f);
            Ok(r)
        }
        None => ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| DecodeRejection::Unreadable(e.to_string())),
    }
}

/// Two readers over the same bytes, deliberately: `into_dimensions` consumes
/// the reader, and building a second one is free — it is a `Cursor` over a
/// borrowed slice. The first reads the header only and allocates no pixels,
/// so the area check happens before any large allocation is possible.
fn decode_inner(
    bytes: &[u8],
    format: Option<ImageFormat>,
) -> Result<DynamicImage, DecodeRejection> {
    let (w, h) = reader_for(bytes, format)?
        .into_dimensions()
        .map_err(|e| DecodeRejection::Unreadable(e.to_string()))?;
    check_area(w, h)?;

    let mut decoder = reader_for(bytes, format)?;
    decoder.limits(limits());
    decoder
        .decode()
        .map_err(|e| DecodeRejection::Unreadable(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_is_rejected_not_panicking() {
        assert!(matches!(
            decode_bounded(b"definitely not an image"),
            Err(DecodeRejection::Unreadable(_))
        ));
    }

    /// The budget is an area, so the shape that matters is total pixels, not
    /// either side. Pins the arithmetic, not a decode.
    #[test]
    fn area_budget_rejects_bombs_and_admits_real_sensors() {
        // 8000x6000 = 48 MP — a large but entirely real camera file.
        assert!(check_area(8_000, 6_000).is_ok());
        // 60000x60000 = 3.6 GP — the classic decompression bomb shape, a few
        // KB on the wire.
        assert!(matches!(
            check_area(60_000, 60_000),
            Err(DecodeRejection::TooManyPixels { .. })
        ));
        // Exactly at the budget is allowed; one pixel over is not.
        assert!(check_area(10_000, 5_000).is_ok());
        assert!(check_area(10_001, 5_000).is_err());
    }

    #[test]
    fn explicit_alloc_ceiling_matches_the_crate_default() {
        // Not a coincidence worth drifting: the crate's own default is
        // 512 MiB, and stating it explicitly is only safe while the two
        // agree in magnitude.
        assert_eq!(MAX_ALLOC_BYTES, Limits::default().max_alloc.unwrap());
    }
}
