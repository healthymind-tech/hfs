//! The frame of the standalone pages both consoles render outside their
//! shell — sign-in, break-glass sign-in, forced password change, "no
//! access", sign-in failures, sign-out (design §10.2 "Sign-in page").
//!
//! Each console keeps its own `layouts/auth.html` (its own asset paths and
//! catalog), and composes it from the pieces here so the two cannot drift:
//! [`auth_brand`] (brand panel), [`auth_toolbar`] (language segmented
//! control + theme button), [`auth_footer`] (attribution + version) and
//! [`password_toggle`]. [`theme_attributes`] lets the server stamp a stored
//! theme on `<html>` so even the first byte matches the console's theme; the
//! inline [`crate::theme_boot`] script still runs and resolves `system`.

use askama::Template;

use crate::{ChromeLabels, THEME_COOKIE, theme_toggle};

/// The official product names, used for the second-language line under the
/// localized name in the brand panel.
#[derive(Debug, Clone, Copy)]
pub struct ProductNames<'a> {
    /// English name (`HealthyMind FHIR Server Manager`).
    pub en: &'a str,
    /// Traditional Chinese name (`療心智能 FHIR 伺服器管理平台`).
    pub zh_hant: &'a str,
}

/// What the brand panel says and where its assets live.
#[derive(Debug, Clone, Copy)]
pub struct AuthBrand<'a> {
    /// Console home (`/ui`, `/ui/hts`) the logo links to.
    pub home: &'a str,
    /// Asset base (`/ui/assets`, `/ui/hts/assets`).
    pub assets: &'a str,
    /// Fluent key of the localized product name.
    pub product_key: &'a str,
    /// Both official names; the one *not* in the request's language is shown
    /// as the second line.
    pub names: ProductNames<'a>,
    /// Fluent key of the one-line tagline.
    pub tagline_key: &'a str,
    /// Fluent keys of the highlights (may be empty).
    pub highlight_keys: &'a [&'a str],
}

#[derive(Template)]
#[template(path = "partials/auth-brand.html")]
struct AuthBrandTemplate<'a> {
    i18n: &'a dyn ChromeLabels,
    brand: AuthBrand<'a>,
    alt_name: &'a str,
    alt_lang: &'a str,
}

/// The brand panel.
///
/// # Errors
///
/// Propagates [`askama::Error`] from rendering.
pub fn auth_brand(i18n: &dyn ChromeLabels, brand: AuthBrand<'_>) -> Result<String, askama::Error> {
    let zh = i18n.lang().starts_with("zh");
    let (alt_name, alt_lang) = if zh {
        (brand.names.en, "en")
    } else {
        (brand.names.zh_hant, "zh-Hant")
    };
    AuthBrandTemplate {
        i18n,
        brand,
        alt_name,
        alt_lang,
    }
    .render()
}

/// One entry of the language segmented control.
#[derive(Debug, Clone)]
pub struct LangLink {
    /// BCP-47 code (`en`, `zh-Hant`).
    pub code: &'static str,
    /// Endonym shown on the control (`English`, `繁體中文`).
    pub label: &'static str,
    /// Target (`?lang=en&next=…`).
    pub href: String,
    /// Whether it is the language in effect.
    pub current: bool,
}

/// The languages the standalone pages offer (the manager's two: design
/// §10.4 — de/es remain reachable from the console's own account menu).
/// `suffix` is appended to each `?lang=` query (e.g. `&next=%2Fui`).
pub fn lang_links(current: &str, suffix: &str) -> Vec<LangLink> {
    [("en", "English"), ("zh-Hant", "繁體中文")]
        .into_iter()
        .map(|(code, label)| LangLink {
            code,
            label,
            href: format!("?lang={code}{suffix}"),
            current: current.eq_ignore_ascii_case(code),
        })
        .collect()
}

#[derive(Template)]
#[template(path = "partials/auth-toolbar.html")]
struct AuthToolbarTemplate<'a> {
    i18n: &'a dyn ChromeLabels,
    langs: &'a [LangLink],
    toggle: String,
}

/// Language segmented control + theme button.
///
/// # Errors
///
/// Propagates [`askama::Error`] from rendering.
pub fn auth_toolbar(i18n: &dyn ChromeLabels, langs: &[LangLink]) -> Result<String, askama::Error> {
    AuthToolbarTemplate {
        i18n,
        langs,
        toggle: theme_toggle(i18n)?,
    }
    .render()
}

#[derive(Template)]
#[template(path = "partials/auth-footer.html")]
struct AuthFooterTemplate<'a> {
    i18n: &'a dyn ChromeLabels,
    version: &'a str,
}

/// Attribution footer (`療心智能 HealthyMind Tech · Based on Helios FHIR
/// Server (MIT License) … · v0.2.9`).
///
/// # Errors
///
/// Propagates [`askama::Error`] from rendering.
pub fn auth_footer(i18n: &dyn ChromeLabels, version: &str) -> Result<String, askama::Error> {
    AuthFooterTemplate { i18n, version }.render()
}

