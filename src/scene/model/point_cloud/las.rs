// ASPRS LAS (.las, versions 1.0–1.4) point reader.
//
// Layout (little-endian):
//   0x00  "LASF"
//   0x18  u8 version major, u8 minor
//   0x5E  u16 header size
//   0x60  u32 offset to the point records
//   0x68  u8  point format (bit 7 set: LASzip compressed, see `laz`)
//   0x69  u16 point record length
//   0x6B  u32 legacy point count (1.4: u64 at 0xF7 when the header is long enough)
//   0x83  3 × f64 scale, then 3 × f64 offset (X, Y, Z)
// Every record starts with X, Y, Z as i32 (value * scale + offset) and a u16
// intensity at +12. Formats 2, 3, 5 carry RGB after the GPS time (+20 or +28),
// formats 7, 8 and 10 at +30; the colours are 16-bit.
//
// A LAS file is one scan in the drawing's own coordinates: no transform, and
// the bounds are taken from the points, as headers are often stale.

use super::rcs::Scan;

const SIGNATURE: &[u8; 4] = b"LASF";
/// Smallest record length each point format (0–10) can have.
const MIN_RECORD: [usize; 11] = [20, 28, 26, 34, 57, 63, 30, 36, 38, 59, 67];

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at.checked_add(8)?)?.try_into().ok()?))
}

fn f64_at(b: &[u8], at: usize) -> Option<f64> {
    u64_at(b, at).map(f64::from_bits).filter(|v| v.is_finite())
}

fn vec3_at(b: &[u8], at: usize) -> Option<[f64; 3]> {
    Some([f64_at(b, at)?, f64_at(b, at + 8)?, f64_at(b, at + 16)?])
}

/// What a file's header says about its point records.
pub struct Header {
    /// Point data format, 0–10 (the compression flag removed).
    pub format: u8,
    pub compressed: bool,
    pub record_len: usize,
    pub count: usize,
    pub offset: usize,
    scale: [f64; 3],
    origin: [f64; 3],
}

impl Header {
    pub fn read(b: &[u8]) -> Option<Header> {
        if b.get(0..4)? != SIGNATURE {
            return None;
        }
        let minor = *b.get(25)?;
        let header_size = usize::from(u16_at(b, 94)?);
        let offset = usize::try_from(u32_at(b, 96)?).ok()?;
        let raw_format = *b.get(104)?;
        let format = raw_format & 0x3F;
        let record_len = usize::from(u16_at(b, 105)?);
        if usize::from(format) >= MIN_RECORD.len() || record_len < MIN_RECORD[usize::from(format)] {
            return None;
        }
        let mut count = u64::from(u32_at(b, 107)?);
        if minor >= 4 && header_size >= 375 {
            if let Some(wide) = u64_at(b, 247).filter(|&n| n != 0) {
                count = wide;
            }
        }
        let scale = vec3_at(b, 131)?;
        if scale.iter().any(|&s| s == 0.0) {
            return None;
        }
        Some(Header {
            format,
            compressed: raw_format & 0x80 != 0,
            record_len,
            count: usize::try_from(count).ok()?,
            offset,
            scale,
            origin: vec3_at(b, 155)?,
        })
    }
}

/// Byte offset of the 16-bit RGB triple in a record of `format`, when it has one.
fn rgb_offset(format: u8) -> Option<usize> {
    match format {
        2 => Some(20),
        3 | 5 => Some(28),
        7 | 8 | 10 => Some(30),
        _ => None,
    }
}

/// The points of an uncompressed file (`b` is the whole file).
pub fn decode(b: &[u8]) -> Option<Scan> {
    let header = Header::read(b)?;
    if header.compressed {
        return None;
    }
    decode_records(&header, b.get(header.offset..)?)
}

