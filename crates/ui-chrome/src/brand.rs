//! The HealthyMind brand chrome both consoles render (design §10.2): the
//! sidebar brand (official horizontal logo, heart mark on the collapsed rail,
//! product name as real text), the head's favicon / app-icon links, and the
//! attribution footer ("Based on Helios FHIR Server (MIT)").

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
    /// Fluent key of the full product name (`app-title`, `hts-app-title`).
    pub title_key: &'a str,
    /// Fluent key of the product line under the logo.
    pub product_line_key: &'a str,
    /// Version label (`v0.2.9`, `hts v0.2.9`).
    pub version: &'a str,
    /// Fluent key of "Home" for the link's accessible name.
    pub home_key: &'a str,
}

#[derive(Template)]
#[template(path = "partials/brand.html")]
struct BrandTemplate<'a> {
    i18n: &'a dyn ChromeLabels,
    links: BrandLinks<'a>,
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
    BrandTemplate { i18n, links }.render()
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
            title_key: "hts-app-title",
            product_line_key: "hts-app-product-line",
            version: "hts v1",
            home_key: "hts-nav-home",
        }
    }

    #[test]
    fn brand_uses_the_official_logo_and_real_text() {
        let html = brand(&Keys, links()).unwrap();
        assert!(html.contains(r#"href="/ui/hts""#));
        assert!(html.contains("/ui/hts/assets/brand/healthymind-logo-horizontal.png"));
        assert!(html.contains("/ui/hts/assets/brand/mark-64.png"));
        assert!(html.contains("[hts-app-product-line]"));
        assert!(html.contains(r#"aria-label="[hts-app-title] hts v1 — [hts-nav-home]""#));
        // Decorative images: the name is text.
        assert!(html.matches(r#"alt="""#).count() == 2);
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
