//! Decoding every supported input to scene-linear light.
//!
//! * Camera RAW (CR3, CR2, NEF, ARW, RAF, RW2, ORF, PEF, DNG and more) through FlashRaw. The full
//!   active sensor area is used, white balance can be fixed for a whole set, and working copies use
//!   FlashRaw's half-size superpixel decode, which is several times faster than a full demosaic.
//! * TIFF (8/16/32-bit integer, 16/32-bit float, BigTIFF, any compression the `tiff` crate reads).
//! * Photoshop PSD and PSB composites (8/16/32-bit).
//! * JPEG, PNG and WebP.
//!
//! Embedded ICC profiles are honoured (Adobe RGB, ProPhoto, Display P3 …): pixels are converted to
//! linear Rec. 709 primaries with moxcms. Images without a profile are treated as sRGB; 32-bit float
//! data is treated as already linear.

use crate::pixels::{self, LinearImage};
use crate::project::ExifInfo;
use anyhow::{anyhow, bail, ensure, Context, Result};
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// What kind of file an image came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Raw,
    Jpeg,
    Png,
    Tiff,
    Psd,
    Webp,
    Other,
}

/// Size, metadata and format of a source image (orientation applied).
#[derive(Clone, Debug)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub exif: ExifInfo,
    pub kind: SourceKind,
    /// Short format description for the UI, e.g. "CR3 14-bit" or "TIFF 16-bit".
    pub format: String,
    /// As-shot white balance multipliers (R, G, B; G = 1) for RAW files.
    pub as_shot_wb: Option<[f32; 3]>,
}

/// How to decode.
#[derive(Clone, Debug, Default)]
pub struct DecodeSettings {
    /// Longest side of the returned image; 0 = full resolution.
    pub max_dim: u32,
    /// RAW white balance multipliers (R, G, B). `None` uses each file's as-shot balance.
    pub white_balance: Option<[f32; 3]>,
}

/// File extensions the stitcher accepts (lower case).
pub const EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "png", "tif", "tiff", "webp", "psd", "psb", "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf",
    "rw2", "rwl", "orf", "pef", "srw", "x3f", "3fr", "fff", "iiq", "erf", "kdc", "dcr", "mef", "mos", "mrw",
];

pub fn is_supported_path(path: &Path) -> bool {
    path.extension().map(|e| EXTENSIONS.contains(&e.to_string_lossy().to_lowercase().as_str())).unwrap_or(false)
}

fn head(path: &Path, n: usize) -> Result<Vec<u8>> {
    let mut f = File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        let k = f.read(&mut buf[got..])?;
        if k == 0 {
            break;
        }
        got += k;
    }
    buf.truncate(got);
    Ok(buf)
}

/// Works out the kind of file from its first bytes (falling back to the extension).
pub fn sniff(path: &Path) -> Result<SourceKind> {
    let h = head(path, 4096)?;
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if crate::psd::is_psd(&h) {
        return Ok(SourceKind::Psd);
    }
    if h.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Ok(SourceKind::Jpeg);
    }
    if h.starts_with(b"\x89PNG") {
        return Ok(SourceKind::Png);
    }
    if h.len() >= 12 && &h[0..4] == b"RIFF" && &h[8..12] == b"WEBP" {
        return Ok(SourceKind::Webp);
    }
    // TIFF-based RAWs (DNG, NEF, ARW, CR2 …) share the TIFF signature: let FlashRaw decide first,
    // unless the extension says plain TIFF.
    let tiff_sig = h.starts_with(b"II*\0") || h.starts_with(b"MM\0*") || h.starts_with(b"II+\0") || h.starts_with(b"MM\0+");
    if tiff_sig && (ext == "tif" || ext == "tiff") {
        return Ok(SourceKind::Tiff);
    }
    if flashraw::supports(&h).is_some() {
        return Ok(SourceKind::Raw);
    }
    if tiff_sig {
        return Ok(SourceKind::Tiff);
    }
    Ok(SourceKind::Other)
}

