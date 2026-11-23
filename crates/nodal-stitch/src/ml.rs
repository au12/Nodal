//! Learned matching for photos the SIFT pass cannot connect: low texture, repeated patterns, big
//! exposure or viewpoint changes. RaCo keypoints with ALIKED descriptors are matched by LightGlue,
//! all run on ONNX Runtime. The two models (about 65 MB) are downloaded once, on first use, from
//! pinned Hugging Face revisions and checked against their SHA-256.
//!
//! Model credit: RaCo (Apache-2.0, ETH CVG), ALIKED (BSD-3-Clause, Zhao et al.), LightGlue
//! (Apache-2.0, ETH CVG); ONNX exports by kornia.

use crate::align::{Rescue, RescueMatches};
use crate::pixels::resize_area;
use crate::Source;
use anyhow::{bail, Context, Result};
use image::RgbImage;
use nalgebra::DMatrix;
use ort::session::Session;
use ort::value::Tensor;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub struct ModelSpec {
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

pub const EXTRACTOR: ModelSpec = ModelSpec {
    file: "raco_aliked_extractor_k1024.onnx",
    url: "https://huggingface.co/kornia/raco-aliked/resolve/8f251612206a539335b62671c970bd80911495e3/raco_aliked_extractor_k1024.onnx",
    sha256: "33d788905259eba848f77a099b30089c425f72f7afb9c07f67cc79e8ad2949a6",
    size: 14_259_603,
};

pub const MATCHER: ModelSpec = ModelSpec {
    file: "lightglue_matcher_k1024.onnx",
    url: "https://huggingface.co/kornia/lightglue/resolve/68c04837a6fe52a082beb1fecded7438a99c8496/lightglue_matcher_k1024.onnx",
    sha256: "0b95d616137367a50b5b1c656672a6375ce6facf6a68040744c4dec2fabab499",
    size: 51_396_510,
};

/// Keypoints per image (fixed when the models were exported).
const K: usize = 1024;
const DESC: usize = 128;
/// Longest side of the image the extractor sees. Its cost grows with the pixel count; 960 keeps
/// thin overlaps and low-contrast detail while staying around a second per photo on a laptop.
pub const DEFAULT_MAX_SIDE: u32 = 960;
/// LightGlue confidence below which a match is discarded.
const MIN_SCORE: f32 = 0.2;
/// Keypoints whose surroundings vary by less than this (8-bit luma standard deviation) sit in
/// featureless areas: sensor noise and smooth gradients, which no matcher can pair reliably.
const MIN_CONTRAST: f32 = 4.0;

/// Where models are kept: `$NODAL_MODELS`, else the user cache directory.
pub fn model_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("NODAL_MODELS") {
        return PathBuf::from(d);
    }
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("nodal").join("models")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Returns the path of a model, downloading and verifying it first when it is missing.
/// `progress` receives the downloaded fraction.
pub fn fetch(spec: &ModelSpec, dir: &Path, progress: &dyn Fn(f32)) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let path = dir.join(spec.file);
    // Files only reach their final name after the hash matched, so a complete file is a good one.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() == spec.size) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!("{}.part", spec.file));
    let response = ureq::get(spec.url).call().with_context(|| format!("downloading {}", spec.file))?;
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        done += n as u64;
        progress((done as f64 / spec.size as f64).min(1.0) as f32);
    }
    file.flush()?;
    drop(file);
    let got = hex(&hasher.finalize());
    if got != spec.sha256 || done != spec.size {
        let _ = std::fs::remove_file(&tmp);
        bail!("{} failed its integrity check (got {} bytes, SHA-256 {got})", spec.file, done);
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Keypoints and descriptors of one photo.
pub struct LearnedFeatures {
    /// Keypoints in working-copy pixels.
    pub kp: Vec<[f32; 2]>,
    /// Keypoints normalised the way the matcher expects.
    norm: Vec<f32>,
    /// `K × 128`, L2-normalised.
    pub desc: Vec<f32>,
    /// Working-copy pixels per extractor pixel (localisation noise scales with it).
    pub px: f32,
    /// Local contrast at each keypoint: standard deviation of 8-bit luma in a small window.
    /// Featureless areas (clear sky, blown highlights) give the network nothing to go on, and
    /// with nothing to go on LightGlue tends to pair keypoints by position alone.
    pub contrast: Vec<f32>,
}

/// Luma standard deviation in a square window around each point (8-bit levels).
fn local_contrast(img: &RgbImage, pts: &[[f32; 2]], radius: i64) -> Vec<f32> {
    let (w, h) = (img.width() as i64, img.height() as i64);
    let raw = img.as_raw();
    pts.iter()
        .map(|p| {
            let (cx, cy) = (p[0].round() as i64, p[1].round() as i64);
            let (mut s, mut s2, mut n) = (0.0f64, 0.0f64, 0.0f64);
            for y in (cy - radius).max(0)..=(cy + radius).min(h - 1) {
                for x in (cx - radius).max(0)..=(cx + radius).min(w - 1) {
                    let i = ((y * w + x) * 3) as usize;
                    let l = 0.299 * raw[i] as f64 + 0.587 * raw[i + 1] as f64 + 0.114 * raw[i + 2] as f64;
                    s += l;
                    s2 += l * l;
                    n += 1.0;
                }
            }
            if n < 2.0 {
                return 0.0;
            }
            let m = s / n;
            ((s2 / n - m * m).max(0.0)).sqrt() as f32
        })
        .collect()
}

/// The two ONNX sessions. Loading takes about a second, so a process keeps one (see [`shared`]).
pub struct LearnedMatcher {
    extractor: Mutex<Session>,
    matcher: Mutex<Session>,
    pub max_side: u32,
}

fn session(env: &ort::environment::Environment, path: &Path) -> Result<Session> {
    use ort::ep::CPU;
    // The arena would keep the peak working memory of the largest image for the process lifetime.
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let b = Session::builder(env)?
        .with_execution_providers([CPU::default().with_arena_allocator(false).build()])
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut b = b.with_intra_threads(threads).map_err(|e| anyhow::anyhow!("{e}"))?;
    let s = b.commit_from_file(path).with_context(|| format!("loading {}", path.display()))?;
    Ok(s)
}

impl LearnedMatcher {
    pub fn load(dir: &Path, progress: &dyn Fn(f32, &str)) -> Result<LearnedMatcher> {
        let total = (EXTRACTOR.size + MATCHER.size) as f32;
        let ex = fetch(&EXTRACTOR, dir, &|f| progress(f * EXTRACTOR.size as f32 / total, "Downloading learned matcher"))?;
        let mt =
            fetch(&MATCHER, dir, &|f| progress((EXTRACTOR.size as f32 + f * MATCHER.size as f32) / total, "Downloading learned matcher"))?;
        let env = ort::init().with_name("nodal").build()?;
        Ok(LearnedMatcher {
            extractor: Mutex::new(session(&env, &ex)?),
            matcher: Mutex::new(session(&env, &mt)?),
            max_side: std::env::var("NODAL_ML_SIDE").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MAX_SIDE),
        })
    }

    /// Detects keypoints and descriptors on an 8-bit sRGB working copy.
    pub fn extract(&self, img: &RgbImage) -> Result<LearnedFeatures> {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let s = (self.max_side as f64 / w.max(h) as f64).min(1.0);
        let mw = (((w as f64 * s) / 32.0).round() as usize).max(1) * 32;
        let mh = (((h as f64 * s) / 32.0).round() as usize).max(1) * 32;
        let src: Vec<f32> = img.as_raw().iter().map(|&v| v as f32 / 255.0).collect();
        // Shrinking uses exact area averaging; the rare upscale (tiny photos) is nearest-neighbour.
        let small = if mw <= w && mh <= h {
            resize_area(&src, w, h, 3, mw, mh)
        } else {
            let mut out = vec![0.0f32; mw * mh * 3];
            for y in 0..mh {
                for x in 0..mw {
                    let (sx, sy) = ((x * w / mw).min(w - 1), (y * h / mh).min(h - 1));
                    out[(y * mw + x) * 3..(y * mw + x) * 3 + 3].copy_from_slice(&src[(sy * w + sx) * 3..(sy * w + sx) * 3 + 3]);
                }
            }
            out
        };
        let mut chw = vec![0.0f32; 3 * mw * mh];
        for (i, p) in small.chunks_exact(3).enumerate() {
            chw[i] = p[0];
            chw[mw * mh + i] = p[1];
            chw[2 * mw * mh + i] = p[2];
        }
        let input = Tensor::from_array(([1usize, 3, mh, mw], chw.into_boxed_slice()))?;
        let mut ex = self.extractor.lock().unwrap();
        let out = ex.run(ort::inputs!["images" => input])?;
        let (_, kp) = out["keypoints"].try_extract_tensor::<f32>()?;
        let (_, nk) = out["normalized_keypoints"].try_extract_tensor::<f32>()?;
        let (_, de) = out["descriptors"].try_extract_tensor::<f32>()?;
        anyhow::ensure!(kp.len() == K * 2 && nk.len() == K * 2 && de.len() == K * DESC, "unexpected extractor output");
        let (sx, sy) = (w as f32 / mw as f32, h as f32 / mh as f32);
        let kp: Vec<[f32; 2]> = kp.chunks_exact(2).map(|p| [(p[0] + 0.5) * sx - 0.5, (p[1] + 0.5) * sy - 0.5]).collect();
        let px = 0.5 * (sx + sy);
        let contrast = local_contrast(img, &kp, (3.0 * px).round().max(3.0) as i64);
        Ok(LearnedFeatures { kp, norm: nk.to_vec(), desc: de.to_vec(), px, contrast })
    }

    /// LightGlue matches between two photos: (index in a, index in b, confidence).
    pub fn match_pair(&self, a: &LearnedFeatures, b: &LearnedFeatures) -> Result<Vec<(u16, u16, f32)>> {
        let mut nk = Vec::with_capacity(4 * K);
        nk.extend_from_slice(&a.norm);
        nk.extend_from_slice(&b.norm);
        let mut de = Vec::with_capacity(2 * K * DESC);
        de.extend_from_slice(&a.desc);
        de.extend_from_slice(&b.desc);
        let nk = Tensor::from_array(([2usize, 1, K, 2], nk.into_boxed_slice()))?;
        let de = Tensor::from_array(([2usize, 1, K, DESC], de.into_boxed_slice()))?;
        let mut mt = self.matcher.lock().unwrap();
        let out = mt.run(ort::inputs!["normalized_keypoints" => nk, "descriptors" => de])?;
        let (_, m0) = out["matches0"].try_extract_tensor::<i32>()?;
        let (_, s0) = out["mscores0"].try_extract_tensor::<f32>()?;
        Ok(m0
            .iter()
            .zip(s0)
            .enumerate()
            .filter(|(_, (&j, &s))| j >= 0 && (j as usize) < K && s > MIN_SCORE)
            .map(|(i, (&j, &s))| (i as u16, j as u16, s))
            .collect())
    }
}

