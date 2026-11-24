//! Layout guards for the shared stylesheet (hm/fix-layout-overflow).
//!
//! The layout auditor (hfs-eval/eval/scripts/ui-functional/layout_audit.py)
//! measures the rendered console; these checks keep the rules it found
//! missing from being dropped again in a later stylesheet edit.

const CSS: &str = include_str!("../assets/app.css");

/// The body of the last top-level rule whose selector list contains `selector`.
fn rule(selector: &str) -> &'static str {
    let at = CSS
        .rfind(&format!("{selector} {{"))
        .or_else(|| CSS.rfind(&format!("{selector},")))
        .unwrap_or_else(|| panic!("no rule for {selector} in app.css"));
    let open = at + CSS[at..].find('{').unwrap();
    let close = open + CSS[open..].find('}').unwrap();
    &CSS[open + 1..close]
}

#[test]
fn form_controls_use_the_console_font() {
    // Browsers default <input>/<button>/<select>/<textarea> to their own font
    // (Arial on Linux): list search boxes and icon buttons rendered in it.
    assert!(rule(":where(button, input, select, textarea)").contains("font-family: inherit"));
}

#[test]
fn name_cells_keep_tags_beside_a_truncating_url() {
    // The ICD-10-CM zh-Hant "supplement" tag was painted over its URL.
    let url = rule(".data-table .col-name .url");
    for decl in [
        "display: block",
        "overflow: hidden",
        "text-overflow: ellipsis",
        "white-space: nowrap",
    ] {
        assert!(url.contains(decl), "{decl} on .col-name .url");
    }
    let line = rule(".data-table .col-name__line");
    assert!(line.contains("display: flex") && line.contains("min-width: 0"));
    assert!(rule(".data-table .col-name__line > .tag").contains("flex: none"));
}

#[test]
fn disclosures_share_one_style() {
    // Inline disclosures: no native marker, a chevron, padding, focus ring.
    let inline = rule(".diff__textual > summary");
    for decl in [
        "list-style: none",
        "padding: 6px 10px 6px 8px",
        "cursor: pointer",
    ] {
        assert!(inline.contains(decl), "{decl} on inline summaries");
    }
    assert!(CSS.contains("details > summary:focus-visible {"));
    // Card disclosures keep the card-head padding (the import job's
    // "Raw result" label sat on the pill's left edge) and drop the head's
    // bottom rule while closed.
    assert!(rule("details.card > summary.card-head").contains("padding: 12px 14px"));
    assert!(
        rule("details.card:not([open]) > summary.card-head")
            .contains("border-bottom-color: transparent")
    );
}

#[test]
fn stacked_cards_are_spaced() {
    // Cards after a wrapper (htmx target, two-column grid) or inside a
    // classless wrapper (job detail, jobs list) touched each other.
    assert!(
        rule(".content:not(.content--app) > div:not([class]) > :is(.card, details.card) ~ :is(.card, details.card)")
            .contains("margin-top: 20px")
    );
}

#[test]
fn fieldsets_draw_no_browser_frame() {
    // The endpoint Security "Authorization server" group kept the browser's
    // groove frame; its bottom rule lay on the Issuer/Audience/JWKS labels.
    let fieldset = rule(":where(fieldset)");
    for decl in ["border: 0", "padding: 0", "margin: 0", "min-width: 0"] {
        assert!(fieldset.contains(decl), "{decl} on fieldsets");
    }
    assert!(rule(".idp-provider").contains("margin: 4px 0 20px"));
}

#[test]
fn workspace_rail_is_narrower_and_collapses() {
    // hm/vd-layout: the 280px rail left most of its column empty on View
    // Definitions; it is 232px and collapses to a 44px strip.
    let two = rule(".filter-layout--two");
    assert!(two.contains("--rail-w: 232px"), "{two}");
    assert!(two.contains("grid-template-columns: var(--rail-w) minmax(0, 1fr)"));
    assert!(CSS.contains("--rail-w: 44px;"));
    // The editor/form split and the view modes.
    assert!(CSS.contains("flex: var(--ws-split, 50) 1 0;"));
    assert!(CSS.contains("flex: calc(100 - var(--ws-split, 50)) 1 0;"));
    assert!(CSS.contains(r#"html[data-ws-view="json"] .editor__grid--stretch[data-ws-pair] > .editor-form"#));
    assert!(CSS.contains(
        ".editor__grid--stretch[data-ws-pair=\"fill\"] {\n    height: var(--ws-fill, 70vh);"
    ));
}

#[test]
fn collapsed_sidebar_is_a_64px_rail_that_never_moves_on_hover() {
    let at = CSS
        .find("Collapsible console sidebar (hm/vd-layout)")
        .expect("sidebar collapse block");
    let at = CSS[..at].rfind("/*").expect("the block's header comment");
    let block = &CSS[at..];
    assert!(block.contains(r#":root[data-sb="collapsed"] {"#));
    assert!(block.contains("--sidebar-w: 64px;"));
    // Hover only ever changes colours: no width/padding/transform under :hover.
    for (i, _) in block.match_indices(":hover") {
        // Skip mentions inside comments.
        let before = &block[..i];
        if before.rfind("/*") > before.rfind("*/") {
            continue;
        }
        let open = i + block[i..].find('{').unwrap();
        let close = open + block[open..].find('}').unwrap();
        let body = &block[open + 1..close];
        for prop in ["width", "padding", "margin", "transform", "gap", "display"] {
            assert!(
                !body.lines().any(|l| l.trim_start().starts_with(&format!("{prop}:"))),
                "a :hover rule in the sidebar block sets {prop}: {body}"
            );
        }
    }
    // The animation runs only while `.sb-anim` is set (a click or Ctrl+B),
    // never on load or resize.
    assert!(block.contains(":root.sb-anim body {"));
}

#[test]
fn first_paint_boot_stamps_the_layout_preferences() {
    let boot = include_str!("../../ui-chrome/templates/partials/theme-boot.html");
    for needle in [
        r#"getItem("hfs-sidebar")"#,
        r#"e.setAttribute("data-sb",b)"#,
        r#"w.innerWidth<1280?"collapsed":"expanded""#,
        r#"getItem("hfs-ws-rail")"#,
        r#"getItem("hfs-ws-view")"#,
        r#""--ws-split""#,
    ] {
        assert!(boot.contains(needle), "theme-boot.html lacks {needle}");
    }
}
