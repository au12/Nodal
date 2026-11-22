//! Image loading with EXIF (orientation applied) and output encoding.

use crate::project::ExifInfo;
use anyhow::{Context, Result};
use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage};
use std::io::Cursor;
use std::path::Path;

fn rational(f: &exif::Field) -> Option<f64> {
    match &f.value {
        exif::Value::Rational(v) => v.first().map(|r| r.to_f64()),
        exif::Value::SRational(v) => v.first().map(|r| r.to_f64()),
        exif::Value::Short(v) => v.first().map(|&x| x as f64),
        exif::Value::Long(v) => v.first().map(|&x| x as f64),
        exif::Value::Float(v) => v.first().map(|&x| x as f64),
        exif::Value::Double(v) => v.first().copied(),
        _ => None,
    }
    .filter(|v| v.is_finite())
}

fn text(f: &exif::Field) -> Option<String> {
    match &f.value {
        exif::Value::Ascii(v) => {
            let s = v.iter().map(|b| String::from_utf8_lossy(b).trim().to_string()).collect::<Vec<_>>().join(" ");
            let s = s.trim_matches(char::from(0)).trim().to_string();
            (!s.is_empty()).then_some(s)
        }
        _ => None,
    }
}

/// Reads the EXIF fields the stitcher uses. Missing or malformed data simply leaves fields empty.
pub fn read_exif(bytes: &[u8]) -> ExifInfo {
    match exif::Reader::new().read_from_container(&mut Cursor::new(bytes)) {
        Ok(ex) => exif_info(&ex),
        Err(_) => ExifInfo::default(),
    }
}

/// Extracts the fields the stitcher uses from parsed EXIF data.
pub fn exif_info(ex: &exif::Exif) -> ExifInfo {
    let mut info = ExifInfo::default();
    let get = |tag| ex.get_field(tag, exif::In::PRIMARY);
    info.make = get(exif::Tag::Make).and_then(text);
    info.model = get(exif::Tag::Model).and_then(text);
    info.lens = get(exif::Tag::LensModel).and_then(text);
    info.focal_mm = get(exif::Tag::FocalLength).and_then(rational).filter(|v| *v > 0.0);
    info.focal_35mm = get(exif::Tag::FocalLengthIn35mmFilm).and_then(rational).filter(|v| *v > 0.0);
    info.exposure_time = get(exif::Tag::ExposureTime).and_then(rational).filter(|v| *v > 0.0);
    info.f_number = get(exif::Tag::FNumber).and_then(rational).filter(|v| *v > 0.0);
    info.iso = get(exif::Tag::PhotographicSensitivity).and_then(rational).filter(|v| *v > 0.0);
    info.taken = get(exif::Tag::DateTimeOriginal).and_then(text);
    // Derive the 35 mm equivalent from the sensor size when the camera did not record it.
    if info.focal_35mm.is_none() {
        if let (Some(f), Some(res), Some(px)) =
            (info.focal_mm, get(exif::Tag::FocalPlaneXResolution).and_then(rational), get(exif::Tag::PixelXDimension).and_then(rational))
        {
            let unit_mm = match get(exif::Tag::FocalPlaneResolutionUnit).and_then(rational).unwrap_or(2.0) as u32 {
                3 => 10.0,
                4 => 1.0,
                5 => 0.001,
                _ => 25.4,
            };
            let py = get(exif::Tag::PixelYDimension).and_then(rational).unwrap_or(px * 2.0 / 3.0);
            let res_y = get(exif::Tag::FocalPlaneYResolution).and_then(rational).unwrap_or(res);
            if res > 0.0 && res_y > 0.0 {
                let (sw, sh) = (px / res * unit_mm, py / res_y * unit_mm);
                let diag = (sw * sw + sh * sh).sqrt();
                if diag > 1.0 && diag < 120.0 {
                    info.focal_35mm = Some(f * 43.2666 / diag);
                }
            }
        }
    }
    info
}

/// Decodes an image to 8-bit RGB with its EXIF orientation applied.
pub fn decode(bytes: &[u8]) -> Result<(RgbImage, ExifInfo)> {
    let exif = read_exif(bytes);
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    reader.no_limits();
    let mut decoder = reader.into_decoder().context("unsupported or corrupt image")?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).context("could not decode image")?;
    img.apply_orientation(orientation);
    Ok((img.into_rgb8(), exif))
}

