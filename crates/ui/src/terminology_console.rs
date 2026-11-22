//! The Terminology section of the one console (unified-server design §7.1,
//! §9): the terminology console (`helios-hts-ui`) embedded at
//! `/ui/terminology`.
//!
//! * **One sign-in.** The section is a route of this router, so the console
//!   session gate (`login::require_session`) applies unchanged: the same
//!   `hfs_console` cookie, `/ui/login?next=…`, CSRF. The stand-alone HTS
//!   administrator sign-in is not involved.
//! * **One role table.** Every request is classified with
//!   [`helios_terminology::access::classify_console`] and compared with the
//!   requester's terminology role (max of the global role and a
//!   `terminology` binding); a refusal is the console's 403 panel inside
//!   the shell.
//! * **One shell.** Pages are rendered inside this crate's document shell
//!   ([`helios_ui_chrome::shell::HostFrame`]), so moving between FHIR and
//!   terminology pages keeps the sidebar.
//! * **Legacy URLs.** `/ui/hts/**` answers a permanent redirect
//!   ([`helios_terminology::routes::legacy_console_redirect`]).
//!
//! Until the embedded terminology runtime lands (U2) the pages talk to the
//! configured terminology server (`HFS_TERMINOLOGY_SERVER`) over HTTP, with
//! `HFS_TERMINOLOGY_SERVER_TOKEN` as the administrator credential of the
//! management pages; the browser admin API (`/ui/terminology/api/admin`) is
//! proxied to that server's `/admin`.

use std::sync::{Arc, OnceLock};

use askama::Template;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use helios_hts_ui::paths::{self, TerminologyPaths};
use helios_terminology::access::{TerminologyAction, classify_console, required_role};

use crate::{I18n, RequestLocale, RequestTenant, RequestVersion, Status, WebState, current_status};

/// What the server tells the console about its terminology service.
#[derive(Debug, Clone)]
pub struct TerminologyConsoleConfig {
    /// Base URL of the terminology server (`HFS_TERMINOLOGY_SERVER`).
    pub upstream_url: String,
    /// Administrator credential sent to its `/admin` API and imports
    /// (`HFS_TERMINOLOGY_SERVER_TOKEN`); without it the management pages
    /// are hidden.
    pub admin_token: Option<String>,
    /// FHIR release label shown on the terminology pages (`R4`, …).
    pub fhir_version: &'static str,
    /// Server version.
    pub version: &'static str,
}

struct Installed {
    router: Router,
    admin: bool,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// Installs the Terminology section. Call once at startup, inside the
/// runtime (the home page's request-rate sampler is spawned here), before
/// the UI is mounted. Returns `false` when it was already installed.
pub fn install(config: TerminologyConsoleConfig) -> bool {
    if INSTALLED.get().is_some() {
        return false;
    }
    paths::configure(TerminologyPaths::EMBEDDED);
    let upstream_url = config.upstream_url.trim_end_matches('/').to_string();
    let token = config
        .admin_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    let upstream = match helios_hts_ui::UpstreamClient::new(upstream_url.clone()) {
        Ok(client) => match token
            .as_deref()
            .and_then(|t| HeaderValue::from_str(&format!("Bearer {t}")).ok())
        {
            Some(value) => client.with_admin_authorization(value),
            None => client,
        },
        Err(err) => {
            tracing::error!(?err, "terminology console: upstream client failed to build");
            return false;
        }
    };
    let state = Arc::new(helios_hts_ui::HtsUiState {
        fhir_version: config.fhir_version,
        version: config.version,
        upstream,
        bundled_data_bytes: None,
        metrics_ring: Default::default(),
    });
    helios_hts_ui::spawn_metrics_sampler(&state);
    let admin = token.is_some();
    let proxy = token.map(|token| {
        let target = Arc::new(AdminProxy {
            base: format!("{upstream_url}/admin"),
            token,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .unwrap_or_default(),
        });
        Router::new().fallback(any(admin_proxy)).with_state(target)
    });
    let router = helios_hts_ui::embedded_router(state, proxy);
    helios_hts_ui::register_nav(admin);
    INSTALLED
        .set(Installed { router, admin })
        .is_ok()
}

/// Whether the section is installed.
pub(crate) fn installed() -> bool {
    INSTALLED.get().is_some()
}

/// The nav key component for the section (its entries depend on it).
pub(crate) fn nav_key_extra() -> &'static str {
    match INSTALLED.get() {
        Some(i) if i.admin => "tx+",
        Some(_) => "tx",
        None => "",
    }
}

