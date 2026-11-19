//! The shared theme switcher and its first-paint boot script.

use helios_ui_chrome::{ChromeLabels, LEGACY_THEME_KEY, THEME_COOKIE, theme_boot, theme_toggle};

struct Stub;

impl ChromeLabels for Stub {
    fn lang(&self) -> String {
        "en".to_string()
    }

    fn t(&self, key: &str) -> String {
        format!("[{key}]<&>")
    }
}

#[test]
fn toggle_is_one_button_with_all_three_mode_labels_and_icons() {
    let html = theme_toggle(&Stub).expect("renders");
    assert_eq!(html.matches("<button").count(), 1, "one button, not three");
    assert!(html.contains(r#"type="button""#));
    assert!(html.contains("data-theme-cycle"));
    // Default label is the default mode, so a page whose script has not run
    // yet still announces something true for a first visit.
    assert!(
        html.contains(r#"aria-label="[theme-toggle-system]&#60;&#38;&#62;""#)
            || html.contains(r#"aria-label="[theme-toggle-system]&lt;&amp;&gt;""#),
        "label is escaped: {html}"
    );
    for mode in ["system", "light", "dark"] {
        assert!(
            html.contains(&format!("data-label-{mode}=")),
            "{mode} label"
        );
        assert!(
            html.contains(&format!("theme-toggle__icon--{mode}")),
            "{mode} icon"
        );
    }
    assert_eq!(html.matches("<svg").count(), 3);
    assert!(html.starts_with("        <button"), "column-8 indentation");
    assert!(!html.ends_with('\n'), "the caller supplies the newline");
}

#[test]
fn boot_script_reads_the_shared_cookie_then_the_legacy_key() {
    let html = theme_boot().expect("renders");
    assert_eq!(THEME_COOKIE, "hfs_theme");
    assert!(html.starts_with("<script>") && html.ends_with("</script>"));
    assert!(html.contains(&format!("{THEME_COOKIE}=(system|light|dark)")));
    assert!(html.contains(&format!(r#"localStorage.getItem("{LEGACY_THEME_KEY}")"#)));
    assert!(html.contains("prefers-color-scheme: dark"));
    assert!(html.contains(r#"p=p||"system""#), "system is the default");
    assert!(html.contains("data-theme-pref") && html.contains(r#""data-theme""#));
    assert!(
        !html.contains("&quot;") && !html.contains("&#34;"),
        "not HTML-escaped"
    );
}