/// The points in `records`, a run of `header.record_len`-byte point records.
/// A file cut short yields the records it holds.
pub fn decode_records(header: &Header, records: &[u8]) -> Option<Scan> {
    let count = header.count.min(records.len() / header.record_len);
    if count == 0 {
        return None;
    }
    let rgb = rgb_offset(header.format);
    let mut scan = Scan {
        local: Vec::with_capacity(count),
        colors: Vec::with_capacity(count),
        intensity: Vec::with_capacity(count),
        normals: vec![[0.0, 0.0, 1.0]; count],
        has_rgb: false,
        has_normals: false,
        has_intensity: false,
        translation: [0.0; 3],
        rotation: [0.0; 3],
        scale: [1.0; 3],
        bounds: [[f64::MAX; 3], [f64::MIN; 3]],
        id: String::new(),
    };
    let mut raw_intensity = Vec::with_capacity(count);
    let mut raw_colors: Vec<[u16; 3]> = Vec::with_capacity(if rgb.is_some() { count } else { 0 });
    for rec in records.chunks_exact(header.record_len).take(count) {
        let coordinate = |axis: usize| {
            let at = axis * 4;
            let v = i32::from_le_bytes(rec[at..at + 4].try_into().ok()?);
            Some(f64::from(v) * header.scale[axis] + header.origin[axis])
        };
        let p = [coordinate(0)?, coordinate(1)?, coordinate(2)?];
        for k in 0..3 {
            scan.bounds[0][k] = scan.bounds[0][k].min(p[k]);
            scan.bounds[1][k] = scan.bounds[1][k].max(p[k]);
        }
        scan.local.push(p);
        raw_intensity.push(u16::from_le_bytes([rec[12], rec[13]]));
        if let Some(at) = rgb {
            raw_colors.push([
                u16::from_le_bytes([rec[at], rec[at + 1]]),
                u16::from_le_bytes([rec[at + 2], rec[at + 3]]),
                u16::from_le_bytes([rec[at + 4], rec[at + 5]]),
            ]);
        }
    }

    // 16-bit values scale to a byte; files that keep 8-bit data in the
    // 16-bit fields (maximum below 256) are taken as they are.
    let to_byte = |value: u16, max: u16| -> u8 {
        if max <= 255 {
            value as u8
        } else {
            (u32::from(value) * 255 / u32::from(max)) as u8
        }
    };
    let max_intensity = raw_intensity.iter().copied().max().unwrap_or(0);
    scan.has_intensity = max_intensity > 0;
    scan.intensity = raw_intensity.iter().map(|&i| to_byte(i, max_intensity)).collect();

    let max_color = raw_colors.iter().flatten().copied().max().unwrap_or(0);
    scan.has_rgb = max_color > 0;
    scan.colors = if scan.has_rgb {
        // Colour channels share one scale, so a hue is not skewed.
        let channel = |v: u16| if max_color <= 255 { v as u8 } else { (v >> 8) as u8 };
        raw_colors.iter().map(|c| [channel(c[0]), channel(c[1]), channel(c[2])]).collect()
    } else {
        // No colours: the intensity stands in, as a scan without them does.
        scan.intensity.iter().map(|&i| [i; 3]).collect()
    };
    Some(scan)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A LAS 1.2 file of `points` ((x, y, z) in metres, intensity, rgb) in `format`.
    fn file(format: u8, minor: u8, points: &[([f64; 3], u16, [u16; 3])]) -> Vec<u8> {
        let record_len = MIN_RECORD[usize::from(format)];
        let mut b = vec![0u8; 227];
        b[0..4].copy_from_slice(SIGNATURE);
        b[24] = 1;
        b[25] = minor;
        b[94..96].copy_from_slice(&227u16.to_le_bytes());
        b[96..100].copy_from_slice(&227u32.to_le_bytes());
        b[104] = format;
        b[105..107].copy_from_slice(&(record_len as u16).to_le_bytes());
        b[107..111].copy_from_slice(&(points.len() as u32).to_le_bytes());
        for (k, scale) in [0.01f64, 0.01, 0.001].iter().enumerate() {
            b[131 + k * 8..139 + k * 8].copy_from_slice(&scale.to_le_bytes());
        }
        for (k, offset) in [500000.0f64, 4500000.0, 100.0].iter().enumerate() {
            b[155 + k * 8..163 + k * 8].copy_from_slice(&offset.to_le_bytes());
        }
        for (p, intensity, rgb) in points {
            let mut rec = vec![0u8; record_len];
            for (axis, scale) in [0.01f64, 0.01, 0.001].iter().enumerate() {
                let origin = [500000.0f64, 4500000.0, 100.0][axis];
                let v = ((p[axis] - origin) / scale).round() as i32;
                rec[axis * 4..axis * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
            rec[12..14].copy_from_slice(&intensity.to_le_bytes());
            if let Some(at) = rgb_offset(format) {
                for (k, c) in rgb.iter().enumerate() {
                    rec[at + k * 2..at + k * 2 + 2].copy_from_slice(&c.to_le_bytes());
                }
            }
            b.extend_from_slice(&rec);
        }
        b
    }

    fn close(a: [f64; 3], b: [f64; 3]) -> bool {
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-6)
    }

    #[test]
    fn reads_scaled_coordinates_and_rgb() {
        let points = [
            ([500010.25, 4500020.5, 101.125], 1000, [65535, 0, 32768]),
            ([500011.0, 4500021.0, 102.0], 500, [0, 65535, 0]),
        ];
        let scan = decode(&file(3, 2, &points)).expect("a valid LAS");
        assert_eq!(scan.local.len(), 2);
        assert!(close(scan.local[0], [500010.25, 4500020.5, 101.125]));
        assert!(close(scan.local[1], [500011.0, 4500021.0, 102.0]));
        assert!(scan.has_rgb && scan.has_intensity && !scan.has_normals);
        assert_eq!(scan.colors[0], [255, 0, 128]);
        assert_eq!(scan.colors[1], [0, 255, 0]);
        // Intensity scales against the file's maximum.
        assert_eq!(scan.intensity, vec![255, 127]);
        assert!(close(scan.bounds[0], [500010.25, 4500020.5, 101.125]));
        assert!(close(scan.bounds[1], [500011.0, 4500021.0, 102.0]));
        assert_eq!(scan.normals.len(), 2);
    }

    #[test]
    fn eight_bit_colours_in_sixteen_bit_fields_are_kept() {
        let points = [([500000.0, 4500000.0, 100.0], 7, [200, 100, 50])];
        let scan = decode(&file(2, 2, &points)).unwrap();
        assert_eq!(scan.colors[0], [200, 100, 50]);
        assert_eq!(scan.intensity[0], 7);
    }

    #[test]
    fn a_format_without_colour_shows_intensity() {
        let points = [([500000.0, 4500000.0, 100.0], 40, [0; 3]), ([500001.0, 4500000.0, 100.0], 80, [0; 3])];
        let scan = decode(&file(1, 2, &points)).unwrap();
        assert!(!scan.has_rgb);
        assert_eq!(scan.colors, vec![[40; 3], [80; 3]]);
    }

    #[test]
    fn a_truncated_file_yields_the_records_it_holds() {
        let points = [
            ([500000.0, 4500000.0, 100.0], 1, [0; 3]),
            ([500001.0, 4500000.0, 100.0], 1, [0; 3]),
            ([500002.0, 4500000.0, 100.0], 1, [0; 3]),
        ];
        let mut bytes = file(0, 2, &points);
        bytes.truncate(bytes.len() - 15);
        assert_eq!(decode(&bytes).unwrap().local.len(), 2);
    }

    #[test]
    fn rejects_other_files_and_compressed_data() {
        assert!(decode(b"").is_none());
        assert!(decode(b"LASF").is_none());
        assert!(decode(&[0u8; 400]).is_none());
        let mut compressed = file(0, 2, &[([500000.0, 4500000.0, 100.0], 1, [0; 3])]);
        compressed[104] |= 0x80;
        assert!(decode(&compressed).is_none());
        // A zero scale or an impossible record length is not a point file.
        let mut flat = file(0, 2, &[([500000.0, 4500000.0, 100.0], 1, [0; 3])]);
        flat[131..139].copy_from_slice(&0f64.to_le_bytes());
        assert!(decode(&flat).is_none());
        let mut short = file(0, 2, &[([500000.0, 4500000.0, 100.0], 1, [0; 3])]);
        short[105..107].copy_from_slice(&10u16.to_le_bytes());
        assert!(decode(&short).is_none());
    }

    #[test]
    fn las_14_uses_the_wide_point_count() {
        let points = [([500000.0, 4500000.0, 100.0], 1, [0; 3]), ([500001.0, 4500000.0, 100.0], 1, [0; 3])];
        let mut bytes = file(0, 4, &points);
        // Grow the header to its 1.4 size, with the legacy count zeroed.
        let tail = bytes.split_off(227);
        bytes.resize(375, 0);
        bytes[94..96].copy_from_slice(&375u16.to_le_bytes());
        bytes[96..100].copy_from_slice(&375u32.to_le_bytes());
        bytes[107..111].copy_from_slice(&0u32.to_le_bytes());
        bytes[247..255].copy_from_slice(&2u64.to_le_bytes());
        bytes.extend_from_slice(&tail);
        assert_eq!(decode(&bytes).unwrap().local.len(), 2);
    }
}
