//! Colours. Dark theme only.
//!
//! Two encodings, each a single-hue ramp validated against the dark surface (dataviz rules:
//! one hue per magnitude, monotone lightness, text stays in ink colours):
//! * size → blue ramp (steps 600 → 200), log scale from 1 MB to 100 GB, drawn as bar fill
//! * copies → orange ramp (steps 550, 450, 350, 250) for 2, 3, 4–5, 6+ copies
//!
//! Status colours (good / warning / serious / critical) are reserved for alerts and always
//! come with an icon and a label.

use egui::Color32;

const fn rgb(h: u32) -> Color32 {
    Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

// surfaces and ink
pub const PAGE: Color32 = rgb(0x0d0d0d);
pub const PANEL: Color32 = rgb(0x141413);
pub const SURFACE: Color32 = rgb(0x1a1a19);
pub const SURFACE_ALT: Color32 = rgb(0x1f1f1d);
pub const SURFACE_RAISED: Color32 = rgb(0x242422);
pub const INK: Color32 = rgb(0xf0efec);
pub const INK_SECONDARY: Color32 = rgb(0xc3c2b7);
pub const INK_MUTED: Color32 = rgb(0x898781);
pub const INK_ON_LIGHT: Color32 = rgb(0x0b0b0b);
pub const GRID: Color32 = rgb(0x2c2c2a);
pub const BASELINE: Color32 = rgb(0x383835);
pub const ACCENT: Color32 = rgb(0x3987e5);
pub const SELECTION: Color32 = rgb(0x184f95);
/// Link text (blue step 300, readable on every surface).
pub const LINK: Color32 = rgb(0x6da7ec);
/// Table rows. The selected row is a blue-grey (not a ramp colour) so size bars stay visible on it.
pub const ROW_HOVER: Color32 = rgb(0x1f1f1d);
pub const ROW_SELECTED: Color32 = rgb(0x1e2a3a);
pub const ROW_STRIPE: Color32 = rgb(0x171716);

// status (reserved; always with icon + label). "Serious" (#ec835a) joins when something needs it.
pub const GOOD: Color32 = rgb(0x0ca30c);
pub const WARNING: Color32 = rgb(0xfab219);
pub const CRITICAL: Color32 = rgb(0xd03b3b);

/// Blue ramp steps 600, 500, 400, 300, 200 (OKLCH L 0.43 → 0.81).
const SIZE_RAMP: [Color32; 5] = [rgb(0x184f95), rgb(0x256abf), rgb(0x3987e5), rgb(0x6da7ec), rgb(0x9ec5f4)];

/// Orange ramp steps 550, 450, 350, 250: 2, 3, 4–5, 6+ copies.
const COPIES_RAMP: [Color32; 4] = [rgb(0x9e3601), rgb(0xcb4801), rgb(0xf16224), rgb(0xfe9068)];

/// Duplication accent (bars for "% elsewhere"): orange step 450.
pub const DUP: Color32 = rgb(0xcb4801);

const SIZE_LO: f64 = 1_048_576.0; // 1 MB
const SIZE_HI: f64 = 107_374_182_400.0; // 100 GB

/// Colour for an absolute size (log scale, 1 MB → 100 GB).
pub fn size_color(bytes: u64) -> Color32 {
    let v = (bytes.max(1) as f64).log10();
    let t = (v - SIZE_LO.log10()) / (SIZE_HI.log10() - SIZE_LO.log10());
    ramp(&SIZE_RAMP, t.clamp(0.0, 1.0) as f32)
}

/// Fill for a copies pill; `None` for a unique file.
pub fn copies_fill(copies: u32) -> Option<Color32> {
    match copies {
        0 | 1 => None,
        2 => Some(COPIES_RAMP[0]),
        3 => Some(COPIES_RAMP[1]),
        4 | 5 => Some(COPIES_RAMP[2]),
        _ => Some(COPIES_RAMP[3]),
    }
}

/// Ink for text drawn on a filled background: whichever of white / near-black contrasts more.
pub fn label_on(fill: Color32) -> Color32 {
    if contrast(fill, Color32::WHITE) >= contrast(fill, INK_ON_LIGHT) {
        Color32::WHITE
    } else {
        INK_ON_LIGHT
    }
}

/// WCAG contrast ratio.
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn luminance(c: Color32) -> f32 {
    let f = |v: u8| -> f32 {
        let v = v as f32 / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
}

/// Interpolate a ramp in OKLab (perceptually even steps).
fn ramp(stops: &[Color32], t: f32) -> Color32 {
    let pos = t * (stops.len() - 1) as f32;
    let i = (pos.floor() as usize).min(stops.len() - 2);
    let f = pos - i as f32;
    let a = to_oklab(stops[i]);
    let b = to_oklab(stops[i + 1]);
    from_oklab([a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f, a[2] + (b[2] - a[2]) * f])
}

fn srgb_to_linear(v: u8) -> f32 {
    let v = v as f32 / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f32) -> u8 {
    let v = if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

pub(crate) fn to_oklab(c: Color32) -> [f32; 3] {
    let (r, g, b) = (srgb_to_linear(c.r()), srgb_to_linear(c.g()), srgb_to_linear(c.b()));
    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

fn from_oklab(lab: [f32; 3]) -> Color32 {
    let [ll, a, b] = lab;
    let l = (ll + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m = (ll - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
    let s = (ll - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    Color32::from_rgb(
        linear_to_srgb(4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s),
        linear_to_srgb(-1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s),
        linear_to_srgb(-0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s),
    )
}

/// Same colour with alpha (premultiplied for egui).
pub fn alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_ramp_is_monotone_in_lightness_and_spans_the_stops() {
        let mut last = -1.0f32;
        for exp in 18..40 {
            let l = to_oklab(size_color(1u64 << exp))[0];
            assert!(l >= last - 1e-4, "lightness must not decrease");
            last = l;
        }
        assert_eq!(size_color(1), SIZE_RAMP[0]);
        assert_eq!(size_color(1 << 20), SIZE_RAMP[0]);
        assert_eq!(size_color(u64::MAX), SIZE_RAMP[4]);
    }

    #[test]
    fn oklab_round_trip() {
        for c in SIZE_RAMP.iter().chain(COPIES_RAMP.iter()) {
            let back = from_oklab(to_oklab(*c));
            assert!((back.r() as i32 - c.r() as i32).abs() <= 1, "{c:?} {back:?}");
            assert!((back.g() as i32 - c.g() as i32).abs() <= 1);
            assert!((back.b() as i32 - c.b() as i32).abs() <= 1);
        }
    }

    #[test]
    fn copies_pills_are_ordered_and_labels_readable() {
        assert_eq!(copies_fill(1), None);
        let fills: Vec<Color32> = [2, 3, 4, 6].iter().map(|&n| copies_fill(n).unwrap()).collect();
        for w in fills.windows(2) {
            assert!(to_oklab(w[1])[0] > to_oklab(w[0])[0] + 0.06, "visible lightness steps");
        }
        for f in fills {
            assert!(contrast(f, label_on(f)) >= 4.5, "pill label contrast for {f:?}");
        }
        assert_eq!(copies_fill(50), Some(COPIES_RAMP[3]));
    }

    #[test]
    fn ink_contrast_on_surfaces() {
        for bg in [PAGE, PANEL, SURFACE, SURFACE_ALT, ROW_HOVER, ROW_STRIPE] {
            assert!(contrast(INK, bg) >= 12.0);
            assert!(contrast(INK_SECONDARY, bg) >= 7.0);
            assert!(contrast(INK_MUTED, bg) >= 4.5);
            assert!(contrast(LINK, bg) >= 4.5);
        }
        // selected rows keep readable text and visible size bars (every ramp step is lighter)
        assert!(contrast(INK, ROW_SELECTED) >= 10.0);
        assert!(contrast(INK_MUTED, ROW_SELECTED) >= 3.5);
        for c in SIZE_RAMP {
            assert!(to_oklab(c)[0] > to_oklab(ROW_SELECTED)[0] + 0.1, "{c:?} visible on the selection");
        }
    }
}
