//! Every color the GUI paints with, declared in OKLCH and converted to sRGB
//! at compile time.
//!
//! OKLCH is worth the conversion because its three coordinates are the three
//! questions a palette actually has to answer: how bright (L), how vivid (C),
//! and which color (H). Fixing L and C across a set and varying only H is the
//! whole reason the status hints read as one system — no hue shouts louder
//! than its neighbors, and "readable on this background" becomes one number
//! checked once rather than a judgement made per color.
//!
//! The conversion is a fixed pipeline — polar to rectangular, a 3x3 matrix, a
//! cube, a second 3x3 matrix, the sRGB transfer curve — but none of it is
//! available in a `const fn`: `sqrt`, `cbrt`, `powf`, `sin` and `cos` are all
//! still non-const. Hence the numerics below, which are exact enough that
//! their output matches `std`'s to the last bit of every channel (see the
//! tests). Doing this at compile time is what keeps the declarations honest:
//! a palette written as `Color32::from_rgb` literals hides every relationship
//! that makes it a palette.

use egui::Color32;

// ---------------------------------------------------------------------------
// Const numerics
// ---------------------------------------------------------------------------

const PI: f64 = std::f64::consts::PI;

/// Iterations for the Newton loops below. Each step doubles the correct
/// digits, so this is far past f64's 53 bits from any starting guess in
/// range — the cost is paid once, at compile time, and buys the luxury of
/// not having to reason about how good the guess was.
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

// ---------------------------------------------------------------------------
// OKLab / OKLCH
// ---------------------------------------------------------------------------

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
///
/// `Color32` stores its channels premultiplied, and a premultiplied channel
/// is not a color — it is a color already faded toward whatever it will be
/// drawn on. Measuring one without undoing that reports every translucent
/// color as darker than it is.
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
///
/// Const, so a palette declared with it costs nothing at runtime.
pub const fn oklch(l: f64, c: f64, h_deg: f64) -> Color32 {
    let h = h_deg * PI / 180.0;
    from_oklab(l, c * cos_rad(h), c * sin_rad(h))
}

/// Blend two colors through OKLab, where the half-way point looks half-way.
/// Interpolating sRGB bytes instead darkens and desaturates the middle of
/// every fade.
///
/// Opacity is blended too, and separately: a fade that ends on one of egui's
/// translucent theme colors has to actually arrive there, not at an opaque
/// impostor of it. The ends are returned untouched rather than round-tripped,
/// so `t` of 0 or 1 is exactly the color that was passed in.
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
///
/// The point of the macro is the shape of what it accepts: three numbers per
/// color, in the same order, so a palette reads as a table and an outlier in
/// it is visible on the page.
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

// ---------------------------------------------------------------------------
// The palette
// ---------------------------------------------------------------------------

/// Text lightness and chroma on the dark theme's near-black panels. The
/// chroma is as much as the *least* accommodating hue can carry at this
/// lightness (blue, which runs out first), because a palette is only uniform
/// if every member can actually reach the shared value.
const DARK_L: f64 = 0.75;
const DARK_C: f64 = 0.12;

/// The same on the light theme's white. Yellow is the binding constraint
/// here, and it is why "yellow" on white is necessarily a gold: anything
/// brighter cannot clear the 4.5:1 contrast the rest of the palette holds.
const LIGHT_L: f64 = 0.52;
const LIGHT_C: f64 = 0.11;

/// The rank chips paint their own background and lay near-black text over
/// it, so they are lighter than either text palette and share one set of
/// values across both themes.
const CHIP_L: f64 = 0.75;
const CHIP_C: f64 = 0.12;

/// One hue per role family. Hue is the only thing that separates the colors
/// in a palette, so these are spread as evenly as five families and the
/// red-through-yellow crowding allow: the tightest neighbors are 40 degrees
/// apart, some three times the smallest difference the eye can find.
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

