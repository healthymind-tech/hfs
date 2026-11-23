//! HealthyMind design tokens (design `docs/design/fhir-server-manager.md`
//! §10.2), shared by the HFS and HTS consoles.
//!
//! The values live in the shared stylesheet (`crates/ui/assets/app.css`,
//! which HTS serves too); this module is their typed mirror so the contrast
//! requirements are *asserted* rather than hoped for: every text/background
//! pair the components use is listed in [`TEXT_PAIRS`] and must clear WCAG
//! 2.2 AA (4.5:1), every non-text indicator in [`UI_PAIRS`] 3:1. The HFS test
//! suite also checks that each token value appears in the stylesheet, so the
//! two cannot drift apart.
//!
//! The raw logo gradient (`#0AEAFF → #3EADFF → #756DFF`) fails text contrast
//! on white and is used only decoratively (logo, bands, sparklines, focus
//! glow); text and controls use the derived shades below.

/// The decorative brand gradient, as a CSS value.
pub const BRAND_GRADIENT: &str = "linear-gradient(135deg, #0aeaff 0%, #3eadff 50%, #756dff 100%)";

/// One theme's token values (lower-case `#rrggbb`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// `light` or `dark`.
    pub name: &'static str,
    /// Content background (`--bg-content`).
    pub bg: &'static str,
    /// Sidebar / topbar background (`--bg`).
    pub frame: &'static str,
    /// Cards (`--surface`).
    pub surface: &'static str,
    /// Body text (`--text`).
    pub text: &'static str,
    /// Secondary text (`--muted`).
    pub muted: &'static str,
    /// Links and primary buttons (`--color-primary` = `--accent-strong`).
    pub primary: &'static str,
    /// Text on primary buttons (`--color-on-primary`).
    pub on_primary: &'static str,
    /// Accent (`--color-accent`).
    pub accent: &'static str,
    /// Info text (`--color-info`).
    pub info: &'static str,
    /// Success / warning / danger (`--ok`, `--warn`, `--danger`).
    pub ok: &'static str,
    /// Warning.
    pub warn: &'static str,
    /// Danger.
    pub danger: &'static str,
    /// Pill backgrounds and their text: (`--ok-soft`, `--ok-text`), …
    pub ok_pill: (&'static str, &'static str),
    /// Warning pill.
    pub warn_pill: (&'static str, &'static str),
    /// Danger pill.
    pub danger_pill: (&'static str, &'static str),
    /// Info pill (`--color-info-soft`, `--color-info-text`).
    pub info_pill: (&'static str, &'static str),
    /// Muted pill (`--muted-soft`, `--muted-text`).
    pub muted_pill: (&'static str, &'static str),
    /// Focus ring (`--color-focus-ring`).
    pub focus: &'static str,
    /// Inset / hover surface (`--surface-2`, console redesign §4.1).
    pub surface_2: &'static str,
    /// Captions, table headers, meta (`--text-3`).
    pub text_3: &'static str,
}

/// Light theme.
pub const LIGHT: Theme = Theme {
    name: "light",
    bg: "#f6f8fc",
    frame: "#eef2f9",
    surface: "#ffffff",
    text: "#0e1726",
    muted: "#4a5568",
    primary: "#2f5be0",
    on_primary: "#ffffff",
    accent: "#5a54e8",
    info: "#0a7bb8",
    ok: "#17663f",
    warn: "#9a5b00",
    danger: "#c62828",
    ok_pill: ("#e3f3ea", "#17663f"),
    warn_pill: ("#fbf0dc", "#7f4b00"),
    danger_pill: ("#fbe7e7", "#a81f1f"),
    info_pill: ("#e3f1f9", "#07618f"),
    muted_pill: ("#e9edf4", "#4a5568"),
    focus: "#1a8cff",
    surface_2: "#f1f4f9",
    text_3: "#5f6b7d",
};

/// Dark theme.
pub const DARK: Theme = Theme {
    name: "dark",
    bg: "#0e1726",
    frame: "#0b1320",
    surface: "#131c2e",
    text: "#e8eef8",
    muted: "#a7b3c6",
    primary: "#5cb8ff",
    on_primary: "#0e1726",
    accent: "#a5a0ff",
    info: "#49c6ff",
    ok: "#5fd39a",
    warn: "#f2b84b",
    danger: "#ff7a7a",
    ok_pill: ("#123026", "#74dca8"),
    warn_pill: ("#2f2612", "#f5c66a"),
    danger_pill: ("#3a1f27", "#ff9696"),
    info_pill: ("#12293d", "#7dd6ff"),
    muted_pill: ("#222d42", "#b4bfd0"),
    focus: "#49c6ff",
    surface_2: "#162133",
    text_3: "#8592a7",
};

