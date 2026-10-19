//! ONNX-backed face analyzer (SCRFD detector + ArcFace embedder).
//!
//! Compiled only with the `faces-onnx` cargo feature. Mirrors the
//! immich/InsightFace pipeline: detect faces + 5-point landmarks (SCRFD),
//! similarity-align each face to the canonical 112×112 template, then embed
//! (ArcFace) into an L2-normalized 512-d vector. All inference runs on a
//! blocking thread (`spawn_blocking`) so it never stalls a Tokio worker, and
//! each ONNX session is serialized behind a `Mutex` (ORT's `run` needs `&mut`).
//!
//! The heavy numerical post-processing lives in [`super::face_geometry`] (plain
//! Rust, unit-tested); this module only wires it to ONNX Runtime.
//!
//! **Models are operator-provided at runtime, never committed.** `load` returns
//! an error (→ caller falls back to the no-op analyzer) if the ONNX Runtime
//! dylib or either model file is missing or incompatible — the server still
//! boots. The dylib is loaded via [`ort::init_from`] (a fallible path) rather
//! than ORT's lazy loader, which would `panic` on a missing library (fatal
//! under `panic = "abort"`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use image::RgbImage;
use ort::session::Session;
use ort::value::Tensor;

use super::face_geometry as geom;
use crate::application::ports::face_ports::FaceAnalyzerPort;
use crate::common::errors::DomainError;
use crate::domain::entities::face::{BoundingBox, DetectedFace, EMBEDDING_DIM};

/// SCRFD pyramid strides for the 3- and 5-level model variants.
const STRIDES_3: [u32; 3] = [8, 16, 32];
const STRIDES_5: [u32; 5] = [8, 16, 32, 64, 128];

/// Discard faces smaller than this (original-image pixels) — embeddings of tiny
/// faces are unreliable.
const MIN_FACE_PX: f32 = 24.0;
/// Hard cap on faces processed per image (bounds work on crowd shots).
const MAX_FACES: usize = 64;

/// Output layout of an InsightFace SCRFD model, inferred from its output count.
#[derive(Clone, Copy)]
struct ScrfdLayout {
    /// Feature-map count per output kind (3 for strides 8/16/32, 5 with 64/128).
    fmc: usize,
    num_anchors: u32,
    use_kps: bool,
}

impl ScrfdLayout {
    fn from_num_outputs(n: usize) -> Option<Self> {
        match n {
            6 => Some(Self {
                fmc: 3,
                num_anchors: 2,
                use_kps: false,
            }),
            9 => Some(Self {
                fmc: 3,
                num_anchors: 2,
                use_kps: true,
            }),
            10 => Some(Self {
                fmc: 5,
                num_anchors: 1,
                use_kps: false,
            }),
            15 => Some(Self {
                fmc: 5,
                num_anchors: 1,
                use_kps: true,
            }),
            _ => None,
        }
    }

    fn strides(&self) -> &'static [u32] {
        if self.fmc == 3 {
            &STRIDES_3
        } else {
            &STRIDES_5
        }
    }
}

/// Where to find the runtime + models, plus detector knobs. Borrowed paths;
/// nothing is retained after [`OnnxFaceAnalyzer::load`].
pub struct OnnxLoadConfig<'a> {
    /// Path to `libonnxruntime.{so,dylib,dll}`.
    pub dylib: &'a Path,
    /// SCRFD detector `.onnx`.
    pub detector: &'a Path,
    /// ArcFace embedder `.onnx`.
    pub embedder: &'a Path,
    pub det_size: u32,
    pub det_threshold: f32,
    pub nms_threshold: f32,
    /// ORT intra-op threads (0 = let ONNX Runtime decide).
    pub intra_threads: usize,
}

struct Inner {
    detector: Mutex<Session>,
    embedder: Mutex<Session>,
    layout: ScrfdLayout,
    det_size: u32,
    det_threshold: f32,
    nms_threshold: f32,
}

/// Real face analyzer. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct OnnxFaceAnalyzer {
    inner: Arc<Inner>,
}

fn dom(e: impl std::fmt::Display) -> DomainError {
    DomainError::internal_error("Faces", e.to_string())
}

