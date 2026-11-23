//! Server-rendered, HTMX-first administrative UI for the Helios Terminology
//! Server (HTS).
//!
//! This crate follows the same rules of the road as [`helios_ui`]: handlers
//! parse the request, gather data over the HTS HTTP surface, and render an
//! [`askama`] template. All markup lives in `templates/`; static assets
//! (htmx, CSS, JS) are embedded at compile time via [`rust_embed`] and served
//! by [`axum_embed`], so there is no runtime CDN dependency (#551 D6).
//!
//! Handlers branch on the `HX-Request` header — read through the infallible
//! [`axum_htmx::HxRequest`] extractor — to return an HTML *fragment* for
//! htmx-driven swaps and a *full page* for hard navigations, so the UI
//! degrades to working full-page loads without JavaScript.
//!
//! # Mounting
//!
//! [`router`] returns an [`axum::Router`] meant to be mounted at `/ui` in the
//! HTS binary. The router uses the `/hts` prefix internally, so the resulting
//! URL space is `/ui/hts`, `/ui/hts/code-systems`,
//! `/ui/hts/code-systems/{id}`, `/ui/hts/value-sets`, `/ui/hts/concept-maps`,
//! `/ui/hts/import` and `/ui/hts/capability-statement` — one page tree under
//! the same `/ui` mount point HFS uses.
//!
//! # Upstream contract
//!
//! HTS-UI ships inside the `hts` binary but the handlers still speak HTTP to
//! the HTS REST surface. That keeps the UI honest (every card and cell is
//! what a browser sees), and makes the `HTS_UI_UPSTREAM_URL` override — the
//! canonical degraded-state trigger per design doc §7 — useful without a
//! rebuild. When neither is set the mount site derives a loopback URL from
//! the binary's own host:port. `HFS_TERMINOLOGY_SERVER` is HFS-side and has
//! no meaning here.
//!
//! # Shared chrome and assets
//!
//! The `Assets` embed points at `../ui/assets`: HTS-UI and HFS-UI share CSS
//! and vendored htmx from one place (see design doc §9.2). Extracting those
//! bytes behind a crate boundary is gated on #543.
//!
//! Shared *markup* already lives in [`helios_ui_chrome`], which #799
//! introduced for the topbar account menu — see [`Chrome::user_menu`]. The
//! crate owns templates only, not assets, so the two concerns move on their
//! own schedules.

mod admin;
// Not referenced yet, but linked on purpose: the crate's `R4`/`R4B`/`R5`/`R6`
// features select the FHIR version through `helios-fhir` so it matches the
// hts binary (see Cargo.toml).
use helios_fhir as _;

mod capability;
mod chart;
mod code_systems;
mod concept_maps;
mod concepts;
mod explore;
mod home;
mod i18n;
mod import;
pub mod in_process;
mod login;
mod metrics_parse;
mod metrics_ring;
/// Console paths (`/ui/hts` stand-alone, `/ui/terminology` embedded).
pub mod paths;
mod raw_fold;
mod raw_json_fragment;
mod upstream;
mod value_sets;

use axum::{
    Router,
    response::{Html, IntoResponse, Response},
};
use axum_embed::ServeEmbed;
use axum_htmx::AutoVaryLayer;
use rust_embed::RustEmbed;
use std::sync::Arc;

