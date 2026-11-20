//! Credentials of external peers: write-only API inputs, sealed at rest
//! (design D-12), opened only in memory by the worker that uses them.
//!
//! A job body carries no credential values in the peer definitions; it
//! carries `secrets: { "<side>.auth.<field>": SecretInput }`. The current
//! sealed value is kept when the input is `keep` (the default) and the
//! auth type did not change, so a client that PUTs back a GET response (where
//! every secret reads `{"set": true}`) never clears or replaces a secret.
//! AAD: `sync-job:{job id}:{side}.auth.{field}`.

use std::collections::BTreeMap;

use base64::Engine as _;
use helios_endpoints::model::ValidationErrors;
use helios_endpoints::secret::{
    ENVELOPE_PREFIX, Plaintext, SealedSecret, SecretInput, SecretKeyRing, redact_sealed,
};
use helios_endpoints::sync::{ExternalAuth, SyncJob, SyncJobSpec, SyncPeer, SyncTrigger};
use rand::RngCore;
use serde_json::Value;

use crate::error::SyncError;

/// The sealed field of each auth type (JSON name).
pub fn secret_field(auth_type: &str) -> Option<&'static str> {
    match auth_type {
        "bearer" => Some("token"),
        "basic" => Some("password"),
        "client-credentials" => Some("clientSecret"),
        "private-key-jwt" => Some("privateKeyJwk"),
        _ => None,
    }
}

/// AAD of one secret of a job.
pub fn aad(job_id: &str, path: &str) -> String {
    format!("sync-job:{job_id}:{path}")
}

fn current_secret(current: Option<&SyncJobSpec>, side: &str, auth_type: &str) -> Option<SealedSecret> {
    let peer = match side {
        "source" => &current?.source,
        _ => &current?.target,
    };
    let SyncPeer::External(x) = peer else {
        return None;
    };
    match (&x.auth, auth_type) {
        (ExternalAuth::Bearer { token }, "bearer") => Some(token.clone()),
        (ExternalAuth::Basic { password, .. }, "basic") => Some(password.clone()),
        (ExternalAuth::ClientCredentials { client_secret, .. }, "client-credentials") => {
            Some(client_secret.clone())
        }
        (ExternalAuth::PrivateKeyJwt { private_key_jwk, .. }, "private-key-jwt") => {
            Some(private_key_jwk.clone())
        }
        _ => None,
    }
}

/// Turns an API body into a stored spec: applies `secrets` inputs (sealing
/// new values under `job_id`), keeps current secrets, generates the notify
/// secret of a subscription trigger (returned in plaintext once, when new).
pub fn spec_from_body(
    mut body: Value,
    job_id: &str,
    current: Option<&SyncJobSpec>,
    ring: Option<&SecretKeyRing>,
) -> Result<(SyncJobSpec, Option<String>), SyncError> {
    let obj = body
        .as_object_mut()
        .ok_or_else(|| crate::error::invalid("", "the body must be a JSON object"))?;
    let inputs: BTreeMap<String, SecretInput> = match obj.remove("secrets") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(v) => serde_json::from_value(v)
            .map_err(|e| crate::error::invalid("secrets", format!("invalid secret inputs: {e}")))?,
    };
    // Server-maintained fields are never taken from a body.
    for k in ["id", "revision", "createdAt", "updatedAt", "createdBy", "nextRunAt", "spec"] {
        obj.remove(k);
    }
    let mut errors = ValidationErrors::default();
    let mut used: Vec<String> = Vec::new();
    for side in ["source", "target"] {
        let Some(auth) = obj
            .get_mut(side)
            .filter(|p| p.get("kind").and_then(Value::as_str) == Some("external"))
            .and_then(|p| p.get_mut("auth"))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        let auth_type = auth
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("none")
            .to_string();
        let Some(field) = secret_field(&auth_type) else {
            continue;
        };
        let path = format!("{side}.auth.{field}");
        // A value in the body itself is only accepted as the redacted
        // marker a GET returns (= keep); never as plaintext or an envelope.
        match auth.remove(field) {
            None | Some(Value::Null) => {}
            Some(Value::Object(m)) if m.get("set") == Some(&Value::Bool(true)) => {}
            Some(_) => {
                errors.push(
                    &path,
                    format!("send credentials in \"secrets\": {{\"{path}\": {{\"set\": \"…\"}}}}"),
                );
                continue;
            }
        }
        let input = inputs.get(&path).cloned().unwrap_or_default();
        used.push(path.clone());
        let now = current_secret(current, side, &auth_type);
        match input.apply(now, ring, &aad(job_id, &path)) {
            Ok(Some(sealed)) => {
                auth.insert(field.to_string(), Value::String(sealed.envelope().to_string()));
            }
            Ok(None) => errors.push(&path, "is required for this auth type"),
            Err(_) => return Err(SyncError::NoKeyRing),
        }
    }
    for k in inputs.keys() {
        if !used.contains(k) {
            errors.push(&format!("secrets.{k}"), "no such secret field for this job");
        }
    }
    // Subscription trigger: the notify secret is server-generated.
    let mut fresh_notify = None;
    if let Some(trigger) = obj.get_mut("trigger").and_then(Value::as_object_mut)
        && trigger.get("type").and_then(Value::as_str) == Some("subscription")
    {
        trigger.remove("notifySecret");
        let existing = match current.map(|c| &c.trigger) {
            Some(SyncTrigger::Subscription {
                notify_secret: Some(s),
                ..
            }) => Some(s.clone()),
            _ => None,
        };
        let sealed = match existing {
            Some(s) => s,
            None => {
                let ring = ring.ok_or(SyncError::NoKeyRing)?;
                let mut raw = [0u8; 24];
                rand::rngs::OsRng.fill_bytes(&mut raw);
                let plain = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
                let s = ring.seal_str(&plain, &aad(job_id, "trigger.notifySecret"));
                fresh_notify = Some(plain);
                s
            }
        };
        trigger.insert(
            "notifySecret".into(),
            Value::String(sealed.envelope().to_string()),
        );
    }
    if !errors.0.is_empty() {
        return Err(SyncError::Invalid(errors));
    }
    let spec: SyncJobSpec = serde_json::from_value(body)
        .map_err(|e| crate::error::invalid("", format!("invalid sync job: {e}")))?;
    Ok((spec, fresh_notify))
}

