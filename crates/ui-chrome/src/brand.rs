//! The HealthyMind brand chrome both consoles render (design §10.2): the
//! sidebar brand and its twin in the phone / tablet topbar (the official heart
//! mark followed by the English wordmark "HealthyMind" and the product line,
//! set as real text in the console's own font), the head's favicon / app-icon
//! links, and the attribution footer ("Based on Helios FHIR Server (MIT)").
//!
//! The wordmark is English in every UI language — it is the brand, not UI
//! copy — so the brand block never shows the Chinese company name, and the
//! version lives in the footer, the About page and the brand's tooltip rather
//! than in the block itself.

use askama::Template;

use crate::ChromeLabels;

/// Where a console serves the brand assets and how it names itself.
#[derive(Debug, Clone, Copy)]
pub struct BrandLinks<'a> {
    /// The console home (`/ui`, `/ui/hts`).
    pub home: &'a str,
    /// The console's asset base (`/ui/assets`, `/ui/hts/assets`); the
    /// brand files are under `{assets}/brand/`.
    pub assets: &'a str,
    /// The English product line under the wordmark (`FHIR Server Manager`,
    /// `Terminology Server`). Deliberately not a Fluent key: the brand block
    /// reads the same in every language.
    pub product_line: &'a str,
    /// Version label (`v0.2.9`, `hts v0.2.9`), shown in the tooltip.
    pub version: &'a str,
    /// Fluent key of "Home" for the link's accessible name.
    pub home_key: &'a str,
}

#[derive(Template)]
#[template(path = "partials/brand.html")]
struct BrandTemplate<'a> {
    i18n: &'a dyn ChromeLabels,
    links: BrandLinks<'a>,
    /// `brand` (sidebar) or `topbar__brand` (phone / tablet topbar).
    class: &'a str,
}

#[derive(Template)]
#[template(path = "partials/brand-head.html")]
struct BrandHeadTemplate<'a> {
    assets: &'a str,
}

#[derive(Template)]
#[template(path = "partials/site-footer.html")]
struct FooterTemplate<'a> {
    i18n: &'a dyn ChromeLabels,
    links: BrandLinks<'a>,
    about_href: &'a str,
}

/// The sidebar brand link.
///
/// # Errors
///
/// Propagates [`askama::Error`] from rendering.
pub fn brand(i18n: &dyn ChromeLabels, links: BrandLinks<'_>) -> Result<String, askama::Error> {
    BrandTemplate {
        i18n,
        links,
        class: "brand",
    }
    .render()
}

/// The same brand link for the topbar, shown below the drawer breakpoint
/// where the sidebar is off-canvas.
///
/// # Errors
///
/// Propagates [`askama::Error`] from rendering.
pub fn topbar_brand(
    i18n: &dyn ChromeLabels,
    links: BrandLinks<'_>,
) -> Result<String, askama::Error> {
    BrandTemplate {
        i18n,
        links,
        class: "topbar__brand",
    }
    .render()
}

/// `<link rel="icon">`, apple-touch icon, manifest and theme colour for
/// `<head>`.
///
/// # Errors
///
/// Propagates [`askama::Error`] from rendering.
pub fn brand_head(assets: &str) -> Result<String, askama::Error> {
    BrandHeadTemplate { assets }.render()
}