/// Mutual nearest neighbours between two descriptor sets that also pass a ratio test.
pub fn mutual_matches(a: &LearnedFeatures, b: &LearnedFeatures) -> Vec<(u16, u16)> {
    let da = DMatrix::<f32>::from_row_slice(K, DESC, &a.desc);
    let db = DMatrix::<f32>::from_row_slice(K, DESC, &b.desc);
    // Column-major K×K: column j holds the similarities of every descriptor in a to b's j-th.
    let sim = &da * db.transpose();
    let mut row_best = vec![(f32::MIN, f32::MIN, 0usize); K];
    let mut col_best = vec![0usize; K];
    for (j, col) in sim.as_slice().chunks_exact(K).enumerate() {
        let mut best = (f32::MIN, 0usize);
        for (i, &s) in col.iter().enumerate() {
            if s > best.0 {
                best = (s, i);
            }
            let r = &mut row_best[i];
            if s > r.0 {
                *r = (s, r.0, j);
            } else if s > r.1 {
                r.1 = s;
            }
        }
        col_best[j] = best.1;
    }
    // For unit vectors d² = 2 − 2s, so the distance-ratio test d1 < 0.9·d2 becomes (1−s1) < 0.81·(1−s2).
    row_best
        .iter()
        .enumerate()
        .filter(|(i, &(s1, s2, j))| col_best[j] == *i && (1.0 - s1) < 0.81 * (1.0 - s2))
        .map(|(i, &(_, _, j))| (i as u16, j as u16))
        .collect()
}