/// Reads size and metadata without decoding pixels (where the format allows).
pub fn probe(path: &Path) -> Result<ImageInfo> {
    let kind = sniff(path)?;
    match kind {
        SourceKind::Raw => {
            let raw = flashraw::FlashRaw::open_path(path)
                .map_err(|e| anyhow!("{e}"))
                .with_context(|| format!("could not read {}", path.display()))?;
            Ok(raw_info(&raw, true))
        }
        SourceKind::Psd => {
            let h = crate::psd::probe(path)?;
            Ok(ImageInfo {
                width: h.width,
                height: h.height,
                exif: ExifInfo::default(),
                kind,
                format: format!("{} {}-bit", if h.psb { "PSB" } else { "PSD" }, h.depth),
                as_shot_wb: None,
            })
        }
        _ => {
            // Cheap for JPEG/PNG/WebP: the image crate reads only the header here.
            let exif = read_exif_file(path);
            let reader = image::ImageReader::open(path)?.with_guessed_format()?;
            let mut decoder = reader.into_decoder().context("unsupported or corrupt image")?;
            use image::ImageDecoder;
            let (w, h) = decoder.dimensions();
            let o = decoder.orientation().map(orientation_value).unwrap_or(1);
            let (w, h) = if (5..=8).contains(&o) { (h, w) } else { (w, h) };
            let bits = decoder.color_type().bits_per_pixel() / decoder.color_type().channel_count() as u16;
            Ok(ImageInfo { width: w, height: h, exif, kind, format: format!("{} {bits}-bit", kind_name(kind)), as_shot_wb: None })
        }
    }
}

fn kind_name(k: SourceKind) -> &'static str {
    match k {
        SourceKind::Raw => "RAW",
        SourceKind::Jpeg => "JPEG",
        SourceKind::Png => "PNG",
        SourceKind::Tiff => "TIFF",
        SourceKind::Psd => "PSD",
        SourceKind::Webp => "WebP",
        SourceKind::Other => "Image",
    }
}

fn orientation_value(o: image::metadata::Orientation) -> u32 {
    use image::metadata::Orientation::*;
    match o {
        NoTransforms => 1,
        FlipHorizontal => 2,
        Rotate180 => 3,
        FlipVertical => 4,
        Rotate90FlipH => 5,
        Rotate90 => 6,
        Rotate270FlipH => 7,
        Rotate270 => 8,
    }
}

fn raw_info(raw: &flashraw::FlashRaw, _with_size: bool) -> ImageInfo {
    let m = raw.metadata();
    let fi = raw.frame_info();
    let area = fi.active_area;
    let (mut w, mut h) = if area.width > 0 && area.height > 0 { (area.width, area.height) } else { (m.width, m.height) };
    let o = m.orientation as u32;
    if (5..=8).contains(&o) {
        std::mem::swap(&mut w, &mut h);
    }
    let fmt = m.format.map(|f| format!("{f:?}").to_uppercase()).unwrap_or_else(|| "RAW".into());
    let mut exif = ExifInfo {
        make: Some(m.canonical_make.clone()).filter(|s| !s.is_empty()),
        model: Some(m.canonical_model.clone()).filter(|s| !s.is_empty()),
        lens: m.lens.model.clone(),
        focal_mm: m.exposure.focal_length.map(|v| v as f64).filter(|v| *v > 0.0),
        focal_35mm: m.exposure.focal_length_35mm.map(|v| v as f64).filter(|v| *v > 0.0),
        exposure_time: m.exposure.exposure_time.filter(|v| *v > 0.0),
        f_number: m.exposure.f_number.map(|v| v as f64).filter(|v| *v > 0.0),
        iso: m.exposure.iso.map(|v| v as f64).filter(|v| *v > 0.0),
        taken: m.capture_time.clone(),
    };
    // The EXIF block fills in what the RAW metadata lacks (35 mm equivalent from the focal plane
    // resolution, in particular).
    if let Some(ex) = raw.exif_bytes().map(read_exif_raw) {
        exif.focal_35mm = exif.focal_35mm.or(ex.focal_35mm);
        exif.lens = exif.lens.or(ex.lens);
        exif.focal_mm = exif.focal_mm.or(ex.focal_mm);
        exif.taken = exif.taken.or(ex.taken);
    }
    let wb = fi.white_balance.as_ref().map(|wb| [wb.coeffs[0], wb.coeffs[1], wb.coeffs[2]]);
    ImageInfo { width: w, height: h, exif, kind: SourceKind::Raw, format: format!("{fmt} {}-bit", m.bits_per_sample), as_shot_wb: wb }
}

/// Decodes to scene-linear RGB with orientation applied.
pub fn decode_linear(path: &Path, settings: &DecodeSettings) -> Result<(LinearImage, ImageInfo)> {
    let kind = sniff(path)?;
    let (img, info) = match kind {
        SourceKind::Raw => decode_raw(path, settings)?,
        SourceKind::Tiff => decode_tiff(path).or_else(|e| decode_generic(path, kind).map_err(|_| e))?,
        SourceKind::Psd => decode_psd(path)?,
        _ => decode_generic(path, kind)?,
    };
    let img = if settings.max_dim > 0 { img.downscale_to(settings.max_dim as usize) } else { img };
    Ok((img, info))
}