// `I18n` is re-exported for `tests/chrome_parity.rs`, which rebuilds the shared
// account menu from the real Fluent catalogs and asserts the page contains it
// (#799). `crates/ui` exports it for the same reason.
pub use i18n::{I18n, RequestLocale, negotiate_locale};
/// The administrator session cookie (`hts_admin`, `Path=/ui/hts`) the `hts`
/// binary configures its [`helios_admin_auth::AdminGate`] with.
pub use login::COOKIE as ADMIN_COOKIE;
// Home request-rate chart (§7.1): the sample ring lives on `HtsUiState`, so
// its type must be nameable by the `hts` binary and by the test rings.
// `MetricsSample`/`StatusCounts` come with it — `MetricsRing::push` takes a
// sample, so without them the public ring would have no callable surface.
pub use home::spawn_metrics_sampler;
pub use metrics_parse::StatusCounts;
pub use metrics_ring::{MetricsRing, MetricsSample};
pub use upstream::{
    ClosureConcept, ClosureEdge, ClosureParams, ClosureResult, CmBrowserFilters, CmBrowserPage,
    CmBrowserRow, CodeSystemSummary, ConceptMapSummary, CsBrowserFilters, CsBrowserPage,
    CsBrowserRow, ExpandParams, ExpansionConcept, ExpansionDesignation, ExpansionResult,
    HTS_UI_BATCH_FANOUT_CONCURRENCY, HTS_UI_MAX_EXPANSION_SIZE_HINT, ImportCounts, ImportResult,
    ImportStatus, LookupDesignation, LookupParams, LookupProperty, LookupResult, MappingKind,
    OutcomeView, SubsumesParams, SubsumesResult, TranslateDirection, TranslateMatch,
    TranslateParams, TranslateResult, UpstreamCapabilitiesCodeSystem, UpstreamClient,
    UpstreamError, UpstreamHealth, UpstreamTerminologyCapabilities, ValidateCodeParams,
    ValidateCodeResult, ValidateInputMode, ValueSetSummary, VsBrowserFilters, VsBrowserPage,
    VsBrowserRow, VsValidateMode, VsValidateParams, VsValidateResult, VsValidateSource,
};
// Capability & Conformance page projections. Appended below Slice F's
// block to avoid touching the alphabetized list.
pub use upstream::{CapabilityView, TerminologyCapabilitiesView};
// Slice H additions (the concept information plane, Direction B). Appended
// below rather than folded into the alphabetized list above so the two
// concurrent slices do not collide on the same lines.
pub use upstream::{
    ComparatorKind, ConceptIdentity, ConceptMappingGroup, ConceptMappingRow, ConceptMappings,
    ConceptRef, ConceptRelative, MappingDirection, SubsumptionReport, SubsumptionRow,
};

/// Static UI assets (htmx, CSS, JS) embedded into the binary at compile time.
///
/// Points at the sibling `crates/ui/assets` directory: the two products share
/// bytes rather than duplicating them (see design doc §9.2), and the topbar
/// markup that consumes those bytes is now itself shared — see
/// [`Chrome::user_menu`].
///
/// The `helios-ui-chrome` crate that #799 introduced owns **markup only**. The
/// reach across into `../ui/assets` (via the `ui-assets` symlink, so the
/// published tarball carries the bytes) therefore stays exactly as it is: moving
/// the CSS, the vendored htmx and the JS behind a crate boundary is gated on
/// #543, which has to settle asset *ownership* first. Until then this embed is
/// the single source of those bytes for both binaries, and nothing here is
/// waiting on a later phase.
#[derive(Clone, RustEmbed)]
// `build.rs` sets the cfg when the `ui-assets` symlink did not materialize as
// a directory (Windows checkout without `core.symlinks`, #1257).
#[cfg_attr(not(helios_workspace_ui_assets), folder = "ui-assets")]
#[cfg_attr(helios_workspace_ui_assets, folder = "../ui/assets")]
struct Assets;

/// Shared router state: values that are constant for the process lifetime.
///
/// Cheap to `Arc<HtsUiState>::clone`.
#[derive(Clone)]
pub struct HtsUiState {
    /// The FHIR version the `hts` binary was built for (`R4`, `R4B`, `R5`,
    /// or `R6`). Rendered in the sidebar as a metadata chip — compile-time
    /// constant, not an interactive selector (design doc §7.1 HTS binary
    /// chrome contract).
    pub fhir_version: &'static str,

    /// The `hts` binary version string, wired to `env!("CARGO_PKG_VERSION")`
    /// at the mount site and shown next to the product name.
    pub version: &'static str,

    /// Upstream HTS HTTP client. Base URL comes from `HTS_UI_UPSTREAM_URL`
    /// when set, otherwise loopback to the same binary (design doc §7
    /// degraded state contract).
    pub upstream: UpstreamClient,

    /// Total on-disk size of the configured `HTS_BOOTSTRAP_DIR` in bytes.
    /// `None` when no bootstrap directory was set — the dashboard tile then
    /// renders an em-dash rather than a misleading zero.
    pub bundled_data_bytes: Option<u64>,

    /// Rolling `/metrics` samples backing the Home request-rate chart
    /// (§7.1). Fed from the `/metrics` leg the Home cards fetch already
    /// performs, so the chart adds no upstream traffic of its own.
    ///
    /// Deliberately a *state field* rather than a module-level `static`: a
    /// process-global would be shared by every `#[tokio::test]` in this
    /// crate, so one test's samples would leak into another's assertions.
    /// `Arc` because `HtsUiState` is `Clone` and the ring must not be.
    ///
    /// Use [`Default::default()`] at construction sites that do not care
    /// about the chart.
    pub metrics_ring: Arc<metrics_ring::MetricsRing>,
}