static SHARED: OnceLock<Mutex<Option<Arc<LearnedMatcher>>>> = OnceLock::new();

/// The process-wide matcher, loaded (and downloaded) on first use.
pub fn shared(progress: &dyn Fn(f32, &str)) -> Result<Arc<LearnedMatcher>> {
    let cell = SHARED.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap();
    if let Some(m) = guard.as_ref() {
        return Ok(m.clone());
    }
    let m = Arc::new(LearnedMatcher::load(&model_dir(), progress)?);
    *guard = Some(m.clone());
    Ok(m)
}

/// The rescue pass over a set of loaded photos: features are extracted lazily, once per photo.
pub struct SourceRescue {
    images: Vec<Arc<RgbImage>>,
    /// Photos with almost no texture (clear sky): skipped.
    featureless: Vec<OnceLock<bool>>,
    /// Full-resolution pixels per working-copy pixel.
    to_full: Vec<f32>,
    feats: Vec<OnceLock<Arc<LearnedFeatures>>>,
}

impl SourceRescue {
    pub fn new(sources: &[Source]) -> SourceRescue {
        SourceRescue {
            images: sources.iter().map(|s| s.image.clone()).collect(),
            featureless: (0..sources.len()).map(|_| OnceLock::new()).collect(),
            to_full: sources.iter().map(|s| 1.0 / s.work_scale()).collect(),
            feats: (0..sources.len()).map(|_| OnceLock::new()).collect(),
        }
    }

