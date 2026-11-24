//! Pictures of the guest's screen as PNG files: 8-bit RGB, compressed with zlib.

/// Each row is filtered the way that leaves the smallest sum of differences, as PNG encoders
/// commonly guess. A desktop is mostly flat colour, which this makes runs of zeros.
fn filtered(width: usize, height: usize, rgb: &[u8]) -> Vec<u8> {
    let stride = width * 3;
    let mut out = Vec::with_capacity((stride + 1) * height);
    let mut candidates = [vec![0u8; stride], vec![0u8; stride], vec![0u8; stride]];
    for y in 0..height {
        let row = &rgb[y * stride..(y + 1) * stride];
        let above = (y > 0).then(|| &rgb[(y - 1) * stride..y * stride]);
        // 0: none, 1: sub (the pixel to the left), 2: up (the pixel above).
        candidates[0].copy_from_slice(row);
        for x in 0..stride {
            let left = if x >= 3 { row[x - 3] } else { 0 };
            candidates[1][x] = row[x].wrapping_sub(left);
            candidates[2][x] = row[x].wrapping_sub(above.map_or(0, |above| above[x]));
        }
        let cost = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|&byte| (byte as i8).unsigned_abs() as u64)
                .sum::<u64>()
        };
        let best = (0..3).min_by_key(|&kind| cost(&candidates[kind])).unwrap_or(0);
        out.push(best as u8);
        out.extend_from_slice(&candidates[best]);
    }
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(kind);
    hasher.update(data);
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&hasher.finalize().to_be_bytes());
}

/// A PNG of `width` by `height` pixels, given as red, green and blue bytes from the top left.
pub(crate) fn encode(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    assert_eq!(rgb.len(), width as usize * height as usize * 3);
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    // 8 bits a channel, colour without alpha, the only compression, filtering and order there are.
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    let data = miniz_oxide::deflate::compress_to_vec_zlib(&filtered(width as usize, height as usize, rgb), 6);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &data);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Undoes the filters, as a reader does.
    fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let (mut at, mut width, mut height, mut data) = (8, 0, 0, Vec::new());
        while at < png.len() {
            let length = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            let kind = &png[at + 4..at + 8];
            let body = &png[at + 8..at + 8 + length];
            let crc = u32::from_be_bytes(png[at + 8 + length..at + 12 + length].try_into().unwrap());
            assert_eq!(crc, crc32fast::hash(&png[at + 4..at + 8 + length]));
            match kind {
                b"IHDR" => {
                    width = u32::from_be_bytes(body[..4].try_into().unwrap());
                    height = u32::from_be_bytes(body[4..8].try_into().unwrap());
                    assert_eq!(&body[8..], &[8, 2, 0, 0, 0]);
                }
                b"IDAT" => data.extend_from_slice(body),
                _ => {}
            }
            at += 12 + length;
        }
        let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&data).unwrap();
        let stride = width as usize * 3;
        let mut rgb: Vec<u8> = Vec::new();
        for y in 0..height as usize {
            let line = &raw[y * (stride + 1)..(y + 1) * (stride + 1)];
            for x in 0..stride {
                let left = if x >= 3 { rgb[y * stride + x - 3] } else { 0 };
                let above = if y > 0 { rgb[(y - 1) * stride + x] } else { 0 };
                rgb.push(match line[0] {
                    0 => line[1 + x],
                    1 => line[1 + x].wrapping_add(left),
                    2 => line[1 + x].wrapping_add(above),
                    other => panic!("filter {other}"),
                });
            }
        }
        (width, height, rgb)
    }

    #[test]
    fn a_picture_reads_back_as_it_was() {
        let (width, height) = (37, 11);
        let rgb: Vec<u8> = (0..width * height * 3)
            .map(|index| (index * 7 % 251) as u8)
            .collect();
        assert_eq!(decode(&encode(width, height, &rgb)), (width, height, rgb));
        let flat = vec![200; (width * height * 3) as usize];
        assert_eq!(decode(&encode(width, height, &flat)), (width, height, flat));
    }
}