/// The section's routes on the UI router, and the legacy redirects.
pub(crate) fn routes() -> Router<WebState> {
    let mut router = Router::new()
        .route("/ui/hts", any(legacy_redirect))
        .route("/ui/hts/", any(legacy_redirect))
        .route("/ui/hts/{*rest}", any(legacy_redirect));
    if installed() {
        router = router
            .route("/ui/terminology", any(section))
            .route("/ui/terminology/", any(section))
            .route("/ui/terminology/{*rest}", any(section));
    }
    router
}

/// `/ui/hts/**` → `/ui/terminology/**`: 301 for GET/HEAD, 308 otherwise.
async fn legacy_redirect(request: Request) -> Response {
    let pq = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_default();
    let Some(target) = helios_terminology::routes::legacy_console_redirect(&pq) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let status = if matches!(*request.method(), Method::GET | Method::HEAD) {
        StatusCode::MOVED_PERMANENTLY
    } else {
        StatusCode::PERMANENT_REDIRECT
    };
    let mut response = status.into_response();
    if let Ok(v) = HeaderValue::from_str(&target) {
        response.headers_mut().insert(header::LOCATION, v);
    }
    // A stale stand-alone session cookie is useless here; drop it.
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static("hts_admin=; Path=/ui/hts; Max-Age=0; HttpOnly; SameSite=Lax"),
    );
    response
}

/// The host shell around a terminology page: this crate's layout rendered
/// with the four slot markers.
#[derive(Template)]
#[template(path = "pages/shell-frame.html")]
pub(crate) struct ShellFrame {
    pub(crate) status: Status,
    pub(crate) i18n: I18n,
    pub(crate) active_page: &'static str,
}

impl ShellFrame {
    fn slot_title(&self) -> &'static str {
        helios_ui_chrome::shell::SLOT_TITLE
    }
    fn slot_body(&self) -> &'static str {
        helios_ui_chrome::shell::SLOT_BODY_CLASS
    }
    fn slot_main(&self) -> &'static str {
        helios_ui_chrome::shell::SLOT_CONTENT_CLASS
    }
    fn slot_content(&self) -> &'static str {
        helios_ui_chrome::shell::SLOT_CONTENT
    }
}

/// The 403 panel inside the shell (§9.4 item 10).
#[derive(Template)]
#[template(path = "pages/terminology-denied.html")]
struct DeniedPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    need: &'static str,
    have: &'static str,
}

/// The sidebar entry a terminology path belongs to.
pub(crate) fn active_page(path: &str) -> &'static str {
    let rest = path
        .strip_prefix(TerminologyPaths::EMBEDDED.ui)
        .unwrap_or("")
        .trim_start_matches('/');
    match rest.split('/').next().unwrap_or("") {
        "" => "tx-home",
        "code-systems" | "concepts" => "tx-code-systems",
        "value-sets" => "tx-value-sets",
        "concept-maps" => "tx-concept-maps",
        "manage" => "tx-manage",
        "import" => "tx-import",
        "jobs" => "tx-jobs",
        "capability-statement" => "tx-capability",
        _ => "",
    }
}

fn role_str(role: helios_endpoints::ConsoleRole) -> &'static str {
    role.as_str()
}

