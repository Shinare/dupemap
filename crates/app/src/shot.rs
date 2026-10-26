//! `--screenshot <file.png>`: once everything is loaded, save the window as a PNG and exit.
//! Used by CI to check that the app starts and renders; not needed for normal use.

use std::io::Write;
use std::path::Path;

/// Write RGBA pixels as a PNG (stored deflate blocks: no compression library needed).
pub fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(&encode_png(width, height, rgba))?;
    f.flush()
}

pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    assert_eq!(rgba.len(), (width * height * 4) as usize);
    let stride = (width * 4) as usize;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgba.chunks(stride) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65_535).peekable();
    if chunks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(c) = chunks.next() {
        z.push(u8::from(chunks.peek().is_none()));
        let len = c.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (n, t) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c = table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffff_ffff
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65_521;
        b %= 65_521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn png_structure() {
        let px: Vec<u8> = (0..3 * 2 * 4).map(|i| i as u8).collect();
        let png = encode_png(3, 2, &px);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 3);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        // a large image spans several stored blocks
        let big = vec![7u8; 200 * 120 * 4];
        let png = encode_png(200, 120, &big);
        assert!(png.len() > big.len());
    }
}
