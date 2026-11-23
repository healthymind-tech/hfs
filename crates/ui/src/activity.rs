//! The topbar activity chip (console redesign §5.2): how many terminology
//! imports and sync runs are queued or running right now, with a pulsing
//! dot while anything is. `GET /ui/activity` answers the chip as a fragment;
//! the topbar loads it once and refreshes it every 30 s.

use askama::Template;
use axum::{Router, response::Response, routing::get};
use helios_endpoints::sync::SyncRunState;

use crate::i18n::{I18n, RequestLocale};
use crate::{WebState, render};

/// The route (merged into the console router).
pub(crate) fn routes() -> Router<WebState> {
    Router::new().route("/ui/activity", get(chip))
}

#[derive(Template)]
#[template(path = "partials/shell/activity-chip.html")]
struct Chip {
    i18n: I18n,
    imports: u32,
    syncs: u32,
    href: &'static str,
}

impl Chip {
    fn running(&self) -> u32 {
        self.imports + self.syncs
    }
}

async fn chip(locale: RequestLocale) -> Response {
    let imports = match crate::terminology_console::upstream() {
        Some(up) => helios_hts_ui::active_jobs(up).await.unwrap_or(0) as u32,
        None => 0,
    };
    let syncs = match helios_sync::global() {
        Some(s) => s.list().await.map_or(0, |jobs| {
            jobs.iter()
                .filter(|j| {
                    j.last_run
                        .as_ref()
                        .is_some_and(|r| matches!(r.state, SyncRunState::Running | SyncRunState::Queued))
                })
                .count() as u32
        }),
        None => 0,
    };
    let href = if imports > 0 || (syncs == 0 && crate::terminology_console::installed()) {
        "/ui/terminology/jobs"
    } else {
        "/ui/sync"
    };
    render(Chip {
        i18n: I18n::new(locale),
        imports,
        syncs,
        href,
    })
}