fn decode_raw(path: &Path, settings: &DecodeSettings) -> Result<(LinearImage, ImageInfo)> {
    use flashraw::{Backend, DecodeOptions, Demosaic, OutputFormat};
    let raw =
        flashraw::FlashRaw::open_path(path).map_err(|e| anyhow!("{e}")).with_context(|| format!("could not read {}", path.display()))?;
    let info = raw_info(&raw, true);
    let fi = raw.frame_info();
    let area = fi.active_area;
    // Half-size superpixels when the caller wants at most about half the resolution.
    let long = area.width.max(area.height).max(1);
    let half = settings.max_dim > 0 && settings.max_dim * 2 <= long + 2 && fi.cfa.is_some();
    let mut opts = DecodeOptions {
        backend: Backend::Auto,
        output: OutputFormat::LinearRgbF32,
        demosaic: if half { Demosaic::HalfSize } else { Demosaic::Auto },
        crop: false,
        // Clipped highlights fade to white instead of turning magenta or cyan.
        highlights: flashraw::Highlights::Blend,
        ..Default::default()
    };
    if let Some(wb) = settings.white_balance {
        opts.white_balance_multipliers = Some(wb);
    }
    let frame = raw.decode(opts).map_err(|e| anyhow!("{e}")).with_context(|| format!("could not decode {}", path.display()))?;
    let (fw, fh) = (frame.width() as usize, frame.height() as usize);
    let data = frame.data.as_f32().ok_or_else(|| anyhow!("unexpected RAW output format"))?.to_vec();
    let mut img = LinearImage::from_data(fw, fh, data);
    // Cut the active area (the uncropped frame includes masked border pixels).
    let (fullw, fullh) = (frame.metadata().raw_width.max(1) as usize, frame.metadata().raw_height.max(1) as usize);
    let sx = fw as f64 / fullw as f64;
    let sy = fh as f64 / fullh as f64;
    if area.width > 0 && area.height > 0 && ((sx - 1.0).abs() < 1e-9 || (sx - 0.5).abs() < 0.01) {
        let x0 = (area.x as f64 * sx).round() as usize;
        let y0 = (area.y as f64 * sy).round() as usize;
        let w = ((area.width as f64 * sx).round() as usize).min(fw - x0);
        let h = ((area.height as f64 * sy).round() as usize).min(fh - y0);
        if w > 0 && h > 0 && (w, h) != (fw, fh) {
            img = img.crop(x0, y0, w, h);
        }
    }
    let o = frame.orientation as u32;
    let img = img.oriented(o);
    Ok((img, info))
}

/// Linear Rec. 709 profile (sRGB primaries, linear tone curve).
fn linear_srgb_profile() -> moxcms::ColorProfile {
    let mut p = moxcms::ColorProfile::new_srgb();
    let lin = moxcms::ToneReprCurve::Parametric(vec![1.0]);
    p.red_trc = Some(lin.clone());
    p.green_trc = Some(lin.clone());
    p.blue_trc = Some(lin);
    p.cicp = None;
    p
}

/// Converts interleaved normalised samples (1 or 3 channels) with an ICC profile to linear Rec. 709
/// RGB. Returns `None` if the profile cannot be used (the caller then assumes sRGB).
fn icc_to_linear(icc: &[u8], src: &[f32], channels: usize) -> Option<Vec<f32>> {
    let profile = moxcms::ColorProfile::new_from_slice(icc).ok()?;
    let dst = linear_srgb_profile();
    let layout = if channels == 1 { moxcms::Layout::Gray } else { moxcms::Layout::Rgb };
    let opts = moxcms::TransformOptions { allow_extended_range_rgb_xyz: true, ..Default::default() };
    let xf = profile.create_transform_f32(layout, &dst, moxcms::Layout::Rgb, opts).ok()?;
    let n = src.len() / channels;
    let mut out = vec![0.0f32; n * 3];
    const CHUNK: usize = 16384;
    let ok = out.par_chunks_mut(CHUNK * 3).zip(src.par_chunks(CHUNK * channels)).all(|(o, s)| xf.transform(s, o).is_ok());
    ok.then_some(out)
}

/// Is this profile plain sRGB? (Skips a needless transform for the common case.)
fn is_srgb_profile(icc: &[u8]) -> bool {
    let s = String::from_utf8_lossy(icc);
    s.contains("sRGB") && !s.contains("scRGB")
}

