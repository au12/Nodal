//! Photoshop PSD / PSB reader for the merged (composite) image.
//!
//! Reads 8, 16 and 32-bit RGB and greyscale documents, with raw, RLE (PackBits) or ZIP
//! compression, from both PSD and PSB (large document) files. Layers are skipped: the composite
//! that Photoshop stores when "Maximize Compatibility" is on is what the stitcher needs. The ICC
//! profile and EXIF block are returned so colour and lens data survive.

use anyhow::{bail, ensure, Context, Result};
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Composite pixels, planar per channel, normalised to [0, 1] (32-bit data is returned as is).
pub struct PsdImage {
    pub width: usize,
    pub height: usize,
    /// 1 (grey) or 3 (RGB) colour channels; alpha, if any, is dropped.
    pub channels: usize,
    pub depth: u16,
    /// Interleaved samples, `width × height × channels`.
    pub data: Vec<f32>,
    pub icc: Option<Vec<u8>>,
    pub exif: Option<Vec<u8>>,
}

pub struct PsdHeader {
    pub psb: bool,
    pub channels: u16,
    pub width: u32,
    pub height: u32,
    pub depth: u16,
    pub mode: u16,
}

fn be_u16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn read_u16<R: Read>(r: &mut R) -> Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}
fn read_u32<R: Read>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}
fn read_u64<R: Read>(r: &mut R) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

pub fn is_psd(bytes: &[u8]) -> bool {
    bytes.len() >= 6 && &bytes[0..4] == b"8BPS" && matches!(be_u16(&bytes[4..6]), 1 | 2)
}

fn read_header<R: Read>(r: &mut R) -> Result<PsdHeader> {
    let mut h = [0u8; 26];
    r.read_exact(&mut h).context("file too short for a Photoshop header")?;
    ensure!(&h[0..4] == b"8BPS", "not a Photoshop file");
    let version = be_u16(&h[4..6]);
    ensure!(version == 1 || version == 2, "unknown Photoshop file version {version}");
    Ok(PsdHeader {
        psb: version == 2,
        channels: be_u16(&h[12..14]),
        height: be_u32(&h[14..18]),
        width: be_u32(&h[18..22]),
        depth: be_u16(&h[22..24]),
        mode: be_u16(&h[24..26]),
    })
}

/// Reads only the header (size, depth, colour mode).
pub fn probe(path: &Path) -> Result<PsdHeader> {
    let mut r = BufReader::new(File::open(path)?);
    read_header(&mut r)
}