/// The console embedded in a host console (HFS, unified-server design §7.1,
/// §9.2): the host mounts it at `/ui/terminology` (mapping the paths onto
/// these `/hts/...` routes with [`paths::internal_path`]), guards it with its
/// own console sign-in and role table, serves the assets from its own
/// `/ui/assets`, and renders every page inside its shell
/// ([`helios_ui_chrome::shell::scope_host_frame`]). No administrator
/// sign-in, no `hts_admin` cookie, no product switcher.
///
/// `admin` is the browser admin API (`/hts/api/admin/...`), already guarded
/// by the host. Call [`paths::configure`] with
/// [`paths::TerminologyPaths::EMBEDDED`] before rendering any page.
pub fn embedded_router(state: Arc<HtsUiState>, admin: Option<Router>) -> Router {
    let base = match admin {
        Some(admin) => Router::new().nest_service("/hts/api/admin", admin),
        None => Router::new(),
    };
    base.merge(home::routes())
        .merge(code_systems::routes())
        .merge(value_sets::routes())
        .merge(concept_maps::routes())
        .merge(concepts::routes())
        .merge(explore::routes())
        .merge(import::routes())
        .merge(admin::routes())
        .merge(capability::routes())
        .merge(raw_json_fragment::concept_fragment_routes())
        .merge(raw_json_fragment::workbench_fragment_routes())
        .fallback(not_found_page)
        .with_state(state)
        // No `AutoVaryLayer` here: the host router already has one, and the
        // layer refuses to be stacked twice on one request.
        .layer(axum::middleware::from_fn(i18n::negotiate_locale))
}

/// The Terminology section's sidebar entries (design §9.1), registered in
/// the shared navigation registry when the console is embedded. `admin`:
/// whether the management pages exist (the upstream admin API is reachable).
pub fn register_nav(admin: bool) {
    use helios_ui_chrome::nav::{NavItem, NavRole, NavScope, NavSection, register};
    let p = paths::get();
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let item = |id: &'static str, label: &'static str, href: String, icon: &'static str, order| {
        let href = leak(href);
        NavItem::new(NavSection::Terminology, id, label, href)
            .icon(icon)
            .order(order)
            .scope(NavScope::Terminology)
    };
    // The console redesign's sidebar (§5.1): Overview, Browse (route tabs:
    // code systems, value sets, concept maps, capability) and Loaded &
    // imports (loaded terminologies, import, jobs).
    const BROWSE: (&str, &str, &str) = ("tx-browse", "hts-nav-group-browse", "bookopen");
    const LOADED: (&str, &str, &str) = ("tx-loaded", "hts-nav-group-loaded", "package");
    let item =
        |id: &'static str, label: &'static str, href: String, icon: &'static str, order: u16| {
            let it: NavItem = item(id, label, href, icon, order);
            match id {
                "tx-home" => it.group("tx-home", "hts-nav-overview", "book"),
                "tx-code-systems" | "tx-value-sets" | "tx-concept-maps" | "tx-capability" => {
                    it.group(BROWSE.0, BROWSE.1, BROWSE.2)
                }
                "tx-manage" | "tx-import" | "tx-jobs" => it.group(LOADED.0, LOADED.1, LOADED.2),
                _ => it,
            }
        };
    register(item("tx-home", "hts-nav-home", p.ui.to_string(), "home", 0));
    register(item(
        "tx-code-systems",
        "hts-nav-code-systems",
        format!("{}/code-systems", p.ui),
        "book",
        10,
    ));
    register(item(
        "tx-value-sets",
        "hts-nav-value-sets",
        format!("{}/value-sets", p.ui),
        "bookmark",
        20,
    ));
    register(item(
        "tx-concept-maps",
        "hts-nav-concept-maps",
        format!("{}/concept-maps", p.ui),
        "arrows-exchange",
        30,
    ));
    register(
        item(
            "tx-capability",
            "nav-capability-conformance",
            format!("{}/capability-statement", p.ui),
            "shield",
            70,
        )
        .min_role(NavRole::Viewer),
    );
    if admin {
        register(
            item(
                "tx-manage",
                "hts-nav-terminology",
                p.manage.to_string(),
                "sliders",
                40,
            )
            .min_role(NavRole::Admin),
        );
        register(
            item(
                "tx-import",
                "hts-nav-import",
                format!("{}/import", p.ui),
                "import",
                50,
            )
            .min_role(NavRole::Admin)
            .full_load(),
        );
        register(
            item(
                "tx-jobs",
                "hts-nav-jobs",
                format!("{}/jobs", p.ui),
                "sync",
                60,
            )
            .min_role(NavRole::Admin),
        );
    }
}

