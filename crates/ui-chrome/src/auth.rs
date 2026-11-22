//! The frame of the standalone pages both consoles render outside their
//! shell — sign-in, break-glass sign-in, forced password change, "no
//! access", sign-in failures, sign-out (design §10.2 "Sign-in page").
//!
//! Each console keeps its own `layouts/auth.html` (its own asset paths and
//! catalog), and composes it from the pieces here so the two cannot drift:
//! [`auth_brand`] (logo + product name at the top of the card), [`auth_toolbar`] (language segmented
//! control + theme button), [`auth_footer`] (attribution + version) and
//! [`password_toggle`]. [`theme_attributes`] lets the server stamp a stored
//! theme on `<html>` so even the first byte matches the console's theme; the
//! inline [`crate::theme_boot`] script still runs and resolves `system`.

use askama::Template;

use crate::{ChromeLabels, THEME_COOKIE, theme_toggle};

/// What the card's brand header says and where its assets live.
#[derive(Debug, Clone, Copy)]
pub struct AuthBrand<'a> {
    /// Console home (`/ui`, `/ui/hts`) the logo links to.
    pub home: &'a str,
    /// Asset base (`/ui/assets`, `/ui/hts/assets`).
    pub assets: &'a str,
    /// Fluent key of the localized product name.
    pub product_key: &'a str,
}

#[derive(Template)]
#[template(path = "partials/auth-brand.html")]
struct AuthBrandTemplate<'a> {
    i18n: &'a dyn ChromeLabels,
    brand: AuthBrand<'a>,
}

/// The brand header of the sign-in card: logo + product name.
///
/// # Errors
///
/// Propagates [`askama::Error`] from rendering.
pub fn auth_brand(i18n: &dyn ChromeLabels, brand: AuthBrand<'_>) -> Result<String, askama::Error> {
    AuthBrandTemplate { i18n, brand }.render()
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
        }
    }

    #[test]
    fn card_brand_shows_the_logo_and_the_product_name() {
        let html = auth_brand(&Lang("en"), brand()).unwrap();
        assert!(html.contains("/ui/assets/brand/healthymind-logo-horizontal@2x.png"));
        assert!(html.contains(r#"<p class="auth-logo__name">[console-product]</p>"#));
        assert!(html.contains(r#"href="/ui""#));
        assert!(html.contains(r#"alt="""#), "the wordmark is decorative");
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
