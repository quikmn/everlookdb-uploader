// BLP, Blizzard's texture format: BLP2 (palette, DXT1/3/5, uncompressed BGRA) and BLP1 with a palette. Only what an
// addon icon needs: one mipmap, decoded to RGBA. BLP1 with JPEG inside isn't read (no addon icon we've seen uses it).

/// An RGBA image: width, height, 4 bytes per pixel.
#[derive(Debug)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| "BLP is cut short".to_string())
}

/// Decode the smallest mipmap that is still at least `want` pixels on its longer side (or the largest there is).
pub fn decode(b: &[u8], want: u32) -> Result<Rgba, String> {
    let magic = b.get(0..4).ok_or("not a BLP")?;
    let (blp2, compression, alpha_depth, alpha_type, width, height, mips, palette_at) = match magic {
        b"BLP2" => {
            let h = b.get(8..12).ok_or("BLP is cut short")?;
            (true, h[0] as u32, h[1] as u32, h[2], u32_at(b, 12)?, u32_at(b, 16)?, 20, 148)
        }
        b"BLP1" => {
            let compression = match u32_at(b, 4)? {
                0 => return Err("BLP1 with JPEG isn't supported".into()),
                1 => 1,
                c => return Err(format!("unknown BLP1 compression {c}")),
            };
            (false, compression, u32_at(b, 8)?, 0, u32_at(b, 12)?, u32_at(b, 16)?, 28, 156)
        }
        _ => return Err("not a BLP".into()),
    };
    if width == 0 || height == 0 || width > 8192 || height > 8192 {
        return Err(format!("BLP size {width}x{height} isn't plausible"));
    }
    // mipmap i is (w >> i) x (h >> i); take the smallest one that is big enough and present
    let mut pick = None;
    for i in 0..16u32 {
        let (w, h) = ((width >> i).max(1), (height >> i).max(1));
        let off = u32_at(b, mips + 4 * i as usize)? as usize;
        let len = u32_at(b, mips + 64 + 4 * i as usize)? as usize;
        if len == 0 || off == 0 || off.checked_add(len).is_none_or(|end| end > b.len()) {
            break;
        }
        if pick.is_none() || w.max(h) >= want {
            pick = Some((w, h, &b[off..off + len]));
        }
        if w.max(h) <= want || (w == 1 && h == 1) {
            break;
        }
    }
    let (w, h, data) = pick.ok_or("BLP has no image data")?;
    let n = (w * h) as usize;
    let pixels = match compression {
        1 => {
            let pal = b.get(palette_at..palette_at + 1024).ok_or("BLP palette is cut short")?;
            palette(data, pal, n, alpha_depth)?
        }
        2 if blp2 => {
            let kind = if alpha_depth <= 1 { Dxt::One } else if alpha_type == 7 { Dxt::Five } else { Dxt::Three };
            let mut px = dxt(data, w, h, kind)?;
            if alpha_depth == 0 {
                px.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p[3] = 255);
            }
            px
        }
        3 if blp2 => {
            let src = data.get(..n * 4).ok_or("BLP image is cut short")?;
            let mut px = Vec::with_capacity(n * 4);
            for p in src.as_chunks::<4>().0 {
                px.extend_from_slice(&[p[2], p[1], p[0], if alpha_depth == 0 { 255 } else { p[3] }]);
            }
            px
        }
        c => return Err(format!("unknown BLP compression {c}")),
    };
    Ok(Rgba { width: w, height: h, pixels })
}