/// The GUI's colors for one theme, named by hue rather than by job: each one
/// carries several jobs, and naming it for one of them would make the other
/// call sites read like accidents.
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
/// `palette(ui.visuals().dark_mode)` and never cache the result: `[ui]
/// color_scheme` is applied without a restart (see
/// [`crate::app::apply_theme`]), so the theme can change between one frame
/// and the next.
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

// ---------------------------------------------------------------------------
// The rank ramp
// ---------------------------------------------------------------------------

/// Rank chips run from blue at the strongest match to red at the weakest.
const RANK_HUE_BEST: f64 = 250.0;
const RANK_HUE_WORST: f64 = 25.0;

/// Hue of rank tier `i`, counting from 0: an even sweep across the arc.
const fn rank_hue(i: usize) -> f64 {
    RANK_HUE_BEST - (i as f64) * (RANK_HUE_BEST - RANK_HUE_WORST) / ((RANK_TIERS - 1) as f64)
}

const RANK_TIERS: usize = 11;

/// The chip colorbar: one lightness and chroma, hue doing all the work, so
/// no tier draws the eye harder than its neighbors and every chip holds the
/// same contrast against the near-black text printed on it.
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
mod tests {
    use super::*;

    /// The same pipeline written against `std`, which is what the const
    /// numerics above have to reproduce.
    fn reference(l: f64, c: f64, h_deg: f64) -> Color32 {
        let h = h_deg.to_radians();
        let (a, b) = (c * h.cos(), c * h.sin());
        let l_ = l + 0.3963377774 * a + 0.2158037573 * b;
        let m_ = l - 0.1055613458 * a - 0.0638541728 * b;
        let s_ = l - 0.0894841775 * a - 1.2914855480 * b;
        let (l3, m3, s3) = (l_.powi(3), m_.powi(3), s_.powi(3));
        let f = |v: f64| {
            let v = v.clamp(0.0, 1.0);
            let g = if v <= 0.0031308 {
                12.92 * v
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            };
            (g * 255.0).round() as u8
        };
        Color32::from_rgb(
            f(4.0767416621 * l3 - 3.3077115913 * m3 + 0.2309699292 * s3),
            f(-1.2684380046 * l3 + 2.6097574011 * m3 - 0.3413193965 * s3),
            f(-0.0041960863 * l3 - 0.7034186147 * m3 + 1.7076147010 * s3),
        )
    }

