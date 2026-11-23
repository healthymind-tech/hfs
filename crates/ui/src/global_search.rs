//! The console's global search (topbar, `/` or Ctrl/⌘+K), modelled on
//! macOS System Settings search: every hit says *where* it lives — page,
//! section, field — as a breadcrumb path, and choosing one opens that page
//! and pulses the exact section or field (`assets/search.js`).
//!
//! Sources, none of them scraped from rendered HTML:
//!
//! 1. **Pages** — the navigation registry (`manager::nav::all_items`, the
//!    same entries the sidebar shows, including those other packages
//!    register), plus a few pages the sidebar does not list
//!    ([`EXTRA_PAGES`]: *New endpoint*, *New sync job*, the authorization
//!    server tabs…).
//! 2. **Sections and fields** — the declarative [`SPOTS`] table: (page, anchor
//!    id, label key, parent section, synonyms). Every anchor is a stable `id`
//!    in the page's template; `tests/global_search_http.rs` renders the pages
//!    and fails when one goes missing.
//! 3. **Data objects** — endpoints, authorization servers with their apps and
//!    users, sync jobs, code systems and value sets (title / URL), and a FHIR
//!    resource named exactly as `Type/id`; looked up per request through the
//!    services the console's own list pages use, each capped and timed out.
//!
//! Per-endpoint pages and settings are generated for every endpoint, the
//! endpoint's name in the path. Nothing the requester could not open is
//! listed: each candidate passes the sidebar's own role filter (minimum role
//! and scope of the nav entry) *and* the console guard's route table
//! (`helios_admin_auth::console::access::requirement`).
//!
//! `GET /ui/find?q=…&part=static|data` answers an HTML fragment (the
//! dropdown's two halves, fetched in parallel — the static half answers in
//! microseconds, the data half after the lookups); without `part` it is a
//! full results page, so the topbar form also works without JavaScript.

use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, Method, header};
use axum::response::Response;
use helios_admin_auth::console::access::{Requirement, requirement};
use helios_endpoints::console::ConsoleVia;
use helios_endpoints::{ConsolePrincipal, ConsoleRole};
use serde::Deserialize;

use crate::i18n::{I18n, RequestLocale};
use crate::manager::nav::{NavItem, NavRole, NavScope, NavSection, nav_role};
use crate::{RequestTenant, RequestVersion, Status, WebState, current_status, render};

/// Longest query considered (characters).
const MAX_QUERY: usize = 100;
/// At most this many words of a query are matched.
const MAX_TOKENS: usize = 6;
/// Caps per result group.
const MAX_SETTINGS: usize = 12;
const MAX_PAGES: usize = 8;
const MAX_DATA: usize = 12;
/// Per data source.
const MAX_PER_SOURCE: usize = 5;
/// Budget of one slow data source (the terminology service, the IdP store).
const SOURCE_TIMEOUT: Duration = Duration::from_millis(900);
/// Budget of the terminology search (its own part of the dropdown).
const TERMINOLOGY_TIMEOUT: Duration = Duration::from_millis(2500);

// ---------------------------------------------------------------- index

/// What a [`Spot`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotKind {
    /// A titled part of a page (a card, a disclosure, a sub-heading).
    Section,
    /// One setting: an input, a switch, a radio group.
    Field,
}

/// On which endpoints a per-endpoint spot exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// Always rendered.
    Always,
    /// Only on the default (root) endpoint.
    DefaultEndpoint,
    /// Only on the other endpoints.
    OtherEndpoints,
    /// Rendered only in some server states (an OIDC client configured, a
    /// built-in account signed in…): the search still lists it, and the
    /// browser falls back to its parent section when it is absent.
    Conditional,
}

/// One section or field of a page.
#[derive(Debug, Clone, Copy)]
pub struct Spot {
    /// The page: a nav entry id or an [`ExtraPage`] id.
    pub page: &'static str,
    /// The element id on that page.
    pub anchor: &'static str,
    /// Fluent key of its label.
    pub label: &'static str,
    /// The anchor of the enclosing spot (`""` = directly on the page).
    pub parent: &'static str,
    /// Section or field.
    pub kind: SpotKind,
    /// Synonyms, comma separated (English and Chinese terms people search
    /// for that the label does not contain).
    pub keywords: &'static str,
    /// Where it exists.
    pub when: When,
}

const fn sec(
    page: &'static str,
    anchor: &'static str,
    label: &'static str,
    keywords: &'static str,
) -> Spot {
    Spot {
        page,
        anchor,
        label,
        parent: "",
        kind: SpotKind::Section,
        keywords,
        when: When::Always,
    }
}

const fn sub(
    page: &'static str,
    anchor: &'static str,
    label: &'static str,
    parent: &'static str,
    keywords: &'static str,
) -> Spot {
    Spot {
        page,
        anchor,
        label,
        parent,
        kind: SpotKind::Section,
        keywords,
        when: When::Always,
    }
}

const fn field(
    page: &'static str,
    anchor: &'static str,
    label: &'static str,
    parent: &'static str,
    keywords: &'static str,
) -> Spot {
    Spot {
        page,
        anchor,
        label,
        parent,
        kind: SpotKind::Field,
        keywords,
        when: When::Always,
    }
}

const fn only(mut spot: Spot, when: When) -> Spot {
    spot.when = when;
    spot
}

