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