pub fn load(path: &Path) -> Result<(RgbImage, ExifInfo)> {
    let bytes = std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    decode(&bytes).with_context(|| format!("could not load {}", path.display()))
}

/// XMP packet with Google Photo Sphere (GPano) metadata, so viewers recognise the panorama and
/// know which part of the sphere a cropped image covers.
pub fn gpano_xmp(pano: &crate::render::Panorama) -> String {
    format!(
        concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Nodal\">",
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">",
            "<rdf:Description rdf:about=\"\" xmlns:GPano=\"http://ns.google.com/photos/1.0/panorama/\" ",
            "GPano:ProjectionType=\"equirectangular\" GPano:UsePanoramaViewer=\"True\" GPano:StitchingSoftware=\"Nodal {}\" ",
            "GPano:CroppedAreaImageWidthPixels=\"{}\" GPano:CroppedAreaImageHeightPixels=\"{}\" ",
            "GPano:FullPanoWidthPixels=\"{}\" GPano:FullPanoHeightPixels=\"{}\" ",
            "GPano:CroppedAreaLeftPixels=\"{}\" GPano:CroppedAreaTopPixels=\"{}\"/>",
            "</rdf:RDF></x:xmpmeta>"
        ),
        env!("CARGO_PKG_VERSION"),
        pano.width,
        pano.height,
        pano.full_width,
        pano.full_height,
        pano.crop_x,
        pano.crop_y
    )
}

/// Encodes a panorama as JPEG with GPano metadata.
pub fn encode_jpeg(pano: &crate::render::Panorama, quality: u8) -> Result<Vec<u8>> {
    anyhow::ensure!(pano.width <= 65535 && pano.height <= 65535, "JPEG cannot store images larger than 65535 pixels; use TIFF or PNG");
    let mut out = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100));
    enc.set_sampling_factor(if quality >= 90 { jpeg_encoder::SamplingFactor::R_4_4_4 } else { jpeg_encoder::SamplingFactor::R_4_2_0 });
    let mut xmp = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    xmp.extend_from_slice(gpano_xmp(pano).as_bytes());
    enc.add_app_segment(1, &xmp)?;
    enc.add_icc_profile(&srgb_icc())?;
    enc.encode(&pano.rgb8(), pano.width as u16, pano.height as u16, jpeg_encoder::ColorType::Rgb)?;
    Ok(out)
}

/// ICC profile of the output colour space: a standard ICC v2 sRGB display profile (D50 PCS,
/// Bradford-adapted primaries, 1024-entry tone curves), built here so every reader (Photoshop,
/// libpng, ImageIO) accepts it.
pub fn srgb_icc() -> Vec<u8> {
    static ICC: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    ICC.get_or_init(build_srgb_icc).clone()
}