/// Every searchable section and field of the console.
pub const SPOTS: &[Spot] = &[
    // ---- Endpoint › General (`/ui/e/{ep}/config`)
    sec(
        "ep-config",
        "sec-identity",
        "mgr-sec-identity",
        "name,identity,名稱",
    ),
    field(
        "ep-config",
        "f-display_name",
        "mgr-field-display-name",
        "sec-identity",
        "title,name,顯示名稱",
    ),
    field(
        "ep-config",
        "f-description",
        "mgr-field-description",
        "sec-identity",
        "describe,說明,描述",
    ),
    field(
        "ep-config",
        "f-labels",
        "mgr-field-labels",
        "sec-identity",
        "tags,env,label,標籤",
    ),
    sec(
        "ep-config",
        "sec-address",
        "mgr-sec-address",
        "url,base url,address,path,release,位址,網址",
    ),
    only(
        field(
            "ep-config",
            "f-path",
            "mgr-field-path",
            "sec-address",
            "url,base url,route,segment,路徑",
        ),
        When::OtherEndpoints,
    ),
    field(
        "ep-config",
        "f-fhir_version",
        "mgr-field-fhir-version",
        "sec-address",
        "R4,R4B,R5,R6,release,FHIR 版本",
    ),
    sec(
        "ep-config",
        "sec-console",
        "mgr-sec-console",
        "read-only,data access,console access,主控台",
    ),
    field(
        "ep-config",
        "f-console_access",
        "mgr-field-console-access",
        "sec-console",
        "role-based,read-only,off,唯讀",
    ),
    only(
        field(
            "ep-config",
            "f-legacy_tenant_header",
            "mgr-field-legacy-header",
            "sec-console",
            "X-Tenant-ID,tenant,multi-tenancy,header,租戶",
        ),
        When::DefaultEndpoint,
    ),
    // ---- Endpoint › Security (`/ui/e/{ep}/security`)
    sec(
        "ep-security",
        "sec-auth",
        "mgr-sec-auth",
        "oauth,oauth2,oidc,openid connect,token,jwt,bearer,smart,authentication,authorization,keycloak,idp,認證,授權,權杖,身分驗證",
    ),
    field(
        "ep-security",
        "f-auth_mode",
        "mgr-field-auth-mode",
        "sec-auth",
        "write-protect,full,off,protect,read only,認證模式",
    ),
    field(
        "ep-security",
        "f-auth_issuer",
        "mgr-field-issuer",
        "sec-auth",
        "iss,issuer,identity provider,keycloak,oidc,realm,簽發者",
    ),
    field(
        "ep-security",
        "f-auth_audience",
        "mgr-field-audience",
        "sec-auth",
        "aud,audience,受眾",
    ),
    field(
        "ep-security",
        "f-auth_jwks_url",
        "mgr-field-jwks",
        "sec-auth",
        "jwks,keys,signing keys,public key,certificate,金鑰,公鑰",
    ),
    field(
        "ep-security",
        "f-scopes",
        "mgr-field-scopes",
        "sec-auth",
        "scope,scopes,patient/,user/,system/,v1,v2,範圍",
    ),
    field(
        "ep-security",
        "f-scopes_non_compartment",
        "mgr-field-non-compartment",
        "sec-auth",
        "compartment,shared,reference,patient token",
    ),
    sub(
        "ep-security",
        "sec-auth-advanced",
        "mgr-sec-advanced",
        "sec-auth",
        "advanced,algorithm,clock,cache,進階",
    ),
    field(
        "ep-security",
        "f-auth_algorithms",
        "mgr-field-algorithms",
        "sec-auth-advanced",
        "RS256,RS384,ES256,ES384,alg,演算法",
    ),
    field(
        "ep-security",
        "f-auth_clock_skew",
        "mgr-field-clock-skew",
        "sec-auth-advanced",
        "skew,leeway,clock,time,時鐘",
    ),
    field(
        "ep-security",
        "f-auth_jwks_max_age",
        "mgr-field-jwks-max-age",
        "sec-auth-advanced",
        "cache,jwks cache,ttl,max-age,快取",
    ),
    sub(
        "ep-security",
        "sec-smart",
        "mgr-sec-smart",
        "sec-auth",
        "smart on fhir,.well-known,smart-configuration,discovery,well known,探索",
    ),
    field(
        "ep-security",
        "f-smart_discover",
        "mgr-smart-discover",
        "sec-smart",
        "discovery,openid configuration,auto,自動",
    ),
    field(
        "ep-security",
        "f-smart_authorization_endpoint",
        "mgr-smart-authorize",
        "sec-smart",
        "authorize,authorization endpoint,oauth,login url",
    ),
    field(
        "ep-security",
        "f-smart_token_endpoint",
        "mgr-smart-token",
        "sec-smart",
        "token,token endpoint,oauth,token url",
    ),
    field(
        "ep-security",
        "f-smart_jwks_uri",
        "mgr-smart-jwks",
        "sec-smart",
        "jwks,jwks_uri,public keys,keys",
    ),
    field(
        "ep-security",
        "f-accept_admin",
        "mgr-accept-admin",
        "sec-auth",
        "admin password,break-glass,administrator,管理員",
    ),
    sec(
        "ep-security",
        "sec-cors",
        "mgr-sec-cors",
        "cors,origin,cross-origin,browser,access-control,跨來源,跨域",
    ),
    field(
        "ep-security",
        "f-cors_enabled",
        "mgr-cors-enabled",
        "sec-cors",
        "enable cors,on,off",
    ),
    field(
        "ep-security",
        "f-cors_origins",
        "mgr-cors-origins",
        "sec-cors",
        "origins,allowed origins,allow-origin,domain,網域",
    ),
    field(
        "ep-security",
        "f-cors_credentials",
        "mgr-cors-credentials",
        "sec-cors",
        "credentials,cookies,allow-credentials",
    ),
    field(
        "ep-security",
        "f-cors_headers",
        "mgr-cors-headers",
        "sec-cors",
        "headers,allow-headers,request headers",
    ),
    sec(
        "ep-security",
        "sec-rate",
        "mgr-sec-rate",
        "rate limit,throttle,throttling,quota,requests per minute,rpm,429,限流,速率,流量",
    ),
    field(
        "ep-security",
        "f-rl_anonymous",
        "mgr-rate-anonymous",
        "sec-rate",
        "anonymous,unauthenticated,匿名",
    ),
    field(
        "ep-security",
        "f-rl_authenticated",
        "mgr-rate-authenticated",
        "sec-rate",
        "authenticated,signed in,token",
    ),
    field(
        "ep-security",
        "f-rl_burst",
        "mgr-rate-burst",
        "sec-rate",
        "burst,bucket,突發",
    ),
    // ---- Endpoint › Validation & IGs (`/ui/e/{ep}/validation`)
    sec(
        "ep-validation",
        "sec-validation",
        "mgr-sec-validation",
        "validate,validation,profile,profiles,enforce,conformance,驗證,檢核",
    ),
    field(
        "ep-validation",
        "f-val_mode",
        "mgr-field-validation-mode",
        "sec-validation",
        "enforce,log,off,reject,拒絕",
    ),
    field(
        "ep-validation",
        "f-val_terminology",
        "mgr-field-terminology-check",
        "sec-validation",
        "terminology,binding,value set,code check,valueset,術語",
    ),
    field(
        "ep-validation",
        "f-val_stored_profiles",
        "mgr-stored-profiles",
        "sec-validation",
        "StructureDefinition,stored profiles,profile",
    ),
    field(
        "ep-validation",
        "f-val_required",
        "mgr-required-profiles",
        "sec-validation",
        "required profile,us core,tw core,meta.profile,必要",
    ),
    sec(
        "ep-validation",
        "sec-igs",
        "mgr-sec-igs",
        "IG,implementation guide,package,packages,npm,tw core,us core,ips,套件,實作指引",
    ),
    // ---- Endpoint › Terminology service (`/ui/e/{ep}/terminology`)
    sec(
        "ep-terminology",
        "sec-terminology",
        "mgr-sec-terminology",
        "terminology server,tx,hts,$expand,$validate-code,$lookup,loinc,snomed,術語,術語伺服器",
    ),
    field(
        "ep-terminology",
        "f-term_server_url",
        "mgr-term-server",
        "sec-terminology",
        "url,server url,tx server,endpoint",
    ),
    field(
        "ep-terminology",
        "f-term_public_url",
        "mgr-term-public",
        "sec-terminology",
        "public url,browser,link",
    ),
    field(
        "ep-terminology",
        "f-term_timeout",
        "mgr-term-timeout",
        "sec-terminology",
        "timeout,seconds,逾時",
    ),
    field(
        "ep-terminology",
        "f-term_token",
        "mgr-term-token",
        "sec-terminology",
        "token,bearer,access token,secret,權杖",
    ),
    sec(
        "ep-terminology",
        "sec-hts",
        "mgr-sec-hts",
        "code systems,value sets,concept maps,browse,瀏覽",
    ),
    // ---- Endpoint home (`/ui/e/{ep}`)
    sec(
        "ep-home",
        "cfg-title",
        "mgr-home-config",
        "configuration,summary,overview,設定摘要",
    ),
    sec(
        "ep-home",
        "types-title",
        "mgr-home-types",
        "resource types,counts,statistics,統計",
    ),
    sec(
        "ep-home",
        "audit-title",
        "mgr-home-activity",
        "activity,audit,recent,最近",
    ),
    sec(
        "ep-home",
        "rev-title",
        "mgr-home-changes",
        "changes,revisions,history,變更",
    ),
    // ---- Settings › Console sign-in (`/ui/settings/auth`)
    field(
        "settings-auth",
        "cs-mode",
        "console-settings-mode",
        "",
        "sign-in mode,login mode,sso,oidc,local,none,登入方式",
    ),
    field(
        "settings-auth",
        "cs-anonymousRole",
        "console-settings-anonymous-role",
        "",
        "anonymous,guest,public,匿名",
    ),
    field(
        "settings-auth",
        "cs-breakGlass",
        "console-settings-break-glass",
        "",
        "break-glass,HFS_ADMIN_PASSWORD,emergency,admin password,緊急",
    ),
    sec(
        "settings-auth",
        "cs-oidc",
        "console-settings-oidc",
        "oidc,openid,sso,single sign-on,keycloak,azure,entra,google,單一登入",
    ),
    field(
        "settings-auth",
        "oidc-register",
        "console-settings-redirect-uri",
        "cs-oidc",
        "redirect uri,callback,post logout,回呼",
    ),
    field(
        "settings-auth",
        "cs-oidc.issuer",
        "console-settings-oidc-issuer",
        "cs-oidc",
        "issuer,iss,idp,簽發者",
    ),
    field(
        "settings-auth",
        "cs-oidc.clientId",
        "console-settings-oidc-client-id",
        "cs-oidc",
        "client id,client,app id",
    ),
    field(
        "settings-auth",
        "cs-oidc.scopes",
        "console-settings-oidc-scopes",
        "cs-oidc",
        "scope,openid profile email",
    ),
    field(
        "settings-auth",
        "cs-oidc.roleClaim",
        "console-settings-oidc-role-claim",
        "cs-oidc",
        "claim,roles,groups,realm_access",
    ),
    field(
        "settings-auth",
        "cs-oidc.roleMapping.admin",
        "console-settings-oidc-admin-values",
        "cs-oidc",
        "admin role,mapping",
    ),
    field(
        "settings-auth",
        "cs-oidc.roleMapping.editor",
        "console-settings-oidc-editor-values",
        "cs-oidc",
        "editor role,mapping",
    ),
    field(
        "settings-auth",
        "cs-oidc.roleMapping.viewer",
        "console-settings-oidc-viewer-values",
        "cs-oidc",
        "viewer role,mapping",
    ),
    field(
        "settings-auth",
        "cs-oidc.clientSecret",
        "console-settings-oidc-secret",
        "cs-oidc",
        "secret,client secret,confidential,密鑰",
    ),
    field(
        "settings-auth",
        "cs-oidc.defaultRole",
        "console-settings-oidc-default-role",
        "cs-oidc",
        "default role,fallback role",
    ),
    sec(
        "settings-auth",
        "cs-local",
        "console-settings-local",
        "password,login,lockout,session,timeout,accounts,密碼,登入,工作階段,逾時",
    ),
    field(
        "settings-auth",
        "cs-local.minPasswordLength",
        "console-settings-min-length",
        "cs-local",
        "password,length,complexity,policy,密碼長度",
    ),
    field(
        "settings-auth",
        "cs-local.lockoutFailures",
        "console-settings-lockout-failures",
        "cs-local",
        "lockout,brute force,failed login,鎖定",
    ),
    field(
        "settings-auth",
        "cs-local.lockoutMinutes",
        "console-settings-lockout-minutes",
        "cs-local",
        "lockout,minutes,鎖定",
    ),
    field(
        "settings-auth",
        "cs-sessionIdleMinutes",
        "console-settings-session-idle",
        "cs-local",
        "session timeout,idle,inactivity,logout,timeout,閒置,逾時",
    ),
    field(
        "settings-auth",
        "cs-sessionAbsoluteHours",
        "console-settings-session-absolute",
        "cs-local",
        "session timeout,absolute,lifetime,逾時",
    ),
    field(
        "settings-auth",
        "cs-allowedOrigins",
        "console-settings-allowed-origins",
        "cs-local",
        "origin,csrf,reverse proxy,來源",
    ),
    only(
        sec(
            "settings-auth",
            "oidc-test",
            "console-settings-test-title",
            "test login,test sign-in,測試",
        ),
        When::Conditional,
    ),
    // ---- Settings › Users & roles (`/ui/settings/users`)
    sec(
        "settings-users",
        "users-list",
        "console-users-list",
        "accounts,users,disable,reset password,帳號,使用者",
    ),
    sec(
        "settings-users",
        "users-create",
        "console-users-create",
        "add user,new account,invite,新增帳號",
    ),
    field(
        "settings-users",
        "uc-username",
        "admin-login-user",
        "users-create",
        "username,login,帳號",
    ),
    field(
        "settings-users",
        "uc-email",
        "console-users-email",
        "users-create",
        "email,mail,信箱",
    ),
    field(
        "settings-users",
        "uc-role",
        "console-users-role",
        "users-create",
        "role,admin,editor,viewer,角色",
    ),
    field(
        "settings-users",
        "uc-password",
        "console-users-initial-password",
        "users-create",
        "password,initial password,密碼",
    ),
    sec(
        "settings-users",
        "users-bindings",
        "console-bindings-title",
        "per-endpoint role,binding,permission,access,terminology role,權限,綁定",
    ),
    sec(
        "settings-users",
        "users-sessions",
        "console-sessions-title",
        "sessions,signed in,revoke,logout,工作階段,登出",
    ),
    // ---- Settings › My account (`/ui/account`)
    sec(
        "account",
        "account-identity",
        "auth-account-identity",
        "profile,who am i,role,身分",
    ),
    only(
        sec(
            "account",
            "account-password",
            "console-account-password",
            "password,change password,reset,密碼,變更密碼",
        ),
        When::Conditional,
    ),
    only(
        field(
            "account",
            "acc-current",
            "console-account-current",
            "account-password",
            "old password,current",
        ),
        When::Conditional,
    ),
    only(
        field(
            "account",
            "acc-new",
            "console-account-new",
            "account-password",
            "new password",
        ),
        When::Conditional,
    ),
    // ---- Settings › About (`/ui/about`)
    sec(
        "about",
        "attr-title",
        "mgr-about-attribution",
        "credits,helios,healthymind,attribution",
    ),
    sec(
        "about",
        "lic-title",
        "mgr-about-licenses",
        "license,open source,oss,third-party,授權條款",
    ),
    // ---- Server › New sync job (`/ui/sync/new`)
    sec(
        "sync-new",
        "sync-basics",
        "sync-step-basics",
        "name,source,target,peer,來源,目標",
    ),
    sec(
        "sync-new",
        "sync-data",
        "sync-step-data",
        "resource types,history,filter,資源類型",
    ),
    field(
        "sync-new",
        "f-resource_types",
        "sync-resource-types",
        "sync-data",
        "types,Patient,Observation,資源",
    ),
    sec(
        "sync-new",
        "sync-schedule",
        "sync-step-schedule",
        "schedule,interval,cron,poll,trigger,排程",
    ),
    field(
        "sync-new",
        "f-batch_size",
        "sync-batch-size",
        "sync-schedule",
        "batch,page size,批次",
    ),
    // ---- Authorization server tabs (`/ui/idp/{realm}/…`)
    sec(
        "idp-tab-clients",
        "client-add",
        "idpc-client-add",
        "new app,register client,client id,redirect uri,新增應用",
    ),
    sec(
        "idp-tab-users",
        "user-add",
        "idpc-user-add",
        "new user,add user,patient user,新增使用者",
    ),
    sec(
        "idp-tab-keys",
        "keys",
        "idpc-keys-published",
        "jwks,kid,public keys,金鑰",
    ),
    sec(
        "idp-tab-keys",
        "keys-rotate",
        "idpc-keys-rotate",
        "rotate,rotation,key rollover,輪替",
    ),
    sec(
        "idp-tab-keys",
        "keys-emergency",
        "idpc-keys-emergency",
        "compromised,revoke,emergency,緊急",
    ),
    sec(
        "idp-tab-settings",
        "general",
        "idpc-step-basics",
        "name,display name,名稱",
    ),
    sec(
        "idp-tab-settings",
        "capabilities",
        "idpc-step-capabilities",
        "smart capabilities,launch,pkce,refresh token,功能",
    ),
    sec(
        "idp-tab-settings",
        "security",
        "idpc-settings-security",
        "password policy,lockout,key rotation,安全",
    ),
    field(
        "idp-tab-settings",
        "f-security.passwordMinLength",
        "idpc-password-min",
        "security",
        "password length,密碼長度",
    ),
    field(
        "idp-tab-settings",
        "f-security.lockoutThreshold",
        "idpc-lockout",
        "security",
        "lockout,brute force,鎖定",
    ),
    field(
        "idp-tab-settings",
        "f-rotation.rotateAfterDays",
        "idpc-rotate-days",
        "security",
        "rotate,days,key rotation,輪替",
    ),
    sec(
        "idp-tab-settings",
        "endpoints",
        "idpc-step-endpoints",
        "bind endpoint,fhir endpoint,trust,綁定",
    ),
    sec(
        "idp-tab-test",
        "test",
        "idpc-test-title",
        "diagnose,check,test token,測試",
    ),
];