/// One byte per pixel into a BGRA palette, then the alpha: 1, 4 or 8 bits per pixel (or none: opaque).
fn palette(data: &[u8], pal: &[u8], n: usize, depth: u32) -> Result<Vec<u8>, String> {
    let idx = data.get(..n).ok_or("BLP image is cut short")?;
    let alpha = &data[n..];
    let need = match depth {
        0 => 0,
        1 => n.div_ceil(8),
        4 => n.div_ceil(2),
        8 => n,
        d => return Err(format!("unknown BLP alpha depth {d}")),
    };
    if alpha.len() < need {
        return Err("BLP alpha is cut short".into());
    }
    let mut px = Vec::with_capacity(n * 4);
    for (i, &c) in idx.iter().enumerate() {
        let p = &pal[c as usize * 4..c as usize * 4 + 4];
        let a = match depth {
            0 => 255,
            1 => {
                if alpha[i / 8] >> (i % 8) & 1 == 1 {
                    255
                } else {
                    0
                }
            }
            4 => (alpha[i / 2] >> ((i % 2) * 4) & 0x0f) * 17,
            _ => alpha[i],
        };
        px.extend_from_slice(&[p[2], p[1], p[0], a]);
    }
    Ok(px)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Dxt {
    One,
    Three,
    Five,
}

fn rgb565(c: u16) -> [u8; 3] {
    let (r, g, b) = ((c >> 11) & 31, (c >> 5) & 63, c & 31);
    [(r << 3 | r >> 2) as u8, (g << 2 | g >> 4) as u8, (b << 3 | b >> 2) as u8]
}

/// The 16 colours of a DXT colour block (8 bytes). DXT1 may use 3 colours plus transparent black.
fn colour_block(b: &[u8], dxt1: bool) -> [[u8; 4]; 16] {
    let (c0, c1) = (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]]));
    let (a, z) = (rgb565(c0), rgb565(c1));
    let mix = |x: u8, y: u8, wx: u16, wy: u16| ((x as u16 * wx + y as u16 * wy) / (wx + wy)) as u8;
    let mut pal = [[a[0], a[1], a[2], 255], [z[0], z[1], z[2], 255], [0; 4], [0; 4]];
    if c0 > c1 || !dxt1 {
        pal[2] = [mix(a[0], z[0], 2, 1), mix(a[1], z[1], 2, 1), mix(a[2], z[2], 2, 1), 255];
        pal[3] = [mix(a[0], z[0], 1, 2), mix(a[1], z[1], 1, 2), mix(a[2], z[2], 1, 2), 255];
    } else {
        pal[2] = [mix(a[0], z[0], 1, 1), mix(a[1], z[1], 1, 1), mix(a[2], z[2], 1, 1), 255];
    }
    let bits = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    std::array::from_fn(|i| pal[(bits >> (2 * i) & 3) as usize])
}

/// The 16 alphas of a DXT5 alpha block (8 bytes).
fn dxt5_alpha(b: &[u8]) -> [u8; 16] {
    let (a0, a1) = (b[0] as u16, b[1] as u16);
    let mut pal = [0u8; 8];
    pal[0] = a0 as u8;
    pal[1] = a1 as u8;
    if a0 > a1 {
        for i in 1..7u16 {
            pal[i as usize + 1] = (((7 - i) * a0 + i * a1) / 7) as u8;
        }
    } else {
        for i in 1..5u16 {
            pal[i as usize + 1] = (((5 - i) * a0 + i * a1) / 5) as u8;
        }
        pal[6] = 0;
        pal[7] = 255;
    }
    let bits = b[2..8].iter().rev().fold(0u64, |acc, &x| acc << 8 | x as u64);
    std::array::from_fn(|i| pal[(bits >> (3 * i) & 7) as usize])
}

