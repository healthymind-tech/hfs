//! The frame of the console's standalone pages (sign-in, break-glass
//! sign-in, forced password change, "not allowed", "no access", sign-in
//! failures, sign-out): `templates/layouts/auth.html`, composed from the
//! shared pieces in `helios_ui_chrome::auth` so the HTS console's sign-in
//! (`crates/hts-ui`) cannot drift from it. Same tokens, fonts and theme
//! logic as the console shell: the shared `hfs_theme` cookie (stamped on
//! `<html>` by the server when it says light/dark, and by the inline boot
//! script otherwise, which also follows `prefers-color-scheme`).

use axum::http::{HeaderMap, header};

use crate::I18n;

/// Asset base of this console.
const ASSETS: &str = "/ui/assets";

/// Request-derived parts of the standalone page frame.
#[derive(Debug, Clone)]
pub(crate) struct AuthShell {
    theme: &'static str,
    langs: Vec<helios_ui_chrome::LangLink>,
}

impl AuthShell {
    /// `query_suffix` is appended to each language link's `?lang=…` (e.g.
    /// `&next=%2Fui`), so switching language keeps the page's own query.
    pub(crate) fn new(i18n: &I18n, headers: Option<&HeaderMap>, query_suffix: &str) -> Self {
        let get = |name: &str| {
            headers
                .and_then(|h| h.get(name))
                .and_then(|v| v.to_str().ok())
        };
        Self {
            theme: helios_ui_chrome::theme_attributes(
                get(header::COOKIE.as_str()),
                get("sec-ch-prefers-color-scheme"),
            ),
            langs: helios_ui_chrome::lang_links(&i18n.lang(), query_suffix),
        }
    }

    /// Attributes for `<html>` (empty, or a leading space and the theme).
    pub(crate) fn theme_attrs(&self) -> &'static str {
        self.theme
    }

    pub(crate) fn brand_head(&self) -> Result<String, askama::Error> {
        helios_ui_chrome::brand_head(ASSETS)
    }

    pub(crate) fn theme_boot(&self) -> Result<String, askama::Error> {
        helios_ui_chrome::theme_boot()
    }

    pub(crate) fn brand(&self, i18n: &I18n) -> Result<String, askama::Error> {
        helios_ui_chrome::auth_brand(
            i18n,
            helios_ui_chrome::AuthBrand {
                home: helios_ui_chrome::HFS_CONSOLE_PATH,
                assets: ASSETS,
                product_key: "console-product",
            },
        )
    }

    pub(crate) fn toolbar(&self, i18n: &I18n) -> Result<String, askama::Error> {
        helios_ui_chrome::auth_toolbar(i18n, &self.langs)
    }

    pub(crate) fn footer(&self, i18n: &I18n) -> Result<String, askama::Error> {
        helios_ui_chrome::auth_footer(i18n, concat!("v", env!("CARGO_PKG_VERSION")))
    }

    /// The "show password" button for a password field.
    pub(crate) fn reveal(&self, i18n: &I18n) -> Result<String, askama::Error> {
        helios_ui_chrome::password_toggle(i18n)
    }
}

/// A message banner on a standalone page.
#[derive(Debug, Clone)]
pub(crate) struct Banner {
    /// `ok`, `info`, `warn` or `danger` (the `.notice--*` modifier).
    pub(crate) kind: &'static str,
    /// The text.
    pub(crate) text: String,
    /// Technical detail, folded under the text.
    pub(crate) detail: Option<String>,
}

impl Banner {
    pub(crate) fn ok(text: String) -> Self {
        Self {
            kind: "ok",
            text,
            detail: None,
        }
    }
    pub(crate) fn info(text: String) -> Self {
        Self {
            kind: "info",
            text,
            detail: None,
        }
    }
    pub(crate) fn warn(text: String) -> Self {
        Self {
            kind: "warn",
            text,
            detail: None,
        }
    }
    pub(crate) fn danger(text: String) -> Self {
        Self {
            kind: "danger",
            text,
            detail: None,
        }
    }
    /// With technical detail (shown folded).
    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
    /// `alert` for problems, `status` for confirmations.
    pub(crate) fn role(&self) -> &'static str {
        match self.kind {
            "ok" | "info" => "status",
            _ => "alert",
        }
    }
}