/// Normalised encoded samples (`channels` per pixel, 0–1) → linear RGB, honouring an ICC profile.
fn to_linear(samples: Vec<f32>, w: usize, h: usize, channels: usize, icc: Option<&[u8]>, float_input: bool) -> LinearImage {
    if float_input {
        let data = expand_to_rgb(samples, channels);
        return LinearImage::from_data(w, h, data);
    }
    if let Some(icc) = icc.filter(|p| !is_srgb_profile(p)) {
        if let Some(data) = icc_to_linear(icc, &samples, channels) {
            return LinearImage::from_data(w, h, data);
        }
    }
    let mut data = expand_to_rgb(samples, channels);
    data.par_iter_mut().for_each(|v| *v = pixels::srgb_to_linear(*v));
    LinearImage::from_data(w, h, data)
}

fn expand_to_rgb(samples: Vec<f32>, channels: usize) -> Vec<f32> {
    match channels {
        3 => samples,
        1 => samples.iter().flat_map(|&v| [v, v, v]).collect(),
        c => samples.chunks_exact(c).flat_map(|p| [p[0], p[1], p[2]]).collect(),
    }
}

fn decode_psd(path: &Path) -> Result<(LinearImage, ImageInfo)> {
    let img = crate::psd::read(path).with_context(|| format!("could not read {}", path.display()))?;
    let exif = img.exif.as_deref().map(read_exif_raw).unwrap_or_default();
    let info = ImageInfo {
        width: img.width as u32,
        height: img.height as u32,
        exif,
        kind: SourceKind::Psd,
        format: format!("{} {}-bit", if img.width > 30000 || img.height > 30000 { "PSB" } else { "PSD" }, img.depth),
        as_shot_wb: None,
    };
    let lin = to_linear(img.data, img.width, img.height, img.channels, img.icc.as_deref(), img.depth == 32);
    Ok((lin, info))
}

fn decode_tiff(path: &Path) -> Result<(LinearImage, ImageInfo)> {
    use tiff::decoder::{Decoder, DecodingResult, Limits};
    use tiff::tags::Tag;
    use tiff::ColorType;
    let file = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut dec = Decoder::new(file)?.with_limits(Limits::unlimited());
    let (w, h) = dec.dimensions()?;
    let ct = dec.colortype()?;
    let (channels, bits) = match ct {
        ColorType::Gray(b) => (1usize, b),
        ColorType::GrayA(b) => (2, b),
        ColorType::RGB(b) => (3, b),
        ColorType::RGBA(b) => (4, b),
        other => bail!("TIFF colour type {other:?} is not supported; save as RGB"),
    };
    let orientation = dec.find_tag_unsigned::<u32>(Tag::Orientation).ok().flatten().unwrap_or(1);
    let icc = match dec.find_tag(Tag::Unknown(34675)) {
        Ok(Some(v)) => v.into_u8_vec().ok(),
        _ => None,
    };
    let result = dec.read_image()?;
    let (samples, float_input): (Vec<f32>, bool) = match result {
        DecodingResult::U8(v) => (v.par_iter().map(|&x| x as f32 / 255.0).collect(), false),
        DecodingResult::U16(v) => (v.par_iter().map(|&x| x as f32 / 65535.0).collect(), false),
        DecodingResult::U32(v) => (v.par_iter().map(|&x| x as f32 / u32::MAX as f32).collect(), false),
        DecodingResult::F16(v) => (v.par_iter().map(|&x| x.to_f32()).collect(), true),
        DecodingResult::F32(v) => (v, true),
        DecodingResult::F64(v) => (v.par_iter().map(|&x| x as f32).collect(), true),
        _ => bail!("unsupported TIFF sample format"),
    };
    ensure!(samples.len() >= w as usize * h as usize * channels, "truncated TIFF data");
    let lin = to_linear(samples, w as usize, h as usize, channels, icc.as_deref(), float_input);
    let lin = lin.oriented(orientation);
    let exif = read_exif_file(path);
    let info = ImageInfo {
        width: lin.width as u32,
        height: lin.height as u32,
        exif,
        kind: SourceKind::Tiff,
        format: format!("TIFF {bits}-bit{}", if float_input { " float" } else { "" }),
        as_shot_wb: None,
    };
    Ok((lin, info))
}