fn build_srgb_icc() -> Vec<u8> {
    fn s15(v: f64) -> [u8; 4] {
        ((v * 65536.0).round() as i32).to_be_bytes()
    }
    fn xyz(x: f64, y: f64, z: f64) -> Vec<u8> {
        let mut t = b"XYZ \0\0\0\0".to_vec();
        t.extend_from_slice(&s15(x));
        t.extend_from_slice(&s15(y));
        t.extend_from_slice(&s15(z));
        t
    }
    let desc = {
        let text = b"sRGB IEC61966-2.1 (Nodal)\0";
        let mut t = b"desc\0\0\0\0".to_vec();
        t.extend_from_slice(&(text.len() as u32).to_be_bytes());
        t.extend_from_slice(text);
        t.extend_from_slice(&[0u8; 4 + 4 + 2 + 1 + 67]);
        t
    };
    let cprt = {
        let mut t = b"text\0\0\0\0".to_vec();
        t.extend_from_slice(b"No copyright, use freely\0");
        t
    };
    let curv = {
        const N: usize = 1024;
        let mut t = b"curv\0\0\0\0".to_vec();
        t.extend_from_slice(&(N as u32).to_be_bytes());
        for i in 0..N {
            let v = crate::pixels::srgb_to_linear(i as f32 / (N - 1) as f32) as f64;
            t.extend_from_slice(&((v * 65535.0).round().clamp(0.0, 65535.0) as u16).to_be_bytes());
        }
        t
    };
    // Tag data blocks; the three tone curves share one block.
    let blocks: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"desc", desc),
        (b"cprt", cprt),
        (b"wtpt", xyz(0.9642, 1.0, 0.8249)),
        (b"rXYZ", xyz(0.436066, 0.222488, 0.013916)),
        (b"gXYZ", xyz(0.385147, 0.716873, 0.097076)),
        (b"bXYZ", xyz(0.143066, 0.060608, 0.714096)),
        (b"rTRC", curv),
    ];
    let n_tags = blocks.len() + 2;
    let mut offset = 128 + 4 + 12 * n_tags;
    let mut table = Vec::new();
    let mut data = Vec::new();
    let mut trc_at = (0u32, 0u32);
    for (sig, block) in &blocks {
        while (offset + data.len()) % 4 != 0 {
            data.push(0);
        }
        let at = (offset + data.len()) as u32;
        table.push((**sig, at, block.len() as u32));
        if *sig == b"rTRC" {
            trc_at = (at, block.len() as u32);
        }
        data.extend_from_slice(block);
    }
    table.push((*b"gTRC", trc_at.0, trc_at.1));
    table.push((*b"bTRC", trc_at.0, trc_at.1));
    while (offset + data.len()) % 4 != 0 {
        data.push(0);
    }
    offset += data.len();
    let size = offset as u32;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&[0u8; 4]); // CMM
    out.extend_from_slice(&[0x02, 0x10, 0, 0]); // version 2.1
    out.extend_from_slice(b"mntrRGB XYZ ");
    for v in [2026u16, 1, 1, 0, 0, 0] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(b"acsp");
    out.extend_from_slice(&[0u8; 4]); // platform
    out.extend_from_slice(&[0u8; 4]); // flags
    out.extend_from_slice(&[0u8; 8]); // manufacturer, model
    out.extend_from_slice(&[0u8; 8]); // attributes
    out.extend_from_slice(&0u32.to_be_bytes()); // intent: perceptual
    out.extend_from_slice(&s15(0.9642));
    out.extend_from_slice(&s15(1.0));
    out.extend_from_slice(&s15(0.8249));
    out.extend_from_slice(&[0u8; 4]); // creator
    out.extend_from_slice(&[0u8; 16]); // profile ID
    out.extend_from_slice(&[0u8; 28]); // reserved
    debug_assert_eq!(out.len(), 128);
    out.extend_from_slice(&(n_tags as u32).to_be_bytes());
    for (sig, at, len) in &table {
        out.extend_from_slice(sig);
        out.extend_from_slice(&at.to_be_bytes());
        out.extend_from_slice(&len.to_be_bytes());
    }
    out.extend_from_slice(&data);
    debug_assert_eq!(out.len(), size as usize);
    out
}

#[cfg(test)]
mod icc_tests {
    #[test]
    fn srgb_profile_parses_and_round_trips() {
        let icc = super::srgb_icc();
        let p = moxcms::ColorProfile::new_from_slice(&icc).expect("valid ICC");
        let dst = moxcms::ColorProfile::new_srgb();
        let xf = p.create_transform_8bit(moxcms::Layout::Rgb, &dst, moxcms::Layout::Rgb, Default::default()).unwrap();
        let src = [10u8, 128, 250, 0, 255, 77];
        let mut out = [0u8; 6];
        xf.transform(&src, &mut out).unwrap();
        for (a, b) in src.iter().zip(&out) {
            assert!((*a as i32 - *b as i32).abs() <= 2, "{src:?} -> {out:?}");
        }
    }
}

/// 16-bit samples, RGB or RGBA (alpha where the panorama has voids).
fn samples16(pano: &crate::render::Panorama) -> (Vec<u16>, bool) {
    use rayon::prelude::*;
    if pano.has_gaps() {
        let mut rgba = vec![0u16; pano.width * pano.height * 4];
        rgba.par_chunks_mut(4).zip(pano.rgb.par_chunks(3)).zip(pano.alpha.par_iter()).for_each(|((o, p), a)| {
            o[..3].copy_from_slice(p);
            o[3] = *a as u16 * 257;
        });
        (rgba, true)
    } else {
        (pano.rgb.clone(), false)
    }
}

/// TIFF tag payload of type UNDEFINED (the ICC profile tag requires it).
struct Undefined<'a>(&'a [u8]);

