//! Step 0 — configuration preflight. Pure checks over the stored realm,
//! clients, keys and endpoint bindings; they run before any request so a
//! misconfiguration is reported at the field that causes it (with a "Go to
//! field" anchor), not only as the protocol error it later produces.

use chrono::{DateTime, Duration, Utc};
use helios_idp::IdpError;
use helios_idp::client::ClientSpec;
use helios_idp::keys::{KeyRecord, KeyStatus};
use helios_idp::realm::RealmSpec;
use serde::{Deserialize, Serialize};
use url::Url;

use super::report::{Check, Step};

/// How an endpoint's auth section points at an authorization server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum EndpointProvider {
    /// External issuer (may still be this realm's issuer URL — the
    /// compatibility binding used until the `Internal` provider lands).
    External {
        issuer: Option<String>,
    },
    Internal {
        realm: String,
    },
}

/// An endpoint as the diagnostic sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointView {
    pub name: String,
    /// Public FHIR base URL (the `aud` the realm issues for it).
    pub base_url: String,
    /// `off` | `write-protect` | `full`.
    pub auth_mode: String,
    pub provider: EndpointProvider,
    /// Default FHIR release (`R4`, `R4B`, `R5`, `R6`; empty = unknown).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fhir_version: String,
}

impl EndpointView {
    /// Whether this endpoint accepts tokens of the realm with `issuer`.
    pub fn trusts(&self, realm: &str, issuer: &str) -> bool {
        match &self.provider {
            EndpointProvider::Internal { realm: r } => r == realm,
            EndpointProvider::External { issuer: Some(i) } => {
                i.trim_end_matches('/') == issuer.trim_end_matches('/')
            }
            EndpointProvider::External { issuer: None } => false,
        }
    }
}

/// A registered app to exercise in addition to the throw-away client:
/// pinpoints "this app's redirect URI is not registered" and "this app is
/// disabled".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppProbe {
    pub client_id: String,
    pub redirect_uri: String,
}

/// Everything the preflight reads.
#[derive(Debug, Clone)]
pub struct PreflightInput<'a> {
    pub realm: &'a RealmSpec,
    pub public_base: &'a Url,
    pub clients: &'a [ClientSpec],
    pub keys: &'a [KeyRecord],
    /// Every endpoint of the server (bound or not).
    pub endpoints: &'a [EndpointView],
    pub app: Option<&'a AppProbe>,
    pub now: DateTime<Utc>,
}

/// Maps a `helios_idp` validation error onto a console field anchor.
pub fn field_of(err: &IdpError) -> Option<String> {
    match err {
        IdpError::Invalid(field, _) => Some(field.clone()),
        _ => None,
    }
}

fn invalid_message(err: &IdpError) -> String {
    match err {
        IdpError::Invalid(field, msg) => format!("{field}: {msg}"),
        other => other.to_string(),
    }
}

pub fn run(input: &PreflightInput<'_>) -> Step {
    let mut step = Step::new(0, "preflight", "Configuration");
    let realm = input.realm;
    let name = realm.name.as_str();
    let issuer = realm.issuer(input.public_base);

    // Realm switched on and valid.
    if realm.enabled {
        step.push(Check::pass(
            "preflight.realm-enabled",
            "realm-enabled",
            "The authorization server is enabled.",
        ));
    } else {
        step.push(
            Check::fail(
                "preflight.realm-enabled",
                "realm-disabled",
                "The authorization server is disabled: every request to it is refused.",
            )
            .field("realm.enabled")
            .fix("Enable it in Settings → General."),
        );
    }
    match realm.validate() {
        Ok(()) => step.push(Check::pass(
            "preflight.realm-valid",
            "realm-valid",
            "Realm settings are valid.",
        )),
        Err(e) => step.push(
            Check::fail(
                "preflight.realm-valid",
                "realm-invalid",
                format!("Realm settings are invalid — {}", invalid_message(&e)),
            )
            .param("detail", invalid_message(&e))
            .field(format!("realm.{}", field_of(&e).unwrap_or_default()))
            .fix("Correct the highlighted setting and save."),
        ),
    }

    keys(&mut step, input);
    clients(&mut step, input);
    endpoints(&mut step, input, name, &issuer);
    step.finish();
    step
}