/// Decode DXT1/3/5 blocks into RGBA.
pub fn dxt(data: &[u8], w: u32, h: u32, kind: Dxt) -> Result<Vec<u8>, String> {
    let (bw, bh) = (w.div_ceil(4) as usize, h.div_ceil(4) as usize);
    let size = if kind == Dxt::One { 8 } else { 16 };
    if data.len() < bw * bh * size {
        return Err("BLP image is cut short".into());
    }
    let (w, h) = (w as usize, h as usize);
    let mut px = vec![0u8; w * h * 4];
    for (n, block) in data.chunks_exact(size).take(bw * bh).enumerate() {
        let (bx, by) = (n % bw * 4, n / bw * 4);
        let (alpha, colours): (Option<[u8; 16]>, _) = match kind {
            Dxt::One => (None, colour_block(block, true)),
            Dxt::Three => {
                let bits = u64::from_le_bytes(block[..8].try_into().unwrap());
                (Some(std::array::from_fn(|i| (bits >> (4 * i) & 0x0f) as u8 * 17)), colour_block(&block[8..], false))
            }
            Dxt::Five => (Some(dxt5_alpha(block)), colour_block(&block[8..], false)),
        };
        for (i, c) in colours.iter().enumerate() {
            let (x, y) = (bx + i % 4, by + i / 4);
            if x < w && y < h {
                let at = (y * w + x) * 4;
                px[at..at + 4].copy_from_slice(c);
                if let Some(a) = alpha {
                    px[at + 3] = a[i];
                }
            }
        }
    }
    Ok(px)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A BLP2 file with one mipmap per entry of `mips` (the image data of each).
    pub fn blp2(compression: u8, alpha_depth: u8, alpha_type: u8, w: u32, h: u32, palette: &[[u8; 4]], mips: &[Vec<u8>]) -> Vec<u8> {
        let mut b = b"BLP2".to_vec();
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&[compression, alpha_depth, alpha_type, (mips.len() > 1) as u8]);
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
        let mut offsets = [0u32; 16];
        let mut sizes = [0u32; 16];
        let mut at = 148 + 1024;
        for (i, m) in mips.iter().enumerate() {
            offsets[i] = at as u32;
            sizes[i] = m.len() as u32;
            at += m.len();
        }
        offsets.iter().chain(sizes.iter()).for_each(|v| b.extend_from_slice(&v.to_le_bytes()));
        for i in 0..256 {
            b.extend_from_slice(palette.get(i).unwrap_or(&[0; 4]));
        }
        mips.iter().for_each(|m| b.extend_from_slice(m));
        b
    }

    fn px(img: &Rgba, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * img.width + x) * 4) as usize;
        img.pixels[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn colours_565() {
        assert_eq!(rgb565(0xffff), [255, 255, 255]);
        assert_eq!(rgb565(0xf800), [255, 0, 0]);
        assert_eq!(rgb565(0x07e0), [0, 255, 0]);
        assert_eq!(rgb565(0x001f), [0, 0, 255]);
    }

    #[test]
    fn dxt1_block() {
        // c0 red > c1 blue: four colours; pixel 0 = c0, 1 = c1, 2 = 2/3 red, 3 = 1/3 red, rest c0
        let block = [0x00, 0xf8, 0x1f, 0x00, 0b1110_0100, 0, 0, 0];
        let px = dxt(&block, 4, 4, Dxt::One).unwrap();
        assert_eq!(&px[0..4], &[255, 0, 0, 255]);
        assert_eq!(&px[4..8], &[0, 0, 255, 255]);
        assert_eq!(&px[8..12], &[170, 0, 85, 255]);
        assert_eq!(&px[12..16], &[85, 0, 170, 255]);
        // c0 <= c1: three colours and transparent black
        let block = [0x1f, 0x00, 0x00, 0xf8, 0b1110_0100, 0, 0, 0];
        let px = dxt(&block, 4, 4, Dxt::One).unwrap();
        assert_eq!(&px[8..12], &[127, 0, 127, 255]);
        assert_eq!(&px[12..16], &[0, 0, 0, 0]);
    }

    #[test]
    fn dxt3_and_dxt5_alpha() {
        let colour = [0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]; // all white
        // DXT3: pixel 0 alpha 0xf, pixel 1 alpha 0x8
        let mut b3 = vec![0x8f, 0, 0, 0, 0, 0, 0, 0];
        b3.extend_from_slice(&colour);
        let px = dxt(&b3, 4, 4, Dxt::Three).unwrap();
        assert_eq!(&px[0..4], &[255, 255, 255, 255]);
        assert_eq!(px[7], 0x88);
        assert_eq!(px[11], 0);
        // DXT5: a0 = 255, a1 = 0 (8 levels); pixel 0 index 0, pixel 1 index 1, pixel 2 index 7 (1/7 of a0)
        let bits: u64 = 1 << 3 | 7 << 6;
        let mut b5 = vec![255, 0];
        b5.extend_from_slice(&bits.to_le_bytes()[..6]);
        b5.extend_from_slice(&colour);
        let px = dxt(&b5, 4, 4, Dxt::Five).unwrap();
        assert_eq!([px[3], px[7], px[11]], [255, 0, 36]);
        // a0 <= a1: 6 levels plus 0 and 255
        assert_eq!(dxt5_alpha(&[0, 255, 0b0011_1110, 0, 0, 0, 0, 0])[..2], [0, 255]);
    }

    #[test]
    fn partial_blocks_stay_in_bounds() {
        let px = dxt(&[0xff, 0xff, 0, 0, 0, 0, 0, 0], 2, 3, Dxt::One).unwrap();
        assert_eq!(px.len(), 2 * 3 * 4);
        assert!(px.chunks(4).all(|p| p == [255, 255, 255, 255]));
    }

    #[test]
    fn blp2_palette_with_alpha() {
        let pal = [[0, 0, 255, 0], [255, 0, 0, 0]]; // BGRA: red, blue
        let mut data = vec![0, 1, 1, 0]; // 2x2 indices
        data.push(0b0000_1001); // 1-bit alpha: pixels 0 and 3 opaque
        let file = blp2(1, 1, 0, 2, 2, &pal, &[data]);
        let img = decode(&file, 64).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(px(&img, 0, 0), [255, 0, 0, 255]);
        assert_eq!(px(&img, 1, 0), [0, 0, 255, 0]);
        assert_eq!(px(&img, 1, 1), [255, 0, 0, 255]);
        // 8-bit alpha
        let file = blp2(1, 8, 8, 2, 1, &pal, &[vec![1, 0, 10, 200]]);
        let img = decode(&file, 64).unwrap();
        assert_eq!(px(&img, 0, 0), [0, 0, 255, 10]);
        assert_eq!(px(&img, 1, 0), [255, 0, 0, 200]);
        // 4-bit alpha
        let file = blp2(1, 4, 8, 2, 1, &pal, &[vec![0, 0, 0xf3]]);
        let img = decode(&file, 64).unwrap();
        assert_eq!([px(&img, 0, 0)[3], px(&img, 1, 0)[3]], [0x33, 0xff]);
    }

    #[test]
    fn blp2_uncompressed_and_dxt() {
        let file = blp2(3, 8, 8, 1, 1, &[], &[vec![10, 20, 30, 40]]);
        assert_eq!(decode(&file, 64).unwrap().pixels, vec![30, 20, 10, 40]);
        let file = blp2(3, 0, 8, 1, 1, &[], &[vec![10, 20, 30, 40]]);
        assert_eq!(decode(&file, 64).unwrap().pixels, vec![30, 20, 10, 255]);
        // DXT1, no alpha: the transparent-black entry turns opaque
        let file = blp2(2, 0, 0, 4, 4, &[], &[vec![0x1f, 0x00, 0x00, 0xf8, 0xff, 0xff, 0xff, 0xff]]);
        assert!(decode(&file, 64).unwrap().pixels.chunks(4).all(|p| p == [0, 0, 0, 255]));
        // DXT5 (alpha depth 8, type 7)
        let mut b5 = vec![128, 128, 0, 0, 0, 0, 0, 0];
        b5.extend_from_slice(&[0x00, 0xf8, 0x00, 0xf8, 0, 0, 0, 0]);
        let img = decode(&blp2(2, 8, 7, 4, 4, &[], &[b5]), 64).unwrap();
        assert_eq!(px(&img, 3, 3), [255, 0, 0, 128]);
    }

    #[test]
    fn picks_a_small_enough_mipmap() {
        // 256x256 uncompressed with mipmaps down to 1x1: asking for 64 takes the 64x64 one
        let mips: Vec<Vec<u8>> = (0..9).map(|i| vec![i as u8; ((256u32 >> i).pow(2) * 4) as usize]).collect();
        let img = decode(&blp2(3, 8, 8, 256, 256, &[], &mips), 64).unwrap();
        assert_eq!((img.width, img.height), (64, 64));
        assert_eq!(img.pixels[0], 2);
        // only the full size present: that one
        let img = decode(&blp2(3, 8, 8, 128, 128, &[], &mips[1..2]), 64).unwrap();
        assert_eq!(img.width, 128);
    }

    #[test]
    fn rejects_broken_files() {
        assert!(decode(b"PNG.", 64).is_err());
        assert!(decode(b"BLP2", 64).is_err());
        let mut file = blp2(3, 8, 8, 4, 4, &[], &[vec![0; 64]]);
        file.truncate(file.len() - 10);
        assert!(decode(&file, 64).is_err());
        let mut b1 = b"BLP1".to_vec();
        b1.extend_from_slice(&0u32.to_le_bytes());
        b1.resize(200, 0);
        assert!(decode(&b1, 64).unwrap_err().contains("JPEG"));
    }

    #[test]
    fn blp1_palette() {
        let mut b = b"BLP1".to_vec();
        for v in [1u32, 8, 2, 1, 4, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        let mut offsets = [0u32; 16];
        let mut sizes = [0u32; 16];
        offsets[0] = 156 + 1024;
        sizes[0] = 4;
        offsets.iter().chain(sizes.iter()).for_each(|v| b.extend_from_slice(&v.to_le_bytes()));
        let mut pal = vec![0u8; 1024];
        pal[4..8].copy_from_slice(&[0, 255, 0, 0]); // index 1: green
        b.extend_from_slice(&pal);
        b.extend_from_slice(&[1, 0, 77, 255]);
        let img = decode(&b, 64).unwrap();
        assert_eq!(img.pixels, vec![0, 255, 0, 77, 0, 0, 0, 255]);
    }
}