impl tiff::encoder::TiffValue for Undefined<'_> {
    const BYTE_LEN: u8 = 1;
    const FIELD_TYPE: tiff::tags::Type = tiff::tags::Type::UNDEFINED;
    fn count(&self) -> usize {
        self.0.len()
    }
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Borrowed(self.0)
    }
}

fn write_tiff16<W: std::io::Write + std::io::Seek, K: tiff::encoder::TiffKind>(
    mut enc: tiff::encoder::TiffEncoder<W, K>,
    pano: &crate::render::Panorama,
    data: &[u16],
    alpha: bool,
) -> Result<()> {
    use tiff::encoder::colortype;
    use tiff::tags::Tag;
    let (w, h) = (pano.width as u32, pano.height as u32);
    let xmp = gpano_xmp(pano).into_bytes();
    let icc = srgb_icc();
    macro_rules! write {
        ($ct:ty) => {{
            let mut img = enc.new_image::<$ct>(w, h)?;
            img.encoder().write_tag(Tag::Unknown(700), &xmp[..])?;
            if !icc.is_empty() {
                img.encoder().write_tag(Tag::Unknown(34675), Undefined(&icc))?;
            }
            if alpha {
                // Unassociated alpha.
                img.encoder().write_tag(Tag::ExtraSamples, &[2u16][..])?;
            }
            img.rows_per_strip(64)?;
            img.write_data(data)?;
        }};
    }
    if alpha {
        write!(colortype::RGBA16);
    } else {
        write!(colortype::RGB16);
    }
    Ok(())
}

fn save_tiff16(pano: &crate::render::Panorama, path: &Path) -> Result<()> {
    use tiff::encoder::{Compression, DeflateLevel, TiffEncoder};
    let (data, alpha) = samples16(pano);
    let file = std::io::BufWriter::with_capacity(1 << 22, std::fs::File::create(path)?);
    if data.len() * 2 > 3_500_000_000 {
        write_tiff16(TiffEncoder::new_big(file)?.with_compression(Compression::Deflate(DeflateLevel::Fast)), pano, &data, alpha)
    } else {
        write_tiff16(TiffEncoder::new(file)?.with_compression(Compression::Deflate(DeflateLevel::Fast)), pano, &data, alpha)
    }
}

fn save_png16(pano: &crate::render::Panorama, path: &Path) -> Result<()> {
    use image::ImageEncoder;
    let (data, alpha) = samples16(pano);
    let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_be_bytes()).collect();
    let file = std::io::BufWriter::with_capacity(1 << 22, std::fs::File::create(path)?);
    let mut enc = image::codecs::png::PngEncoder::new_with_quality(file, image::codecs::png::CompressionType::Fast, image::codecs::png::FilterType::Adaptive);
    let _ = enc.set_icc_profile(srgb_icc());
    // The image crate expects native-endian u16 bytes for 16-bit buffers.
    let native: Vec<u8> = data.iter().flat_map(|v| v.to_ne_bytes()).collect();
    drop(bytes);
    enc.write_image(&native, pano.width as u32, pano.height as u32, if alpha { image::ExtendedColorType::Rgba16 } else { image::ExtendedColorType::Rgb16 })?;
    Ok(())
}

fn save_psd16(pano: &crate::render::Panorama, path: &Path) -> Result<()> {
    let (data, alpha) = samples16(pano);
    let ch = if alpha { 4 } else { 3 };
    let f: Vec<f32> = data.iter().map(|&v| v as f32 / 65535.0).collect();
    crate::psd::write(path, pano.width, pano.height, ch, 16, &f, Some(&srgb_icc()))
}

/// Saves a panorama; the format follows the file extension: `.jpg` (8-bit), `.tif` / `.png` /
/// `.psd` / `.psb` (16-bit). Formats with alpha keep transparency where voids were left unfilled.
pub fn save_panorama(pano: &crate::render::Panorama, path: &Path, quality: u8) -> Result<()> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => std::fs::write(path, encode_jpeg(pano, quality)?)?,
        "tif" | "tiff" => save_tiff16(pano, path)?,
        "png" => save_png16(pano, path)?,
        "psd" | "psb" => save_psd16(pano, path)?,
        other => anyhow::bail!("unsupported output format \".{other}\" (use .jpg, .tif, .png or .psd)"),
    }
    Ok(())
}