fn keys(step: &mut Step, input: &PreflightInput<'_>) {
    let realm = input.realm;
    let policy = &realm.rotation;
    for alg in &realm.signing_algs {
        let id = format!("preflight.key.{}", alg.as_str());
        let active: Vec<&KeyRecord> = input
            .keys
            .iter()
            .filter(|k| k.alg == *alg && k.status == KeyStatus::Active)
            .collect();
        let Some(k) = active.first() else {
            step.push(
                Check::fail(
                    &id,
                    "key-missing",
                    format!(
                        "No active {} signing key: tokens cannot be signed with this algorithm.",
                        alg.as_str()
                    ),
                )
                .param("alg", alg.as_str())
                .field("keys")
                .spec("design §6 (one active key per algorithm)")
                .fix("Open Signing keys and choose “Rotate now” to generate a key."),
            );
            continue;
        };
        // A key past its explicit expiry, or overdue for rotation by more
        // than the pre-publication window (the rotation job is not running).
        let since = k.activate_at.unwrap_or(k.created_at);
        let overdue = policy.rotate_after_days > 0
            && input.now
                > since
                    + Duration::days(policy.rotate_after_days as i64)
                    + Duration::hours(policy.prepublish_hours.max(1) as i64);
        let expired = k.expire_at.is_some_and(|e| e <= input.now);
        if expired || overdue {
            let age_days = (input.now - since).num_days();
            step.push(
                Check::fail(&id, "key-expired", format!(
                    "The active {} key {} is expired ({} days old; the rotation policy is {} days). Relying parties may refuse its tokens.",
                    alg.as_str(), short(&k.kid), age_days, policy.rotate_after_days
                ))
                .param("alg", alg.as_str())
                .param("kid", short(&k.kid))
                .param("days", age_days.to_string())
                .param("policy", policy.rotate_after_days.to_string())
                .field("keys")
                .spec("design §6 rotation")
                .fix("Open Signing keys and choose “Rotate now”. If this keeps happening, the rotation job is not running on any instance."),
            );
        } else {
            step.push(
                Check::pass(
                    &id,
                    "key-active",
                    format!("Active {} key {}.", alg.as_str(), short(&k.kid)),
                )
                .param("alg", alg.as_str())
                .param("kid", short(&k.kid)),
            );
        }
        if active.len() > 1 {
            step.push(
                Check::warn(
                    &format!("{id}.duplicate"),
                    "key-duplicate-active",
                    format!(
                        "{} active {} keys; only one should sign.",
                        active.len(),
                        alg.as_str()
                    ),
                )
                .param("alg", alg.as_str())
                .field("keys"),
            );
        }
    }
}

