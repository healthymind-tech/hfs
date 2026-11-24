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

/// The last block of app.css whose header comment names `marker`.
fn block(marker: &str) -> &'static str {
    let at = CSS
        .rfind(marker)
        .unwrap_or_else(|| panic!("no {marker} block in app.css"));
    &CSS[at..]
}

#[test]
fn master_detail_shows_no_detail_column_until_selected() {
    // hm/md-detail: with nothing selected the list spans the detail track
    // (no empty placeholder column); the open detail clips while its track
    // animates; the list's columns follow its own width, never a scrollbar.
    let css = block("Master-detail: detail column only when selected (hm/md-detail)");
    assert!(css.contains(
        ".filter-layout[data-md]:not([data-md-open]) > [data-md-list] {\n    grid-column: 2 / -1;"
    ));
    // Only side by side: the stacked layout (<= 1250px) keeps one column.
    let span = css.find("grid-column: 2 / -1").unwrap();
    assert!(css[..span].contains("@media (min-width: 1251px)"));
    assert!(css.contains(".filter-layout[data-md].md-resizing {\n  overflow: clip;"));
    assert!(
        css.contains("[data-md] > [data-md-list] > .card {\n  container: md-list / inline-size;")
    );
    assert!(css.contains("width: var(--md-list-w, auto);"));
    for (width, col) in [
        ("720px", ".md-col-expr"),
        ("560px", ".md-col-base"),
        ("400px", ".md-col-type"),
    ] {
        let q =
            format!("@container md-list (max-width: {width}) {{\n  {col} {{\n    display: none;");
        assert!(css.contains(&q), "{q}");
    }
}

#[test]
fn master_detail_motion_goes_through_the_shared_helper() {
    let js = include_str!("../assets/shell.js");
    // The open/close animate the grid tracks through `HM.motion` (reduced
    // motion → no animation), and the panel slides with `motion.enter` /
    // `motion.leave`.
    assert!(js.contains("tracks: function (el, from, to, o)"));
    assert!(js.contains("if (!el || REDUCED || typeof el.animate !== \"function\") return null;"));
    assert!(js.contains("function mdOpen(md, ndet)"));
    assert!(js.contains("function mdClose(md, det)"));
    assert!(js.contains("motion.tracks(md, from, t,"));
    // Esc and a second click on the selected row close through the × link.
    assert!(js.contains("e.key !== \"Escape\""));
    assert!(js.contains("A second click on the selected row closes the detail."));
}