/// Build the HTS UI router.
///
/// Mount this at `/ui` in the HTS binary so the routes below become
/// `/ui/hts`, `/ui/hts/assets/*`, etc. Without an admin API there is nothing
/// to sign in to, so this mounts no administrator guard.
pub fn router(state: Arc<HtsUiState>) -> Router {
    router_with_admin(state, None, helios_admin_auth::AdminGate::Open)
}

/// [`router`] plus the HTS admin API nested at `/hts/api/admin`, so the
/// console's browser code (chunked upload, live job progress) reaches it on
/// the console's own origin and path prefix — `/ui/hts/api/admin/...` — which
/// any reverse proxy forwarding `/ui/hts` already routes to this binary.
/// `admin` is `None` when `HTS_ADMIN_UI` is off.
///
/// `gate` is the administrator sign-in (`HTS_ADMIN_PASSWORD`): it guards the
/// management pages (`/hts/terminology`, `/hts/jobs`, `/hts/import`) and
/// backs `/hts/login`. `admin` must already be wrapped in the API guard for
/// the same gate — this function does not guard it — so `/hts/api/admin`
/// and `/admin` refuse alike.
pub fn router_with_admin(
    state: Arc<HtsUiState>,
    admin: Option<Router>,
    gate: helios_admin_auth::AdminGate,
) -> Router {
    // HTS_CONSOLE_REQUIRE_LOGIN (P3): never open when sign-in is required.
    let gate = login::effective_gate(gate);
    let base = match admin {
        Some(admin) => Router::new().nest_service("/hts/api/admin", admin),
        None => Router::new(),
    };
    base.merge(home::routes())
        .merge(code_systems::routes())
        .merge(value_sets::routes())
        .merge(concept_maps::routes())
        .merge(concepts::routes())
        .merge(explore::routes())
        .merge(import::routes())
        .merge(admin::routes())
        .merge(login::routes())
        .merge(capability::routes())
        .merge(raw_json_fragment::concept_fragment_routes())
        .merge(raw_json_fragment::workbench_fragment_routes())
        .nest_service("/hts/assets", ServeEmbed::<Assets>::new())
        // Anything else under the mount is a page that does not exist: answer
        // with the shell-wrapped not-found page rather than axum's empty 404,
        // which a browser shows as a blank tab with no title or language.
        .fallback(not_found_page)
        .with_state(state)
        // Inner to the guard: exposes the signed-in administrator it stamped
        // to page rendering (the account menu's Sign out).
        .layer(axum::middleware::from_fn(login::scope_signed_in))
        // Whether an HFS console shares this origin, for the topbar product
        // switcher (UI/UX review §4.2).
        .layer(axum::middleware::from_fn(scope_hfs_console))
        .layer(axum::middleware::from_fn_with_state(
            helios_admin_auth::Guard {
                gate: gate.clone(),
                protects: login::protects,
                style: helios_admin_auth::Style::Browser {
                    login_path: login::LOGIN_PATH,
                },
            },
            helios_admin_auth::guard,
        ))
        .layer(axum::Extension(gate))
        .layer(axum::middleware::from_fn(i18n::negotiate_locale))
        .layer(AutoVaryLayer)
}

tokio::task_local! {
    static HFS_CONSOLE: bool;
}

/// Middleware: remember, for page rendering, whether the browser carries the
/// cookie an HFS console sets on every page it serves
/// ([`helios_ui_chrome::HFS_CONSOLE_COOKIE`]). Only an HFS console on this
/// same origin can have set it, so only then does the product switcher link
/// to it — a stand-alone HTS shows no dead "FHIR Server" entry.
async fn scope_hfs_console(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let known = helios_ui_chrome::has_hfs_console_cookie(
        request
            .headers()
            .get_all(axum::http::header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok()),
    );
    HFS_CONSOLE.scope(known, next.run(request)).await
}

/// Whether an HFS console shares this origin (see [`scope_hfs_console`]).
fn hfs_console_known() -> bool {
    HFS_CONSOLE.try_with(|known| *known).unwrap_or(false)
}