/// Synonyms of the nav pages, by nav id.
const PAGE_KEYWORDS: &[(&str, &str)] = &[
    (
        "overview",
        "endpoints,dashboard,home,overview,servers,端點,總覽,首頁",
    ),
    ("ep-home", "endpoint home,dashboard,kpi,activity,首頁"),
    (
        "resources",
        "resource,patient,observation,browse,crud,data,資源,病人",
    ),
    ("batch", "bundle,transaction,batch,批次,交易"),
    ("validate", "$validate,validation,profile,check,驗證"),
    (
        "bulk-import",
        "$import,bulk import,ndjson,bulk submit,upload,load,匯入",
    ),
    (
        "bulk-export",
        "$export,bulk export,ndjson,backup,download,匯出",
    ),
    (
        "sql-view-definitions",
        "SQL on FHIR,ViewDefinition,view,flatten,檢視",
    ),
    ("sql-queries", "SQL,query,select,查詢"),
    ("sql-views", "SQL view,檢視"),
    ("sql-export", "$sql-export,parquet,csv,export,匯出"),
    (
        "capability-statement",
        "metadata,CapabilityStatement,conformance,能力聲明",
    ),
    (
        "search-parameters",
        "SearchParameter,search param,index,搜尋參數",
    ),
    (
        "compartments",
        "CompartmentDefinition,patient compartment,區隔",
    ),
    (
        "subscriptions",
        "Subscription,webhook,notification,topic,訂閱,通知",
    ),
    (
        "ep-config",
        "general,name,path,url,fhir version,console access,一般,設定",
    ),
    (
        "ep-security",
        "security,auth,oauth,smart,cors,rate limit,安全",
    ),
    ("ep-validation", "validation,IG,profile,package,驗證"),
    ("ep-terminology", "terminology server,tx,hts,術語"),
    ("ep-sync", "sync,replication,mirror,同步"),
    ("ep-audit", "audit,AuditEvent,log,logs,access log,稽核,日誌"),
    (
        "ep-history",
        "history,revisions,changes,diff,rollback,歷史,版本",
    ),
    (
        "settings-auth",
        "login,sign-in,password,oidc,sso,session,lockout,登入,密碼",
    ),
    (
        "settings-users",
        "users,accounts,roles,admin,editor,viewer,permissions,bindings,使用者,角色,權限",
    ),
    (
        "account",
        "password,my account,profile,change password,密碼,帳號,個人",
    ),
    ("tenants", "tenant,multi-tenancy,租戶"),
    ("about", "version,license,attribution,關於,版本"),
    ("sync", "sync,replication,jobs,schedule,同步,排程"),
    (
        "idp-realms",
        "idp,oauth,oidc,authorization server,realm,keycloak,sso,授權伺服器,身分提供者",
    ),
    (
        "tx-home",
        "terminology,術語,loinc,snomed,icd,rxnorm,code system,value set",
    ),
    (
        "tx-code-systems",
        "CodeSystem,loinc,snomed,icd-10,rxnorm,code system,碼系統,代碼系統",
    ),
    ("tx-value-sets", "ValueSet,value set,$expand,值集"),
    ("tx-concept-maps", "ConceptMap,$translate,mapping,對照,映射"),
    (
        "tx-manage",
        "loaded,installed,inventory,loinc,snomed,delete,current version,已載入,已安裝",
    ),
    (
        "tx-import",
        "upload,import,load,loinc,snomed,zip,package,匯入",
    ),
    ("tx-jobs", "jobs,progress,import job,工作,匯入工作"),
    (
        "tx-capability",
        "metadata,TerminologyCapabilities,CapabilityStatement,能力聲明",
    ),
];

/// Pages served only with console sign-in installed.
const CONSOLE_PAGES: &[&str] = &["settings-auth", "settings-users", "account"];

/// Where an [`ExtraPage`] lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageScope {
    /// Server-wide (`/ui/...`).
    Server,
    /// Per endpoint (`{ep}` in the href).
    Endpoint,
    /// Per authorization server (`{realm}` in the href).
    Realm,
}

/// A page the sidebar does not list.
#[derive(Debug, Clone, Copy)]
pub struct ExtraPage {
    /// Id (spots refer to it).
    pub id: &'static str,
    /// Fluent key of its title.
    pub label: &'static str,
    /// Address; `{ep}` / `{realm}` placeholders.
    pub href: &'static str,
    /// Icon.
    pub icon: &'static str,
    /// The nav entry it sits under (its path), `""` for none.
    pub parent: &'static str,
    /// Scope.
    pub scope: PageScope,
    /// Hidden below this role.
    pub min_role: NavRole,
    /// Synonyms.
    pub keywords: &'static str,
    /// Loads as a full document (its scripts are not lifecycle-clean).
    pub full_load: bool,
}

#[allow(clippy::too_many_arguments)]
const fn extra(
    id: &'static str,
    label: &'static str,
    href: &'static str,
    icon: &'static str,
    parent: &'static str,
    scope: PageScope,
    min_role: NavRole,
    keywords: &'static str,
) -> ExtraPage {
    ExtraPage {
        id,
        label,
        href,
        icon,
        parent,
        scope,
        min_role,
        keywords,
        full_load: false,
    }
}