/// The job as the API shows it: every sealed secret is `{"set": true}`.
pub fn redacted_job(job: &SyncJob) -> Value {
    let mut v = serde_json::to_value(job).expect("job serializes");
    redact_sealed(&mut v);
    v
}

/// Whether a JSON text contains any sealed envelope (test / audit helper).
pub fn contains_envelope(text: &str) -> bool {
    text.contains(ENVELOPE_PREFIX)
}

/// Credentials opened for one connection, in memory only.
pub enum ResolvedAuth {
    /// None.
    None,
    /// `Authorization: Bearer`.
    Bearer(Plaintext),
    /// HTTP Basic.
    Basic {
        /// User.
        username: String,
        /// Password.
        password: Plaintext,
    },
    /// OAuth client credentials with a secret.
    ClientCredentials {
        /// Token endpoint.
        token_url: String,
        /// Client id.
        client_id: String,
        /// Secret.
        client_secret: Plaintext,
        /// Scope.
        scope: Option<String>,
    },
    /// SMART Backend Services.
    PrivateKeyJwt {
        /// Token endpoint.
        token_url: String,
        /// Client id.
        client_id: String,
        /// PEM or JWK private key.
        key: Plaintext,
        /// `RS384` / `ES384`.
        alg: String,
        /// Key id.
        kid: Option<String>,
        /// Scope.
        scope: Option<String>,
    },
}

impl std::fmt::Debug for ResolvedAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            ResolvedAuth::None => "none",
            ResolvedAuth::Bearer(_) => "bearer",
            ResolvedAuth::Basic { .. } => "basic",
            ResolvedAuth::ClientCredentials { .. } => "client-credentials",
            ResolvedAuth::PrivateKeyJwt { .. } => "private-key-jwt",
        };
        write!(f, "ResolvedAuth({kind}, [redacted])")
    }
}

/// Opens the credentials of one side of a job.
pub fn open_auth(
    auth: &ExternalAuth,
    job_id: &str,
    side: &str,
    ring: Option<&SecretKeyRing>,
) -> Result<ResolvedAuth, SyncError> {
    let open = |s: &SealedSecret, field: &str| -> Result<Plaintext, SyncError> {
        let ring = ring.ok_or(SyncError::NoKeyRing)?;
        Ok(ring.open(s, &aad(job_id, &format!("{side}.auth.{field}")))?)
    };
    Ok(match auth {
        ExternalAuth::None => ResolvedAuth::None,
        ExternalAuth::Bearer { token } => ResolvedAuth::Bearer(open(token, "token")?),
        ExternalAuth::Basic { username, password } => ResolvedAuth::Basic {
            username: username.clone(),
            password: open(password, "password")?,
        },
        ExternalAuth::ClientCredentials {
            token_url,
            client_id,
            client_secret,
            scope,
        } => ResolvedAuth::ClientCredentials {
            token_url: token_url.clone(),
            client_id: client_id.clone(),
            client_secret: open(client_secret, "clientSecret")?,
            scope: scope.clone(),
        },
        ExternalAuth::PrivateKeyJwt {
            token_url,
            client_id,
            private_key_jwk,
            alg,
            kid,
            scope,
        } => ResolvedAuth::PrivateKeyJwt {
            token_url: token_url.clone(),
            client_id: client_id.clone(),
            key: open(private_key_jwk, "privateKeyJwk")?,
            alg: alg.clone(),
            kid: kid.clone(),
            scope: scope.clone(),
        },
    })
}