/// Environment variable naming the FHIR console the topbar switcher links
/// to (`/ui` when HFS shares the origin, or an absolute URL).
pub const HFS_CONSOLE_URL_ENV: &str = "HTS_UI_HFS_CONSOLE_URL";

/// The configured FHIR console URL, read once.
fn configured_hfs_console() -> Option<&'static str> {
    static URL: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    URL.get_or_init(|| hfs_console_from(std::env::var(HFS_CONSOLE_URL_ENV).ok()))
        .as_deref()
}

/// Normalise the configured value: blank means unset.
fn hfs_console_from(raw: Option<String>) -> Option<String> {
    raw.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Where the switcher's "FHIR Server" entry points, if anywhere.
///
/// The explicit setting wins. Without it, the `helios_hfs_console` cookie
/// is the only evidence of a same-origin HFS — and it is only set once the
/// browser has loaded an HFS page, so an operator who went straight to
/// `/ui/hts/login` got a topbar with no way back (UIF defect 4).
fn hfs_console_href(configured: Option<&'static str>, cookie: bool) -> Option<&'static str> {
    configured.or(cookie.then_some(helios_ui_chrome::HFS_CONSOLE_PATH))
}

/// Up to two initials for the account avatar — the FHIR console's rule
/// (`helios_ui_chrome::avatar_initials`).
fn initials(name: &str) -> String {
    helios_ui_chrome::avatar_initials(name)
}

/// The generic "not found" page, `404` inside the full chrome.
#[derive(askama::Template)]
#[template(path = "pages/not-found.html")]
struct NotFoundPage<'a> {
    chrome: Chrome<'a>,
}

async fn not_found_page(
    axum::extract::State(state): axum::extract::State<Arc<HtsUiState>>,
    locale: RequestLocale,
) -> Response {
    let page = NotFoundPage {
        chrome: Chrome {
            i18n: I18n::new(locale),
            active_page: "",
            fhir_version: state.fhir_version,
            version: state.version,
        },
    };
    let mut response = render_page(askama::Template::render(&page));
    if response.status() == axum::http::StatusCode::OK {
        *response.status_mut() = axum::http::StatusCode::NOT_FOUND;
    }
    response
}

// ── Page context ────────────────────────────────────────────────────────────

/// Values every HTS-UI page needs for the sidebar/topbar chrome.
///
/// Kept as a plain struct (not a template macro) so each page template can
/// embed it via `{% include %}` or destructure fields, and so tests can
/// build one without going through a full request.
#[derive(Clone, Copy)]
pub(crate) struct Chrome<'a> {
    pub i18n: I18n,
    pub active_page: &'a str,
    pub fhir_version: &'a str,
    pub version: &'a str,
}