/// Pages reachable from inside other pages only.
pub const EXTRA_PAGES: &[ExtraPage] = &[
    extra(
        "endpoint-new",
        "mgr-action-new-endpoint",
        "/ui/endpoints/new",
        "plus",
        "overview",
        PageScope::Server,
        NavRole::Admin,
        "create endpoint,add endpoint,new endpoint,新增端點,建立",
    ),
    ExtraPage {
        full_load: true,
        ..extra(
            "packages",
            "nav-packages",
            "/ui/e/{ep}/data/packages",
            "package",
            "",
            PageScope::Endpoint,
            NavRole::Admin,
            "IG,implementation guide,package,npm,install,tw core,us core,套件,實作指引",
        )
    },
    extra(
        "sync-new",
        "sync-new",
        "/ui/sync/new",
        "plus",
        "sync",
        PageScope::Server,
        NavRole::Admin,
        "create sync,new job,replication,新增同步",
    ),
    extra(
        "idp-new",
        "idpc-new",
        "/ui/idp/new",
        "plus",
        "idp-realms",
        PageScope::Server,
        NavRole::Admin,
        "create realm,new authorization server,wizard,新增",
    ),
    extra(
        "idp-tab-overview",
        "idpc-tab-overview",
        "/ui/idp/{realm}",
        "shield",
        "",
        PageScope::Realm,
        NavRole::Admin,
        "discovery,issuer,overview,總覽",
    ),
    extra(
        "idp-tab-clients",
        "idpc-tab-clients",
        "/ui/idp/{realm}/clients",
        "key",
        "",
        PageScope::Realm,
        NavRole::Admin,
        "apps,clients,client id,oauth client,redirect uri,應用程式,用戶端",
    ),
    extra(
        "idp-tab-users",
        "idpc-tab-users",
        "/ui/idp/{realm}/users",
        "users",
        "",
        PageScope::Realm,
        NavRole::Admin,
        "users,accounts,patients,practitioners,使用者",
    ),
    extra(
        "idp-tab-keys",
        "idpc-tab-keys",
        "/ui/idp/{realm}/keys",
        "key",
        "",
        PageScope::Realm,
        NavRole::Admin,
        "jwks,signing keys,rotate,kid,金鑰,簽章",
    ),
    extra(
        "idp-tab-sessions",
        "idpc-tab-sessions",
        "/ui/idp/{realm}/sessions",
        "activity",
        "",
        PageScope::Realm,
        NavRole::Admin,
        "sessions,tokens,refresh tokens,revoke,工作階段",
    ),
    extra(
        "idp-tab-settings",
        "idpc-tab-settings",
        "/ui/idp/{realm}/settings",
        "sliders",
        "",
        PageScope::Realm,
        NavRole::Admin,
        "settings,policy,設定",
    ),
    extra(
        "idp-tab-test",
        "idpc-tab-test",
        "/ui/idp/{realm}/test",
        "activity",
        "",
        PageScope::Realm,
        NavRole::Admin,
        "test,diagnose,check,測試",
    ),
];

/// The page id a spot refers to must be a nav id or an extra page; every
/// label and keyword list is static. A flat view of the index for tests.
#[doc(hidden)]
pub fn spot_table() -> &'static [Spot] {
    SPOTS
}

/// The extra pages, for tests.
#[doc(hidden)]
pub fn extra_pages() -> &'static [ExtraPage] {
    EXTRA_PAGES
}

/// The nav page ids that carry synonyms, for tests.
#[doc(hidden)]
pub fn page_keyword_ids() -> Vec<&'static str> {
    PAGE_KEYWORDS.iter().map(|(id, _)| *id).collect()
}

/// The fluent keys the search itself renders (tests check en / zh-Hant).
#[doc(hidden)]
pub const UI_KEYS: &[&str] = &[
    "gs-label",
    "gs-placeholder",
    "gs-open",
    "gs-close",
    "gs-results",
    "gs-group-settings",
    "gs-group-pages",
    "gs-group-data",
    "gs-group-terminology",
    "gs-kind-page",
    "gs-kind-section",
    "gs-kind-field",
    "gs-kind-endpoint",
    "gs-kind-realm",
    "gs-kind-app",
    "gs-kind-user",
    "gs-kind-sync",
    "gs-kind-codesystem",
    "gs-kind-valueset",
    "gs-kind-resource",
    "gs-root-endpoints",
    "gs-matched",
    "gs-empty-title",
    "gs-empty-text",
    "gs-recent",
    "gs-clear-recent",
    "gs-suggestions",
    "gs-no-results",
    "gs-no-results-text",
    "gs-count-zero",
    "gs-count-one",
    "gs-count-other",
    "gs-loading",
    "gs-help-navigate",
    "gs-help-open",
    "gs-help-close",
    "gs-page-title",
    "gs-page-lede",
    "gs-page-empty",
    "gs-sync-route",
    "gs-suggest-password",
    "gs-suggest-rate",
    "gs-suggest-security",
    "gs-suggest-terminology",
    "gs-suggest-users",
];

// ---------------------------------------------------------------- access

/// What the requester may open.
struct Access<'a> {
    principal: Option<&'a ConsolePrincipal>,
    /// Role without a console principal (admin-password gate, open console).
    fallback: ConsoleRole,
    terminology: ConsoleRole,
}

impl Access<'_> {
    fn global(&self) -> ConsoleRole {
        self.principal.map_or(self.fallback, |p| p.role)
    }

    fn on(&self, endpoint: &str) -> ConsoleRole {
        self.principal
            .map_or(self.fallback, |p| p.role_for(endpoint))
    }

    fn signed_in(&self) -> bool {
        self.principal
            .is_some_and(|p| p.via != ConsoleVia::Anonymous)
    }

    /// The console guard's own verdict on a `GET` of `href`.
    fn can_open(&self, href: &str) -> bool {
        let path = href.split(['?', '#']).next().unwrap_or(href);
        match requirement(&Method::GET, path) {
            None | Some(Requirement::Public) => true,
            Some(Requirement::SignedIn) => self.signed_in(),
            Some(Requirement::Role(r)) => self.global() >= r,
            Some(Requirement::EndpointRole(ep, r)) => self.on(&ep) >= r,
        }
    }

    /// The sidebar's verdict on a nav entry (on `endpoint` for per-endpoint
    /// sections).
    fn nav_ok(&self, item: &NavItem, endpoint: Option<&str>) -> bool {
        let role = match (item.scope, item.section.per_endpoint()) {
            (NavScope::Terminology, _) => self.terminology,
            (NavScope::Console, true) => endpoint.map_or(self.global(), |e| self.on(e)),
            (NavScope::Console, false) => self.global(),
        };
        nav_role(role) >= item.min_role
    }
}

// ---------------------------------------------------------------- matching

/// Lower-cased, one char per char (so positions map back to the label).
fn fold(s: &str) -> Vec<char> {
    s.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
}

/// The best occurrence of `needle` in `hay`: (position, quality) where
/// quality 3 = at the start, 2 = at a word start, 1 = inside a word.
fn best_match(hay: &[char], needle: &[char]) -> Option<(usize, u8)> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    let mut best: Option<(usize, u8)> = None;
    for pos in 0..=hay.len() - needle.len() {
        if hay[pos..pos + needle.len()] != *needle {
            continue;
        }
        let quality = if pos == 0 {
            3
        } else if !hay[pos - 1].is_alphanumeric() || is_cjk(hay[pos]) || is_cjk(hay[pos - 1]) {
            2
        } else {
            1
        };
        if best.is_none_or(|(_, q)| quality > q) {
            best = Some((pos, quality));
            if quality == 3 {
                break;
            }
        }
    }
    best
}

/// The words of a query (at most [`MAX_TOKENS`], folded).
fn tokens(q: &str) -> Vec<Vec<char>> {
    q.split(|c: char| c.is_whitespace() || c == '›' || c == '>')
        .filter(|t| !t.is_empty())
        .take(MAX_TOKENS)
        .map(fold)
        .collect()
}

/// A run of label text, marked when it matched the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seg {
    pub text: String,
    pub mark: bool,
}

/// `label` cut into runs at the matched `ranges` (char positions).
fn segments(label: &str, ranges: &[(usize, usize)]) -> Vec<Seg> {
    let chars: Vec<char> = label.chars().collect();
    let mut marked = vec![false; chars.len()];
    for &(start, len) in ranges {
        for m in marked.iter_mut().skip(start).take(len) {
            *m = true;
        }
    }
    let mut out: Vec<Seg> = Vec::new();
    for (c, m) in chars.into_iter().zip(marked) {
        match out.last_mut() {
            Some(last) if last.mark == m => last.text.push(c),
            _ => out.push(Seg {
                text: c.to_string(),
                mark: m,
            }),
        }
    }
    out
}

// ---------------------------------------------------------------- candidates

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupId {
    Settings,
    Pages,
}

/// Something the static half can return.
struct Cand {
    group: GroupId,
    kind_key: &'static str,
    icon: &'static str,
    label: String,
    label_f: Vec<char>,
    en_f: Vec<char>,
    crumbs: Vec<String>,
    ctx_f: Vec<char>,
    keywords: &'static str,
    href: String,
    target: String,
    fallback: String,
    full_load: bool,
    bonus: i32,
}

/// One rendered hit.
#[derive(Debug, Clone)]
pub(crate) struct Hit {
    pub dom_id: String,
    pub icon: &'static str,
    pub kind_key: &'static str,
    pub segs: Vec<Seg>,
    pub crumbs: Vec<String>,
    pub href: String,
    pub target: String,
    pub fallback: String,
    pub full_load: bool,
    /// The synonym that matched, when the label itself did not.
    pub note: String,
    score: i32,
}

/// One rendered group.
#[derive(Debug, Clone)]
pub(crate) struct Group {
    pub id: &'static str,
    pub title_key: &'static str,
    pub hits: Vec<Hit>,
}

/// A candidate's score, the matched label ranges and the synonym that
/// matched when the label did not.
type Scored = (i32, Vec<(usize, usize)>, String);

impl Cand {
    /// Score against the query, `None` when a word matches nothing.
    fn score(&self, toks: &[Vec<char>]) -> Option<Scored> {
        let mut total = 0;
        let mut ranges = Vec::new();
        let mut note = String::new();
        let mut label_hits = 0;
        let mut context_only = 0;
        for t in toks {
            if let Some((pos, q)) = best_match(&self.label_f, t) {
                total += if self.label_f == *t {
                    130
                } else {
                    [0, 60, 85, 100][q as usize]
                };
                ranges.push((pos, t.len()));
                label_hits += 1;
                continue;
            }
            if let Some((_, q)) = best_match(&self.en_f, t) {
                total += 40 + 5 * i32::from(q);
                continue;
            }
            let kw = self
                .keywords
                .split(',')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .filter_map(|k| best_match(&fold(k), t).map(|(_, q)| (k, q)))
                .max_by_key(|(_, q)| *q);
            if let Some((k, q)) = kw {
                total += 30 + 6 * i32::from(q);
                if note.is_empty() {
                    note = k.to_string();
                }
                continue;
            }
            if let Some((_, q)) = best_match(&self.ctx_f, t) {
                total += 8 + 4 * i32::from(q);
                context_only += 1;
                continue;
            }
            return None;
        }
        // Only the path matched ("sandbox" against every page of that
        // endpoint): noise — the endpoint itself is a data object.
        if context_only == toks.len() {
            return None;
        }
        if label_hits > 0 {
            note.clear();
        }
        let length_penalty = (self.label_f.len() as i32 / 12).min(4);
        Some((total + self.bonus - length_penalty, ranges, note))
    }
}