fn clients(step: &mut Step, input: &PreflightInput<'_>) {
    if input.clients.is_empty() {
        step.push(
            Check::warn(
                "preflight.clients",
                "clients-none",
                "No apps are registered: nothing can sign in yet.",
            )
            .field("clients")
            .fix("Add an app under Clients."),
        );
    }
    for c in input.clients {
        let base = format!("preflight.client.{}", c.client_id);
        if let Err(e) = c.validate() {
            step.push(
                Check::fail(
                    &format!("{base}.valid"),
                    "client-invalid",
                    format!(
                        "App {} is misconfigured — {}",
                        c.client_id,
                        invalid_message(&e)
                    ),
                )
                .param("client", &c.client_id)
                .param("detail", invalid_message(&e))
                .field(format!(
                    "client.{}.{}",
                    c.client_id,
                    field_of(&e).unwrap_or_default()
                ))
                .fix("Open the app and correct the highlighted field."),
            );
        }
        if c.disabled {
            step.push(
                Check::warn(
                    &format!("{base}.disabled"),
                    "client-disabled",
                    format!(
                        "App {} is disabled: it gets invalid_client on every request.",
                        c.client_id
                    ),
                )
                .param("client", &c.client_id)
                .field(format!("client.{}.disabled", c.client_id))
                .fix("Enable the app if it should be able to sign in."),
            );
        }
    }
    if let Some(app) = input.app {
        let base = format!("preflight.app.{}", app.client_id);
        match input.clients.iter().find(|c| c.client_id == app.client_id) {
            None => step.push(
                Check::fail(
                    &base,
                    "app-unknown",
                    format!(
                        "No app with client ID {} is registered in this authorization server.",
                        app.client_id
                    ),
                )
                .param("client", &app.client_id)
                .field("clients")
                .fix("Check the client ID the app is configured with, or register the app."),
            ),
            Some(c) => {
                if c.disabled {
                    step.push(
                        Check::fail(&format!("{base}.disabled"), "app-disabled", format!("App {} is disabled: the authorization server answers invalid_client.", c.client_id))
                            .param("client", &c.client_id)
                            .field(format!("client.{}.disabled", c.client_id))
                            .spec("RFC 6749 §5.2 invalid_client")
                            .fix("Open the app and clear “Disabled”."),
                    );
                }
                if c.redirect_uri_allowed(&app.redirect_uri) {
                    step.push(
                        Check::pass(
                            &format!("{base}.redirect"),
                            "app-redirect-ok",
                            format!(
                                "Redirect URI {} is registered for {}.",
                                app.redirect_uri, c.client_id
                            ),
                        )
                        .param("client", &c.client_id)
                        .param("uri", &app.redirect_uri),
                    );
                } else {
                    let registered = if c.redirect_uris.is_empty() {
                        "(none)".to_string()
                    } else {
                        c.redirect_uris.join(", ")
                    };
                    step.push(
                        Check::fail(&format!("{base}.redirect"), "app-redirect-unregistered", format!(
                            "Redirect URI {} is not registered for app {}. Registered: {}. The sign-in page shows an error and never redirects.",
                            app.redirect_uri, c.client_id, registered
                        ))
                        .param("client", &c.client_id)
                        .param("uri", &app.redirect_uri)
                        .param("registered", registered)
                        .field(format!("client.{}.redirectUris", c.client_id))
                        .spec("RFC 9700 §4.1.3 (exact redirect URI matching)")
                        .fix("Add the exact URI (scheme, host, port, path; no wildcard) to the app's redirect URIs, or fix the URI in the app."),
                    );
                }
            }
        }
    }
}