fn build_session(path: &Path, intra_threads: usize) -> Result<Session, DomainError> {
    let mut builder = Session::builder().map_err(dom)?;
    if intra_threads > 0 {
        builder = builder.with_intra_threads(intra_threads).map_err(dom)?;
    }
    builder.commit_from_file(path).map_err(dom)
}

impl OnnxFaceAnalyzer {
    /// Load the ONNX Runtime dylib and both models. Returns an error (caller
    /// falls back to the no-op analyzer) on any missing/incompatible artifact.
    pub fn load(cfg: &OnnxLoadConfig<'_>) -> Result<Self, DomainError> {
        // Fallible dylib load — populates ORT's global handle so later calls
        // never hit the panicking lazy loader.
        ort::init_from(cfg.dylib)
            .map_err(|e| dom(format!("ONNX Runtime dylib: {e}")))?
            .commit();

        let detector = build_session(cfg.detector, cfg.intra_threads)?;
        let embedder = build_session(cfg.embedder, cfg.intra_threads)?;

        let n_out = detector.outputs().len();
        let layout = ScrfdLayout::from_num_outputs(n_out).ok_or_else(|| {
            dom(format!(
                "detector has {n_out} outputs; expected an SCRFD model (6/9/10/15)"
            ))
        })?;
        if !layout.use_kps {
            tracing::warn!(
                target: "oxicloud::faces",
                "SCRFD model has no landmark outputs; face alignment will be approximate"
            );
        }

        tracing::info!(
            target: "oxicloud::faces",
            "ONNX face analyzer ready (detector {} outputs, embedder loaded, det_size={})",
            n_out, cfg.det_size
        );

        Ok(Self {
            inner: Arc::new(Inner {
                detector: Mutex::new(detector),
                embedder: Mutex::new(embedder),
                layout,
                det_size: cfg.det_size,
                det_threshold: cfg.det_threshold,
                nms_threshold: cfg.nms_threshold,
            }),
        })
    }
}

impl Inner {
    /// Full synchronous pipeline for one encoded image.
    fn analyze_blocking(&self, image_bytes: &[u8]) -> Result<Vec<DetectedFace>, DomainError> {
        // Bounded decode — untrusted bytes, and WebP ignores the crate's
        // `max_alloc` default, so an unguarded `load_from_memory` here is an
        // OOM vector that no panic guard can catch.
        let orig = crate::infrastructure::services::image_decode::decode_bounded(image_bytes)
            .map_err(|e| dom(format!("decode image: {e}")))?
            .to_rgb8();
        let (w0, h0) = (orig.width(), orig.height());
        if w0 == 0 || h0 == 0 {
            return Ok(Vec::new());
        }

        let dets = self.detect(&orig)?;

        let mut faces = Vec::new();
        for det in dets.into_iter().take(MAX_FACES) {
            let fw = det.bbox[2] - det.bbox[0];
            let fh = det.bbox[3] - det.bbox[1];
            if fw < MIN_FACE_PX || fh < MIN_FACE_PX {
                continue;
            }
            let Some(embedding) = self.embed(&orig, &det)? else {
                continue;
            };
            let aligned_quality = {
                let inv = geom::similarity_transform_inverse(&det.kps, &geom::ARCFACE_TEMPLATE);
                let aligned = geom::warp_to_aligned(&orig, &inv);
                geom::laplacian_variance(&aligned)
            };
            let x = (det.bbox[0] / w0 as f32).clamp(0.0, 1.0);
            let y = (det.bbox[1] / h0 as f32).clamp(0.0, 1.0);
            let bw = (fw / w0 as f32).clamp(0.0, 1.0);
            let bh = (fh / h0 as f32).clamp(0.0, 1.0);
            faces.push(DetectedFace {
                bbox: BoundingBox { x, y, w: bw, h: bh },
                det_score: det.score,
                quality: Some(aligned_quality),
                embedding,
            });
        }
        Ok(faces)
    }