/// An endpoint the requester can see.
#[derive(Clone)]
struct Ep {
    name: String,
    display: String,
    path: String,
    is_default: bool,
    current: bool,
}

/// An authorization server.
struct Realm {
    name: String,
    display: String,
}

struct Builder<'a> {
    i18n: &'a I18n,
    en: &'a I18n,
    access: &'a Access<'a>,
    items: Vec<NavItem>,
    endpoints: Vec<Ep>,
    realms: Vec<Realm>,
    package_admin: bool,
    /// Console sign-in (P3) installed: its pages (sign-in settings, users,
    /// my account) answer 404 without it.
    console_signin: bool,
    out: Vec<Cand>,
}

impl Builder<'_> {
    fn t(&self, key: &str) -> String {
        self.i18n.t(key)
    }

    fn en_t(&self, key: &str) -> String {
        if self.i18n.is_english() {
            String::new()
        } else {
            self.en.t(key)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        group: GroupId,
        kind_key: &'static str,
        icon: &'static str,
        label_key: &str,
        crumbs: Vec<String>,
        context: &str,
        keywords: &'static str,
        href: String,
        target: &str,
        fallback: &str,
        full_load: bool,
        bonus: i32,
    ) {
        let label = self.t(label_key);
        let mut crumbs = crumbs;
        // "Authorization servers › Authorization servers": one is enough.
        crumbs.dedup();
        if crumbs.last() == Some(&label) {
            crumbs.pop();
        }
        let ctx = format!("{} {}", crumbs.join(" "), context);
        self.out.push(Cand {
            group,
            kind_key,
            icon,
            label_f: fold(&label),
            en_f: fold(&self.en_t(label_key)),
            label,
            ctx_f: fold(&ctx),
            crumbs,
            keywords,
            href,
            target: target.to_string(),
            fallback: fallback.to_string(),
            full_load,
            bonus,
        });
    }

    /// A page, then its spots.
    #[allow(clippy::too_many_arguments)]
    fn page_and_spots(
        &mut self,
        page_id: &str,
        label_key: &str,
        icon: &'static str,
        href: String,
        crumbs: Vec<String>,
        context: &str,
        keywords: &'static str,
        full_load: bool,
        bonus: i32,
        endpoint: Option<(bool, bool)>,
    ) {
        self.push(
            GroupId::Pages,
            "gs-kind-page",
            icon,
            label_key,
            crumbs.clone(),
            context,
            keywords,
            href.clone(),
            "",
            "",
            full_load,
            bonus + 6,
        );
        let page_label = self.t(label_key);
        for spot in SPOTS.iter().filter(|s| s.page == page_id) {
            if let Some((is_default, _)) = endpoint {
                match spot.when {
                    When::DefaultEndpoint if !is_default => continue,
                    When::OtherEndpoints if is_default => continue,
                    _ => {}
                }
            }
            let mut path = crumbs.clone();
            path.push(page_label.clone());
            // Parent chain, outermost first.
            let mut chain = Vec::new();
            let mut parent = spot.parent;
            while !parent.is_empty() && chain.len() < 4 {
                match SPOTS
                    .iter()
                    .find(|s| s.page == page_id && s.anchor == parent)
                {
                    Some(p) => {
                        chain.push(p.label);
                        parent = p.parent;
                    }
                    None => break,
                }
            }
            for key in chain.iter().rev() {
                path.push(self.t(key));
            }
            let (group, kind_key, spot_icon) = match spot.kind {
                SpotKind::Section => (GroupId::Settings, "gs-kind-section", "sliders"),
                SpotKind::Field => (GroupId::Settings, "gs-kind-field", "pencil"),
            };
            self.push(
                group,
                kind_key,
                spot_icon,
                spot.label,
                path,
                context,
                spot.keywords,
                format!("{href}#{}", spot.anchor),
                spot.anchor,
                spot.parent,
                full_load,
                bonus + if spot.kind == SpotKind::Section { 3 } else { 0 },
            );
        }
    }

    /// The sidebar entry a page sits under when it is one of a group's
    /// route tabs (console redesign §5.1), as one more breadcrumb step.
    fn group_crumb(&self, item: &NavItem) -> Option<String> {
        match item.group {
            Some((gid, label, _)) if gid != item.id && item.sidebar => Some(self.t(label)),
            _ => None,
        }
    }

    fn build(&mut self) {
        let items = std::mem::take(&mut self.items);
        let root = self.t("gs-root-endpoints");
        for item in &items {
            if !self.console_signin && CONSOLE_PAGES.contains(&item.id) {
                continue;
            }
            let keywords = PAGE_KEYWORDS
                .iter()
                .find(|(id, _)| *id == item.id)
                .map_or("", |(_, k)| *k);
            if item.section.per_endpoint() {
                let eps: Vec<Ep> = self.endpoints.clone();
                for Ep {
                    name,
                    display,
                    path,
                    is_default,
                    current,
                } in eps
                {
                    let href = item.href.replace("{ep}", &name);
                    if !self.access.nav_ok(item, Some(&name)) || !self.access.can_open(&href) {
                        continue;
                    }
                    self.page_and_spots(
                        item.id,
                        item.label_key,
                        item.icon,
                        href,
                        {
                            let mut c = vec![root.clone(), display.clone()];
                            c.extend(self.group_crumb(item));
                            c
                        },
                        &format!("{name} {path}"),
                        keywords,
                        !item.boost,
                        if current { 12 } else { 0 },
                        Some((is_default, current)),
                    );
                }
            } else {
                if !self.access.nav_ok(item, None) || !self.access.can_open(item.href) {
                    continue;
                }
                let mut crumbs = match item.section {
                    NavSection::Manager => vec![],
                    s => vec![self.t(s.label_key())],
                };
                crumbs.extend(self.group_crumb(item));
                self.page_and_spots(
                    item.id,
                    item.label_key,
                    item.icon,
                    item.href.to_string(),
                    crumbs,
                    "",
                    keywords,
                    !item.boost,
                    0,
                    None,
                );
            }
        }
        for page in EXTRA_PAGES {
            if page.id == "packages" && !self.package_admin {
                continue;
            }
            let parent = items.iter().find(|i| i.id == page.parent);
            if !page.parent.is_empty() && parent.is_none() {
                // Its section is not installed (no sync, no IdP).
                continue;
            }
            let mut base: Vec<String> = Vec::new();
            if let Some(p) = parent {
                if p.section != NavSection::Manager {
                    base.push(self.t(p.section.label_key()));
                }
                base.push(self.t(p.label_key));
            }
            match page.scope {
                PageScope::Server => {
                    if nav_role(self.access.global()) < page.min_role
                        || !self.access.can_open(page.href)
                    {
                        continue;
                    }
                    self.page_and_spots(
                        page.id,
                        page.label,
                        page.icon,
                        page.href.to_string(),
                        base,
                        "",
                        page.keywords,
                        page.full_load,
                        0,
                        None,
                    );
                }
                PageScope::Endpoint => {
                    let eps: Vec<Ep> = self.endpoints.clone();
                    for Ep {
                        name,
                        display,
                        path,
                        is_default,
                        current,
                    } in eps
                    {
                        let href = page.href.replace("{ep}", &name);
                        if nav_role(self.access.on(&name)) < page.min_role
                            || !self.access.can_open(&href)
                        {
                            continue;
                        }
                        self.page_and_spots(
                            page.id,
                            page.label,
                            page.icon,
                            href,
                            vec![root.clone(), display],
                            &format!("{name} {path}"),
                            page.keywords,
                            page.full_load,
                            if current { 12 } else { 0 },
                            Some((is_default, current)),
                        );
                    }
                }
                PageScope::Realm => {
                    if nav_role(self.access.global()) < page.min_role {
                        continue;
                    }
                    let section = self.t(NavSection::AuthorizationServers.label_key());
                    let realms: Vec<(String, String)> = self
                        .realms
                        .iter()
                        .map(|r| (r.name.clone(), r.display.clone()))
                        .collect();
                    for (name, display) in realms {
                        let href = page.href.replace("{realm}", &name);
                        if !self.access.can_open(&href) {
                            continue;
                        }
                        self.page_and_spots(
                            page.id,
                            page.label,
                            page.icon,
                            href,
                            vec![section.clone(), display],
                            &name,
                            page.keywords,
                            // The realm pages carry their own scripts.
                            true,
                            0,
                            None,
                        );
                    }
                }
            }
        }
    }
}

/// The static half: pages, sections and fields matching `q`.
fn static_groups(b: Builder<'_>, q: &str) -> Vec<Group> {
    let mut b = b;
    b.build();
    let toks = tokens(q);
    if toks.is_empty() {
        return Vec::new();
    }
    let mut settings = Vec::new();
    let mut pages = Vec::new();
    for c in &b.out {
        let Some((score, ranges, note)) = c.score(&toks) else {
            continue;
        };
        let hit = Hit {
            dom_id: String::new(),
            icon: c.icon,
            kind_key: c.kind_key,
            segs: segments(&c.label, &ranges),
            crumbs: c.crumbs.clone(),
            href: c.href.clone(),
            target: c.target.clone(),
            fallback: c.fallback.clone(),
            full_load: c.full_load,
            note,
            score,
        };
        match c.group {
            GroupId::Pages => pages.push(hit),
            GroupId::Settings => settings.push(hit),
        }
    }
    let mut groups = Vec::new();
    for (id, key, mut hits, cap) in [
        ("settings", "gs-group-settings", settings, MAX_SETTINGS),
        ("pages", "gs-group-pages", pages, MAX_PAGES),
    ] {
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.href.cmp(&b.href)));
        // One per spot: the same setting of every endpoint would bury the
        // rest. The endpoint in view wins (its bonus) unless the query names
        // another one ("lab jwks"), whose context match then outranks it.
        let mut seen = std::collections::HashSet::new();
        hits.retain(|h| seen.insert(endpoint_agnostic(&h.href)));
        hits.truncate(cap);
        if !hits.is_empty() {
            groups.push(Group {
                id,
                title_key: key,
                hits,
            });
        }
    }
    groups
}

