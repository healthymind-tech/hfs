//! "+N older": the other stored versions of one canonical, as table rows the
//! terminology browsers insert under its grouped row (console redesign,
//! version grouping). `GET /hts/{code-systems,value-sets,concept-maps}/versions?url=`.

use askama::Template;
use axum::{
    Router,
    extract::{Query, State},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::i18n::{I18n, RequestLocale};
use crate::upstream::VersionChoice;
use crate::{Chrome, HtsUiState};

pub(crate) fn routes() -> Router<Arc<HtsUiState>> {
    Router::new()
        .route("/hts/code-systems/versions", get(code_systems))
        .route("/hts/value-sets/versions", get(value_sets))
        .route("/hts/concept-maps/versions", get(concept_maps))
}

async fn code_systems(
    state: State<Arc<HtsUiState>>,
    locale: RequestLocale,
    query: Query<VersionsQuery>,
) -> Response {
    older_rows(state, locale, query, Kind::CodeSystem).await
}

async fn value_sets(
    state: State<Arc<HtsUiState>>,
    locale: RequestLocale,
    query: Query<VersionsQuery>,
) -> Response {
    older_rows(state, locale, query, Kind::ValueSet).await
}

async fn concept_maps(
    state: State<Arc<HtsUiState>>,
    locale: RequestLocale,
    query: Query<VersionsQuery>,
) -> Response {
    older_rows(state, locale, query, Kind::ConceptMap).await
}

/// Which browser the rows belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    CodeSystem,
    ValueSet,
    ConceptMap,
}

impl Kind {
    pub(crate) fn resource_type(self) -> &'static str {
        match self {
            Kind::CodeSystem => "CodeSystem",
            Kind::ValueSet => "ValueSet",
            Kind::ConceptMap => "ConceptMap",
        }
    }

    /// Route segment of the browser and detail pages.
    pub(crate) fn segment(self) -> &'static str {
        match self {
            Kind::CodeSystem => "code-systems",
            Kind::ValueSet => "value-sets",
            Kind::ConceptMap => "concept-maps",
        }
    }

    fn active_page(self) -> &'static str {
        self.segment()
    }

    /// The detail page's operation tab (the bare `{id}` route redirects
    /// there and would drop the query).
    fn tab(self) -> &'static str {
        match self {
            Kind::CodeSystem => "lookup",
            Kind::ValueSet => "expand",
            Kind::ConceptMap => "translate",
        }
    }

    /// Detail page of one stored version.
    pub(crate) fn detail_href(self, id: &str, version: &str) -> String {
        let mut href = format!(
            "{}/{}/{}/{}",
            crate::paths::ui(),
            self.segment(),
            enc(id),
            self.tab()
        );
        if !version.is_empty() {
            href.push_str("?version=");
            href.push_str(&enc(version));
        }
        href
    }

    /// The "+N older" rows of one canonical.
    pub(crate) fn older_href(self, url: &str) -> String {
        format!(
            "{}/{}/versions?url={}",
            crate::paths::ui(),
            self.segment(),
            enc(url)
        )
    }
}

fn enc(raw: &str) -> String {
    crate::upstream::urlencoding_component(raw)
}

/// `?version=` on a detail tab: which stored version the page shows
/// (absent: the one a version-less request resolves to).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct VersionQuery {
    #[serde(default)]
    version: Option<String>,
}

impl VersionQuery {
    pub(crate) fn get(&self) -> Option<&str> {
        self.version.as_deref().filter(|v| !v.is_empty())
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct VersionsQuery {
    #[serde(default)]
    url: String,
}

#[derive(Template)]
#[template(path = "partials/hts-browser-older.html")]
struct OlderRows<'a> {
    chrome: Chrome<'a>,
    kind: Kind,
    rows: Vec<VersionChoice>,
    failed: bool,
}

async fn older_rows(
    State(state): State<Arc<HtsUiState>>,
    locale: RequestLocale,
    Query(q): Query<VersionsQuery>,
    kind: Kind,
) -> Response {
    let chrome = Chrome {
        i18n: I18n::new(locale),
        active_page: kind.active_page(),
        fhir_version: state.fhir_version,
        version: state.version,
    };
    let (rows, failed) = if q.url.is_empty() {
        (Vec::new(), false)
    } else {
        match state
            .upstream
            .versions_of(kind.resource_type(), &q.url, true)
            .await
        {
            Ok(rows) => (rows, false),
            Err(_) => (Vec::new(), true),
        }
    };
    match (OlderRows {
        chrome,
        kind,
        rows,
        failed,
    })
    .render()
    {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            tracing::error!(?err, "hts-ui older-versions render failed");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Html(String::from("<pre>hts-ui: template render error</pre>")),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hrefs_encode_ids_versions_and_canonicals() {
        assert_eq!(
            Kind::CodeSystem.detail_href("icd9cm", "2015|x"),
            format!(
                "{}/code-systems/icd9cm/lookup?version=2015%7Cx",
                crate::paths::ui()
            )
        );
        assert_eq!(
            Kind::ValueSet.detail_href("v", ""),
            format!("{}/value-sets/v/expand", crate::paths::ui())
        );
        assert!(
            Kind::ConceptMap
                .older_href("http://x/cm?a=1")
                .ends_with("/concept-maps/versions?url=http%3A%2F%2Fx%2Fcm%3Fa%3D1")
        );
    }
}
