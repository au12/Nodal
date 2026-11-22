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
    let mut info = ExifInfo::default();
    let Ok(ex) = exif::Reader::new().read_from_container(&mut Cursor::new(bytes)) else { return info };
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
        if let (Some(f), Some(res), Some(px)) = (
            info.focal_mm,
            get(exif::Tag::FocalPlaneXResolution).and_then(rational),
            get(exif::Tag::PixelXDimension).and_then(rational),
        ) {
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