/// The attribution footer. `about_href` links the product's About page.
///
/// # Errors
///
/// Propagates [`askama::Error`] from rendering.
pub fn site_footer(
    i18n: &dyn ChromeLabels,
    links: BrandLinks<'_>,
    about_href: &str,
) -> Result<String, askama::Error> {
    FooterTemplate {
        i18n,
        links,
        about_href,
    }
    .render()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Keys;
    impl ChromeLabels for Keys {
        fn lang(&self) -> String {
            "en".into()
        }
        fn t(&self, key: &str) -> String {
            format!("[{key}]")
        }
    }

    fn links() -> BrandLinks<'static> {
        BrandLinks {
            home: "/ui/hts",
            assets: "/ui/hts/assets",
            product_line: "Terminology Server",
            version: "hts v1",
            home_key: "hts-nav-home",
        }
    }

    /// Han ideographs (CJK Unified Ideographs and Extension A).
    fn has_han(s: &str) -> bool {
        s.chars()
            .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3400}'..='\u{4dbf}').contains(&c))
    }

    struct Chinese;
    impl ChromeLabels for Chinese {
        fn lang(&self) -> String {
            "zh-Hant".into()
        }
        fn t(&self, key: &str) -> String {
            match key {
                "hts-nav-home" => "首頁".into(),
                _ => "療心智能".into(),
            }
        }
    }

    #[test]
    fn brand_is_the_heart_mark_and_an_english_wordmark() {
        let html = brand(&Keys, links()).unwrap();
        assert!(html.contains(r#"<a class="brand" href="/ui/hts""#), "{html}");
        // Heart mark only, crisp at 1x/2x/3x; decorative (the name is text).
        assert!(html.contains(r#"src="/ui/hts/assets/brand/heart-mark.png""#));
        assert!(html.contains("/ui/hts/assets/brand/heart-mark@2x.png 2x"));
        assert!(html.contains("/ui/hts/assets/brand/heart-mark@3x.png 3x"));
        assert_eq!(html.matches("<img").count(), 1, "one image: {html}");
        assert!(html.contains(r#"alt="""#));
        assert!(!html.contains("logo-horizontal"), "no horizontal logo");
        assert!(html.contains(r#"<span class="brand__name">HealthyMind</span>"#));
        assert!(html.contains(r#"<span class="brand__product">Terminology Server</span>"#));
        // The version is a tooltip, not a line of the block.
        assert!(html.contains(r#"title="HealthyMind Terminology Server hts v1""#));
        assert!(!html.contains("brand__version"));
        // The link names itself by its visible text plus where it goes.
        assert!(html.contains("[hts-nav-home]"));
        assert!(!html.contains("aria-label"), "visible text is the name: {html}");
    }

    #[test]
    fn brand_shows_no_chinese_in_any_language() {
        for html in [
            brand(&Chinese, links()).unwrap(),
            topbar_brand(&Chinese, links()).unwrap(),
        ] {
            // Only the screen-reader-only "Home" hint is localized.
            let visible = html.replace(r#"<span class="visually-hidden"> — 首頁</span>"#, "");
            assert!(!has_han(&visible), "Chinese in the brand: {html}");
            assert!(visible.contains("HealthyMind"));
        }
    }

    #[test]
    fn topbar_brand_is_the_same_link() {
        let html = topbar_brand(&Keys, links()).unwrap();
        assert!(html.contains(r#"<a class="topbar__brand" href="/ui/hts""#), "{html}");
        assert!(html.contains("/ui/hts/assets/brand/heart-mark.png"));
        assert!(html.contains(r#"<span class="brand__name">HealthyMind</span>"#));
        assert!(html.contains(r#"<span class="brand__product">Terminology Server</span>"#));
    }

    #[test]
    fn head_links_favicons_and_manifest() {
        let html = brand_head("/ui/assets").unwrap();
        for needle in [
            r#"href="/ui/assets/brand/favicon.svg""#,
            r#"href="/ui/assets/brand/favicon-32.png""#,
            r#"href="/ui/assets/brand/apple-touch-icon-180.png""#,
            r#"href="/ui/assets/brand/manifest.webmanifest""#,
            r##"content="#2f5be0""##,
        ] {
            assert!(html.contains(needle), "{needle} in {html}");
        }
    }

    #[test]
    fn footer_keeps_the_upstream_attribution() {
        let html = site_footer(&Keys, links(), "/ui/about").unwrap();
        assert!(html.contains("[mgr-footer-based-on]"));
        assert!(html.contains("[mgr-footer-brand]"));
        assert!(html.contains(r#"href="/ui/about""#));
        assert!(html.contains("hts v1"));
    }
}