/// The notify secret of a subscription-triggered job, in plaintext.
pub fn open_notify_secret(
    spec: &SyncJobSpec,
    job_id: &str,
    ring: Option<&SecretKeyRing>,
) -> Option<Plaintext> {
    let SyncTrigger::Subscription {
        notify_secret: Some(s),
        ..
    } = &spec.trigger
    else {
        return None;
    };
    ring?.open(s, &aad(job_id, "trigger.notifySecret")).ok()
}

/// Re-seals every secret of `spec` under the active key (key rotation).
pub fn rewrap(spec: &mut SyncJobSpec, job_id: &str, ring: &SecretKeyRing) -> Result<usize, SyncError> {
    let mut n = 0;
    for (side, peer) in [("source", &mut spec.source), ("target", &mut spec.target)] {
        let SyncPeer::External(x) = peer else { continue };
        let (field, s): (&str, &mut SealedSecret) = match &mut x.auth {
            ExternalAuth::None => continue,
            ExternalAuth::Bearer { token } => ("token", token),
            ExternalAuth::Basic { password, .. } => ("password", password),
            ExternalAuth::ClientCredentials { client_secret, .. } => ("clientSecret", client_secret),
            ExternalAuth::PrivateKeyJwt { private_key_jwk, .. } => ("privateKeyJwk", private_key_jwk),
        };
        if ring.needs_rewrap(s) {
            *s = ring.rewrap(s, &aad(job_id, &format!("{side}.auth.{field}")))?;
            n += 1;
        }
    }
    if let SyncTrigger::Subscription {
        notify_secret: Some(s),
        ..
    } = &mut spec.trigger
        && ring.needs_rewrap(s)
    {
        *s = ring.rewrap(s, &aad(job_id, "trigger.notifySecret"))?;
        n += 1;
    }
    Ok(n)
}

