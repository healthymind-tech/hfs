//! Shared navigation chrome: the topbar product switcher and the breadcrumb
//! trail (UI/UX review 2026-10-13 §4.2, "導覽模型與資訊架構").
//!
//! Both consoles present one system to an operator, yet until now the only
//! way from HFS to HTS was a sidebar entry that opened a new tab, and there
//! was no way back at all. The switcher puts both products side by side in
//! the topbar of each console. The breadcrumb gives HFS's editor and history
//! pages the trail HTS's detail pages already had in their back link.

use askama::Template;

use crate::ChromeLabels;

/// Set by the HFS console on every page it serves (`Path=/`), so a HTS
/// console served from the **same origin** knows there is a FHIR console at
/// [`HFS_CONSOLE_PATH`] to switch to. A HTS reached on its own origin never
/// sees it and shows no switcher entry, rather than a link that would 404.
pub const HFS_CONSOLE_COOKIE: &str = "helios_hfs_console";

/// Where the HFS console is mounted. Fixed by `crates/hfs` (`/ui`).
pub const HFS_CONSOLE_PATH: &str = "/ui";

/// Where the HTS console is mounted. Fixed by `crates/hts` (`/ui/hts`).
pub const HTS_CONSOLE_PATH: &str = "/ui/hts";

/// Which console is rendering the switcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Product {
    /// The Helios FHIR Server console (`crates/ui`).
    Fhir,
    /// The Helios Terminology Server console (`crates/hts-ui`).
    Terminology,
}

/// The two entries of the product switcher. `None` for a product the current
/// console cannot reach; the switcher is then not rendered at all, since a
/// one-entry switcher switches nothing.
#[derive(Clone, Copy, Debug)]
pub struct ProductLinks<'a> {
    /// The console doing the rendering.
    pub current: Product,
    /// The HFS console's home, as the browser should follow it.
    pub fhir: Option<&'a str>,
    /// The HTS console's home, as the browser should follow it.
    pub terminology: Option<&'a str>,
}

impl ProductLinks<'_> {
    fn href(&self, product: Product) -> Option<&str> {
        match product {
            Product::Fhir => self.fhir,
            Product::Terminology => self.terminology,
        }
    }
}

/// Whether the HFS-console cookie is present in a `Cookie` header value.
pub fn has_hfs_console_cookie<'a>(cookie_headers: impl IntoIterator<Item = &'a str>) -> bool {
    cookie_headers
        .into_iter()
        .flat_map(|value| value.split(';'))
        .any(|pair| {
            pair.trim()
                .strip_prefix(HFS_CONSOLE_COOKIE)
                .is_some_and(|rest| rest.starts_with('='))
        })
}

/// The `Set-Cookie` value the HFS console sends so a same-origin HTS console
/// can offer the way back. Carries no data beyond "there is one".
pub fn hfs_console_set_cookie() -> String {
    format!("{HFS_CONSOLE_COOKIE}=1; Path=/; Max-Age=31536000; SameSite=Lax")
}

/// Is `href` somewhere other than this origin? Such a link opens in a new
/// tab, like the sidebar's Terminology entry; a same-origin path does not.
fn is_off_origin(href: &str) -> bool {
    !href.starts_with('/') || href.starts_with("//")
}

struct SwitchItem<'a> {
    label: String,
    href: &'a str,
    current: bool,
    new_tab: bool,
}

#[derive(Template)]
#[template(path = "partials/product-switcher.html")]
struct ProductSwitcherTemplate<'a> {
    i18n: &'a dyn ChromeLabels,
    items: Vec<SwitchItem<'a>>,
}

/// The topbar product switcher: a two-entry segmented control, "FHIR Server"
/// and "Terminology", the current console marked `aria-current="page"`.
///
/// Renders an empty string when either entry has no link. Like the other
/// topbar partials, a non-empty render starts at column 6 and carries no
/// trailing newline. Needs `product-switch-label`, `product-switch-fhir`,
/// `product-switch-terminology` and `product-switch-new-tab`.
///
/// # Errors
///
/// Propagates [`askama::Error`] from rendering.
pub fn product_switcher(
    i18n: &dyn ChromeLabels,
    links: ProductLinks<'_>,
) -> Result<String, askama::Error> {
    let mut items = Vec::with_capacity(2);
    for (product, key) in [
        (Product::Fhir, "product-switch-fhir"),
        (Product::Terminology, "product-switch-terminology"),
    ] {
        let Some(href) = links.href(product) else {
            return Ok(String::new());
        };
        let current = product == links.current;
        items.push(SwitchItem {
            label: i18n.t(key),
            href,
            current,
            new_tab: !current && is_off_origin(href),
        });
    }
    ProductSwitcherTemplate { i18n, items }.render()
}

/// One step of a breadcrumb trail. The last crumb is the current page and
/// should carry no `href`.
#[derive(Clone, Debug)]
pub struct Crumb {
    /// Already-localized (or literal, e.g. a resource id) text.
    pub label: String,
    /// Where the step leads; `None` for the current page.
    pub href: Option<String>,
}

impl Crumb {
    /// A crumb that links somewhere.
    pub fn link(label: impl Into<String>, href: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            href: Some(href.into()),
        }
    }

    /// The current page: no link, `aria-current="page"`.
    pub fn here(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            href: None,
        }
    }
}

#[derive(Template)]
#[template(path = "partials/breadcrumb.html")]
struct BreadcrumbTemplate<'a> {
    i18n: &'a dyn ChromeLabels,
    crumbs: &'a [Crumb],
}

/// A `<nav class="breadcrumb">` trail (WAI-ARIA breadcrumb pattern: an
/// ordered list, the current page `aria-current="page"`, separators drawn by
/// CSS so they are not read out). Empty for an empty trail. Needs the
/// `breadcrumb-label` key.
///
/// # Errors
///
/// Propagates [`askama::Error`] from rendering.
pub fn breadcrumb(i18n: &dyn ChromeLabels, crumbs: &[Crumb]) -> Result<String, askama::Error> {
    if crumbs.is_empty() {
        return Ok(String::new());
    }
    BreadcrumbTemplate { i18n, crumbs }.render()
}