fn decode_generic(path: &Path, kind: SourceKind) -> Result<(LinearImage, ImageInfo)> {
    use image::{DynamicImage, ImageDecoder};
    let exif = read_exif_file(path);
    let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
    reader.no_limits();
    let mut decoder = reader.into_decoder().context("unsupported or corrupt image")?;
    let orientation = decoder.orientation().map(orientation_value).unwrap_or(1);
    let icc = decoder.icc_profile().ok().flatten();
    let img = DynamicImage::from_decoder(decoder).context("could not decode image")?;
    let bits = img.color().bits_per_pixel() / img.color().channel_count() as u16;
    let (w, h) = (img.width() as usize, img.height() as usize);
    let lin = match &img {
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_) | DynamicImage::ImageLuma8(_) | DynamicImage::ImageLumaA8(_)
            if icc.as_deref().map(is_srgb_profile).unwrap_or(true) =>
        {
            LinearImage::from_srgb8(&img.to_rgb8())
        }
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_) => {
            let f = img.to_rgb32f();
            LinearImage::from_data(w, h, f.into_raw())
        }
        _ => {
            let f = img.to_rgb32f();
            to_linear(f.into_raw(), w, h, 3, icc.as_deref(), false)
        }
    };
    let lin = lin.oriented(orientation);
    let info = ImageInfo {
        width: lin.width as u32,
        height: lin.height as u32,
        exif,
        kind,
        format: format!("{} {bits}-bit", kind_name(kind)),
        as_shot_wb: None,
    };
    Ok((lin, info))
}

fn read_exif_file(path: &Path) -> ExifInfo {
    let Ok(f) = File::open(path) else { return ExifInfo::default() };
    let mut r = BufReader::new(f);
    match exif::Reader::new().read_from_container(&mut r) {
        Ok(ex) => crate::io::exif_info(&ex),
        Err(_) => ExifInfo::default(),
    }
}

fn read_exif_raw(data: &[u8]) -> ExifInfo {
    match exif::Reader::new().read_raw(data.to_vec()) {
        Ok(ex) => crate::io::exif_info(&ex),
        Err(_) => ExifInfo::default(),
    }
}

/// Median as-shot white balance of a set of RAW files, so every frame of a panorama is developed
/// with the same colour balance.
pub fn common_white_balance(infos: &[ImageInfo]) -> Option<[f32; 3]> {
    let wbs: Vec<[f32; 3]> = infos.iter().filter_map(|i| i.as_shot_wb).collect();
    if wbs.is_empty() {
        return None;
    }
    let mut out = [1.0f32; 3];
    for (c, o) in out.iter_mut().enumerate() {
        let mut v: Vec<f32> = wbs.iter().map(|w| w[c]).collect();
        v.sort_by(|a, b| a.total_cmp(b));
        *o = v[v.len() / 2];
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psd_and_tiff_decode_to_linear() {
        let dir = tempfile::tempdir().unwrap();
        let (w, h) = (40, 24);
        let srgb: Vec<f32> = (0..w * h * 3).map(|i| (i % 97) as f32 / 96.0).collect();
        // 16-bit PSD without a profile is treated as sRGB.
        let p = dir.path().join("a.psd");
        crate::psd::write(&p, w, h, 3, 16, &srgb, None).unwrap();
        let (img, info) = decode_linear(&p, &DecodeSettings::default()).unwrap();
        assert_eq!((img.width, img.height), (w, h));
        assert!(info.format.starts_with("PSD 16"));
        for (a, b) in img.data.iter().zip(&srgb) {
            assert!((a - pixels::srgb_to_linear(*b)).abs() < 1e-4);
        }
        // 16-bit TIFF.
        let t = dir.path().join("a.tif");
        {
            use tiff::encoder::{colortype, TiffEncoder};
            let data: Vec<u16> = srgb.iter().map(|v| (v * 65535.0 + 0.5) as u16).collect();
            let mut enc = TiffEncoder::new(std::fs::File::create(&t).unwrap()).unwrap();
            enc.write_image::<colortype::RGB16>(w as u32, h as u32, &data).unwrap();
        }
        let (img, info) = decode_linear(&t, &DecodeSettings::default()).unwrap();
        assert!(info.format.starts_with("TIFF 16"), "{}", info.format);
        for (a, b) in img.data.iter().zip(&srgb) {
            assert!((a - pixels::srgb_to_linear(*b)).abs() < 1e-4);
        }
        // Downscaled decode.
        let (small, _) = decode_linear(&t, &DecodeSettings { max_dim: 20, ..Default::default() }).unwrap();
        assert_eq!((small.width, small.height), (20, 12));
    }

    #[test]
    fn adobe_rgb_profile_converts() {
        // Pure Adobe RGB green is outside sRGB: red must come out negative in linear Rec. 709.
        let icc = moxcms::ColorProfile::new_adobe_rgb().encode().unwrap();
        let out = icc_to_linear(&icc, &[0.0, 1.0, 0.0], 3).unwrap();
        assert!(out[0] < -0.05 && out[1] > 0.9, "{out:?}");
    }
}
