//! The one console shell (unified-server design §9.3, §9.4).
//!
//! The FHIR pages (`helios-ui`) and the terminology pages (`helios-hts-ui`,
//! embedded at `/ui/terminology`) must share the *same* document shell —
//! head, scripts, sidebar, topbar, footer — byte for byte, so that an
//! in-place navigation from one section to the other keeps the sidebar DOM
//! and nothing in the head differs from what the first page loaded.
//!
//! The host (`helios-ui`) renders its shell once per request around four
//! slot markers and hands the result to the embedded console as a
//! [`HostFrame`] through a task-local ([`scope_host_frame`]). The embedded
//! console's layout writes the frame's segments around its own blocks
//! (`title`, `body_class`, `content_class`, `content`) instead of its
//! stand-alone chrome.

use std::future::Future;
use std::sync::Arc;

/// Marker the host renders in place of the page title.
pub const SLOT_TITLE: &str = "\u{1}HM-SLOT-TITLE\u{1}";
/// Marker the host renders in place of the `<body>` class.
pub const SLOT_BODY_CLASS: &str = "\u{1}HM-SLOT-BODY\u{1}";
/// Marker the host renders in place of the `<main>` extra class.
pub const SLOT_CONTENT_CLASS: &str = "\u{1}HM-SLOT-MAIN\u{1}";
/// Marker the host renders in place of the page content.
pub const SLOT_CONTENT: &str = "\u{1}HM-SLOT-CONTENT\u{1}";

/// The host's document shell cut at the four slots: `segments[0]` up to the
/// title, `[1]` between title and body class, `[2]` between body class and
/// main class, `[3]` between main class and content, `[4]` after content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFrame {
    segments: [String; 5],
}

impl HostFrame {
    /// Cuts a rendered shell at the slot markers; `None` when a marker is
    /// missing or out of order.
    pub fn from_rendered(html: &str) -> Option<Self> {
        let mut rest = html;
        let mut segments: Vec<String> = Vec::with_capacity(5);
        for marker in [SLOT_TITLE, SLOT_BODY_CLASS, SLOT_CONTENT_CLASS, SLOT_CONTENT] {
            let (head, tail) = rest.split_once(marker)?;
            segments.push(head.to_string());
            rest = tail;
        }
        segments.push(rest.to_string());
        Some(Self {
            segments: segments.try_into().ok()?,
        })
    }

    /// Segment `i` (0–4), pre-escaped HTML.
    pub fn seg(&self, i: usize) -> &str {
        self.segments.get(i).map(String::as_str).unwrap_or("")
    }
}

tokio::task_local! {
    static HOST_FRAME: Arc<HostFrame>;
}

/// Runs `fut` with `frame` as the host shell of every page it renders.
pub async fn scope_host_frame<F: Future>(frame: Arc<HostFrame>, fut: F) -> F::Output {
    HOST_FRAME.scope(frame, fut).await
}

/// The host shell in scope, if the page is rendered inside one.
pub fn host_frame() -> Option<Arc<HostFrame>> {
    HOST_FRAME.try_with(Arc::clone).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_cut_at_the_four_slots() {
        let html = format!(
            "<html><title>{SLOT_TITLE}</title><body class=\"{SLOT_BODY_CLASS}\"><main class=\"c{SLOT_CONTENT_CLASS}\">{SLOT_CONTENT}<footer></footer></main></html>"
        );
        let f = HostFrame::from_rendered(&html).unwrap();
        assert_eq!(f.seg(0), "<html><title>");
        assert_eq!(f.seg(1), "</title><body class=\"");
        assert_eq!(f.seg(3), "\">");
        assert_eq!(f.seg(4), "<footer></footer></main></html>");
        assert!(HostFrame::from_rendered("<html>").is_none());
    }

    #[tokio::test]
    async fn the_frame_is_visible_only_inside_its_scope() {
        assert!(host_frame().is_none());
        let html = format!("{SLOT_TITLE}{SLOT_BODY_CLASS}{SLOT_CONTENT_CLASS}{SLOT_CONTENT}");
        let frame = Arc::new(HostFrame::from_rendered(&html).unwrap());
        let seen = scope_host_frame(frame, async { host_frame().is_some() }).await;
        assert!(seen);
    }
}
