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

/// WCAG relative luminance, written from the specification rather than
/// reusing [`decode`]: a test sharing its arithmetic with the code it
/// checks proves less.
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

/// The const numerics have to agree with `std` everywhere, not just on
/// the palette. Exact equality: a channel a step off would be a real
/// regression, not rounding.
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

/// The claim the palette makes: within a theme, only hue varies.
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

/// With L and C shared, hue spacing is the whole design; 40 degrees is
/// the tightest pair (red to orange, orange to yellow).
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

/// Readability fixed each theme's lightness, so it is checked rather than
/// assumed — against the panel and text-field backgrounds.
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
/// nothing else moving.
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

/// Stages outside the cascade share the weakest tier's chip: 0 and 11
/// and up all land there.
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