    fn is_featureless(&self, i: usize) -> bool {
        *self.featureless[i].get_or_init(|| crate::pixels::textured_share(&self.images[i], MIN_CONTRAST) < 0.02)
    }

    fn features(&self, m: &LearnedMatcher, images: &[usize], progress: &dyn Fn(f32)) -> Result<()> {
        let todo: Vec<usize> = images.iter().copied().filter(|&i| self.feats[i].get().is_none() && !self.is_featureless(i)).collect();
        let t = std::time::Instant::now();
        for (k, &i) in todo.iter().enumerate() {
            let f = m.extract(&self.images[i])?;
            if std::env::var_os("NODAL_DEBUG_RESCUE").is_some() {
                let mut c = f.contrast.clone();
                c.sort_by(|a, b| a.total_cmp(b));
                let low = c.iter().filter(|&&v| v < MIN_CONTRAST).count();
                eprintln!(
                    "learned features {}: contrast p10 {:.1} p50 {:.1} p90 {:.1}; {low} of {} below {MIN_CONTRAST}",
                    i + 1,
                    c[c.len() / 10],
                    c[c.len() / 2],
                    c[c.len() * 9 / 10],
                    c.len()
                );
            }
            let _ = self.feats[i].set(Arc::new(f));
            progress((k + 1) as f32 / todo.len() as f32);
        }
        if std::env::var_os("NODAL_PROFILE").is_some() && !todo.is_empty() {
            eprintln!("learned: extracted {} photos in {:.2}s", todo.len(), t.elapsed().as_secs_f32());
        }
        Ok(())
    }
}

impl Rescue for SourceRescue {
    fn name(&self) -> &str {
        "learned matcher (RaCo-ALIKED + LightGlue)"
    }

    fn affinity(&self, pairs: &[(usize, usize)], progress: &dyn Fn(f32, &str)) -> Result<Vec<f32>> {
        use rayon::prelude::*;
        let m = shared(progress)?;
        let mut imgs: Vec<usize> =
            pairs.iter().filter(|&&(a, b)| !self.is_featureless(a) && !self.is_featureless(b)).flat_map(|&(a, b)| [a, b]).collect();
        imgs.sort_unstable();
        imgs.dedup();
        self.features(&m, &imgs, &|f| progress(f, "Learned features"))?;
        let t = std::time::Instant::now();
        let v: Vec<f32> = pairs
            .par_iter()
            .map(|&(a, b)| match (self.feats[a].get(), self.feats[b].get()) {
                (Some(fa), Some(fb)) => mutual_matches(fa, fb).len() as f32,
                _ => 0.0,
            })
            .collect();
        if std::env::var_os("NODAL_PROFILE").is_some() {
            eprintln!("learned: affinity of {} pairs in {:.2}s", pairs.len(), t.elapsed().as_secs_f32());
        }
        Ok(v)
    }