/// Every `/ui/terminology/**` request.
async fn section(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    request: Request,
) -> Response {
    let Some(installed) = INSTALLED.get() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = request.uri().path().to_string();
    let i18n = I18n::new(locale);
    let active = active_page(&path);
    let have = rt.nav.roles.terminology;
    // The browser admin API (uploads, job progress) is an import tool.
    let classified = if path.starts_with("/ui/terminology/api/") {
        Some(TerminologyAction::Import)
    } else {
        classify_console(request.method(), &path)
    };
    if let Some(action) = classified {
        let need = required_role(action);
        let admin_page = matches!(
            action,
            TerminologyAction::Import | TerminologyAction::Maintain | TerminologyAction::ViewOperations
        );
        if have < need || (admin_page && !installed.admin) {
            let page = DeniedPage {
                status: current_status(&state, rv.0, &rt),
                i18n,
                active_page: active,
                need: role_str(need),
                have: role_str(have),
            };
            let mut response = crate::render(page);
            *response.status_mut() = if admin_page && !installed.admin && have >= need {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::FORBIDDEN
            };
            return response;
        }
    }
    // The host shell, rendered once for this request.
    let frame = ShellFrame {
        status: current_status(&state, rv.0, &rt),
        i18n,
        active_page: active,
    };
    let frame = match frame.render() {
        Ok(html) => helios_ui_chrome::shell::HostFrame::from_rendered(&html),
        Err(err) => {
            tracing::error!(?err, "terminology console: shell frame failed to render");
            None
        }
    };
    let Some(frame) = frame else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "shell frame").into_response();
    };
    let (mut parts, body) = request.into_parts();
    let Some(internal) = paths::internal_path(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let pq = match parts.uri.query() {
        Some(q) => format!("{internal}?{q}"),
        None => internal,
    };
    parts.uri = match pq.parse() {
        Ok(uri) => uri,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let request = Request::from_parts(parts, body);
    let router = installed.router.clone();
    helios_ui_chrome::shell::scope_host_frame(Arc::new(frame), async move {
        match tower::ServiceExt::oneshot(router, request).await {
            Ok(response) => response,
            Err(never) => match never {},
        }
    })
    .await
}

struct AdminProxy {
    base: String,
    token: String,
    client: reqwest::Client,
}

/// `/ui/terminology/api/admin/**` → `{terminology server}/admin/**`, with
/// the administrator credential. The console guard and the role check of
/// [`section`] (Import / ViewOperations: admin) already ran.
async fn admin_proxy(State(proxy): State<Arc<AdminProxy>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let tail = parts.uri.path();
    let url = match parts.uri.query() {
        Some(q) => format!("{}{tail}?{q}", proxy.base),
        None => format!("{}{tail}", proxy.base),
    };
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);
    let mut outbound = proxy
        .client
        .request(method, &url)
        .bearer_auth(&proxy.token)
        .body(bytes);
    for name in [header::CONTENT_TYPE, header::ACCEPT, header::CONTENT_RANGE] {
        if let Some(v) = parts.headers.get(&name)
            && let Ok(v) = v.to_str()
        {
            outbound = outbound.header(name.as_str(), v);
        }
    }
    for name in ["upload-offset", "upload-length", "x-upload-name", "x-forwarded-for"] {
        if let Some(v) = parts.headers.get(name)
            && let Ok(v) = v.to_str()
        {
            outbound = outbound.header(name, v);
        }
    }
    match outbound.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body = resp.bytes().await.unwrap_or_default();
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = status;
            if let Some(ct) = content_type.and_then(|c| HeaderValue::from_str(&c).ok()) {
                response.headers_mut().insert(header::CONTENT_TYPE, ct);
            }
            response
        }
        Err(err) => {
            tracing::warn!(error = %err, "terminology admin proxy failed");
            (StatusCode::BAD_GATEWAY, "terminology server unreachable").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn redirect(method: Method, uri: &str) -> (StatusCode, String) {
        let app: Router = Router::new().route("/ui/hts/{*rest}", any(legacy_redirect)).route("/ui/hts", any(legacy_redirect));
        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder().method(method).uri(uri).body(Body::empty()).unwrap(),
        )
        .await
        .unwrap();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        (response.status(), location)
    }

    #[tokio::test]
    async fn legacy_console_urls_redirect_permanently_with_their_query() {
        assert_eq!(
            redirect(Method::GET, "/ui/hts").await,
            (StatusCode::MOVED_PERMANENTLY, "/ui/terminology".into())
        );
        assert_eq!(
            redirect(Method::GET, "/ui/hts/code-systems?sort=name").await,
            (StatusCode::MOVED_PERMANENTLY, "/ui/terminology/code-systems?sort=name".into())
        );
        assert_eq!(
            redirect(Method::GET, "/ui/hts/terminology?populated=true").await,
            (StatusCode::MOVED_PERMANENTLY, "/ui/terminology/manage?populated=true".into())
        );
        assert_eq!(
            redirect(Method::POST, "/ui/hts/import").await,
            (StatusCode::PERMANENT_REDIRECT, "/ui/terminology/import".into())
        );
    }

    #[test]
    fn active_page_follows_the_path() {
        assert_eq!(active_page("/ui/terminology"), "tx-home");
        assert_eq!(active_page("/ui/terminology/value-sets/x"), "tx-value-sets");
        assert_eq!(active_page("/ui/terminology/concepts"), "tx-code-systems");
        assert_eq!(active_page("/ui/terminology/manage/delete"), "tx-manage");
    }
}
