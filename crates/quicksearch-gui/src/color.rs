//! Every color the GUI paints with, declared in OKLCH and converted to sRGB
//! at compile time.
//!
//! The conversion pipeline — polar to rectangular, a 3x3 matrix, a cube, a
//! second 3x3 matrix, the sRGB transfer curve — is not available in a
//! `const fn`: `sqrt`, `cbrt`, `powf`, `sin` and `cos` are all still
//! non-const. Hence the numerics below, exact enough that their output
//! matches `std`'s to the last bit of every channel (see the tests).

use egui::Color32;

// --- Const numerics ---

const PI: f64 = std::f64::consts::PI;

/// Iterations for the Newton loops below: each step doubles the correct
/// digits, so this is far past f64's 53 bits from any in-range guess.
const NEWTON_STEPS: usize = 60;

/// Newton's method for `sqrt`: x <- (x + a/x) / 2.
const fn sqrt(a: f64) -> f64 {
    if a <= 0.0 {
        return 0.0;
    }
    let mut x = if a > 1.0 { a / 2.0 } else { 1.0 };
    let mut i = 0;
    while i < NEWTON_STEPS {
        x = 0.5 * (x + a / x);
        i += 1;
    }
    x
}

/// Newton's method for `cbrt`: x <- (2x + a/x²) / 3.
const fn cbrt(a: f64) -> f64 {
    if a <= 0.0 {
        return 0.0;
    }
    let mut x = if a > 1.0 { a / 3.0 } else { 1.0 };
    let mut i = 0;
    while i < NEWTON_STEPS {
        x = (2.0 * x + a / (x * x)) / 3.0;
        i += 1;
    }
    x
}

/// Newton's method for the fifth root: x <- (4x + a/x⁴) / 5.
const fn fifth_root(a: f64) -> f64 {
    if a <= 0.0 {
        return 0.0;
    }
    let mut x = if a > 1.0 { a / 5.0 } else { 1.0 };
    let mut i = 0;
    while i < NEWTON_STEPS {
        let x4 = x * x * x * x;
        x = (4.0 * x + a / x4) / 5.0;
        i += 1;
    }
    x
}

/// Taylor series for cosine, after reducing the angle to [-pi, pi] where the
/// series converges fastest. Twelve terms there are already below f64's
/// resolution.
const fn cos_rad(x: f64) -> f64 {
    let turns = x / (2.0 * PI);
    let mut r = x - (turns as i64 as f64) * 2.0 * PI;
    if r > PI {
        r -= 2.0 * PI;
    }
    if r < -PI {
        r += 2.0 * PI;
    }
    let x2 = r * r;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut n = 1;
    while n <= 12 {
        term = -term * x2 / (((2 * n - 1) * (2 * n)) as f64);
        sum += term;
        n += 1;
    }
    sum
}

const fn sin_rad(x: f64) -> f64 {
    cos_rad(x - PI / 2.0)
}

/// The sRGB transfer curve, linear light to encoded.
///
/// The exponent is `1/2.4`, which is `5/12` — so three exact roots stand in
/// for the `powf` that is not available here: `x^(5/12)` is the cube root of
/// the fourth root of `x⁵`.
const fn encode(x: f64) -> f64 {
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * cbrt(sqrt(sqrt(x * x * x * x * x))) - 0.055
    }
}

/// The same curve inverted, encoded to linear light. The exponent is `2.4`,
/// which is `2 + 2/5`, so a square and a fifth root of that square do it.
const fn decode(x: f64) -> f64 {
    if x <= 0.040_45 {
        x / 12.92
    } else {
        let y = (x + 0.055) / 1.055;
        let y2 = y * y;
        y2 * fifth_root(y2)
    }
}

/// Linear light to one 8-bit channel, clamped: a color outside the sRGB gamut
/// is pinned to the nearest one that exists rather than wrapping into a
/// different hue entirely.
const fn channel(v: f64) -> u8 {
    let v = if v < 0.0 {
        0.0
    } else if v > 1.0 {
        1.0
    } else {
        v
    };
    let s = encode(v) * 255.0 + 0.5;
    if s <= 0.0 {
        0
    } else if s >= 255.0 {
        255
    } else {
        s as u8
    }
}

// --- OKLab / OKLCH ---