/// `/ui/e/{name}/rest` → `/ui/e/*/rest` (other hrefs unchanged).
fn endpoint_agnostic(href: &str) -> String {
    match href.strip_prefix("/ui/e/") {
        Some(rest) => match rest.find(['/', '#', '?']) {
            Some(i) => format!("/ui/e/*{}", &rest[i..]),
            None => "/ui/e/*".to_string(),
        },
        None => href.to_string(),
    }
}

// ---------------------------------------------------------------- data objects

/// Scores a data object by its name fields: all words must match one of
/// them; the label's own matches are highlighted.
fn data_hit(
    toks: &[Vec<char>],
    kind_key: &'static str,
    icon: &'static str,
    label: &str,
    others: &[&str],
    crumbs: Vec<String>,
    href: String,
) -> Option<Hit> {
    let label_f = fold(label);
    let others_f: Vec<Vec<char>> = others.iter().map(|o| fold(o)).collect();
    let mut score = 0;
    let mut ranges = Vec::new();
    for t in toks {
        if let Some((pos, q)) = best_match(&label_f, t) {
            score += if label_f == *t {
                130
            } else {
                [0, 60, 85, 100][q as usize]
            };
            ranges.push((pos, t.len()));
        } else if let Some(q) = others_f
            .iter()
            .filter_map(|o| best_match(o, t))
            .map(|(_, q)| q)
            .max()
        {
            score += 40 + 5 * i32::from(q);
        } else {
            return None;
        }
    }
    Some(Hit {
        dom_id: String::new(),
        icon,
        kind_key,
        segs: segments(label, &ranges),
        crumbs,
        href,
        target: String::new(),
        fallback: String::new(),
        full_load: false,
        note: String::new(),
        score,
    })
}

/// `Patient/123`-shaped input.
fn resource_ref(q: &str) -> Option<(&str, &str)> {
    let (ty, id) = q.trim().split_once('/')?;
    let ty_ok = ty.len() >= 2
        && ty.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && ty.chars().all(|c| c.is_ascii_alphanumeric());
    let id_ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    (ty_ok && id_ok).then_some((ty, id))
}

async fn timed<T: Default>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(SOURCE_TIMEOUT, fut)
        .await
        .unwrap_or_default()
}

/// The data half.
/// Which data sources a request asks for: the terminology service is the
/// slow one (a remote `title:contains` search), so the dropdown fetches it
/// as a third, separate part instead of holding the others back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataPart {
    /// Endpoints, authorization servers, sync jobs, `Type/id`.
    Local,
    /// Code systems and value sets.
    Terminology,
    /// Both (the no-JavaScript page).
    All,
}

async fn data_groups(
    state: &WebState,
    access: &Access<'_>,
    i18n: &I18n,
    endpoints: &[Ep],
    q: &str,
    part: DataPart,
) -> Vec<Group> {
    let local = part != DataPart::Terminology;
    let toks = tokens(q);
    let q_trim = q.trim();
    if toks.is_empty() || q_trim.chars().count() < 2 {
        return Vec::new();
    }
    let root = i18n.t("gs-root-endpoints");
    let mut hits: Vec<Hit> = Vec::new();

    // Endpoints (the registry snapshot, in memory).
    if let (true, Some(rt)) = (local, &state.manager) {
        let mut found = Vec::new();
        for e in rt.registry.snapshot().endpoints() {
            let name = e.name().to_string();
            let href = format!("/ui/e/{name}");
            if !access.can_open(&href) {
                continue;
            }
            let path = if e.spec.path.is_root() {
                "/".to_string()
            } else {
                format!("/{}", e.spec.path.as_str())
            };
            let labels: Vec<String> = e
                .spec
                .labels
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            let mut others = vec![name.as_str(), path.as_str()];
            let description = e.spec.description.clone().unwrap_or_default();
            others.push(description.as_str());
            others.extend(labels.iter().map(String::as_str));
            if let Some(h) = data_hit(
                &toks,
                "gs-kind-endpoint",
                "server",
                &e.spec.display_name,
                &others,
                vec![root.clone()],
                href,
            ) {
                found.push(h);
            }
        }
        found.sort_by_key(|h| std::cmp::Reverse(h.score));
        hits.extend(found.into_iter().take(MAX_PER_SOURCE));
    }

    let idp = async {
        let mut out = Vec::new();
        if !local || nav_role(access.global()) < NavRole::Admin {
            return out;
        }
        let Some(c) = helios_idp_admin::service::global() else {
            return out;
        };
        let section = i18n.t(NavSection::AuthorizationServers.label_key());
        let realms = c.realms().await.unwrap_or_default();
        for r in &realms {
            let name = r.spec.name.as_str().to_string();
            let href = format!("/ui/idp/{name}");
            if !access.can_open(&href) {
                continue;
            }
            if let Some(h) = data_hit(
                &toks,
                "gs-kind-realm",
                "shield",
                &r.spec.display_name,
                &[name.as_str()],
                vec![section.clone()],
                href,
            ) {
                out.push(h);
            }
        }
        // Apps and users of every realm (a handful of realms).
        for r in &realms {
            let (clients, users) =
                tokio::join!(c.admin.list_clients(&r.id), c.admin.list_users(&r.id));
            let (clients, users) = (clients.unwrap_or_default(), users.unwrap_or_default());
            let name = r.spec.name.as_str();
            let crumbs = vec![section.clone(), r.spec.display_name.clone()];
            for cl in clients.iter().filter(|c| {
                !c.spec
                    .client_id
                    .starts_with(helios_idp_admin::service::DIAGNOSTIC_PREFIX)
            }) {
                let href = format!("/ui/idp/{name}/clients/{}", cl.spec.client_id);
                let mut path = crumbs.clone();
                path.push(i18n.t("idpc-tab-clients"));
                if let Some(h) = data_hit(
                    &toks,
                    "gs-kind-app",
                    "key",
                    &cl.spec.name,
                    &[cl.spec.client_id.as_str()],
                    path,
                    href,
                ) {
                    out.push(h);
                }
            }
            for u in users.iter().filter(|u| {
                !u.username
                    .starts_with(helios_idp_admin::service::DIAGNOSTIC_PREFIX)
            }) {
                let href = format!("/ui/idp/{name}/users/{}", u.id);
                let mut path = crumbs.clone();
                path.push(i18n.t("idpc-tab-users"));
                let display = u.display_name.clone().unwrap_or_default();
                let email = u.email.clone().unwrap_or_default();
                if let Some(h) = data_hit(
                    &toks,
                    "gs-kind-user",
                    "users",
                    &u.username,
                    &[display.as_str(), email.as_str()],
                    path,
                    href,
                ) {
                    out.push(h);
                }
            }
        }
        out.sort_by_key(|h| std::cmp::Reverse(h.score));
        out.truncate(MAX_PER_SOURCE);
        out
    };

    let sync = async {
        let mut out = Vec::new();
        let (true, Some(rt)) = (local, &state.manager) else {
            return out;
        };
        if !access.can_open("/ui/sync") {
            return out;
        }
        let section = i18n.t(NavSection::Server.label_key());
        let page = i18n.t("sync-title");
        let jobs = crate::manager::pages::sync_jobs_of(&rt.backend)
            .await
            .unwrap_or_default();
        for v in jobs {
            let description = v.job.spec.description.clone().unwrap_or_default();
            let route = i18n.t_arg2(
                "gs-sync-route",
                "source",
                peer_label(&v.job.spec.source),
                "target",
                peer_label(&v.job.spec.target),
            );
            if let Some(h) = data_hit(
                &toks,
                "gs-kind-sync",
                "sync",
                &v.job.spec.name,
                &[description.as_str(), route.as_str()],
                vec![section.clone(), page.clone()],
                format!("/ui/sync/{}", v.job.id),
            ) {
                out.push(h);
            }
        }
        out.sort_by_key(|h| std::cmp::Reverse(h.score));
        out.truncate(MAX_PER_SOURCE);
        out
    };

    let terminology = async {
        let mut out = Vec::new();
        let Some(up) = crate::terminology_console::upstream() else {
            return out;
        };
        if part == DataPart::Local
            || nav_role(access.terminology) < NavRole::Viewer
            || q_trim.chars().count() < 3
            || resource_ref(q_trim).is_some()
        {
            return out;
        }
        let section = i18n.t(NavSection::Terminology.label_key());
        let by_url = q_trim.contains("://") || q_trim.starts_with("urn:");
        let mut cs = helios_hts_ui::CsBrowserFilters {
            count: MAX_PER_SOURCE as u32,
            ..Default::default()
        };
        let mut vs = helios_hts_ui::VsBrowserFilters {
            count: MAX_PER_SOURCE as u32,
            ..Default::default()
        };
        if by_url {
            cs.url = Some(q_trim.to_string());
            vs.url = Some(q_trim.to_string());
        } else {
            cs.title = Some(q_trim.to_string());
            vs.title = Some(q_trim.to_string());
        }
        let (cs, vs) = tokio::join!(up.search_code_systems(&cs), up.search_value_sets(&vs));
        let ui = helios_hts_ui::paths::get().ui;
        if let Ok(page) = cs {
            let crumbs = vec![section.clone(), i18n.t("hts-nav-code-systems")];
            for r in page.rows {
                let label = if r.title.is_empty() {
                    r.name.clone()
                } else {
                    r.title.clone()
                };
                let href = format!("{ui}/code-systems/{}", r.id);
                let hit = data_hit(
                    &toks,
                    "gs-kind-codesystem",
                    "book",
                    &label,
                    &[r.url.as_str(), r.name.as_str()],
                    crumbs.clone(),
                    href.clone(),
                )
                .unwrap_or_else(|| {
                    plain_hit("gs-kind-codesystem", "book", &label, crumbs.clone(), href)
                });
                out.push(hit);
            }
        }
        if let Ok(page) = vs {
            let crumbs = vec![section.clone(), i18n.t("hts-nav-value-sets")];
            for r in page.rows {
                let label = if r.title.is_empty() {
                    r.name.clone()
                } else {
                    r.title.clone()
                };
                let href = format!("{ui}/value-sets/{}", r.id);
                let hit = data_hit(
                    &toks,
                    "gs-kind-valueset",
                    "bookmark",
                    &label,
                    &[r.url.as_str(), r.name.as_str()],
                    crumbs.clone(),
                    href.clone(),
                )
                .unwrap_or_else(|| {
                    plain_hit("gs-kind-valueset", "bookmark", &label, crumbs.clone(), href)
                });
                out.push(hit);
            }
        }
        out.truncate(MAX_PER_SOURCE * 2);
        out
    };

    let resource = async {
        let mut out = Vec::new();
        if !local {
            return out;
        }
        let (Some((ty, id)), Some(storage), Some(rt)) = (
            resource_ref(q_trim),
            state.tenants.as_ref(),
            state.manager.as_ref(),
        ) else {
            return out;
        };
        let snapshot = rt.registry.snapshot();
        for ep in endpoints.iter().take(8) {
            let Some(e) = snapshot.endpoints().iter().find(|e| e.name() == ep.name) else {
                continue;
            };
            let href = format!("/ui/e/{}/data/editor?type={ty}&id={id}", ep.name);
            if !access.can_open(&href) {
                continue;
            }
            let ctx = helios_persistence::tenant::TenantContext::new(
                helios_persistence::tenant::TenantId::new(e.tenant_id()),
                helios_persistence::tenant::TenantPermissions::read_only(),
            );
            if let Ok(Some(_)) = storage.read(&ctx, ty, id).await {
                let label = format!("{ty}/{id}");
                out.push(Hit {
                    score: 200 + if ep.current { 10 } else { 0 },
                    ..plain_hit(
                        "gs-kind-resource",
                        "layers",
                        &label,
                        vec![root.clone(), ep.display.clone()],
                        href,
                    )
                });
            }
        }
        out
    };

    let terminology = async {
        tokio::time::timeout(TERMINOLOGY_TIMEOUT, terminology)
            .await
            .unwrap_or_default()
    };
    let (idp, sync, terminology, resource) =
        tokio::join!(timed(idp), timed(sync), terminology, timed(resource));
    hits.extend(resource);
    hits.extend(idp);
    hits.extend(sync);
    hits.extend(terminology);
    hits.sort_by_key(|h| std::cmp::Reverse(h.score));
    hits.truncate(MAX_DATA);
    if hits.is_empty() {
        return Vec::new();
    }
    let (id, title_key) = match part {
        DataPart::Terminology => ("tx", "gs-group-terminology"),
        _ => ("data", "gs-group-data"),
    };
    vec![Group {
        id,
        title_key,
        hits,
    }]
}