/// Reads the merged composite image.
pub fn read(path: &Path) -> Result<PsdImage> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
    let h = read_header(&mut r)?;
    let colour = match h.mode {
        1 => 1usize, // greyscale
        3 => 3,      // RGB
        2 => bail!("indexed-colour Photoshop files are not supported; convert to RGB"),
        4 => bail!("CMYK Photoshop files are not supported; convert to RGB"),
        9 => bail!("Lab Photoshop files are not supported; convert to RGB"),
        m => bail!("Photoshop colour mode {m} is not supported; convert to RGB"),
    };
    ensure!(matches!(h.depth, 8 | 16 | 32), "{}-bit Photoshop files are not supported", h.depth);
    ensure!(h.channels as usize >= colour, "Photoshop file has too few channels");
    let (w, ht) = (h.width as usize, h.height as usize);
    ensure!(w > 0 && ht > 0, "empty Photoshop image");

    // Colour mode data.
    let len = read_u32(&mut r)? as i64;
    r.seek(SeekFrom::Current(len))?;
    // Image resources: ICC profile (1039) and EXIF (1058).
    let res_len = read_u32(&mut r)? as usize;
    let mut res = vec![0u8; res_len];
    r.read_exact(&mut res)?;
    let (mut icc, mut exif) = (None, None);
    let mut p = 0usize;
    while p + 12 <= res.len() {
        if &res[p..p + 4] != b"8BIM" {
            break;
        }
        let id = be_u16(&res[p + 4..p + 6]);
        let name_len = res[p + 6] as usize;
        let mut q = p + 6 + 1 + name_len;
        if (1 + name_len) % 2 == 1 {
            q += 1;
        }
        if q + 4 > res.len() {
            break;
        }
        let size = be_u32(&res[q..q + 4]) as usize;
        let start = q + 4;
        let end = (start + size).min(res.len());
        match id {
            1039 => icc = Some(res[start..end].to_vec()),
            1058 => exif = Some(res[start..end].to_vec()),
            _ => {}
        }
        p = end + (size % 2);
    }
    drop(res);
    // Layer and mask information: skipped.
    let lm_len = if h.psb { read_u64(&mut r)? as i64 } else { read_u32(&mut r)? as i64 };
    r.seek(SeekFrom::Current(lm_len))?;

    // Image data.
    let compression = read_u16(&mut r)?;
    let nch = h.channels as usize;
    let bpc = (h.depth / 8) as usize;
    let row_bytes = w * bpc;
    let plane_bytes = row_bytes * ht;
    // Only the colour channels are decoded.
    let keep = colour;
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(keep);
    match compression {
        0 => {
            for c in 0..nch {
                if c < keep {
                    let mut buf = vec![0u8; plane_bytes];
                    r.read_exact(&mut buf).context("truncated Photoshop image data")?;
                    planes.push(buf);
                } else {
                    break;
                }
            }
        }
        1 => {
            // PackBits: a table of compressed row lengths for every row of every channel.
            let rows = nch * ht;
            let mut counts = vec![0usize; rows];
            for c in counts.iter_mut() {
                *c = if h.psb { read_u32(&mut r)? as usize } else { read_u16(&mut r)? as usize };
            }
            for c in 0..keep {
                let total: usize = counts[c * ht..(c + 1) * ht].iter().sum();
                let mut comp = vec![0u8; total];
                r.read_exact(&mut comp).context("truncated Photoshop image data")?;
                // Row offsets so rows decode in parallel.
                let mut offs = Vec::with_capacity(ht + 1);
                let mut o = 0usize;
                offs.push(0);
                for y in 0..ht {
                    o += counts[c * ht + y];
                    offs.push(o);
                }
                let mut plane = vec![0u8; plane_bytes];
                plane.par_chunks_mut(row_bytes).enumerate().try_for_each(|(y, row)| unpack_bits(&comp[offs[y]..offs[y + 1]], row))?;
                planes.push(plane);
            }
        }
        2 | 3 => {
            // ZIP: one zlib stream holding every channel plane back to back.
            let mut comp = Vec::new();
            r.read_to_end(&mut comp)?;
            let mut raw = Vec::with_capacity(plane_bytes * nch);
            flate2::read::ZlibDecoder::new(&comp[..]).read_to_end(&mut raw).context("corrupt ZIP data in Photoshop file")?;
            ensure!(raw.len() >= plane_bytes * keep, "truncated Photoshop image data");
            for c in 0..keep {
                let mut plane = raw[c * plane_bytes..(c + 1) * plane_bytes].to_vec();
                if compression == 3 {
                    plane.par_chunks_mut(row_bytes).for_each(|row| unpredict(row, w, bpc));
                }
                planes.push(plane);
            }
        }
        c => bail!("unknown Photoshop compression {c}"),
    }

    // Interleave and normalise.
    let mut data = vec![0.0f32; w * ht * keep];
    data.par_chunks_mut(w * keep).enumerate().for_each(|(y, row)| {
        for (c, plane) in planes.iter().enumerate() {
            let src = &plane[y * row_bytes..(y + 1) * row_bytes];
            for x in 0..w {
                row[x * keep + c] = match bpc {
                    1 => src[x] as f32 / 255.0,
                    2 => u16::from_be_bytes([src[2 * x], src[2 * x + 1]]) as f32 / 65535.0,
                    _ => f32::from_be_bytes([src[4 * x], src[4 * x + 1], src[4 * x + 2], src[4 * x + 3]]),
                };
            }
        }
    });
    Ok(PsdImage { width: w, height: ht, channels: keep, depth: h.depth, data, icc, exif })
}

/// PackBits decompression of one row.
fn unpack_bits(src: &[u8], dst: &mut [u8]) -> Result<()> {
    let (mut i, mut o) = (0usize, 0usize);
    while i < src.len() && o < dst.len() {
        let n = src[i] as i8;
        i += 1;
        if n >= 0 {
            let len = n as usize + 1;
            ensure!(i + len <= src.len(), "corrupt PackBits data");
            let len = len.min(dst.len() - o);
            dst[o..o + len].copy_from_slice(&src[i..i + len]);
            i += n as usize + 1;
            o += len;
        } else if n != -128 {
            let len = (1 - n as isize) as usize;
            ensure!(i < src.len(), "corrupt PackBits data");
            let v = src[i];
            i += 1;
            let len = len.min(dst.len() - o);
            dst[o..o + len].fill(v);
            o += len;
        }
    }
    Ok(())
}

/// Undoes Photoshop's ZIP-with-prediction filter on one row.
fn unpredict(row: &mut [u8], w: usize, bpc: usize) {
    match bpc {
        1 => {
            for x in 1..w {
                row[x] = row[x].wrapping_add(row[x - 1]);
            }
        }
        2 => {
            let mut prev = u16::from_be_bytes([row[0], row[1]]);
            for x in 1..w {
                let v = u16::from_be_bytes([row[2 * x], row[2 * x + 1]]).wrapping_add(prev);
                row[2 * x..2 * x + 2].copy_from_slice(&v.to_be_bytes());
                prev = v;
            }
        }
        _ => {
            // 32-bit: bytes are delta-coded across the whole row, then stored as four byte planes
            // (all high bytes first).
            let n = row.len();
            for i in 1..n {
                row[i] = row[i].wrapping_add(row[i - 1]);
            }
            let tmp = row.to_vec();
            for x in 0..w {
                for b in 0..4 {
                    row[4 * x + b] = tmp[b * w + x];
                }
            }
        }
    }
}

