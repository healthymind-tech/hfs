//! The shared product switcher and breadcrumb (UI/UX review §4.2).

use helios_ui_chrome::{
    ChromeLabels, Crumb, HFS_CONSOLE_COOKIE, Product, ProductLinks, breadcrumb,
    has_hfs_console_cookie, hfs_console_set_cookie, product_switcher,
};

struct Stub;

impl ChromeLabels for Stub {
    fn lang(&self) -> String {
        "en".to_string()
    }

    fn t(&self, key: &str) -> String {
        format!("[{key}]")
    }
}

#[test]
fn switcher_lists_both_products_and_marks_the_current_one() {
    let html = product_switcher(
        &Stub,
        ProductLinks {
            current: Product::Fhir,
            fhir: Some("/ui"),
            terminology: Some("/ui/hts"),
        },
    )
    .expect("renders");
    assert!(html.contains(r#"<nav class="product-switch" aria-label="[product-switch-label]">"#));
    assert_eq!(html.matches("product-switch__item").count(), 2);
    assert!(
        html.contains(r#"href="/ui" aria-current="page">[product-switch-fhir]</a>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"href="/ui/hts">[product-switch-terminology]</a>"#),
        "{html}"
    );
    assert!(
        !html.contains("target="),
        "same-origin switch stays in the tab: {html}"
    );
    assert!(html.starts_with("      <nav"), "column-6 indentation");
    assert!(!html.ends_with('\n'));
}

#[test]
fn switcher_opens_an_off_origin_product_in_a_new_tab() {
    let html = product_switcher(
        &Stub,
        ProductLinks {
            current: Product::Fhir,
            fhir: Some("/ui"),
            terminology: Some("https://tx.example.org/ui/hts"),
        },
    )
    .expect("renders");
    assert!(
        html.contains(
            r#"href="https://tx.example.org/ui/hts" target="_blank" rel="noopener noreferrer""#
        ),
        "{html}"
    );
}

#[test]
fn switcher_is_absent_when_the_other_product_is_unreachable() {
    for links in [
        ProductLinks {
            current: Product::Terminology,
            fhir: None,
            terminology: Some("/ui/hts"),
        },
        ProductLinks {
            current: Product::Fhir,
            fhir: Some("/ui"),
            terminology: None,
        },
    ] {
        assert_eq!(product_switcher(&Stub, links).expect("renders"), "");
    }
}

#[test]
fn hfs_console_cookie_round_trips() {
    let set = hfs_console_set_cookie();
    assert!(set.starts_with(&format!("{HFS_CONSOLE_COOKIE}=1;")));
    assert!(set.contains("Path=/") && set.contains("SameSite=Lax"));
    assert!(has_hfs_console_cookie([
        "hfs_lang=de; helios_hfs_console=1"
    ]));
    assert!(has_hfs_console_cookie(["helios_hfs_console=1"]));
    assert!(!has_hfs_console_cookie([
        "hfs_lang=de",
        "helios_hfs_console_x=1"
    ]));
    assert!(!has_hfs_console_cookie([]));
}

#[test]
fn breadcrumb_is_an_ordered_list_ending_on_the_current_page() {
    let crumbs = [
        Crumb::link("Resources", "/ui/resources"),
        Crumb::link("Patient", "/ui/resources?type=Patient&x=<1>"),
        Crumb::here("p-1"),
    ];
    let html = breadcrumb(&Stub, &crumbs).expect("renders");
    assert!(
        html.starts_with(r#"<nav class="breadcrumb" aria-label="[breadcrumb-label]">"#),
        "{html}"
    );
    assert_eq!(html.matches("<li class=\"breadcrumb__item\">").count(), 3);
    assert!(html.contains(r#"<a href="/ui/resources">Resources</a>"#));
    assert!(
        html.contains("&lt;1&gt;") || html.contains("&#60;1&#62;"),
        "hrefs are escaped: {html}"
    );
    assert!(html.contains(r#"<span aria-current="page">p-1</span>"#));
    assert_eq!(html.matches("aria-current").count(), 1);
    assert!(!html.ends_with('\n'));
    assert_eq!(breadcrumb(&Stub, &[]).expect("renders"), "");
}
