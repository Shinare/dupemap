//! On Windows, DupeMap.exe gets its icon and its version information (Explorer shows both, and
//! Task Manager names the app by its description). The resources are written here as a .res
//! file and handed to the linker, so no resource compiler is needed.

use std::path::PathBuf;

#[path = "src/icon_art.rs"]
mod icon_art;

/// Icon sizes in the .exe: Windows picks the nearest for each place it shows the icon.
const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
/// English (United States).
const LANG: u16 = 0x0409;
/// Unicode (UTF-16).
const CODE_PAGE: u16 = 1200;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon_art.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let images: Vec<(u32, Vec<u8>)> = SIZES.iter().map(|&s| (s, icon_image(s))).collect();
    std::fs::write(out.join("DupeMap.ico"), ico_file(&images)).expect("write DupeMap.ico");
    let res = out.join("DupeMap.res");
    std::fs::write(&res, res_file(&images, &version)).expect("write DupeMap.res");
    // the Microsoft linker takes a .res file as it is (GNU ld would need windres: no icon there)
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins={}", res.display());
    }
}

// ------------------------------------------------------------------------------------ the icon

/// One image of the icon: a PNG for 256 px (small, and how Windows stores that size), a
/// 32-bit bitmap with alpha for the small sizes.
fn icon_image(size: u32) -> Vec<u8> {
    let rgba = icon_art::render(size);
    if size >= 256 {
        png(size, &rgba)
    } else {
        dib(size, &rgba)
    }
}