fn plain_hit(
    kind_key: &'static str,
    icon: &'static str,
    label: &str,
    crumbs: Vec<String>,
    href: String,
) -> Hit {
    Hit {
        dom_id: String::new(),
        icon,
        kind_key,
        segs: vec![Seg {
            text: label.to_string(),
            mark: false,
        }],
        crumbs,
        href,
        target: String::new(),
        fallback: String::new(),
        full_load: false,
        note: String::new(),
        score: 50,
    }
}

fn peer_label(peer: &helios_endpoints::sync::SyncPeer) -> String {
    match peer {
        helios_endpoints::sync::SyncPeer::Endpoint { name } => name.clone(),
        helios_endpoints::sync::SyncPeer::External(server) => server.base_url.clone(),
    }
}

/// Numbers the hits for `aria-activedescendant` (unique per half).
fn number(groups: &mut [Group], prefix: &str) {
    let mut n = 0;
    for g in groups.iter_mut() {
        for h in g.hits.iter_mut() {
            h.dom_id = format!("gs-{prefix}-{n}");
            n += 1;
        }
    }
}

// ---------------------------------------------------------------- handler

/// `GET /ui/find` parameters.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct FindQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    part: Option<String>,
}

/// The dropdown's half.
#[derive(Template)]
#[template(path = "partials/global-search-results.html")]
struct ResultsFragment {
    i18n: I18n,
    groups: Vec<Group>,
    part: &'static str,
}

/// The no-JavaScript results page.
#[derive(Template)]
#[template(path = "pages/global-search.html")]
struct ResultsPage {
    status: Status,
    i18n: I18n,
    active_page: &'static str,
    query: String,
    groups: Vec<Group>,
    part: &'static str,
}

impl ResultsFragment {
    fn count(&self) -> usize {
        self.groups.iter().map(|g| g.hits.len()).sum()
    }
}

impl ResultsPage {
    fn count(&self) -> usize {
        self.groups.iter().map(|g| g.hits.len()).sum()
    }
}

/// Everything a request needs from the server state to build the index.
struct Context {
    endpoints: Vec<Ep>,
    realms: Vec<Realm>,
}

async fn context(state: &WebState, access: &Access<'_>, rt: &RequestTenant) -> Context {
    let mut endpoints = Vec::new();
    if let Some(m) = &state.manager {
        for e in m.registry.snapshot().endpoints() {
            let name = e.name().to_string();
            if !access.can_open(&format!("/ui/e/{name}")) {
                continue;
            }
            endpoints.push(Ep {
                current: rt.nav.endpoint.as_deref() == Some(name.as_str()),
                display: e.spec.display_name.clone(),
                path: if e.spec.path.is_root() {
                    "/".to_string()
                } else {
                    format!("/{}", e.spec.path.as_str())
                },
                is_default: e.spec.path.is_root(),
                name,
            });
        }
        // The endpoint in view first.
        endpoints.sort_by_key(|e| !e.current);
    }
    let mut realms = Vec::new();
    if nav_role(access.global()) >= NavRole::Admin
        && let Some(c) = helios_idp_admin::service::global()
    {
        let list = timed(async { c.realms().await.unwrap_or_default() }).await;
        realms = list
            .into_iter()
            .map(|r| Realm {
                name: r.spec.name.as_str().to_string(),
                display: r.spec.display_name.clone(),
            })
            .collect();
    }
    Context { endpoints, realms }
}