fn endpoints(step: &mut Step, input: &PreflightInput<'_>, realm: &str, issuer: &str) {
    let bound = &input.realm.endpoints;
    if bound.is_empty() {
        step.push(
            Check::warn("preflight.endpoints", "endpoints-none", "No FHIR endpoint is bound: tokens from this server are accepted nowhere.")
                .field("endpoints")
                .fix("Bind an endpoint under Settings → Endpoints (its Security page switches to Internal)."),
        );
    }
    for ep in bound {
        let id = format!("preflight.endpoint.{ep}");
        match input.endpoints.iter().find(|e| &e.name == ep) {
            None => step.push(
                Check::fail(&id, "endpoint-missing", format!("Endpoint {ep} is listed as bound but does not exist."))
                    .param("endpoint", ep)
                    .field(format!("endpoints.{ep}"))
                    .fix("Remove it from the bound endpoints, or recreate the endpoint."),
            ),
            Some(e) if !e.trusts(realm, issuer) => {
                let actual = match &e.provider {
                    EndpointProvider::External { issuer: Some(i) } => i.clone(),
                    EndpointProvider::External { issuer: None } if e.auth_mode == "inherit" => "the server-wide authentication settings".into(),
                    EndpointProvider::External { issuer: None } => "no authorization server".into(),
                    EndpointProvider::Internal { realm } => format!("internal realm {realm}"),
                };
                step.push(
                    Check::fail(&id, "endpoint-not-bound", format!(
                        "Endpoint {ep} is listed here but its Security settings trust {actual}, not this server ({issuer}): its tokens are refused there."
                    ))
                    .param("endpoint", ep)
                    .param("actual", actual)
                    .param("expected", issuer)
                    .field(format!("endpoints.{ep}"))
                    .fix("Bind the endpoint again (Settings → Endpoints → Bind), or set its Security → Authorization server to Internal with this server."),
                );
            }
            Some(e) if e.auth_mode == "off" => step.push(
                Check::warn(&id, "endpoint-auth-off", format!("Endpoint {ep} trusts this server but its auth mode is Off: tokens are ignored and data is open."))
                    .param("endpoint", ep)
                    .field(format!("endpoints.{ep}"))
                    .fix("Set the endpoint's auth mode to Write-protect or Full."),
            ),
            Some(_) => step.push(
                Check::pass(&id, "endpoint-bound", format!("Endpoint {ep} accepts tokens from this server."))
                    .param("endpoint", ep),
            ),
        }
    }
    // Endpoints trusting this issuer without being bound: their audience is
    // not issued, so every token is refused with "invalid audience".
    for e in input.endpoints {
        if !bound.contains(&e.name) && e.trusts(realm, issuer) {
            step.push(
                Check::fail(&format!("preflight.endpoint.{}.unlisted", e.name), "endpoint-unlisted", format!(
                    "Endpoint {} trusts this server but is not in its bound endpoints: tokens are not issued for its audience.", e.name
                ))
                .param("endpoint", &e.name)
                .field(format!("endpoints.{}", e.name))
                .fix("Add the endpoint to the bound endpoints."),
            );
        }
    }
}

pub(crate) fn short(kid: &str) -> String {
    kid.chars().take(10).collect()
}

#[cfg(test)]
mod tests {
    use super::super::report::Status;
    use super::*;
    use helios_idp::client::{ClientAuth, GrantType, LaunchType};
    use helios_idp::keys::SigningAlg;
    use helios_idp::realm::RealmName;
    use helios_idp::scope::ScopePolicy;

    fn realm() -> RealmSpec {
        let mut r = RealmSpec::new(RealmName::parse("clinic").unwrap(), "Clinic");
        r.endpoints = vec!["ep1".into()];
        r
    }

    fn key(alg: SigningAlg, age_days: i64, now: DateTime<Utc>) -> KeyRecord {
        KeyRecord {
            kid: format!("kid-{}", alg.as_str()),
            alg,
            status: KeyStatus::Active,
            public_jwk: serde_json::json!({}),
            created_at: now - Duration::days(age_days),
            activate_at: Some(now - Duration::days(age_days)),
            retire_at: None,
            expire_at: None,
        }
    }

    fn client(id: &str) -> ClientSpec {
        ClientSpec {
            client_id: id.into(),
            name: id.into(),
            auth: ClientAuth::None,
            grant_types: vec![GrantType::AuthorizationCode],
            redirect_uris: vec!["https://app.example.org/cb".into()],
            post_logout_redirect_uris: vec![],
            launch_types: vec![LaunchType::Standalone],
            scopes: ScopePolicy::default(),
            trusted: false,
            require_dpop: false,
            require_par: false,
            backchannel_logout_uri: None,
            access_token_ttl_secs: None,
            disabled: false,
        }
    }

    fn ep(issuer: Option<&str>) -> EndpointView {
        EndpointView {
            name: "ep1".into(),
            base_url: "https://h/fhir/ep1".into(),
            auth_mode: "full".into(),
            provider: EndpointProvider::External {
                issuer: issuer.map(String::from),
            },
            fhir_version: "R4".into(),
        }
    }

    struct Fx {
        realm: RealmSpec,
        base: Url,
        clients: Vec<ClientSpec>,
        keys: Vec<KeyRecord>,
        eps: Vec<EndpointView>,
        app: Option<AppProbe>,
        now: DateTime<Utc>,
    }

