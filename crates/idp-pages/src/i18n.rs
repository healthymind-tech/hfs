//! English and Traditional Chinese for the hosted pages (design D-9).
//!
//! The catalogs live in this crate (`locales/{en,zh-Hant}/idp.ftl`) so the
//! console's own catalog-parity tests are unaffected. The language follows
//! the console: `?lang=` (persisted in the shared `hfs_lang` cookie by the
//! language control), then that cookie, then `ui_locales` of the
//! authorization request, then `Accept-Language`, then English.

use std::borrow::Cow;
use std::collections::HashMap;

use axum::http::{HeaderMap, header};
use fluent_templates::fluent_bundle::FluentValue;
use fluent_templates::{Loader, static_loader};
use unic_langid::{LanguageIdentifier, langid};

static_loader! {
    static LOCALES = {
        locales: "./locales",
        fallback_language: "en",
        customise: |bundle| bundle.set_use_isolating(false),
    };
}

static EN: LanguageIdentifier = langid!("en");
static ZH_HANT: LanguageIdentifier = langid!("zh-Hant");

/// The cookie the console's language control writes.
pub const LANG_COOKIE: &str = helios_ui_chrome::LANG_COOKIE;

/// The language of one response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I18n {
    locale: &'static LanguageIdentifier,
}

impl Default for I18n {
    fn default() -> Self {
        Self { locale: &EN }
    }
}

/// `en` / `zh-Hant` for a BCP-47 tag (`zh`, `zh-TW`, `zh-Hant-TW`, … → zh-Hant).
fn match_tag(tag: &str) -> Option<&'static LanguageIdentifier> {
    let t = tag.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    if t == "en" || t.starts_with("en-") {
        return Some(&EN);
    }
    if t == "zh" || t.starts_with("zh-hant") || t == "zh-tw" || t == "zh-hk" || t == "zh-mo" {
        return Some(&ZH_HANT);
    }
    None
}

impl I18n {
    /// The language for a request: explicit `lang` (returns `true` as the
    /// second value so the caller persists it), cookie, `ui_locales`,
    /// `Accept-Language`, English.
    pub fn negotiate(
        explicit: Option<&str>,
        headers: &HeaderMap,
        ui_locales: &[String],
    ) -> (Self, bool) {
        if let Some(l) = explicit.and_then(match_tag) {
            return (Self { locale: l }, true);
        }
        let cookie = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .find_map(|kv| {
                kv.trim()
                    .strip_prefix(LANG_COOKIE)
                    .and_then(|r| r.strip_prefix('='))
                    .map(str::to_string)
            });
        if let Some(l) = cookie.as_deref().and_then(match_tag) {
            return (Self { locale: l }, false);
        }
        if let Some(l) = ui_locales.iter().find_map(|t| match_tag(t)) {
            return (Self { locale: l }, false);
        }
        let accept = headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let l = accept
            .split(',')
            .filter_map(|part| part.split(';').next())
            .find_map(match_tag)
            .unwrap_or(&EN);
        (Self { locale: l }, false)
    }

    /// The `Set-Cookie` that persists an explicit choice (same cookie as
    /// the console).
    pub fn cookie(&self) -> String {
        format!(
            "{LANG_COOKIE}={}; Path=/; Max-Age=31536000; SameSite=Lax",
            self.locale
        )
    }

    pub fn lang(&self) -> String {
        self.locale.to_string()
    }

    pub fn t(&self, key: &str) -> String {
        LOCALES
            .try_lookup(self.locale, key)
            .unwrap_or_else(|| key.to_owned())
    }

    pub fn t_arg(&self, key: &str, name: &str, value: impl Into<String>) -> String {
        let args: HashMap<Cow<'static, str>, FluentValue<'static>> = HashMap::from([(
            Cow::Owned(name.to_owned()),
            FluentValue::from(value.into()),
        )]);
        LOCALES
            .try_lookup_with_args(self.locale, key, &args)
            .unwrap_or_else(|| key.to_owned())
    }

    pub fn t_args(&self, key: &str, args: &[(&str, String)]) -> String {
        let map: HashMap<Cow<'static, str>, FluentValue<'static>> = args
            .iter()
            .map(|(k, v)| (Cow::Owned((*k).to_owned()), FluentValue::from(v.clone())))
            .collect();
        LOCALES
            .try_lookup_with_args(self.locale, key, &map)
            .unwrap_or_else(|| key.to_owned())
    }

    pub fn t_arg2(
        &self,
        key: &str,
        n1: &str,
        v1: impl Into<String>,
        n2: &str,
        v2: impl Into<String>,
    ) -> String {
        let args: HashMap<Cow<'static, str>, FluentValue<'static>> = HashMap::from([
            (Cow::Owned(n1.to_owned()), FluentValue::from(v1.into())),
            (Cow::Owned(n2.to_owned()), FluentValue::from(v2.into())),
        ]);
        LOCALES
            .try_lookup_with_args(self.locale, key, &args)
            .unwrap_or_else(|| key.to_owned())
    }
}

impl helios_ui_chrome::ChromeLabels for I18n {
    fn lang(&self) -> String {
        I18n::lang(self)
    }
    fn t(&self, key: &str) -> String {
        I18n::t(self, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_order() {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_LANGUAGE, "zh-TW,zh;q=0.9".parse().unwrap());
        assert_eq!(I18n::negotiate(None, &h, &[]).0.lang(), "zh-Hant");
        assert_eq!(I18n::negotiate(None, &h, &["en".into()]).0.lang(), "en");
        h.insert(header::COOKIE, "a=b; hfs_lang=en".parse().unwrap());
        assert_eq!(I18n::negotiate(None, &h, &[]).0.lang(), "en");
        let (l, explicit) = I18n::negotiate(Some("zh-Hant"), &h, &[]);
        assert_eq!(l.lang(), "zh-Hant");
        assert!(explicit);
        assert_eq!(I18n::negotiate(Some("klingon"), &HeaderMap::new(), &[]).0.lang(), "en");
    }

    #[test]
    fn catalogs_have_the_same_keys_and_resolve() {
        let keys = |l: &str| -> std::collections::BTreeSet<String> {
            let src = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("locales")
                    .join(l)
                    .join("idp.ftl"),
            )
            .unwrap();
            src.lines()
                .filter(|l| !l.starts_with('#') && !l.starts_with(' ') && l.contains(" ="))
                .map(|l| l.split(" =").next().unwrap().trim().to_string())
                .collect()
        };
        let plain = |l: &str| -> std::collections::BTreeSet<String> {
            let src = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("locales")
                    .join(l)
                    .join("idp.ftl"),
            )
            .unwrap();
            src.lines()
                .filter(|l| !l.starts_with('#') && l.contains(" =") && !l.contains("{ $"))
                .map(|l| l.split(" =").next().unwrap().trim().to_string())
                .collect()
        };
        let en = keys("en");
        assert!(en.len() > 40);
        assert_eq!(en, keys("zh-Hant"), "zh-Hant must define the same keys as en");
        let zh = I18n { locale: &ZH_HANT };
        for k in &plain("zh-Hant") {
            assert_ne!(zh.t(k), *k, "{k} unresolved in zh-Hant");
        }
        assert_eq!(
            zh.t_arg("idp-apps-granted", "date", "2026-01-02"),
            "於 2026-01-02 允許"
        );
        // The brand stays English in every language.
        assert_eq!(zh.t("idp-brand"), "HealthyMind");
    }
}