impl Chrome<'_> {
    /// The host console's shell, when this page is rendered embedded.
    pub(crate) fn host(&self) -> Option<Arc<helios_ui_chrome::shell::HostFrame>> {
        helios_ui_chrome::shell::host_frame()
    }

    /// Render the topbar account menu from the shared chrome (#799).
    ///
    /// Deliberately a method rather than a `Chrome` field, which leaves all
    /// the HTS page structs untouched: the only identity HTS has is the
    /// administrator sign-in (`HTS_ADMIN_PASSWORD`, [`login`]), and the
    /// request's signed-in administrator reaches rendering through a
    /// task-local the sign-in middleware scopes. Signed out, this is
    /// [`helios_ui_chrome::UserIdentity::default`] — every field `None`,
    /// `can_logout: false` — the same shape HFS renders without a session.
    pub(crate) fn user_menu(&self) -> Result<String, askama::Error> {
        // Signed in as the administrator (`HTS_ADMIN_PASSWORD`): name the
        // account and offer Sign out. Otherwise the signed-out shape.
        let user = login::signed_in();
        let hint = self.i18n.t("admin-user-hint");
        let avatar = user.as_deref().map(initials).unwrap_or_default();
        let identity = match user.as_deref() {
            Some(name) => helios_ui_chrome::UserIdentity {
                display: Some(name),
                secondary: Some(hint.as_str()),
                initials: (!avatar.is_empty()).then_some(avatar.as_str()),
                can_logout: true,
                logout_href: login::LOGOUT_PATH,
                ..Default::default()
            },
            None => helios_ui_chrome::UserIdentity::default(),
        };
        helios_ui_chrome::user_menu(&self.i18n, identity)
    }

    /// The topbar theme switcher, shared with HFS (`helios-ui-chrome`).
    pub(crate) fn theme_toggle(&self) -> Result<String, askama::Error> {
        helios_ui_chrome::theme_toggle(&self.i18n)
    }

    /// The topbar product switcher, shared with HFS (UI/UX review §4.2):
    /// rendered only when an HFS console on this origin has announced itself.
    pub(crate) fn product_switcher(&self) -> Result<String, askama::Error> {
        helios_ui_chrome::product_switcher(
            &self.i18n,
            helios_ui_chrome::ProductLinks {
                current: helios_ui_chrome::Product::Terminology,
                fhir: hfs_console_href(configured_hfs_console(), hfs_console_known()),
                terminology: Some(helios_ui_chrome::HTS_CONSOLE_PATH),
            },
        )
    }

    /// The inline first-paint theme stamp for `<head>` (shared with HFS).
    pub(crate) fn theme_boot(&self) -> Result<String, askama::Error> {
        helios_ui_chrome::theme_boot()
    }

    fn brand_links<'b>(&self, version: &'b str) -> helios_ui_chrome::BrandLinks<'b> {
        helios_ui_chrome::BrandLinks {
            home: helios_ui_chrome::HTS_CONSOLE_PATH,
            assets: paths::assets(),
            product_line: "Terminology Server",
            version,
            home_key: "hts-nav-home",
        }
    }

    /// The HealthyMind sidebar brand, shared with HFS (design §10.2).
    pub(crate) fn brand(&self) -> Result<String, askama::Error> {
        let version = format!("hts v{}", self.version);
        helios_ui_chrome::brand(&self.i18n, self.brand_links(&version))
    }

    /// The same brand link for the phone / tablet topbar, shared with HFS.
    pub(crate) fn topbar_brand(&self) -> Result<String, askama::Error> {
        let version = format!("hts v{}", self.version);
        helios_ui_chrome::topbar_brand(&self.i18n, self.brand_links(&version))
    }

    /// Favicon / app-icon links for `<head>`, shared with HFS.
    pub(crate) fn brand_head(&self) -> Result<String, askama::Error> {
        helios_ui_chrome::brand_head(paths::assets())
    }

    /// The attribution footer, shared with HFS; links HFS's About page when
    /// an HFS console shares this origin.
    pub(crate) fn site_footer(&self) -> Result<String, askama::Error> {
        let version = format!("hts v{}", self.version);
        let about = if hfs_console_known() { "/ui/about" } else { "" };
        helios_ui_chrome::site_footer(&self.i18n, self.brand_links(&version), about)
    }
}

/// Askama render helper for full-page and fragment responses that already
/// carry the entire body content. Wraps the result in [`axum::response::Html`]
/// with a diagnostic 500 fallback so template failures fail loudly instead of
/// serving blank pages.
pub(crate) fn render_page(rendered: Result<String, askama::Error>) -> Response {
    match rendered {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            tracing::error!(?err, "hts-ui template render failed");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Html(String::from("<pre>hts-ui: template render error</pre>")),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
fn test_state() -> Arc<HtsUiState> {
    Arc::new(HtsUiState {
        fhir_version: "R4",
        version: "0.0.0-test",
        upstream: UpstreamClient::new("http://127.0.0.1:1").expect("test client"),
        bundled_data_bytes: None,
        metrics_ring: Default::default(),
    })
}

/// Cheap constructor for local mount tests: an [`HtsUiState`] pointed at a
/// closed loopback port so upstream calls fail deterministically.
#[cfg(test)]
mod topbar_tests {
    use super::*;

    #[test]
    fn switcher_links_to_the_configured_console_without_a_cookie() {
        assert_eq!(hfs_console_href(Some("/ui"), false), Some("/ui"));
        assert_eq!(
            hfs_console_href(Some("https://fhir.example.org/ui"), true),
            Some("https://fhir.example.org/ui")
        );
        assert_eq!(hfs_console_href(None, true), Some("/ui"));
        assert_eq!(hfs_console_href(None, false), None);
        assert_eq!(hfs_console_from(Some("  ".into())), None);
        assert_eq!(
            hfs_console_from(Some(" /ui ".into())).as_deref(),
            Some("/ui")
        );
    }

    #[test]
    fn avatar_initials_match_the_fhir_console() {
        assert_eq!(initials("admin"), "A");
        assert_eq!(initials("console-admin"), "CA");
        assert_eq!(initials("Jane Q Doe"), "JQ");
        assert_eq!(initials("Sandbox (demo)"), "SD");
        assert_eq!(initials(""), "");
    }
}