    impl Fx {
        fn healthy() -> Self {
            let now = Utc::now();
            Self {
                realm: realm(),
                base: Url::parse("https://h").unwrap(),
                clients: vec![client("app")],
                keys: vec![
                    key(SigningAlg::RS256, 3, now),
                    key(SigningAlg::ES256, 3, now),
                ],
                eps: vec![ep(Some("https://h/idp/clinic"))],
                app: Some(AppProbe {
                    client_id: "app".into(),
                    redirect_uri: "https://app.example.org/cb".into(),
                }),
                now,
            }
        }
        fn run(&self) -> Step {
            run(&PreflightInput {
                realm: &self.realm,
                public_base: &self.base,
                clients: &self.clients,
                keys: &self.keys,
                endpoints: &self.eps,
                app: self.app.as_ref(),
                now: self.now,
            })
        }
    }

    fn failing(s: &Step) -> Vec<(String, Option<String>)> {
        s.checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .map(|c| (c.code.clone(), c.field.clone()))
            .collect()
    }

    fn f(code: &str, field: &str) -> (String, Option<String>) {
        (code.to_string(), Some(field.to_string()))
    }

    #[test]
    fn healthy_realm_passes() {
        let s = Fx::healthy().run();
        assert_eq!(s.status, Status::Pass, "{:#?}", s.checks);
    }

    #[test]
    fn wrong_redirect_uri_is_pinpointed() {
        let mut fx = Fx::healthy();
        fx.clients[0].redirect_uris = vec!["https://app.example.org/callback".into()];
        let s = fx.run();
        assert_eq!(
            failing(&s),
            vec![f("app-redirect-unregistered", "client.app.redirectUris")]
        );
        let c = s
            .checks
            .iter()
            .find(|c| c.code == "app-redirect-unregistered")
            .unwrap();
        assert!(c.message.contains("https://app.example.org/callback"));
        assert!(!c.fix.is_empty());
    }

    #[test]
    fn disabled_client_is_pinpointed() {
        let mut fx = Fx::healthy();
        fx.clients[0].disabled = true;
        let s = fx.run();
        assert_eq!(failing(&s), vec![f("app-disabled", "client.app.disabled")]);
        // Without an app probe it is still a warning.
        fx.app = None;
        let s = fx.run();
        assert_eq!(s.status, Status::Warn);
    }

    #[test]
    fn expired_and_missing_keys() {
        let mut fx = Fx::healthy();
        fx.keys[1] = key(SigningAlg::ES256, 200, fx.now);
        let s = fx.run();
        assert_eq!(failing(&s), vec![f("key-expired", "keys")]);
        fx.keys[1].activate_at = Some(fx.now - Duration::days(1));
        fx.keys[1].expire_at = Some(fx.now - Duration::minutes(1));
        assert_eq!(failing(&fx.run()), vec![f("key-expired", "keys")]);
        fx.keys.pop();
        assert_eq!(failing(&fx.run()), vec![f("key-missing", "keys")]);
    }

    #[test]
    fn endpoint_not_bound_is_pinpointed() {
        let mut fx = Fx::healthy();
        fx.eps[0] = ep(Some("https://keycloak.example/realms/x"));
        assert_eq!(
            failing(&fx.run()),
            vec![f("endpoint-not-bound", "endpoints.ep1")]
        );
        fx.eps[0].provider = EndpointProvider::Internal {
            realm: "clinic".into(),
        };
        assert_eq!(fx.run().status, Status::Pass);
        // Trusted but unlisted.
        fx.realm.endpoints.clear();
        let got = failing(&fx.run());
        assert_eq!(got, vec![f("endpoint-unlisted", "endpoints.ep1")]);
        fx.realm.endpoints = vec!["gone".into()];
        let got = failing(&fx.run());
        assert!(got.contains(&f("endpoint-missing", "endpoints.gone")));
    }

    #[test]
    fn invalid_realm_maps_to_field() {
        let mut fx = Fx::healthy();
        fx.realm.lifetimes.access_token = 9999;
        assert_eq!(
            failing(&fx.run()),
            vec![f("realm-invalid", "realm.lifetimes.accessToken")]
        );
    }
}