/// Writes a minimal flattened PSD/PSB (RLE-compressed for 8/16-bit) — used by tests and as an
/// output format.
pub fn write(path: &Path, w: usize, h: usize, channels: usize, depth: u16, interleaved: &[f32], icc: Option<&[u8]>) -> Result<()> {
    use std::io::Write;
    let psb = w > 30000 || h > 30000;
    let mut f = std::io::BufWriter::with_capacity(1 << 20, File::create(path)?);
    f.write_all(b"8BPS")?;
    f.write_all(&(if psb { 2u16 } else { 1 }).to_be_bytes())?;
    f.write_all(&[0u8; 6])?;
    f.write_all(&(channels as u16).to_be_bytes())?;
    f.write_all(&(h as u32).to_be_bytes())?;
    f.write_all(&(w as u32).to_be_bytes())?;
    f.write_all(&depth.to_be_bytes())?;
    f.write_all(&(if channels >= 3 { 3u16 } else { 1 }).to_be_bytes())?;
    f.write_all(&0u32.to_be_bytes())?; // colour mode data
                                       // Image resources.
    let mut res = Vec::new();
    if let Some(icc) = icc {
        res.extend_from_slice(b"8BIM");
        res.extend_from_slice(&1039u16.to_be_bytes());
        res.extend_from_slice(&[0, 0]);
        res.extend_from_slice(&(icc.len() as u32).to_be_bytes());
        res.extend_from_slice(icc);
        if icc.len() % 2 == 1 {
            res.push(0);
        }
    }
    f.write_all(&(res.len() as u32).to_be_bytes())?;
    f.write_all(&res)?;
    if psb {
        f.write_all(&0u64.to_be_bytes())?;
    } else {
        f.write_all(&0u32.to_be_bytes())?;
    }
    let bpc = (depth / 8) as usize;
    let sample = |v: f32, out: &mut Vec<u8>| match bpc {
        1 => out.push((v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8),
        2 => out.extend_from_slice(&((v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16).to_be_bytes()),
        _ => out.extend_from_slice(&v.to_be_bytes()),
    };
    if bpc == 4 {
        f.write_all(&0u16.to_be_bytes())?; // raw
        for c in 0..channels {
            let mut buf = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    sample(interleaved[(y * w + x) * channels + c], &mut buf);
                }
            }
            f.write_all(&buf)?;
        }
    } else {
        f.write_all(&1u16.to_be_bytes())?; // RLE
        let rows: Vec<Vec<u8>> = (0..channels * h)
            .into_par_iter()
            .map(|k| {
                let (c, y) = (k / h, k % h);
                let mut raw = Vec::with_capacity(w * bpc);
                for x in 0..w {
                    sample(interleaved[(y * w + x) * channels + c], &mut raw);
                }
                pack_bits(&raw)
            })
            .collect();
        for row in &rows {
            if psb {
                f.write_all(&(row.len() as u32).to_be_bytes())?;
            } else {
                f.write_all(&(row.len() as u16).to_be_bytes())?;
            }
        }
        for row in &rows {
            f.write_all(row)?;
        }
    }
    f.flush()?;
    Ok(())
}

fn pack_bits(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() + src.len() / 128 + 1);
    let mut i = 0;
    while i < src.len() {
        // Run of identical bytes?
        let mut run = 1;
        while i + run < src.len() && run < 128 && src[i + run] == src[i] {
            run += 1;
        }
        if run >= 3 {
            out.push((1i16 - run as i16) as i8 as u8);
            out.push(src[i]);
            i += run;
            continue;
        }
        // Literal run until the next repeat of three.
        let start = i;
        let mut len = 0;
        while i < src.len() && len < 128 {
            if i + 2 < src.len() && src[i] == src[i + 1] && src[i] == src[i + 2] {
                break;
            }
            i += 1;
            len += 1;
        }
        out.push((len - 1) as u8);
        out.extend_from_slice(&src[start..start + len]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packbits_round_trip() {
        let data: Vec<u8> = (0..1000).map(|i| if i % 50 < 20 { 7 } else { (i * 31 % 251) as u8 }).collect();
        let packed = pack_bits(&data);
        let mut out = vec![0u8; data.len()];
        unpack_bits(&packed, &mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn write_then_read_8_16_32_bit() {
        let dir = tempfile::tempdir().unwrap();
        let (w, h) = (53, 31);
        let src: Vec<f32> = (0..w * h * 3).map(|i| ((i * 7919) % 1000) as f32 / 999.0).collect();
        for depth in [8u16, 16, 32] {
            let p = dir.path().join(format!("t{depth}.psd"));
            write(&p, w, h, 3, depth, &src, Some(b"fake icc")).unwrap();
            let img = read(&p).unwrap();
            assert_eq!((img.width, img.height, img.channels, img.depth), (w, h, 3, depth));
            assert_eq!(img.icc.as_deref(), Some(&b"fake icc"[..]));
            let tol = match depth {
                8 => 0.5 / 255.0 + 1e-6,
                16 => 0.5 / 65535.0 + 1e-6,
                _ => 0.0,
            };
            for (a, b) in img.data.iter().zip(&src) {
                assert!((a - b).abs() <= tol, "{depth}: {a} vs {b}");
            }
        }
    }
}