/// Björn Ottosson's OKLab, converted to sRGB.
const fn from_oklab(l: f64, a: f64, b: f64) -> Color32 {
    let l_ = l + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m_ = l - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s_ = l - 0.089_484_177_5 * a - 1.291_485_548_0 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    Color32::from_rgb(
        channel(4.076_741_662_1 * l3 - 3.307_711_591_3 * m3 + 0.230_969_929_2 * s3),
        channel(-1.268_438_004_6 * l3 + 2.609_757_401_1 * m3 - 0.341_319_396_5 * s3),
        channel(-0.004_196_086_3 * l3 - 0.703_418_614_7 * m3 + 1.707_614_701_0 * s3),
    )
}

/// A `Color32`'s channels with its alpha divided back out, in 0..=1.
/// `Color32` stores its channels premultiplied; measuring one without
/// undoing that reports every translucent color as darker than it is.
const fn unmultiplied(c: Color32) -> (f64, f64, f64, f64) {
    let a = c.a() as f64 / 255.0;
    if a <= 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    (
        c.r() as f64 / 255.0 / a,
        c.g() as f64 / 255.0 / a,
        c.b() as f64 / 255.0 / a,
        a,
    )
}

/// The inverse: an sRGB color measured back into OKLab. Alpha is not part of
/// the answer — it is divided out first, so a translucent color reports the
/// color it is rather than the color it would blend to.
pub const fn to_oklab(color: Color32) -> (f64, f64, f64) {
    let (sr, sg, sb, _) = unmultiplied(color);
    let r = decode(sr);
    let g = decode(sg);
    let b = decode(sb);
    let l = cbrt(0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b);
    let m = cbrt(0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b);
    let s = cbrt(0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b);
    (
        0.210_454_255_3 * l + 0.793_617_785_0 * m - 0.004_072_046_8 * s,
        1.977_998_495_1 * l - 2.428_592_205_0 * m + 0.450_593_709_9 * s,
        0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766_0 * s,
    )
}

/// An sRGB color from its OKLCH coordinates: lightness `l` in 0..=1, chroma
/// `c` (roughly 0..=0.37 for colors sRGB can show), hue `h_deg` in degrees.
pub const fn oklch(l: f64, c: f64, h_deg: f64) -> Color32 {
    let h = h_deg * PI / 180.0;
    from_oklab(l, c * cos_rad(h), c * sin_rad(h))
}

/// Blend two colors through OKLab, where the half-way point looks half-way
/// (interpolating sRGB bytes darkens and desaturates the middle). Opacity
/// is blended separately, and the ends are returned untouched rather than
/// round-tripped, so `t` of 0 or 1 is exactly the color passed in.
pub const fn oklab_lerp(from: Color32, to: Color32, t: f32) -> Color32 {
    if t <= 0.0 {
        return from;
    }
    if t >= 1.0 {
        return to;
    }
    let t = t as f64;
    let (l1, a1, b1) = to_oklab(from);
    let (l2, a2, b2) = to_oklab(to);
    let blended = from_oklab(l1 + (l2 - l1) * t, a1 + (a2 - a1) * t, b1 + (b2 - b1) * t);
    let alpha = from.a() as f64 + (to.a() as f64 - from.a() as f64) * t;
    premultiply(blended, alpha / 255.0)
}

/// Fold an opacity back into an opaque color, the way `Color32` stores it.
const fn premultiply(c: Color32, alpha: f64) -> Color32 {
    Color32::from_rgba_premultiplied(
        scale_channel(c.r(), alpha),
        scale_channel(c.g(), alpha),
        scale_channel(c.b(), alpha),
        scale_channel(255, alpha),
    )
}

const fn scale_channel(v: u8, alpha: f64) -> u8 {
    let s = v as f64 * alpha + 0.5;
    if s <= 0.0 {
        0
    } else if s >= 255.0 {
        255
    } else {
        s as u8
    }
}

