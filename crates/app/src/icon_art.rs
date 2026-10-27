//! The DupeMap icon: two overlapping documents (blue = size, orange = copies) on a dark tile.
//! Used for the window icon and, through `build.rs`, for the icon of DupeMap.exe.

/// A rounded rectangle in a 64-unit square: x0, y0, x1, y1, corner radius, colour.
type Shape = (f32, f32, f32, f32, f32, [u8; 3]);

/// Drawn in this order, each over the ones before it.
const SHAPES: [Shape; 4] = [
    (0.0, 0.0, 64.0, 64.0, 12.0, [0x1a, 0x1a, 0x19]),
    (12.0, 10.0, 38.0, 44.0, 4.0, [0x39, 0x87, 0xe5]),
    (26.0, 20.0, 52.0, 54.0, 4.0, [0xf1, 0x62, 0x24]),
    // where the two overlap
    (26.0, 20.0, 38.0, 44.0, 0.0, [0xf0, 0xef, 0xec]),
];

fn inside(x: f32, y: f32, s: &Shape) -> bool {
    let (x0, y0, x1, y1, r, _) = *s;
    if x < x0 || x >= x1 || y < y0 || y >= y1 {
        return false;
    }
    // distance to the nearest point of the rectangle shrunk by the radius
    let (dx, dy) = (x - x.clamp(x0 + r, x1 - r), y - y.clamp(y0 + r, y1 - r));
    dx * dx + dy * dy <= r * r
}

/// The icon as `size` x `size` RGBA pixels (straight alpha), smoothed with 4 x 4 samples per
/// pixel.
pub fn render(size: u32) -> Vec<u8> {
    const S: u32 = 4;
    let unit = 64.0 / size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut sum = [0u32; 3];
            let mut covered = 0u32;
            for sy in 0..S {
                for sx in 0..S {
                    let x = (px as f32 + (sx as f32 + 0.5) / S as f32) * unit;
                    let y = (py as f32 + (sy as f32 + 0.5) / S as f32) * unit;
                    if let Some(s) = SHAPES.iter().rev().find(|s| inside(x, y, s)) {
                        covered += 1;
                        for (k, v) in sum.iter_mut().enumerate() {
                            *v += s.5[k] as u32;
                        }
                    }
                }
            }
            if covered == 0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            for v in sum {
                out.push(((v + covered / 2) / covered) as u8);
            }
            out.push(((covered * 255 + S * S / 2) / (S * S)) as u8);
        }
    }
    out
}