/// The console redesign's scale (REDESIGN.md §4): every value below must be
/// declared in the stylesheet under the given custom property, so the
/// layout auditor's design-scale rule and the CSS agree.
pub const SCALE: &[(&str, &str)] = &[
    ("--fs-xs", "12px"),
    ("--fs-sm", "13px"),
    ("--fs-md", "14px"),
    ("--fs-lg", "16px"),
    ("--fs-xl", "22px"),
    ("--fs-2xl", "28px"),
    ("--s1", "4px"),
    ("--s2", "8px"),
    ("--s3", "12px"),
    ("--s4", "16px"),
    ("--s5", "24px"),
    ("--s6", "32px"),
    ("--s7", "48px"),
    ("--r-sm", "6px"),
    ("--r-md", "10px"),
    ("--r-full", "999px"),
    ("--dur-1", "120ms"),
    ("--dur-2", "180ms"),
    ("--dur-3", "240ms"),
    ("--dur-4", "420ms"),
    ("--sidebar-w", "248px"),
];

/// Both themes.
pub const THEMES: [Theme; 2] = [LIGHT, DARK];

/// Relative luminance of `#rrggbb` (WCAG 2.2).
pub fn luminance(hex: &str) -> f64 {
    let hex = hex.trim_start_matches('#');
    let channel = |i: usize| -> f64 {
        let c = f64::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0)) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4)
}

/// Contrast ratio of two colours (1..=21).
pub fn contrast(a: &str, b: &str) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Every text-on-background pair the components use, per theme:
/// `(label, foreground, background)`. Each must reach 4.5:1.
pub fn text_pairs(t: &Theme) -> Vec<(&'static str, &'static str, &'static str)> {
    let mut v = Vec::new();
    for (bg_name, bg) in [("surface", t.surface), ("bg", t.bg), ("frame", t.frame)] {
        let _ = bg_name;
        v.push(("text", t.text, bg));
        v.push(("muted", t.muted, bg));
        v.push(("primary", t.primary, bg));
        v.push(("accent", t.accent, bg));
        v.push(("danger", t.danger, bg));
    }
    for (label, fg) in [
        ("text", t.text),
        ("muted", t.muted),
        ("text-3", t.text_3),
        ("primary", t.primary),
        ("danger", t.danger),
    ] {
        v.push((label, fg, t.surface_2));
    }
    for bg in [t.surface, t.bg, t.frame] {
        v.push(("text-3", t.text_3, bg));
    }
    v.push(("info", t.info, t.surface));
    v.push(("ok", t.ok, t.surface));
    v.push(("warn", t.warn, t.surface));
    v.push(("on-primary", t.on_primary, t.primary));
    for (label, (bg, fg)) in [
        ("ok-pill", t.ok_pill),
        ("warn-pill", t.warn_pill),
        ("danger-pill", t.danger_pill),
        ("info-pill", t.info_pill),
        ("muted-pill", t.muted_pill),
    ] {
        v.push((label, fg, bg));
    }
    v
}

/// Non-text indicators (focus ring, input borders of errors) — 3:1.
pub fn ui_pairs(t: &Theme) -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("focus-ring", t.focus, t.surface),
        ("focus-ring-bg", t.focus, t.bg),
        ("danger-border", t.danger, t.surface),
        ("primary-border", t.primary, t.surface),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_math_matches_wcag_examples() {
        assert!((contrast("#000000", "#ffffff") - 21.0).abs() < 0.01);
        assert!((contrast("#ffffff", "#ffffff") - 1.0).abs() < 0.001);
        // Design doc §10.2: primary on white is 5.7:1.
        assert!((contrast("#2f5be0", "#ffffff") - 5.69).abs() < 0.02);
    }

    #[test]
    fn every_text_pair_meets_wcag_aa() {
        for theme in THEMES {
            for (label, fg, bg) in text_pairs(&theme) {
                let ratio = contrast(fg, bg);
                assert!(
                    ratio >= 4.5,
                    "{} theme: {label} {fg} on {bg} is {ratio:.2}:1 (< 4.5)",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn every_indicator_meets_three_to_one() {
        for theme in THEMES {
            for (label, fg, bg) in ui_pairs(&theme) {
                let ratio = contrast(fg, bg);
                assert!(
                    ratio >= 3.0,
                    "{} theme: {label} {fg} on {bg} is {ratio:.2}:1 (< 3)",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn the_type_scale_has_six_steps_and_radii_three() {
        let fs = SCALE.iter().filter(|(k, _)| k.starts_with("--fs-")).count();
        let r = SCALE.iter().filter(|(k, _)| k.starts_with("--r-")).count();
        assert_eq!((fs, r), (6, 3));
    }

    #[test]
    fn raw_brand_colours_are_decorative_only() {
        // The reason the gradient never carries text.
        assert!(contrast("#0aeaff", "#ffffff") < 3.0);
        assert!(contrast("#3eadff", "#ffffff") < 3.0);
        assert!(BRAND_GRADIENT.contains("#0aeaff") && BRAND_GRADIENT.contains("#756dff"));
    }
}