/// Constant-time comparison of two secrets.
pub fn secrets_equal(a: &[u8], b: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    let (x, y) = (Sha256::digest(a), Sha256::digest(b));
    x.iter().zip(y.iter()).fold(0u8, |acc, (p, q)| acc | (p ^ q)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ring() -> SecretKeyRing {
        SecretKeyRing::parse(&SecretKeyRing::generate_spec("k1")).unwrap()
    }

    fn body() -> Value {
        json!({
            "name": "push",
            "source": {"kind": "endpoint", "name": "default"},
            "target": {"kind": "external", "baseUrl": "https://hapi.example.org/fhir",
                       "auth": {"type": "bearer"}},
            "secrets": {"target.auth.token": {"set": "tok-123"}}
        })
    }

    #[test]
    fn seals_and_redacts() {
        let r = ring();
        let (spec, notify) = spec_from_body(body(), "job1", None, Some(&r)).unwrap();
        assert!(notify.is_none());
        let SyncPeer::External(x) = &spec.target else { panic!() };
        let ExternalAuth::Bearer { token } = &x.auth else { panic!() };
        assert!(!token.envelope().contains("tok-123"));
        let opened = open_auth(&x.auth, "job1", "target", Some(&r)).unwrap();
        let ResolvedAuth::Bearer(p) = opened else { panic!() };
        assert_eq!(p.expose_str(), Some("tok-123"));
        // Bound to the job id: another job's AAD does not open it.
        assert!(open_auth(&x.auth, "job2", "target", Some(&r)).is_err());
        assert!(format!("{:?}", open_auth(&x.auth, "job1", "target", Some(&r)).unwrap())
            .contains("redacted"));

        let job = SyncJob {
            id: "job1".into(),
            revision: 1,
            spec: spec.clone(),
            next_run_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            created_by: None,
        };
        let shown = redacted_job(&job);
        let text = shown.to_string();
        assert!(!contains_envelope(&text));
        assert_eq!(shown["spec"]["target"]["auth"]["token"], json!({"set": true}));

        // PUT back the GET response (marker = keep): the secret survives.
        let mut back = shown["spec"].clone();
        back["description"] = json!("edited");
        let (again, _) = spec_from_body(back, "job1", Some(&spec), Some(&r)).unwrap();
        assert_eq!(again.target, spec.target);
        assert_eq!(again.description.as_deref(), Some("edited"));
    }

    #[test]
    fn refusals() {
        let r = ring();
        // No key ring.
        assert!(matches!(
            spec_from_body(body(), "j", None, None),
            Err(SyncError::NoKeyRing)
        ));
        // Missing secret.
        let mut b = body();
        b.as_object_mut().unwrap().remove("secrets");
        assert!(matches!(spec_from_body(b, "j", None, Some(&r)), Err(SyncError::Invalid(_))));
        // Plaintext in the peer itself.
        let mut b = body();
        b["target"]["auth"]["token"] = json!("plain");
        assert!(matches!(spec_from_body(b, "j", None, Some(&r)), Err(SyncError::Invalid(_))));
        // Unknown secret path.
        let mut b = body();
        b["secrets"]["source.auth.token"] = json!({"set": "x"});
        assert!(matches!(spec_from_body(b, "j", None, Some(&r)), Err(SyncError::Invalid(_))));
        // Not an object.
        assert!(spec_from_body(json!([1]), "j", None, Some(&r)).is_err());
        // Clear on a required secret.
        let (spec, _) = spec_from_body(body(), "j", None, Some(&r)).unwrap();
        let mut b = body();
        b["secrets"]["target.auth.token"] = json!("clear");
        assert!(matches!(
            spec_from_body(b, "j", Some(&spec), Some(&r)),
            Err(SyncError::Invalid(_))
        ));
    }

    #[test]
    fn auth_types_and_rewrap() {
        let r1 = ring();
        let b = json!({
            "name": "pull",
            "source": {"kind": "external", "baseUrl": "https://src.example.org/fhir",
                       "auth": {"type": "client-credentials", "tokenUrl": "https://src.example.org/token",
                                "clientId": "c"}},
            "target": {"kind": "external", "baseUrl": "https://dst.example.org/fhir",
                       "auth": {"type": "basic", "username": "u"}},
            "trigger": {"type": "subscription", "fallbackPollSecs": 300},
            "secrets": {"source.auth.clientSecret": {"set": "cs"},
                        "target.auth.password": {"set": "pw"}}
        });
        let (mut spec, notify) = spec_from_body(b, "j", None, Some(&r1)).unwrap();
        let notify = notify.expect("fresh notify secret");
        assert_eq!(
            open_notify_secret(&spec, "j", Some(&r1)).unwrap().expose_str(),
            Some(notify.as_str())
        );
        // Updating keeps the notify secret (no new plaintext).
        let mut again = serde_json::to_value(&spec).unwrap();
        redact_sealed(&mut again);
        let (spec2, n2) = spec_from_body(again, "j", Some(&spec), Some(&r1)).unwrap();
        assert!(n2.is_none());
        assert_eq!(spec2.trigger, spec.trigger);

        // Rotation: prepend a new key, rewrap, old key can be dropped.
        let k2 = SecretKeyRing::generate_spec("k2");
        let old = SecretKeyRing::generate_spec("k1");
        let ring_old = SecretKeyRing::parse(&old).unwrap();
        let (mut s3, _) = spec_from_body(
            json!({"name": "x", "source": {"kind": "endpoint", "name": "a"},
                   "target": {"kind": "external", "baseUrl": "https://d.example.org",
                              "auth": {"type": "private-key-jwt", "tokenUrl": "https://d.example.org/t",
                                       "clientId": "c", "alg": "ES384"}},
                   "secrets": {"target.auth.privateKeyJwk": {"set": "{}"}}}),
            "j3",
            None,
            Some(&ring_old),
        )
        .unwrap();
        let both = SecretKeyRing::parse(&format!("{k2},{old}")).unwrap();
        assert_eq!(rewrap(&mut s3, "j3", &both).unwrap(), 1);
        let only_new = SecretKeyRing::parse(&k2).unwrap();
        let SyncPeer::External(x) = &s3.target else { panic!() };
        assert!(open_auth(&x.auth, "j3", "target", Some(&only_new)).is_ok());
        assert_eq!(rewrap(&mut s3, "j3", &both).unwrap(), 0);
        assert_eq!(rewrap(&mut spec, "j", &r1).unwrap(), 0);
        let SyncPeer::External(src) = &spec.source else { panic!() };
        assert!(matches!(
            open_auth(&src.auth, "j", "source", Some(&r1)).unwrap(),
            ResolvedAuth::ClientCredentials { .. }
        ));
    }

    #[test]
    fn constant_time_compare() {
        assert!(secrets_equal(b"abc", b"abc"));
        assert!(!secrets_equal(b"abc", b"abd"));
        assert!(!secrets_equal(b"abc", b"abcd"));
        assert_eq!(secret_field("none"), None);
    }
}