/// Declare `Color32` constants from their OKLCH coordinates.
macro_rules! oklch_colors {
    ($(
        $(#[$attr:meta])*
        $vis:vis const $name:ident = ($l:expr, $c:expr, $h:expr);
    )+) => {
        $(
            $(#[$attr])*
            $vis const $name: egui::Color32 = $crate::color::oklch($l, $c, $h);
        )+
    };
}

// --- The palette ---

/// Text lightness and chroma on the dark theme's near-black panels: the
/// chroma is as much as blue, the least accommodating hue, can carry there.
const DARK_L: f64 = 0.75;
const DARK_C: f64 = 0.12;

/// The same on the light theme's white. Yellow binds here — anything
/// brighter cannot clear the palette's 4.5:1 contrast.
const LIGHT_L: f64 = 0.52;
const LIGHT_C: f64 = 0.11;

/// The rank chips paint their own background under near-black text, so
/// they share one set of values across both themes.
const CHIP_L: f64 = 0.75;
const CHIP_C: f64 = 0.12;

/// One hue per role family, spread as evenly as five families and the
/// red-through-yellow crowding allow: the tightest neighbors are 40° apart.
const HUE_RED: f64 = 20.0;
const HUE_ORANGE: f64 = 60.0;
const HUE_YELLOW: f64 = 100.0;
const HUE_GREEN: f64 = 150.0;
const HUE_BLUE: f64 = 250.0;

oklch_colors! {
    /// Errors, invalid patterns, and the query language's keywords.
    const DARK_RED = (DARK_L, DARK_C, HUE_RED);
    /// Manual mode, cautions, and edits staged but not yet applied.
    const DARK_ORANGE = (DARK_L, DARK_C, HUE_ORANGE);
    /// The walk half of an indexing run.
    const DARK_YELLOW = (DARK_L, DARK_C, HUE_YELLOW);
    /// The extraction half, valid patterns, and query operators.
    const DARK_GREEN = (DARK_L, DARK_C, HUE_GREEN);
    /// Finished work, the primary commit controls, and query arguments.
    const DARK_BLUE = (DARK_L, DARK_C, HUE_BLUE);

    const LIGHT_RED = (LIGHT_L, LIGHT_C, HUE_RED);
    const LIGHT_ORANGE = (LIGHT_L, LIGHT_C, HUE_ORANGE);
    const LIGHT_YELLOW = (LIGHT_L, LIGHT_C, HUE_YELLOW);
    const LIGHT_GREEN = (LIGHT_L, LIGHT_C, HUE_GREEN);
    const LIGHT_BLUE = (LIGHT_L, LIGHT_C, HUE_BLUE);
}

/// The GUI's colors for one theme, named by hue rather than by job — each
/// one carries several jobs:
///
/// | hue | status hint | query syntax | emphasis |
/// |-----|-------------|--------------|----------|
/// | red | — | keyword | invalid pattern |
/// | orange | manual idle | — | caution, staged edit |
/// | yellow | indexing | — | — |
/// | green | extracting text | operator | valid pattern |
/// | blue | done | argument | commit controls |
pub struct Palette {
    pub red: Color32,
    pub orange: Color32,
    pub yellow: Color32,
    pub green: Color32,
    pub blue: Color32,
}

/// The palette for the live theme. Read it as
/// `palette(ui.visuals().dark_mode)` and never cache the result: the theme
/// can change between one frame and the next.
pub fn palette(dark_mode: bool) -> Palette {
    if dark_mode {
        Palette {
            red: DARK_RED,
            orange: DARK_ORANGE,
            yellow: DARK_YELLOW,
            green: DARK_GREEN,
            blue: DARK_BLUE,
        }
    } else {
        Palette {
            red: LIGHT_RED,
            orange: LIGHT_ORANGE,
            yellow: LIGHT_YELLOW,
            green: LIGHT_GREEN,
            blue: LIGHT_BLUE,
        }
    }
}

// --- The rank ramp ---

/// Rank chips run from blue at the strongest match to red at the weakest.
const RANK_HUE_BEST: f64 = 250.0;
const RANK_HUE_WORST: f64 = 25.0;

/// Hue of rank tier `i`, counting from 0: an even sweep across the arc.
const fn rank_hue(i: usize) -> f64 {
    RANK_HUE_BEST - (i as f64) * (RANK_HUE_BEST - RANK_HUE_WORST) / ((RANK_TIERS - 1) as f64)
}

const RANK_TIERS: usize = 11;

/// The chip colorbar: one lightness and chroma, hue doing all the work, so
/// every chip holds the same contrast against its near-black text.
const RANK_RAMP: [Color32; RANK_TIERS] = {
    let mut ramp = [Color32::BLACK; RANK_TIERS];
    let mut i = 0;
    while i < RANK_TIERS {
        ramp[i] = oklch(CHIP_L, CHIP_C, rank_hue(i));
        i += 1;
    }
    ramp
};

/// The chip color for a hit's cascade stage. In tier order: name exact with
/// exact case, name exact any case, name substring exact case, name
/// substring any case, full text exact case, full text any case, fuzzy name,
/// fuzzy full text, path substring exact case, path substring any case, and
/// fuzzy path — which is also where every stage outside the cascade lands.
pub fn rank_tier_color(stage: u8) -> Color32 {
    match stage {
        1..=10 => RANK_RAMP[stage as usize - 1],
        _ => RANK_RAMP[RANK_TIERS - 1],
    }
}

#[cfg(test)]
mod tests;