    /// Run SCRFD and return detections in **original-image pixels**.
    fn detect(&self, orig: &RgbImage) -> Result<Vec<geom::Detection>, DomainError> {
        let det = self.det_size;
        let (nw, nh, scale) = geom::letterbox(orig.width(), orig.height(), det);
        let resized = image::imageops::resize(orig, nw, nh, image::imageops::FilterType::Triangle);
        let mut canvas = RgbImage::new(det, det);
        image::imageops::overlay(&mut canvas, &resized, 0, 0);
        let input = geom::chw_normalized(&canvas, 127.5, 1.0 / 128.0);
        let tensor =
            Tensor::from_array(([1_i64, 3, det as i64, det as i64], input)).map_err(dom)?;

        let layout = self.layout;
        let total = layout.fmc * if layout.use_kps { 3 } else { 2 };
        let raw: Vec<Vec<f32>> = {
            let mut sess = self
                .detector
                .lock()
                .map_err(|_| dom("detector mutex poisoned"))?;
            let outputs = sess.run(ort::inputs![tensor]).map_err(dom)?;
            (0..total)
                .map(|i| {
                    outputs[i]
                        .try_extract_tensor::<f32>()
                        .map(|(_, data)| data.to_vec())
                        .map_err(dom)
                })
                .collect::<Result<_, _>>()?
        };

        let mut dets = Vec::new();
        for (si, &stride) in layout.strides().iter().enumerate() {
            let scores = &raw[si];
            let bbox: Vec<f32> = raw[layout.fmc + si]
                .iter()
                .map(|v| v * stride as f32)
                .collect();
            let kps: Option<Vec<f32>> = if layout.use_kps {
                Some(
                    raw[2 * layout.fmc + si]
                        .iter()
                        .map(|v| v * stride as f32)
                        .collect(),
                )
            } else {
                None
            };
            let feat = det / stride;
            geom::decode_stride(
                scores,
                &bbox,
                kps.as_deref(),
                stride,
                feat,
                feat,
                layout.num_anchors,
                self.det_threshold,
                &mut dets,
            );
        }

        // Scale detector-space coordinates back to the original image.
        let inv_scale = if scale.abs() < 1e-9 { 1.0 } else { 1.0 / scale };
        for d in &mut dets {
            for v in &mut d.bbox {
                *v *= inv_scale;
            }
            for k in &mut d.kps {
                k[0] *= inv_scale;
                k[1] *= inv_scale;
            }
        }
        Ok(geom::nms(dets, self.nms_threshold))
    }

    /// Align one detection and run the ArcFace embedder. Returns `None` if the
    /// embedder produces an unexpected output length.
    fn embed(
        &self,
        orig: &RgbImage,
        det: &geom::Detection,
    ) -> Result<Option<Vec<f32>>, DomainError> {
        let inv = geom::similarity_transform_inverse(&det.kps, &geom::ARCFACE_TEMPLATE);
        let aligned = geom::warp_to_aligned(orig, &inv);
        let input = geom::chw_normalized(&aligned, 127.5, 1.0 / 127.5);
        let size = geom::ALIGN_SIZE as i64;
        let tensor = Tensor::from_array(([1_i64, 3, size, size], input)).map_err(dom)?;

        let mut embedding: Vec<f32> = {
            let mut sess = self
                .embedder
                .lock()
                .map_err(|_| dom("embedder mutex poisoned"))?;
            let outputs = sess.run(ort::inputs![tensor]).map_err(dom)?;
            let (_, data) = outputs[0].try_extract_tensor::<f32>().map_err(dom)?;
            data.to_vec()
        };
        if embedding.len() != EMBEDDING_DIM {
            tracing::warn!(
                target: "oxicloud::faces",
                "embedder returned {} dims, expected {EMBEDDING_DIM}; skipping face",
                embedding.len()
            );
            return Ok(None);
        }
        geom::l2_normalize(&mut embedding);
        Ok(Some(embedding))
    }
}

#[async_trait]
impl FaceAnalyzerPort for OnnxFaceAnalyzer {
    fn is_ready(&self) -> bool {
        true
    }

    async fn analyze(&self, image_bytes: &[u8]) -> Result<Vec<DetectedFace>, DomainError> {
        let inner = self.inner.clone();
        let bytes = image_bytes.to_vec();
        tokio::task::spawn_blocking(move || inner.analyze_blocking(&bytes))
            .await
            .map_err(|e| dom(format!("inference task join: {e}")))?
    }
}