    fn correspondences(&self, pairs: &[(usize, usize)], thorough: bool, progress: &dyn Fn(f32, &str)) -> Result<Vec<RescueMatches>> {
        use rayon::prelude::*;
        let m = shared(progress)?;
        let mut imgs: Vec<usize> =
            pairs.iter().filter(|&&(a, b)| !self.is_featureless(a) && !self.is_featureless(b)).flat_map(|&(a, b)| [a, b]).collect();
        imgs.sort_unstable();
        imgs.dedup();
        self.features(&m, &imgs, &|f| progress(0.6 * f, "Learned features"))?;
        let t = std::time::Instant::now();
        let usable = |a: usize, b: usize| self.feats[a].get().is_some() && self.feats[b].get().is_some();
        let to_full = |a: usize, b: usize, matches: &[(u16, u16)]| {
            let (fa, fb) = (self.feats[a].get().unwrap(), self.feats[b].get().unwrap());
            let (sa, sb) = (self.to_full[a], self.to_full[b]);
            RescueMatches {
                pts: matches
                    .iter()
                    .filter(|&&(i, j)| fa.contrast[i as usize].min(fb.contrast[j as usize]) >= MIN_CONTRAST)
                    .map(|&(i, j)| {
                        let (pa, pb) = (fa.kp[i as usize], fb.kp[j as usize]);
                        [pa[0] * sa, pa[1] * sa, pb[0] * sb, pb[1] * sb]
                    })
                    .collect(),
                sigma: 0.5 * (fa.px * sa + fb.px * sb),
            }
        };
        let out: Vec<RescueMatches> = if thorough {
            // LightGlue: slower, but finds matches nearest-neighbour search cannot.
            let mut out = Vec::with_capacity(pairs.len());
            for (k, &(a, b)) in pairs.iter().enumerate() {
                if !usable(a, b) {
                    out.push(RescueMatches::default());
                    continue;
                }
                let found = m.match_pair(self.feats[a].get().unwrap(), self.feats[b].get().unwrap())?;
                let ij: Vec<(u16, u16)> = found.iter().map(|&(i, j, _)| (i, j)).collect();
                out.push(to_full(a, b, &ij));
                progress(0.6 + 0.4 * (k + 1) as f32 / pairs.len() as f32, "Learned matching");
            }
            out
        } else {
            pairs
                .par_iter()
                .map(|&(a, b)| {
                    if usable(a, b) {
                        to_full(a, b, &mutual_matches(self.feats[a].get().unwrap(), self.feats[b].get().unwrap()))
                    } else {
                        RescueMatches::default()
                    }
                })
                .collect()
        };
        if std::env::var_os("NODAL_PROFILE").is_some() {
            eprintln!(
                "learned: matched {} pairs ({}) in {:.2}s",
                pairs.len(),
                if thorough { "LightGlue" } else { "nearest neighbours" },
                t.elapsed().as_secs_f32()
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feats(desc: Vec<f32>) -> LearnedFeatures {
        LearnedFeatures { kp: vec![[0.0, 0.0]; K], norm: vec![0.0; 2 * K], desc, px: 1.0, contrast: vec![10.0; K] }
    }

    /// Deterministic unit descriptors.
    fn random_desc(seed: u64) -> Vec<f32> {
        let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut out = Vec::with_capacity(K * DESC);
        for _ in 0..K {
            let mut v: Vec<f32> = (0..DESC)
                .map(|_| {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    ((state >> 33) as f32 / (1u64 << 31) as f32) - 0.5
                })
                .collect();
            let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter_mut().for_each(|x| *x /= n);
            out.extend(v);
        }
        out
    }

    #[test]
    fn mutual_matches_recover_a_permutation() {
        let a = random_desc(1);
        // b holds a's descriptors in reverse order, slightly perturbed.
        let mut b = vec![0.0f32; K * DESC];
        for i in 0..K {
            let j = K - 1 - i;
            for d in 0..DESC {
                b[j * DESC + d] = a[i * DESC + d] + if d % 7 == 0 { 0.01 } else { 0.0 };
            }
        }
        let m = mutual_matches(&feats(a), &feats(b));
        assert!(m.len() > K * 9 / 10, "only {} matches", m.len());
        assert!(m.iter().all(|&(i, j)| j as usize == K - 1 - i as usize));
        // Unrelated descriptors give (almost) nothing that passes the ratio test.
        let m2 = mutual_matches(&feats(random_desc(2)), &feats(random_desc(3)));
        assert!(m2.len() < K / 20, "{} spurious matches", m2.len());
    }

    #[test]
    fn featureless_photos_are_recognised() {
        let flat = RgbImage::from_fn(320, 240, |x, y| {
            // Smooth gradient plus faint noise, like clear sky.
            let n = ((x * 7919 + y * 104729) % 3) as u8;
            image::Rgb([90 + (y / 8) as u8 + n, 140 + n, 220])
        });
        assert!(crate::pixels::textured_share(&flat, MIN_CONTRAST) < 0.02);
        let busy = RgbImage::from_fn(320, 240, |x, y| {
            let v = if ((x / 5) + (y / 7)) % 2 == 0 { 40 } else { 200 };
            image::Rgb([v, v, v])
        });
        assert!(crate::pixels::textured_share(&busy, MIN_CONTRAST) > 0.5);
        let c = local_contrast(&busy, &[[100.0, 100.0], [2.0, 2.0]], 4);
        assert!(c.iter().all(|&v| v > MIN_CONTRAST));
    }
}
