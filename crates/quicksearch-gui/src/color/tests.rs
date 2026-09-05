use super::*;

/// The same pipeline against `std`, which the const numerics must reproduce.
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

/// WCAG relative luminance, written from the specification, not from `decode`.
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

/// Exact equality: a channel a step off is a regression, not rounding.
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
    for h in [0.0, 90.0, 217.0, 359.0] {
        let gray = oklch(0.6, 0.0, h);
        assert_eq!(gray.r(), gray.g(), "not gray at H={}: {:?}", h, gray);
        assert_eq!(gray.g(), gray.b(), "not gray at H={}: {:?}", h, gray);
    }
}

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
    assert_eq!(oklab_lerp(a, b, -0.5), a);
    assert_eq!(oklab_lerp(a, b, 1.5), b);
    // The midpoint is not darkened, the way an sRGB byte lerp leaves it.
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

/// 40 degrees is the tightest pair shipped: red to orange, orange to yellow.
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

/// Readability is what fixed each theme's lightness, so it is checked.
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

/// One call has to reach both themes: the live one alone is thrown away the
/// moment the color scheme is switched.
#[test]
fn the_text_greys_land_on_both_themes() {
    let ctx = egui::Context::default();
    apply_text_contrast(&ctx);
    for (theme, stock) in [
        (egui::Theme::Dark, egui::Visuals::dark()),
        (egui::Theme::Light, egui::Visuals::light()),
    ] {
        let visuals = &ctx.style_of(theme).visuals;
        assert_ne!(
            visuals.text_color(),
            stock.text_color(),
            "body text in {:?} is still egui's",
            theme
        );
        assert_ne!(
            visuals.widgets.inactive.text_color(),
            stock.widgets.inactive.text_color(),
            "widget text in {:?} is still egui's",
            theme
        );
    }
}

/// Which way each theme moved. Written against egui's own defaults so that an
/// upgrade quietly moving the baseline past us fails here instead of shipping.
#[test]
fn dark_text_lightens_and_light_text_darkens() {
    let ctx = egui::Context::default();
    apply_text_contrast(&ctx);
    let dark = &ctx.style_of(egui::Theme::Dark).visuals;
    let light = &ctx.style_of(egui::Theme::Light).visuals;
    for (name, ours, stock) in [
        (
            "dark body",
            dark.text_color(),
            egui::Visuals::dark().text_color(),
        ),
        (
            "dark widget",
            dark.widgets.inactive.text_color(),
            egui::Visuals::dark().widgets.inactive.text_color(),
        ),
    ] {
        assert!(
            luminance(ours) > luminance(stock),
            "{} is not lighter than egui's: {:?} vs {:?}",
            name,
            ours,
            stock
        );
    }
    for (name, ours, stock) in [
        (
            "light body",
            light.text_color(),
            egui::Visuals::light().text_color(),
        ),
        (
            "light widget",
            light.widgets.inactive.text_color(),
            egui::Visuals::light().widgets.inactive.text_color(),
        ),
    ] {
        assert!(
            luminance(ours) < luminance(stock),
            "{} is not darker than egui's: {:?} vs {:?}",
            name,
            ours,
            stock
        );
    }
}

/// The floors the greys were picked to clear, each on the fills it is
/// actually painted over.
#[test]
fn plain_text_clears_its_backgrounds() {
    let ctx = egui::Context::default();
    apply_text_contrast(&ctx);
    for (theme, floor) in [(egui::Theme::Dark, 5.5), (egui::Theme::Light, 8.5)] {
        let visuals = &ctx.style_of(theme).visuals;
        // Widget text is painted on the button fill, not on the panel.
        for (name, color, bgs) in [
            (
                "body",
                visuals.text_color(),
                [visuals.panel_fill, visuals.extreme_bg_color],
            ),
            (
                "widget",
                visuals.widgets.inactive.text_color(),
                // A frameless button (the tab strip) keeps the panel behind it.
                [visuals.widgets.inactive.weak_bg_fill, visuals.panel_fill],
            ),
        ] {
            for bg in bgs {
                let ratio = contrast(color, bg);
                assert!(
                    ratio >= floor,
                    "{} text in {:?} is {:.2}:1 on {:?}",
                    name,
                    theme,
                    ratio,
                    bg
                );
            }
        }
    }
}

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

/// The chips carry fixed near-black text: the reason the ramp has its own
/// lightness, apart from the palette's.
#[test]
fn every_chip_holds_its_dark_text() {
    let text = Color32::from_rgb(32, 32, 32);
    for stage in 0..=13u8 {
        let ratio = contrast(rank_tier_color(stage), text);
        assert!(ratio >= 6.5, "stage {} is {:.2}:1", stage, ratio);
    }
}

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