#[derive(Template)]
#[template(path = "partials/password-toggle.html")]
struct PasswordToggleTemplate<'a> {
    i18n: &'a dyn ChromeLabels,
}

/// The "show password" button for a password field (needs the keys
/// `auth-password-show` / `auth-password-hide`).
///
/// # Errors
///
/// Propagates [`askama::Error`] from rendering.
pub fn password_toggle(i18n: &dyn ChromeLabels) -> Result<String, askama::Error> {
    PasswordToggleTemplate { i18n }.render()
}

/// Attributes for `<html>` from the request's stored theme: the shared
/// [`THEME_COOKIE`] when it says `light`/`dark`, else — for `system` or no
/// choice — the `Sec-CH-Prefers-Color-Scheme` client hint when the browser
/// sent one. Empty when neither is known (the inline boot script decides,
/// as on every console page). Returns a leading space when non-empty.
pub fn theme_attributes(cookie_header: Option<&str>, prefers_hint: Option<&str>) -> &'static str {
    let stored = cookie_header.and_then(|raw| {
        raw.split(';').find_map(|pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k == THEME_COOKIE).then(|| v.trim().to_string())
        })
    });
    let hint = || prefers_hint.map(|v| v.trim().trim_matches('"').to_ascii_lowercase());
    match stored.as_deref() {
        Some("dark") => r#" data-theme-pref="dark" data-theme="dark""#,
        Some("light") => r#" data-theme-pref="light" data-theme="light""#,
        _ => match hint().as_deref() {
            Some("dark") => r#" data-theme-pref="system" data-theme="dark""#,
            Some("light") => r#" data-theme-pref="system" data-theme="light""#,
            _ => "",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lang(&'static str);
    impl ChromeLabels for Lang {
        fn lang(&self) -> String {
            self.0.into()
        }
        fn t(&self, key: &str) -> String {
            format!("[{key}]")
        }
    }

    fn brand() -> AuthBrand<'static> {
        AuthBrand {
            home: "/ui",
            assets: "/ui/assets",
            product_key: "console-product",
            names: ProductNames {
                en: "HealthyMind FHIR Server Manager",
                zh_hant: "療心智能 FHIR 伺服器管理平台",
            },
            tagline_key: "auth-tagline",
            highlight_keys: &["auth-point-1", "auth-point-2"],
        }
    }

    #[test]
    fn brand_panel_shows_the_logo_and_both_official_names() {
        let en = auth_brand(&Lang("en"), brand()).unwrap();
        assert!(en.contains("/ui/assets/brand/healthymind-logo-horizontal@2x.png"));
        assert!(en.contains("[console-product]"));
        assert!(en.contains(r#"lang="zh-Hant">療心智能 FHIR 伺服器管理平台"#));
        assert!(en.contains("[auth-point-2]"));
        let zh = auth_brand(&Lang("zh-Hant"), brand()).unwrap();
        assert!(zh.contains(r#"lang="en">HealthyMind FHIR Server Manager"#));
    }

    #[test]
    fn toolbar_marks_the_current_language_and_keeps_the_query() {
        let links = lang_links("zh-Hant", "&next=%2Fui");
        let html = auth_toolbar(&Lang("zh-Hant"), &links).unwrap();
        assert!(
            html.contains(r#"href="?lang=en&amp;next=%2Fui""#)
                || html.contains(r#"href="?lang=en&#38;next=%2Fui""#),
            "{html}"
        );
        assert!(
            html.contains(r#"lang="zh-Hant" hreflang="zh-Hant" aria-current="true""#),
            "{html}"
        );
        assert!(!html.contains(r#"hreflang="en" aria-current"#));
        assert!(html.contains("data-theme-cycle"));
    }

    #[test]
    fn stored_theme_or_client_hint_is_stamped() {
        assert_eq!(
            theme_attributes(Some("a=b; hfs_theme=dark"), None),
            r#" data-theme-pref="dark" data-theme="dark""#
        );
        assert_eq!(
            theme_attributes(Some("hfs_theme=light"), Some("dark")),
            r#" data-theme-pref="light" data-theme="light""#
        );
        assert_eq!(
            theme_attributes(Some("hfs_theme=system"), Some("\"dark\"")),
            r#" data-theme-pref="system" data-theme="dark""#
        );
        assert_eq!(theme_attributes(None, None), "");
        assert_eq!(theme_attributes(Some("hfs_theme=<x>"), None), "");
    }

    #[test]
    fn password_toggle_is_a_labelled_toggle_button() {
        let html = password_toggle(&Lang("en")).unwrap();
        assert!(html.contains(r#"type="button""#));
        assert!(html.contains(r#"aria-pressed="false""#));
        assert!(html.contains("[auth-password-show]"));
        assert!(!html.contains("type=\"password\""));
    }
}