/// A bitmap as icons hold it: the header says twice the height (colour, then the mask), the rows
/// run bottom-up, then a 1-bit mask (set = transparent).
fn dib(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mask_stride = size.div_ceil(32) * 4;
    let mut b = Vec::new();
    put32(&mut b, 40);
    put32(&mut b, size);
    put32(&mut b, size * 2);
    put16(&mut b, 1);
    put16(&mut b, 32);
    put32(&mut b, 0); // BI_RGB
    put32(&mut b, size * size * 4 + mask_stride * size);
    put32(&mut b, 0);
    put32(&mut b, 0);
    put32(&mut b, 0);
    put32(&mut b, 0);
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            b.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    for y in (0..size).rev() {
        let mut row = vec![0u8; mask_stride as usize];
        for x in 0..size {
            if rgba[((y * size + x) * 4 + 3) as usize] == 0 {
                row[(x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
        b.extend_from_slice(&row);
    }
    b
}

fn png(size: u32, rgba: &[u8]) -> Vec<u8> {
    let stride = (size * 4) as usize;
    let mut raw = Vec::with_capacity((stride + 1) * size as usize);
    for row in rgba.chunks(stride) {
        raw.push(0); // no filter
        raw.extend_from_slice(row);
    }
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 9));
    png_chunk(&mut out, b"IEND", &[]);
    out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// The directory entry fields both an .ico file and the icon resource start with.
fn icon_entry(b: &mut Vec<u8>, size: u32, bytes: usize) {
    let side = if size >= 256 { 0 } else { size as u8 };
    b.extend_from_slice(&[side, side, 0, 0]);
    put16(b, 1); // planes
    put16(b, 32); // bits per pixel
    put32(b, bytes as u32);
}

/// A standalone .ico (written next to the .res, for reference).
fn ico_file(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut b = Vec::new();
    put16(&mut b, 0);
    put16(&mut b, 1);
    put16(&mut b, images.len() as u16);
    let mut offset = 6 + 16 * images.len();
    for (size, data) in images {
        icon_entry(&mut b, *size, data.len());
        put32(&mut b, offset as u32);
        offset += data.len();
    }
    for (_, data) in images {
        b.extend_from_slice(data);
    }
    b
}

// ------------------------------------------------------------------------------ the .res file

fn res_file(images: &[(u32, Vec<u8>)], version: &str) -> Vec<u8> {
    let mut b = Vec::new();
    // a 32-bit .res file starts with an empty entry
    res_entry(&mut b, 0, 0, 0, 0, &[]);
    for (i, (_, data)) in images.iter().enumerate() {
        res_entry(&mut b, RT_ICON, i as u16 + 1, 0x1010, LANG, data);
    }
    let mut group = Vec::new();
    put16(&mut group, 0);
    put16(&mut group, 1);
    put16(&mut group, images.len() as u16);
    for (i, (size, data)) in images.iter().enumerate() {
        icon_entry(&mut group, *size, data.len());
        put16(&mut group, i as u16 + 1);
    }
    res_entry(&mut b, RT_GROUP_ICON, 1, 0x1030, LANG, &group);
    res_entry(&mut b, RT_VERSION, 1, 0x0030, LANG, &version_info(version));
    b
}

/// One resource: a header naming its type and id (both numbers), then the data.
fn res_entry(b: &mut Vec<u8>, kind: u16, id: u16, flags: u16, lang: u16, data: &[u8]) {
    put32(b, data.len() as u32);
    put32(b, 32); // header size
    put16(b, 0xffff);
    put16(b, kind);
    put16(b, 0xffff);
    put16(b, id);
    put32(b, 0); // data version
    put16(b, flags);
    put16(b, lang);
    put32(b, 0); // version
    put32(b, 0); // characteristics
    b.extend_from_slice(data);
    pad4(b);
}

// ------------------------------------------------------------------- the version information

enum Value<'a> {
    None,
    Binary(&'a [u8]),
    Text(&'a str),
}

/// A block of a version resource: its length, the length of its value, whether that is text,
/// its key, the value, then the blocks inside it, each starting on a 4-byte boundary. The
/// length leaves out padding after the end.
fn block(key: &str, value: Value, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0u8; 6];
    put_utf16(&mut b, key);
    pad4(&mut b);
    let (value_len, text) = match value {
        Value::None => (0, true),
        Value::Binary(v) => {
            b.extend_from_slice(v);
            (v.len(), false)
        }
        // counted in UTF-16 units, with the closing zero
        Value::Text(t) => {
            put_utf16(&mut b, t);
            (t.encode_utf16().count() + 1, true)
        }
    };
    for c in children {
        pad4(&mut b);
        b.extend_from_slice(c);
    }
    let len = b.len() as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    b[2..4].copy_from_slice(&(value_len as u16).to_le_bytes());
    b[4..6].copy_from_slice(&u16::from(text).to_le_bytes());
    b
}

fn version_info(version: &str) -> Vec<u8> {
    // "1.2.3" or "1.2.3-beta": numbers for the fixed part, the text as it is for the strings
    let mut n = version.split(['.', '-', '+']).map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor, patch) = (n.next().unwrap_or(0), n.next().unwrap_or(0), n.next().unwrap_or(0));
    let ms = (major << 16) | (minor & 0xffff);
    let ls = (patch & 0xffff) << 16;
    let mut fixed = Vec::new();
    for v in [
        0xfeef_04bd, // signature
        0x0001_0000, // structure version
        ms,
        ls, // file version
        ms,
        ls,          // product version
        0x3f,        // flags mask
        0,           // flags
        0x0004_0004, // Windows NT, 32-bit Windows API
        1,           // an application
        0,
        0,
        0, // no date
    ] {
        put32(&mut fixed, v);
    }
    let strings: Vec<Vec<u8>> = [
        ("FileDescription", "DupeMap"),
        ("FileVersion", version),
        ("InternalName", "DupeMap"),
        ("OriginalFilename", "DupeMap.exe"),
        ("ProductName", "DupeMap"),
        ("ProductVersion", version),
        ("Comments", "Finds duplicate files across computers, drives and cloud storage from hash manifests"),
    ]
    .iter()
    .map(|(k, v)| block(k, Value::Text(v), &[]))
    .collect();
    let table = block(&format!("{LANG:04x}{CODE_PAGE:04x}"), Value::None, &strings);
    let string_info = block("StringFileInfo", Value::None, &[table]);
    let mut translation = Vec::new();
    put16(&mut translation, LANG);
    put16(&mut translation, CODE_PAGE);
    let var = block("Translation", Value::Binary(&translation), &[]);
    let var_info = block("VarFileInfo", Value::None, &[var]);
    block("VS_VERSION_INFO", Value::Binary(&fixed), &[string_info, var_info])
}

// ------------------------------------------------------------------------------------ helpers

fn put16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put_utf16(b: &mut Vec<u8>, s: &str) {
    for u in s.encode_utf16().chain([0]) {
        put16(b, u);
    }
}

fn pad4(b: &mut Vec<u8>) {
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
}