    /// WCAG relative luminance, for the contrast checks below. Deliberately
    /// written from the specification rather than reusing [`decode`]: a test
    /// that shares its arithmetic with the code it checks proves less.
    fn luminance(c: Color32) -> f64 {
        let f = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    fn oklch_of(c: Color32) -> (f64, f64, f64) {
        let (l, a, b) = to_oklab(c);
        (
            l,
            (a * a + b * b).sqrt(),
            b.atan2(a).to_degrees().rem_euclid(360.0),
        )
    }

    /// The whole justification for hand-rolling `sqrt`, `cbrt` and `cos`:
    /// they have to agree with `std` everywhere, not just on the palette.
    /// Exact equality rather than a tolerance, because that is what the
    /// numerics actually deliver — a channel that ever landed a step off
    /// would be a real regression, not rounding.
    #[test]
    fn the_const_conversion_matches_std_across_the_whole_space() {
        let mut l = 0.0;
        while l <= 1.0001 {
            let mut c = 0.0;
            while c <= 0.31 {
                let mut h = 0.0;
                while h < 360.0 {
                    assert_eq!(
                        oklch(l, c, h),
                        reference(l, c, h),
                        "L={} C={} H={}",
                        l,
                        c,
                        h
                    );
                    h += 3.0;
                }
                c += 0.01;
            }
            l += 0.025;
        }
    }

    #[test]
    fn the_ends_of_the_scale_are_black_and_white() {
        assert_eq!(oklch(0.0, 0.0, 0.0), Color32::BLACK);
        assert_eq!(oklch(1.0, 0.0, 0.0), Color32::WHITE);
        // A gray is a color with no chroma, whatever hue is named.
        for h in [0.0, 90.0, 217.0, 359.0] {
            let gray = oklch(0.6, 0.0, h);
            assert_eq!(gray.r(), gray.g(), "not gray at H={}: {:?}", h, gray);
            assert_eq!(gray.g(), gray.b(), "not gray at H={}: {:?}", h, gray);
        }
    }

    /// Asking for a color sRGB cannot show must give the nearest one it can,
    /// not a wrapped byte in a different hue family.
    #[test]
    fn out_of_gamut_requests_clamp() {
        for (l, c, h) in [(0.9, 0.4, 250.0), (0.5, 0.35, 20.0), (1.2, 0.1, 150.0)] {
            let color = oklch(l, c, h);
            let _ = color; // reaching here at all means no panic and no wrap
            assert_eq!(color, reference(l, c, h), "L={} C={} H={}", l, c, h);
        }
        assert_eq!(oklch(2.0, 0.0, 0.0), Color32::WHITE);
        assert_eq!(oklch(-1.0, 0.0, 0.0), Color32::BLACK);
    }

    /// `to_oklab` is the inverse of `oklch`, to within the 8 bits a channel
    /// has to hold the answer in.
    #[test]
    fn measuring_a_color_recovers_what_was_asked_for() {
        for (l, c, h) in [
            (DARK_L, DARK_C, HUE_RED),
            (DARK_L, DARK_C, HUE_BLUE),
            (LIGHT_L, LIGHT_C, HUE_YELLOW),
            (CHIP_L, CHIP_C, HUE_GREEN),
        ] {
            let (ml, mc, mh) = oklch_of(oklch(l, c, h));
            assert!((ml - l).abs() < 0.005, "L {} vs {}", ml, l);
            assert!((mc - c).abs() < 0.005, "C {} vs {}", mc, c);
            assert!((mh - h).abs() < 1.5, "H {} vs {}", mh, h);
        }
    }

    #[test]
    fn a_blend_keeps_its_endpoints() {
        let (a, b) = (DARK_RED, LIGHT_BLUE);
        assert_eq!(oklab_lerp(a, b, 0.0), a);
        assert_eq!(oklab_lerp(a, b, 1.0), b);
        // Out-of-range t clamps rather than extrapolating off the scale.
        assert_eq!(oklab_lerp(a, b, -0.5), a);
        assert_eq!(oklab_lerp(a, b, 1.5), b);
        // The midpoint is genuinely between the two, not darkened the way an
        // sRGB byte lerp leaves it.
        let mid = oklab_lerp(a, b, 0.5);
        let (l, _, _) = to_oklab(mid);
        let (la, _, _) = to_oklab(a);
        let (lb, _, _) = to_oklab(b);
        assert!(
            l > la.min(lb) - 0.01 && l < la.max(lb) + 0.01,
            "midpoint lightness {} is outside [{}, {}]",
            l,
            la,
            lb
        );
    }

    /// The claim the palette makes: within a theme, only hue varies. Anything
    /// else and one hint would read as more urgent than another for no
    /// reason the user could name.
    #[test]
    fn each_theme_is_one_lightness_and_one_chroma() {
        for (dark, l, c) in [(true, DARK_L, DARK_C), (false, LIGHT_L, LIGHT_C)] {
            let p = palette(dark);
            for (name, color) in [
                ("red", p.red),
                ("orange", p.orange),
                ("yellow", p.yellow),
                ("green", p.green),
                ("blue", p.blue),
            ] {
                let (ml, mc, _) = oklch_of(color);
                assert!(
                    (ml - l).abs() < 0.005,
                    "{} in dark={} has L={}, palette is {}",
                    name,
                    dark,
                    ml,
                    l
                );
                assert!(
                    (mc - c).abs() < 0.005,
                    "{} in dark={} has C={}, palette is {}",
                    name,
                    dark,
                    mc,
                    c
                );
            }
        }
    }

    /// With L and C shared, hue is the only thing telling two colors apart,
    /// so the spacing is the whole design. 40 degrees is the tightest pair
    /// (red to orange, orange to yellow) and is several times the smallest
    /// hue difference the eye resolves.
    #[test]
    fn no_two_colors_are_closer_than_forty_degrees() {
        let hues = [HUE_RED, HUE_ORANGE, HUE_YELLOW, HUE_GREEN, HUE_BLUE];
        for (i, a) in hues.iter().enumerate() {
            for b in &hues[i + 1..] {
                let d = (a - b).abs();
                let d = if d > 180.0 { 360.0 - d } else { d };
                assert!(d >= 40.0, "{} and {} are {} degrees apart", a, b, d);
            }
        }
    }

    /// Readability is the constraint that fixed the lightness of each theme,
    /// so it is checked rather than assumed — against the panel the status
    /// bar paints on and the text field the query colors paint on.
    #[test]
    fn every_color_clears_wcag_aa_on_its_own_background() {
        for (dark, bgs) in [
            (
                true,
                [
                    egui::Visuals::dark().panel_fill,
                    egui::Visuals::dark().extreme_bg_color,
                ],
            ),
            (
                false,
                [
                    egui::Visuals::light().panel_fill,
                    egui::Visuals::light().extreme_bg_color,
                ],
            ),
        ] {
            let p = palette(dark);
            for (name, color) in [
                ("red", p.red),
                ("orange", p.orange),
                ("yellow", p.yellow),
                ("green", p.green),
                ("blue", p.blue),
            ] {
                for bg in bgs {
                    let ratio = contrast(color, bg);
                    assert!(
                        ratio >= 4.5,
                        "{} in dark={} is {:.2}:1 on {:?}",
                        name,
                        dark,
                        ratio,
                        bg
                    );
                }
            }
        }
    }

    /// The chips read as a colorbar: hue marching one way from blue to red,
    /// nothing else moving. The old ramp asserted this channel by channel,
    /// which a real hue sweep cannot satisfy — red dips as the sweep passes
    /// through cyan — so the claim is made where it actually lives.
    #[test]
    fn the_rank_ramp_is_an_even_sweep_from_blue_to_red() {
        let mut prev: Option<f64> = None;
        for (i, color) in RANK_RAMP.iter().enumerate() {
            let (l, c, h) = oklch_of(*color);
            assert!((l - CHIP_L).abs() < 0.005, "tier {} has L={}", i, l);
            assert!((c - CHIP_C).abs() < 0.005, "tier {} has C={}", i, c);
            if let Some(prev) = prev {
                assert!(h < prev, "tier {} turned back at H={} from {}", i, h, prev);
            }
            prev = Some(h);
        }
        let (_, _, first) = oklch_of(RANK_RAMP[0]);
        let (_, _, last) = oklch_of(RANK_RAMP[RANK_TIERS - 1]);
        assert!(
            (first - RANK_HUE_BEST).abs() < 1.5,
            "best tier at H={}",
            first
        );
        assert!(
            (last - RANK_HUE_WORST).abs() < 1.5,
            "worst tier at H={}",
            last
        );
    }

    /// The chips carry fixed near-black text, so every tier has to stay light
    /// enough to hold it — the reason the ramp has its own lightness.
    #[test]
    fn every_chip_holds_its_dark_text() {
        let text = Color32::from_rgb(32, 32, 32);
        for stage in 0..=13u8 {
            let ratio = contrast(rank_tier_color(stage), text);
            assert!(ratio >= 6.5, "stage {} is {:.2}:1", stage, ratio);
        }
    }

    /// Stages outside the cascade share the weakest tier's chip — 0 and 11
    /// and up all land there, as they did before the ramp moved here.
    #[test]
    fn stages_outside_the_cascade_take_the_last_chip() {
        let worst = RANK_RAMP[RANK_TIERS - 1];
        assert_eq!(rank_tier_color(11), worst);
        assert_eq!(rank_tier_color(12), worst);
        assert_eq!(rank_tier_color(255), worst);
        assert_eq!(rank_tier_color(0), worst);
        for stage in 1..=10u8 {
            assert_eq!(rank_tier_color(stage), RANK_RAMP[stage as usize - 1]);
        }
    }
}