/// `GET /ui/find?q=…[&part=static|data]`.
pub(crate) async fn find(
    State(state): State<WebState>,
    locale: RequestLocale,
    rv: RequestVersion,
    rt: RequestTenant,
    Query(query): Query<FindQuery>,
    request: axum::extract::Request,
) -> Response {
    let principal = request.extensions().get::<ConsolePrincipal>().cloned();
    let console_signin = request
        .extensions()
        .get::<Arc<helios_admin_auth::console::ConsoleAuth>>()
        .is_some();
    let access = Access {
        principal: principal.as_ref(),
        fallback: rt.role,
        terminology: rt.nav.roles.terminology,
    };
    let i18n = I18n::new(locale);
    let en = I18n::english();
    let q: String = query.q.chars().take(MAX_QUERY).collect();
    let ctx = context(&state, &access, &rt).await;
    let builder = |ctx: Context| Builder {
        i18n: &i18n,
        en: &en,
        access: &access,
        items: crate::manager::nav::all_items(),
        endpoints: ctx.endpoints,
        realms: ctx.realms,
        package_admin: crate::package_admin(),
        console_signin,
        out: Vec::new(),
    };
    let mut response = match query.part.as_deref() {
        Some("static") => {
            let mut groups = static_groups(builder(ctx), &q);
            number(&mut groups, "s");
            render(ResultsFragment {
                i18n,
                groups,
                part: "static",
            })
        }
        Some("data") => {
            let mut groups =
                data_groups(&state, &access, &i18n, &ctx.endpoints, &q, DataPart::Local).await;
            number(&mut groups, "d");
            render(ResultsFragment {
                i18n,
                groups,
                part: "data",
            })
        }
        Some("tx") => {
            let mut groups = data_groups(
                &state,
                &access,
                &i18n,
                &ctx.endpoints,
                &q,
                DataPart::Terminology,
            )
            .await;
            number(&mut groups, "t");
            render(ResultsFragment {
                i18n,
                groups,
                part: "tx",
            })
        }
        _ => {
            let data = data_groups(&state, &access, &i18n, &ctx.endpoints, &q, DataPart::All).await;
            let mut groups = static_groups(builder(ctx), &q);
            groups.extend(data);
            number(&mut groups, "p");
            render(ResultsPage {
                status: current_status(&state, rv.0, &rt),
                i18n,
                active_page: "global-search",
                query: q,
                groups,
                part: "page",
            })
        }
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_prefers_starts_and_folds_case() {
        assert_eq!(best_match(&fold("JWKS URL"), &fold("jwks")), Some((0, 3)));
        assert_eq!(
            best_match(&fold("Public JWKS URI"), &fold("jwks")),
            Some((7, 2))
        );
        assert_eq!(best_match(&fold("Rate limits"), &fold("mit")), Some((7, 1)));
        assert_eq!(best_match(&fold("術語服務"), &fold("術語")), Some((0, 3)));
        assert_eq!(best_match(&fold("端點同步"), &fold("同步")), Some((2, 2)));
        assert!(best_match(&fold("abc"), &fold("abcd")).is_none());
    }

    #[test]
    fn segments_mark_ranges() {
        let s = segments("JWKS URL", &[(0, 4)]);
        assert_eq!(s.len(), 2);
        assert!(s[0].mark && s[0].text == "JWKS");
        assert!(!s[1].mark && s[1].text == " URL");
    }

    #[test]
    fn endpoint_agnostic_keys() {
        assert_eq!(
            endpoint_agnostic("/ui/e/lab/security#f-x"),
            "/ui/e/*/security#f-x"
        );
        assert_eq!(endpoint_agnostic("/ui/e/lab"), "/ui/e/*");
        assert_eq!(endpoint_agnostic("/ui/about"), "/ui/about");
    }

    #[test]
    fn resource_refs() {
        assert_eq!(resource_ref("Patient/123"), Some(("Patient", "123")));
        assert_eq!(
            resource_ref(" Observation/a-b.c "),
            Some(("Observation", "a-b.c"))
        );
        assert!(resource_ref("patient/123").is_none());
        assert!(resource_ref("Patient/").is_none());
        assert!(resource_ref("Patient/1 2").is_none());
    }

    fn read_dir_all(dir: &std::path::Path, out: &mut String) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                read_dir_all(&path, out);
            } else if path.extension().is_some_and(|e| e == "html") {
                out.push_str(&std::fs::read_to_string(&path).unwrap());
                out.push('\n');
            }
        }
    }

    fn templates() -> String {
        let mut out = String::new();
        read_dir_all(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates"),
            &mut out,
        );
        out
    }

    /// Every anchor of the index is a stable id in a template (the
    /// rendered pages are checked by `tests/global_search_http.rs`).
    #[test]
    fn every_anchor_is_in_a_template() {
        let html = templates();
        let settings = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../admin-auth/src/console/settings.rs"),
        )
        .unwrap();
        for s in SPOTS {
            let a = s.anchor;
            let literal = html.contains(&format!("id=\"{a}\""))
                || html.contains(&format!("form_section(\"{a}\""));
            let macro_field = a.strip_prefix("f-").is_some_and(|name| {
                html.lines()
                    .any(|l| l.contains("ui::") && l.contains(&format!(", \"{name}\"")))
            });
            let console_field = a.strip_prefix("cs-").is_some_and(|name| {
                html.contains("id=\"cs-{{ name }}\"") && settings.contains(&format!("\"{name}\""))
            });
            assert!(
                literal || macro_field || console_field,
                "{}#{a}: no element with this id in any template",
                s.page
            );
        }
    }

    fn ftl_keys(locale: &str) -> std::collections::BTreeSet<String> {
        use fluent_syntax::ast::Entry;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../locales")
            .join(locale);
        let mut keys = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "ftl") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let resource = fluent_syntax::parser::parse(source)
                .unwrap_or_else(|(_, e)| panic!("{}: {e:?}", path.display()));
            for e in resource.body {
                if let Entry::Message(m) = e {
                    keys.insert(m.id.name.to_string());
                }
            }
        }
        keys
    }

    /// Every label the search shows exists in English and Traditional
    /// Chinese (German and Spanish fall back to English).
    #[test]
    fn every_label_key_is_in_en_and_zh_hant() {
        let mut needed: Vec<&str> = Vec::new();
        needed.extend(SPOTS.iter().map(|s| s.label));
        needed.extend(EXTRA_PAGES.iter().map(|p| p.label));
        needed.extend(UI_KEYS.iter().copied());
        needed.extend(crate::manager::nav::all_items().iter().map(|i| i.label_key));
        for section in [
            NavSection::Manager,
            NavSection::EndpointData,
            NavSection::EndpointConfigure,
            NavSection::EndpointOperate,
            NavSection::Terminology,
            NavSection::AuthorizationServers,
            NavSection::Server,
            NavSection::Settings,
        ] {
            needed.push(section.label_key());
        }
        needed.extend([
            "hts-nav-code-systems",
            "hts-nav-value-sets",
            "idpc-tab-clients",
            "idpc-tab-users",
            "sync-title",
        ]);
        for locale in ["en", "zh-Hant"] {
            let keys = ftl_keys(locale);
            for k in &needed {
                assert!(keys.contains(*k), "{locale}: missing {k}");
            }
        }
    }

    /// Spots and synonyms point at pages that exist.
    #[test]
    fn spot_pages_are_known() {
        let mut ids: Vec<&str> = crate::manager::nav::all_items()
            .iter()
            .map(|i| i.id)
            .collect();
        ids.extend(EXTRA_PAGES.iter().map(|p| p.id));
        // Registered by the packages that own them when installed.
        ids.extend([
            "sync",
            "idp-realms",
            "tx-home",
            "tx-code-systems",
            "tx-value-sets",
            "tx-concept-maps",
            "tx-manage",
            "tx-import",
            "tx-jobs",
            "tx-capability",
        ]);
        for s in SPOTS {
            assert!(
                ids.contains(&s.page),
                "spot {}#{} on unknown page",
                s.page,
                s.anchor
            );
        }
        for (id, _) in PAGE_KEYWORDS {
            assert!(ids.contains(id), "synonyms for unknown page {id}");
        }
        for p in EXTRA_PAGES {
            assert!(
                p.parent.is_empty() || ids.contains(&p.parent),
                "{}: unknown parent",
                p.id
            );
        }
    }

    fn admin_access() -> Access<'static> {
        Access {
            principal: None,
            fallback: ConsoleRole::Admin,
            terminology: ConsoleRole::Admin,
        }
    }

    fn build_with<'a>(i18n: &'a I18n, en: &'a I18n, access: &'a Access<'a>, q: &str) -> Vec<Group> {
        static_groups(
            Builder {
                i18n,
                en,
                access,
                items: crate::manager::nav::all_items(),
                endpoints: vec![
                    Ep {
                        name: "default".into(),
                        display: "Default".into(),
                        path: "/".into(),
                        is_default: true,
                        current: true,
                    },
                    Ep {
                        name: "sandbox".into(),
                        display: "Sandbox".into(),
                        path: "/sandbox".into(),
                        is_default: false,
                        current: false,
                    },
                ],
                realms: vec![],
                package_admin: false,
                console_signin: true,
                out: Vec::new(),
            },
            q,
        )
    }

    fn first(groups: &[Group]) -> &Hit {
        &groups[0].hits[0]
    }

    fn path_of(h: &Hit) -> String {
        let label: String = h.segs.iter().map(|s| s.text.as_str()).collect();
        let mut p = h.crumbs.clone();
        p.push(label);
        p.join(" › ")
    }

    #[test]
    fn jwks_finds_the_security_fields_with_their_path() {
        let i18n = I18n::english();
        let access = admin_access();
        let groups = build_with(&i18n, &i18n, &access, "jwks");
        let settings = groups.iter().find(|g| g.id == "settings").unwrap();
        let top = &settings.hits[0];
        assert!(
            top.href.starts_with("/ui/e/default/security#"),
            "{}",
            top.href
        );
        assert!(
            path_of(top).starts_with("FHIR endpoints › Default › Settings › Security › "),
            "{}",
            path_of(top)
        );
        assert!(
            top.segs
                .iter()
                .any(|s| s.mark && s.text.eq_ignore_ascii_case("jwks"))
        );
        // The SMART one names its disclosure.
        assert!(
            settings
                .hits
                .iter()
                .any(|h| path_of(h) == "FHIR endpoints › Default › Settings › Security › FHIR authentication (SMART on FHIR) › SMART discovery document › Public JWKS URI"),
            "{:?}",
            settings.hits.iter().map(path_of).collect::<Vec<_>>()
        );
        // Another endpoint's by naming it.
        let groups = build_with(&i18n, &i18n, &access, "sandbox jwks");
        assert!(first(&groups).href.starts_with("/ui/e/sandbox/security#"));
        // One hit per spot: the endpoint in view only.
        let groups = build_with(&i18n, &i18n, &access, "jwks");
        assert!(
            groups
                .iter()
                .flat_map(|g| &g.hits)
                .all(|h| !h.href.starts_with("/ui/e/sandbox/"))
        );
    }

    #[test]
    fn synonyms_and_chinese_work() {
        let en = I18n::english();
        let access = admin_access();
        let groups = build_with(&en, &en, &access, "oauth");
        assert_eq!(first(&groups).target, "sec-auth");
        assert!(!first(&groups).note.is_empty());
        let groups = build_with(&en, &en, &access, "session timeout");
        assert!(
            first(&groups)
                .href
                .starts_with("/ui/settings/auth#cs-session"),
            "{}",
            first(&groups).href
        );
        let zh = I18n::from_tag("zh-Hant").unwrap();
        let groups = build_with(&zh, &en, &access, "同步");
        assert!(
            groups
                .iter()
                .flat_map(|g| &g.hits)
                .any(|h| h.href.ends_with("/sync"))
        );
        // English words keep working in Chinese.
        let groups = build_with(&zh, &en, &access, "rate limit");
        assert_eq!(first(&groups).target, "sec-rate");
    }

    #[test]
    fn viewers_do_not_see_admin_pages() {
        let en = I18n::english();
        let viewer = Access {
            principal: None,
            fallback: ConsoleRole::Viewer,
            terminology: ConsoleRole::Viewer,
        };
        for q in ["users", "session", "tenant", "new endpoint", "password"] {
            let groups = build_with(&en, &en, &viewer, q);
            for h in groups.iter().flat_map(|g| &g.hits) {
                assert!(
                    !h.href.starts_with("/ui/settings")
                        && !h.href.starts_with("/ui/tenants")
                        && !h.href.starts_with("/ui/endpoints"),
                    "viewer sees {}",
                    h.href
                );
            }
        }
        // But still finds the endpoint configuration (read-only pages).
        let groups = build_with(&en, &en, &viewer, "jwks");
        assert!(!groups.is_empty());
    }

    #[test]
    fn spots_are_unique_and_parents_exist() {
        let mut seen = std::collections::HashSet::new();
        for s in SPOTS {
            assert!(
                seen.insert((s.page, s.anchor)),
                "duplicate spot {}#{}",
                s.page,
                s.anchor
            );
            if !s.parent.is_empty() {
                assert!(
                    SPOTS
                        .iter()
                        .any(|p| p.page == s.page && p.anchor == s.parent),
                    "{}#{}: parent {} missing",
                    s.page,
                    s.anchor,
                    s.parent
                );
            }
        }
    }
}
